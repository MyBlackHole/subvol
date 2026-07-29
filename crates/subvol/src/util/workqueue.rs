use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::thread::{self, JoinHandle};

pub const WORK_PENDING_BIT: usize = 0;

pub type work_func_t = unsafe fn(*mut work_struct);

#[repr(C)]
pub struct work_struct {
    pub data: AtomicUsize,
    pub entry: crate::btree::types::list_head,
    pub func: Option<work_func_t>,
}

impl Default for work_struct {
    fn default() -> Self {
        Self {
            data: AtomicUsize::new(0),
            entry: Default::default(),
            func: None,
        }
    }
}

#[repr(C)]
pub struct workqueue_struct {
    pub list: crate::btree::types::list_head,
    pub current_work: AtomicPtr<work_struct>,
    pub pending_work: Mutex<VecDeque<usize>>,
    pub worker: Mutex<Option<JoinHandle<()>>>,
    pub name: [u8; 24],
    shutdown: AtomicBool,
    wake: Condvar,
}

unsafe impl Send for workqueue_struct {}
unsafe impl Sync for workqueue_struct {}

impl workqueue_struct {
    fn new(name: &str) -> *mut Self {
        let mut name_buf = [0u8; 24];
        let bytes = name.as_bytes();
        let len = bytes.len().min(name_buf.len() - 1);
        name_buf[..len].copy_from_slice(&bytes[..len]);
        let queue = Box::new(Self {
            list: Default::default(),
            current_work: AtomicPtr::new(core::ptr::null_mut()),
            pending_work: Mutex::new(VecDeque::new()),
            worker: Mutex::new(None),
            name: name_buf,
            shutdown: AtomicBool::new(false),
            wake: Condvar::new(),
        });
        let queue = Box::into_raw(queue);
        all_workqueues().lock().unwrap().push(queue as usize);
        let thread_addr = queue as usize;
        let worker = thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || unsafe { worker_thread(thread_addr as *mut workqueue_struct) })
            .unwrap();
        (*unsafe { &*queue }.worker.lock().unwrap()) = Some(worker);
        queue
    }
}

unsafe fn worker_thread(wq: *mut workqueue_struct) {
    loop {
        let work = {
            let mut pending = (*wq).pending_work.lock().unwrap();
            loop {
                if let Some(work) = pending.pop_front() {
                    break work as *mut work_struct;
                }
                if (*wq).shutdown.load(Ordering::Acquire) {
                    return;
                }
                pending = (*wq).wake.wait(pending).unwrap();
            }
        };

        (*wq).current_work.store(work, Ordering::Release);
        (*work)
            .data
            .fetch_and(!(1usize << WORK_PENDING_BIT), Ordering::AcqRel);
        if let Some(func) = (*work).func {
            func(work);
        }
        (*wq)
            .current_work
            .store(core::ptr::null_mut(), Ordering::Release);
        (*wq).wake.notify_all();
    }
}

pub unsafe fn INIT_WORK(work: *mut work_struct, func: work_func_t) {
    (*work).data.store(0, Ordering::Release);
    crate::btree::types::INIT_LIST_HEAD(&mut (*work).entry);
    (*work).func = Some(func);
}

pub unsafe fn work_pending(work: *const work_struct) -> bool {
    (*work).data.load(Ordering::Acquire) & (1usize << WORK_PENDING_BIT) != 0
}

pub fn alloc_workqueue(name: &str) -> *mut workqueue_struct {
    workqueue_struct::new(name)
}

pub unsafe fn queue_work(wq: *mut workqueue_struct, work: *mut work_struct) -> bool {
    let old = (*work)
        .data
        .fetch_or(1usize << WORK_PENDING_BIT, Ordering::AcqRel);
    if old & (1usize << WORK_PENDING_BIT) != 0 {
        return false;
    }
    (*wq).pending_work.lock().unwrap().push_back(work as usize);
    (*wq).wake.notify_one();
    true
}

pub unsafe fn flush_work(work: *mut work_struct) -> bool {
    let mut flushed = false;
    loop {
        let queues = all_workqueues().lock().unwrap().clone();
        let mut wait_queue = core::ptr::null_mut();
        for queue in queues {
            let wq = queue as *mut workqueue_struct;
            let pending = (*wq).pending_work.lock().unwrap();
            let is_current = (*wq).current_work.load(Ordering::Acquire) == work;
            let is_pending = pending.iter().any(|&item| item == work as usize);
            drop(pending);
            if is_current || is_pending {
                wait_queue = wq;
                break;
            }
        }
        if wait_queue.is_null() {
            break;
        }
        flushed = true;
        let pending = (*wait_queue).pending_work.lock().unwrap();
        let _pending = (*wait_queue).wake.wait(pending).unwrap();
    }
    flushed
}

pub unsafe fn cancel_work_sync(work: *mut work_struct) -> bool {
    let mut cancelled = false;
    let queues = all_workqueues().lock().unwrap().clone();
    for queue in queues {
        let wq = queue as *mut workqueue_struct;
        let mut pending = (*wq).pending_work.lock().unwrap();
        if let Some(index) = pending.iter().position(|&item| item == work as usize) {
            pending.remove(index);
            (*work)
                .data
                .fetch_and(!(1usize << WORK_PENDING_BIT), Ordering::AcqRel);
            cancelled = true;
        }
        while (*wq).current_work.load(Ordering::Acquire) == work {
            pending = (*wq).wake.wait(pending).unwrap();
            cancelled = true;
        }
    }
    cancelled
}

pub unsafe fn schedule_work(work: *mut work_struct) -> bool {
    queue_work(system_wq(), work)
}

pub unsafe fn destroy_workqueue(wq: *mut workqueue_struct) {
    (*wq).shutdown.store(true, Ordering::Release);
    (*wq).wake.notify_all();
    if let Some(worker) = (*wq).worker.lock().unwrap().take() {
        worker.join().unwrap();
    }
    assert!((*wq).pending_work.lock().unwrap().is_empty());
    all_workqueues()
        .lock()
        .unwrap()
        .retain(|&queue| queue != wq as usize);
    drop(Box::from_raw(wq));
}

pub unsafe fn drain_workqueue(wq: *mut workqueue_struct) {
    let mut pending = (*wq).pending_work.lock().unwrap();
    while !pending.is_empty() || !(*wq).current_work.load(Ordering::Acquire).is_null() {
        pending = (*wq).wake.wait(pending).unwrap();
    }
}

fn system_wq_cell() -> &'static OnceLock<usize> {
    static SYSTEM_WQ: OnceLock<usize> = OnceLock::new();
    &SYSTEM_WQ
}

fn all_workqueues() -> &'static Mutex<Vec<usize>> {
    static ALL_WORKQUEUES: OnceLock<Mutex<Vec<usize>>> = OnceLock::new();
    ALL_WORKQUEUES.get_or_init(|| Mutex::new(Vec::new()))
}

pub unsafe fn system_wq() -> *mut workqueue_struct {
    *system_wq_cell().get_or_init(|| workqueue_struct::new("system_wq") as usize)
        as *mut workqueue_struct
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    static RUNS: AtomicUsize = AtomicUsize::new(0);

    unsafe fn increment(_work: *mut work_struct) {
        RUNS.fetch_add(1, Ordering::AcqRel);
    }

    #[test]
    fn workqueue_runs_pending_work_once_and_flushes() {
        unsafe {
            RUNS.store(0, Ordering::Release);
            let wq = alloc_workqueue("test_workqueue");
            let mut work = work_struct::default();
            INIT_WORK(&mut work, increment);
            assert!(queue_work(wq, &mut work));
            assert!(!queue_work(wq, &mut work));
            assert!(flush_work(&mut work));
            assert_eq!(RUNS.load(Ordering::Acquire), 1);
            assert!(!work_pending(&work));
            drain_workqueue(wq);
            destroy_workqueue(wq);
        }
    }
}
