use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Mutex;
use std::thread::{self, Thread, ThreadId};

pub const SIX_LOCK_WANT_BITS: u32 = 2;
pub const SIX_LOCK_WANT_MASK: u64 = (1 << SIX_LOCK_WANT_BITS) - 1;
pub const SIX_LOCK_INLINE_WAITERS: usize = 8;
pub const SIX_LOCK_INIT_PCPU: u32 = 1 << 0;

const SIX_LOCK_HELD_READ: u32 = (1 << 26) - 1;
const SIX_LOCK_HELD_INTENT: u32 = 1 << 26;
const SIX_LOCK_HELD_WRITE: u32 = 1 << 27;
const SIX_LOCK_WAITING_READ: u32 = 1 << 28;
const SIX_LOCK_NOSPIN: u32 = 1 << 31;

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum six_lock_type {
    SIX_LOCK_read,
    SIX_LOCK_intent,
    SIX_LOCK_write,
}

impl Default for six_lock_type {
    fn default() -> Self {
        Self::SIX_LOCK_read
    }
}

pub struct six_lock_waiter {
    pub trans_start_time: u64,
    pub task: Option<Thread>,
    pub lock_want: six_lock_type,
    pub lock_acquired: AtomicBool,
    pub slot_idx: u16,
}

impl Default for six_lock_waiter {
    fn default() -> Self {
        Self {
            trans_start_time: 0,
            task: None,
            lock_want: six_lock_type::SIX_LOCK_read,
            lock_acquired: AtomicBool::new(false),
            slot_idx: 0,
        }
    }
}

#[derive(Clone, Copy)]
pub struct six_lock_wait_slot {
    pub w: *mut six_lock_waiter,
    pub start_time: u64,
}

unsafe impl Send for six_lock_wait_slot {}

impl Default for six_lock_wait_slot {
    fn default() -> Self {
        Self {
            w: core::ptr::null_mut(),
            start_time: 0,
        }
    }
}

pub struct six_lock_wait_fifo {
    pub size: u16,
    pub nr: u16,
    pub next_free_hint: u16,
    pub data: Vec<six_lock_wait_slot>,
}

impl Default for six_lock_wait_fifo {
    fn default() -> Self {
        Self {
            size: SIX_LOCK_INLINE_WAITERS as u16,
            nr: 0,
            next_free_hint: 0,
            data: vec![six_lock_wait_slot::default(); SIX_LOCK_INLINE_WAITERS],
        }
    }
}

pub struct six_lock {
    pub state: AtomicU32,
    pub seq: AtomicU32,
    pub readers: Option<Vec<AtomicU32>>,
    pub intent_lock_recurse: AtomicU32,
    pub write_lock_recurse: AtomicU32,
    pub owner: Mutex<Option<ThreadId>>,
    pub wait_lock: Mutex<six_lock_wait_fifo>,
}

impl Default for six_lock {
    fn default() -> Self {
        Self {
            state: AtomicU32::new(0),
            seq: AtomicU32::new(0),
            readers: None,
            intent_lock_recurse: AtomicU32::new(0),
            write_lock_recurse: AtomicU32::new(0),
            owner: Mutex::new(None),
            wait_lock: Mutex::new(six_lock_wait_fifo::default()),
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct six_lock_count {
    pub n: [u32; 3],
}

pub type six_lock_should_sleep_fn = fn(&six_lock, &mut six_lock_waiter) -> i32;

const fn lock_val(lock_type: six_lock_type) -> u32 {
    match lock_type {
        six_lock_type::SIX_LOCK_read => 1,
        six_lock_type::SIX_LOCK_intent => SIX_LOCK_HELD_INTENT,
        six_lock_type::SIX_LOCK_write => SIX_LOCK_HELD_WRITE,
    }
}

const fn lock_fail(lock_type: six_lock_type) -> u32 {
    match lock_type {
        six_lock_type::SIX_LOCK_read => SIX_LOCK_HELD_WRITE,
        six_lock_type::SIX_LOCK_intent => SIX_LOCK_HELD_INTENT,
        six_lock_type::SIX_LOCK_write => SIX_LOCK_HELD_READ,
    }
}

const fn held_mask(lock_type: six_lock_type) -> u32 {
    match lock_type {
        six_lock_type::SIX_LOCK_read => SIX_LOCK_HELD_READ,
        six_lock_type::SIX_LOCK_intent => SIX_LOCK_HELD_INTENT,
        six_lock_type::SIX_LOCK_write => SIX_LOCK_HELD_WRITE,
    }
}

const fn unlock_wakeup(lock_type: six_lock_type) -> six_lock_type {
    match lock_type {
        six_lock_type::SIX_LOCK_read => six_lock_type::SIX_LOCK_write,
        six_lock_type::SIX_LOCK_intent => six_lock_type::SIX_LOCK_intent,
        six_lock_type::SIX_LOCK_write => six_lock_type::SIX_LOCK_read,
    }
}

const fn waiting_mask(lock_type: six_lock_type) -> u32 {
    SIX_LOCK_WAITING_READ << lock_type as u32
}

pub fn __six_lock_init(lock: &mut six_lock, flags: u32) {
    *lock = six_lock::default();
    if flags & SIX_LOCK_INIT_PCPU != 0 {
        let nr = std::thread::available_parallelism()
            .map(|value| value.get())
            .unwrap_or(1);
        lock.readers = Some((0..nr).map(|_| AtomicU32::new(0)).collect());
    }
}

pub fn six_lock_init(lock: &mut six_lock, flags: u32) {
    __six_lock_init(lock, flags);
}

pub fn six_lock_exit(lock: &mut six_lock) {
    assert_eq!(lock.state.load(Ordering::Relaxed) & SIX_LOCK_HELD_READ, 0);
    lock.readers = None;
    lock.wait_lock.get_mut().unwrap().data.clear();
}

pub fn six_lock_seq(lock: &six_lock) -> u32 {
    lock.seq.load(Ordering::Relaxed)
}

fn six_set_owner(lock: &six_lock, lock_type: six_lock_type, old: u32, owner: ThreadId) {
    if lock_type != six_lock_type::SIX_LOCK_intent {
        return;
    }

    let mut current_owner = lock.owner.lock().unwrap();
    if old & SIX_LOCK_HELD_INTENT == 0 {
        assert!(current_owner.is_none());
        *current_owner = Some(owner);
    } else {
        assert_eq!(*current_owner, Some(owner));
    }
}

fn __do_six_trylock(
    lock: &six_lock,
    lock_type: six_lock_type,
    task: ThreadId,
    try_lock: bool,
) -> bool {
    if lock_type == six_lock_type::SIX_LOCK_write {
        assert_eq!(*lock.owner.lock().unwrap(), Some(task));
        assert_eq!(
            try_lock,
            lock.state.load(Ordering::Relaxed) & SIX_LOCK_HELD_WRITE == 0
        );
    }

    let mut old = lock.state.load(Ordering::Relaxed);
    loop {
        let ret = old & lock_fail(lock_type) == 0;
        if !ret || (lock_type == six_lock_type::SIX_LOCK_write && !try_lock) {
            if ret {
                six_set_owner(lock, lock_type, old, task);
            }
            return ret;
        }

        match lock.state.compare_exchange_weak(
            old,
            old.wrapping_add(lock_val(lock_type)),
            Ordering::Acquire,
            Ordering::Relaxed,
        ) {
            Ok(_) => {
                assert_ne!(lock.state.load(Ordering::Relaxed) & held_mask(lock_type), 0);
                six_set_owner(lock, lock_type, old, task);
                if lock_type == six_lock_type::SIX_LOCK_read {
                    let node = (lock as *const six_lock as usize)
                        - core::mem::offset_of!(super::super::btree::types::btree, c.lock);
                    crate::rewrite_log_debug!(
                        "six read acquire node={node:#x} count={}",
                        lock.state.load(Ordering::Relaxed) & SIX_LOCK_HELD_READ
                    );
                }
                return true;
            }
            Err(new_old) => old = new_old,
        }
    }
}

fn six_lock_wait_fifo_shrink(wf: &mut six_lock_wait_fifo) {
    while wf.nr > 0 && wf.data[wf.nr as usize - 1].w.is_null() {
        wf.nr -= 1;
    }
    if wf.next_free_hint > wf.nr {
        wf.next_free_hint = wf.nr;
    }
}

fn six_lock_wait_fifo_remove(wf: &mut six_lock_wait_fifo, idx: u16) {
    wf.data[idx as usize].w = core::ptr::null_mut();
    wf.next_free_hint = wf.next_free_hint.min(idx);
}

fn six_lock_wait_fifo_insert(wf: &mut six_lock_wait_fifo, wait: &mut six_lock_waiter) {
    let mut i = wf.next_free_hint;
    if !wf.data[i as usize].w.is_null() {
        i = 0;
        while i < wf.nr && !wf.data[i as usize].w.is_null() {
            i += 1;
        }

        if i == wf.size {
            assert!(wf.size < 1 << 15);
            wf.size *= 2;
            wf.data
                .resize(wf.size as usize, six_lock_wait_slot::default());
        }
    }

    wf.data[i as usize].w = wait;
    wf.data[i as usize].start_time = (wait.trans_start_time << SIX_LOCK_WANT_BITS)
        | (wait.lock_want as u64 & SIX_LOCK_WANT_MASK);
    wait.slot_idx = i;
    wf.next_free_hint = (i + 1) & (wf.size - 1);
    wf.nr = wf.nr.max(i + 1);
}

fn __six_lock_wakeup(lock: &six_lock, wf: &mut six_lock_wait_fifo, lock_type: six_lock_type) {
    if lock_type == six_lock_type::SIX_LOCK_read {
        let mut i = 0;
        while i < wf.nr {
            let slot = wf.data[i as usize];
            if slot.w.is_null() || slot.start_time & SIX_LOCK_WANT_MASK != lock_type as u64 {
                i += 1;
                continue;
            }

            let wait = unsafe { &mut *slot.w };
            let task = wait.task.as_ref().unwrap();
            if !__do_six_trylock(lock, lock_type, task.id(), false) {
                break;
            }

            six_lock_wait_fifo_remove(wf, i);
            wait.lock_acquired.store(true, Ordering::Release);
            task.unpark();
            i += 1;
        }
    } else {
        let mut oldest = None;
        let mut n_matches = 0;

        for i in 0..wf.nr {
            let slot = wf.data[i as usize];
            if slot.w.is_null() || slot.start_time & SIX_LOCK_WANT_MASK != lock_type as u64 {
                continue;
            }
            n_matches += 1;
            if oldest.map_or(true, |oldest_i: u16| {
                slot.start_time < wf.data[oldest_i as usize].start_time
            }) {
                oldest = Some(i);
            }
        }

        if let Some(i) = oldest {
            let wait = unsafe { &mut *wf.data[i as usize].w };
            let task = wait.task.as_ref().unwrap();
            if __do_six_trylock(lock, lock_type, task.id(), false) {
                six_lock_wait_fifo_remove(wf, i);
                wait.lock_acquired.store(true, Ordering::Release);
                task.unpark();
                if n_matches > 1 {
                    six_lock_wait_fifo_shrink(wf);
                    return;
                }
            }
        }
    }

    lock.state
        .fetch_and(!waiting_mask(lock_type), Ordering::Relaxed);
    six_lock_wait_fifo_shrink(wf);
}

fn six_lock_wakeup(lock: &six_lock, state: u32, lock_type: six_lock_type) {
    if lock_type == six_lock_type::SIX_LOCK_write && state & SIX_LOCK_HELD_READ != 0 {
        return;
    }
    if state & waiting_mask(lock_type) == 0 {
        return;
    }

    let mut wf = lock.wait_lock.lock().unwrap();
    __six_lock_wakeup(lock, &mut wf, lock_type);
}

pub fn six_trylock_ip(lock: &six_lock, lock_type: six_lock_type, _ip: usize) -> bool {
    __do_six_trylock(lock, lock_type, thread::current().id(), true)
}

pub fn six_trylock_type(lock: &six_lock, lock_type: six_lock_type) -> bool {
    six_trylock_ip(lock, lock_type, 0)
}

pub fn six_relock_ip(lock: &six_lock, lock_type: six_lock_type, seq: u32, ip: usize) -> bool {
    if six_lock_seq(lock) != seq || !six_trylock_ip(lock, lock_type, ip) {
        return false;
    }

    if six_lock_seq(lock) != seq {
        six_unlock_ip(lock, lock_type, ip);
        return false;
    }

    true
}

pub fn six_relock_type(lock: &six_lock, lock_type: six_lock_type, seq: u32) -> bool {
    six_relock_ip(lock, lock_type, seq, 0)
}

fn do_six_unlock_type(lock: &six_lock, lock_type: six_lock_type) {
    if lock_type == six_lock_type::SIX_LOCK_intent {
        *lock.owner.lock().unwrap() = None;
    }

    let mut v = lock_val(lock_type);
    if lock_type != six_lock_type::SIX_LOCK_read {
        v += lock.state.load(Ordering::Relaxed) & SIX_LOCK_NOSPIN;
    }

    assert_ne!(lock.state.load(Ordering::Relaxed) & held_mask(lock_type), 0);
    let state = lock.state.fetch_sub(v, Ordering::Release).wrapping_sub(v);
    if lock_type == six_lock_type::SIX_LOCK_read {
        let node = (lock as *const six_lock as usize)
            - core::mem::offset_of!(super::super::btree::types::btree, c.lock);
        crate::rewrite_log_debug!(
            "six read release node={node:#x} count={}",
            state & SIX_LOCK_HELD_READ
        );
    }
    six_lock_wakeup(lock, state, unlock_wakeup(lock_type));
}

pub fn six_unlock_ip(lock: &six_lock, lock_type: six_lock_type, _ip: usize) {
    if lock_type == six_lock_type::SIX_LOCK_write {
        assert_ne!(lock.state.load(Ordering::Relaxed) & SIX_LOCK_HELD_INTENT, 0);
    }
    if lock_type == six_lock_type::SIX_LOCK_write || lock_type == six_lock_type::SIX_LOCK_intent {
        assert_eq!(*lock.owner.lock().unwrap(), Some(thread::current().id()));
    }

    if lock_type == six_lock_type::SIX_LOCK_intent
        && lock.intent_lock_recurse.load(Ordering::Relaxed) != 0
    {
        lock.intent_lock_recurse.fetch_sub(1, Ordering::Relaxed);
        return;
    }

    if lock_type == six_lock_type::SIX_LOCK_write
        && lock.write_lock_recurse.load(Ordering::Relaxed) != 0
    {
        lock.write_lock_recurse.fetch_sub(1, Ordering::Relaxed);
        return;
    }

    if lock_type == six_lock_type::SIX_LOCK_write {
        lock.seq.fetch_add(1, Ordering::Relaxed);
    }

    do_six_unlock_type(lock, lock_type);
}

pub fn six_unlock_type(lock: &six_lock, lock_type: six_lock_type) {
    six_unlock_ip(lock, lock_type, 0);
}

fn __six_lock_slowpath(
    lock: &six_lock,
    lock_type: six_lock_type,
    wait: &mut six_lock_waiter,
    should_sleep_fn: Option<six_lock_should_sleep_fn>,
) -> i32 {
    if lock_type == six_lock_type::SIX_LOCK_write {
        assert_eq!(lock.state.load(Ordering::Relaxed) & SIX_LOCK_HELD_WRITE, 0);
        lock.state.fetch_add(SIX_LOCK_HELD_WRITE, Ordering::Relaxed);
    }

    wait.task = Some(thread::current());
    wait.lock_want = lock_type;
    wait.lock_acquired.store(false, Ordering::Relaxed);

    {
        let mut wf = lock.wait_lock.lock().unwrap();
        lock.state
            .fetch_or(waiting_mask(lock_type), Ordering::Relaxed);
        if __do_six_trylock(lock, lock_type, thread::current().id(), false) {
            return 0;
        }
        six_lock_wait_fifo_insert(&mut wf, wait);
    }

    loop {
        if wait.lock_acquired.load(Ordering::Acquire) {
            return 0;
        }

        thread::park();

        if wait.lock_acquired.load(Ordering::Acquire) {
            return 0;
        }

        let ret = should_sleep_fn.map_or(0, |f| f(lock, wait));
        if ret != 0 {
            let mut wf = lock.wait_lock.lock().unwrap();
            let acquired = wait.lock_acquired.load(Ordering::Relaxed);
            if !acquired {
                six_lock_wait_fifo_remove(&mut wf, wait.slot_idx);
                six_lock_wait_fifo_shrink(&mut wf);
            }
            drop(wf);

            if acquired {
                do_six_unlock_type(lock, lock_type);
            } else if lock_type == six_lock_type::SIX_LOCK_write {
                lock.state
                    .fetch_and(!SIX_LOCK_HELD_WRITE, Ordering::Relaxed);
                let state = lock.state.load(Ordering::Relaxed);
                six_lock_wakeup(lock, state, six_lock_type::SIX_LOCK_read);
            }
            return ret;
        }
    }
}

pub fn six_lock_ip_waiter(
    lock: &six_lock,
    lock_type: six_lock_type,
    wait: &mut six_lock_waiter,
    should_sleep_fn: Option<six_lock_should_sleep_fn>,
    _ip: usize,
) -> i32 {
    if __do_six_trylock(lock, lock_type, thread::current().id(), true) {
        0
    } else {
        __six_lock_slowpath(lock, lock_type, wait, should_sleep_fn)
    }
}

pub fn six_lock_waiter(
    lock: &six_lock,
    lock_type: six_lock_type,
    wait: &mut six_lock_waiter,
    should_sleep_fn: Option<six_lock_should_sleep_fn>,
) -> i32 {
    six_lock_ip_waiter(lock, lock_type, wait, should_sleep_fn, 0)
}

pub fn six_lock_contended(
    lock: &six_lock,
    lock_type: six_lock_type,
    wait: &mut six_lock_waiter,
    should_sleep_fn: Option<six_lock_should_sleep_fn>,
    ip: usize,
) -> i32 {
    let _ = ip;
    __six_lock_slowpath(lock, lock_type, wait, should_sleep_fn)
}

pub fn six_lock_downgrade(lock: &six_lock) {
    six_lock_increment(lock, six_lock_type::SIX_LOCK_read);
    six_unlock_type(lock, six_lock_type::SIX_LOCK_intent);
}

pub fn six_lock_tryupgrade(lock: &six_lock) -> bool {
    let mut old = lock.state.load(Ordering::Relaxed);
    loop {
        let mut new = old;
        if new & SIX_LOCK_HELD_INTENT != 0 {
            return false;
        }
        assert_ne!(new & SIX_LOCK_HELD_READ, 0);
        new -= lock_val(six_lock_type::SIX_LOCK_read);
        new |= SIX_LOCK_HELD_INTENT;

        match lock
            .state
            .compare_exchange_weak(old, new, Ordering::Acquire, Ordering::Relaxed)
        {
            Ok(_) => {
                six_set_owner(
                    lock,
                    six_lock_type::SIX_LOCK_intent,
                    old,
                    thread::current().id(),
                );
                return true;
            }
            Err(new_old) => old = new_old,
        }
    }
}

pub fn six_trylock_convert(lock: &six_lock, from: six_lock_type, to: six_lock_type) -> bool {
    assert_ne!(to, six_lock_type::SIX_LOCK_write);
    assert_ne!(from, six_lock_type::SIX_LOCK_write);

    if to == from {
        true
    } else if to == six_lock_type::SIX_LOCK_read {
        six_lock_downgrade(lock);
        true
    } else {
        six_lock_tryupgrade(lock)
    }
}

pub fn six_lock_increment(lock: &six_lock, lock_type: six_lock_type) {
    match lock_type {
        six_lock_type::SIX_LOCK_read => {
            assert_ne!(
                lock.state.load(Ordering::Relaxed) & (SIX_LOCK_HELD_READ | SIX_LOCK_HELD_INTENT),
                0
            );
            lock.state.fetch_add(lock_val(lock_type), Ordering::Relaxed);
            let node = (lock as *const six_lock as usize)
                - core::mem::offset_of!(super::super::btree::types::btree, c.lock);
            crate::rewrite_log_debug!(
                "six read increment node={node:#x} count={}",
                lock.state.load(Ordering::Relaxed) & SIX_LOCK_HELD_READ
            );
        }
        six_lock_type::SIX_LOCK_write => {
            lock.write_lock_recurse.fetch_add(1, Ordering::Relaxed);
            assert_ne!(lock.state.load(Ordering::Relaxed) & SIX_LOCK_HELD_INTENT, 0);
            lock.intent_lock_recurse.fetch_add(1, Ordering::Relaxed);
        }
        six_lock_type::SIX_LOCK_intent => {
            assert_ne!(lock.state.load(Ordering::Relaxed) & SIX_LOCK_HELD_INTENT, 0);
            lock.intent_lock_recurse.fetch_add(1, Ordering::Relaxed);
        }
    }
}

pub fn six_lock_wakeup_all(lock: &six_lock) {
    let state = lock.state.load(Ordering::Relaxed);
    six_lock_wakeup(lock, state, six_lock_type::SIX_LOCK_read);
    six_lock_wakeup(lock, state, six_lock_type::SIX_LOCK_intent);
    six_lock_wakeup(lock, state, six_lock_type::SIX_LOCK_write);

    let wf = lock.wait_lock.lock().unwrap();
    for slot in &wf.data[..wf.nr as usize] {
        if !slot.w.is_null() {
            unsafe { (*slot.w).task.as_ref().unwrap().unpark() };
        }
    }
}

pub fn six_lock_counts(lock: &six_lock) -> six_lock_count {
    let state = lock.state.load(Ordering::Relaxed);
    six_lock_count {
        n: [
            state & SIX_LOCK_HELD_READ,
            u32::from(state & SIX_LOCK_HELD_INTENT != 0)
                + lock.intent_lock_recurse.load(Ordering::Relaxed),
            u32::from(state & SIX_LOCK_HELD_WRITE != 0),
        ],
    }
}

pub fn six_lock_readers_add(lock: &six_lock, nr: i32) {
    let state = lock.state.load(Ordering::Relaxed);
    assert!((state & SIX_LOCK_HELD_READ) as i32 + nr >= 0);
    if nr >= 0 {
        lock.state.fetch_add(nr as u32, Ordering::Relaxed);
    } else {
        lock.state.fetch_sub(nr.unsigned_abs(), Ordering::Relaxed);
    }
}

macro_rules! six_type_wrappers {
    ($try_name:ident, $relock_name:ident, $unlock_name:ident, $lock_name:ident, $type:ident) => {
        pub fn $try_name(lock: &six_lock) -> bool {
            six_trylock_type(lock, six_lock_type::$type)
        }

        pub fn $relock_name(lock: &six_lock, seq: u32) -> bool {
            six_relock_type(lock, six_lock_type::$type, seq)
        }

        pub fn $unlock_name(lock: &six_lock) {
            six_unlock_type(lock, six_lock_type::$type);
        }

        pub fn $lock_name(lock: &six_lock) -> i32 {
            let mut wait = six_lock_waiter::default();
            six_lock_waiter(lock, six_lock_type::$type, &mut wait, None)
        }
    };
}

six_type_wrappers!(
    six_trylock_read,
    six_relock_read,
    six_unlock_read,
    six_lock_read,
    SIX_LOCK_read
);
six_type_wrappers!(
    six_trylock_intent,
    six_relock_intent,
    six_unlock_intent,
    six_lock_intent,
    SIX_LOCK_intent
);
six_type_wrappers!(
    six_trylock_write,
    six_relock_write,
    six_unlock_write,
    six_lock_write,
    SIX_LOCK_write
);

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn bcachefs_six_compatibility_and_sequence() {
        let lock = six_lock::default();
        assert!(six_trylock_read(&lock));
        assert!(six_trylock_intent(&lock));
        assert!(!six_trylock_write(&lock));
        six_unlock_read(&lock);
        assert!(six_trylock_write(&lock));
        let seq = six_lock_seq(&lock);
        six_unlock_write(&lock);
        assert_eq!(six_lock_seq(&lock), seq + 1);
        six_unlock_intent(&lock);
        assert_eq!(six_lock_counts(&lock).n, [0, 0, 0]);
    }

    #[test]
    fn bcachefs_six_upgrade_downgrade_and_relock() {
        let lock = six_lock::default();
        assert!(six_trylock_read(&lock));
        assert!(six_lock_tryupgrade(&lock));
        assert_eq!(six_lock_counts(&lock).n, [0, 1, 0]);
        six_lock_downgrade(&lock);
        assert_eq!(six_lock_counts(&lock).n, [1, 0, 0]);
        let seq = six_lock_seq(&lock);
        six_unlock_read(&lock);
        assert!(six_relock_read(&lock, seq));
        six_unlock_read(&lock);
    }

    #[test]
    fn bcachefs_six_blocking_writer_waits_for_readers() {
        use std::sync::mpsc;

        let lock = Arc::new(six_lock::default());
        assert_eq!(six_lock_read(&lock), 0);

        let worker_lock = lock.clone();
        let (intent_tx, intent_rx) = mpsc::channel();
        let (write_tx, write_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            assert_eq!(six_lock_intent(&worker_lock), 0);
            intent_tx.send(()).unwrap();
            assert_eq!(six_lock_write(&worker_lock), 0);
            write_tx.send(()).unwrap();
            six_unlock_write(&worker_lock);
            six_unlock_intent(&worker_lock);
        });

        intent_rx.recv().unwrap();
        assert!(write_rx.try_recv().is_err());
        six_unlock_read(&lock);
        write_rx.recv().unwrap();
        worker.join().unwrap();
        assert_eq!(six_lock_counts(&lock).n, [0, 0, 0]);
    }
}
