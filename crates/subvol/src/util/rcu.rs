use std::cell::Cell;
use std::sync::atomic::{fence, Ordering};

pub type rcu_callback_t = unsafe extern "C" fn(*mut rcu_head);

#[repr(C)]
pub struct rcu_head {
    pub next: *mut rcu_head,
    pub func: Option<rcu_callback_t>,
}

impl Default for rcu_head {
    fn default() -> Self {
        Self {
            next: core::ptr::null_mut(),
            func: None,
        }
    }
}

/* Keep the raw callback/head API needed by the bcachefs-shaped rhashtable on
 * the same liburcu memb implementation used by the safe `urcu` crate. */
#[link(name = "urcu-memb")]
unsafe extern "C" {
    fn urcu_memb_init();
    fn urcu_memb_register_thread();
    fn urcu_memb_unregister_thread();
    fn urcu_memb_read_lock();
    fn urcu_memb_read_unlock();
    fn urcu_memb_synchronize_rcu();
    fn urcu_memb_call_rcu(head: *mut rcu_head, func: rcu_callback_t);
}

thread_local! {
    static READ_DEPTH: Cell<usize> = const { Cell::new(0) };
    static EXTERNAL_REGISTRATION_DEPTH: Cell<usize> = const { Cell::new(0) };
}

/* `urcu::RcuThread` owns registration for engine read transactions.  The raw
 * bcachefs-shaped hash/iterator code may enter a short nested read section;
 * this bridge keeps it from registering the same userspace-RCU thread twice. */
pub(crate) fn rcu_external_registration_enter() {
    EXTERNAL_REGISTRATION_DEPTH.with(|depth| depth.set(depth.get() + 1));
}

pub(crate) fn rcu_external_registration_exit() {
    EXTERNAL_REGISTRATION_DEPTH.with(|depth| {
        debug_assert!(depth.get() != 0);
        depth.set(depth.get() - 1);
    });
}

fn rcu_external_registration_active() -> bool {
    EXTERNAL_REGISTRATION_DEPTH.with(|depth| depth.get() != 0)
}

pub fn rcu_read_lock() {
    READ_DEPTH.with(|depth| {
        if depth.get() == 0 && !rcu_external_registration_active() {
            unsafe {
                urcu_memb_init();
                urcu_memb_register_thread();
            }
        }
        unsafe { urcu_memb_read_lock() };
        depth.set(depth.get() + 1);
    });
}

pub fn rcu_read_unlock() {
    READ_DEPTH.with(|depth| {
        debug_assert!(depth.get() != 0);
        unsafe { urcu_memb_read_unlock() };
        depth.set(depth.get() - 1);
        if depth.get() == 0 && !rcu_external_registration_active() {
            unsafe { urcu_memb_unregister_thread() };
        }
    });
}

pub fn synchronize_rcu() {
    unsafe {
        urcu_memb_init();
        urcu_memb_synchronize_rcu()
    }
}

pub unsafe fn rcu_head_init(head: *mut rcu_head) {
    (*head).func = None;
    (*head).next = core::ptr::null_mut();
}

pub unsafe fn rcu_head_after_call_rcu(head: *const rcu_head, func: rcu_callback_t) -> bool {
    (*head).func.map(|current| current as usize) == Some(func as usize)
}

pub unsafe fn call_rcu(head: *mut rcu_head, func: rcu_callback_t) {
    urcu_memb_init();
    (*head).func = Some(func);
    urcu_memb_call_rcu(head, func);
}

pub unsafe fn rcu_assign_pointer<T>(dst: *mut *mut T, value: *mut T) {
    fence(Ordering::Release);
    core::ptr::write_volatile(dst, value);
}

pub unsafe fn rcu_dereference<T>(src: *const *mut T) -> *mut T {
    let value = core::ptr::read_volatile(src);
    fence(Ordering::Acquire);
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    static CALLBACKS: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn callback(_head: *mut rcu_head) {
        CALLBACKS.fetch_add(1, Ordering::AcqRel);
    }

    #[test]
    fn callback_waits_for_read_side_grace_period() {
        unsafe {
            CALLBACKS.store(0, Ordering::Release);
            let mut head = rcu_head::default();
            rcu_head_init(&mut head);
            rcu_read_lock();
            call_rcu(&mut head, callback);
            std::thread::sleep(Duration::from_millis(2));
            assert_eq!(CALLBACKS.load(Ordering::Acquire), 0);
            assert!(rcu_head_after_call_rcu(&head, callback));
            rcu_read_unlock();
            let deadline = Instant::now() + Duration::from_secs(1);
            while CALLBACKS.load(Ordering::Acquire) == 0 && Instant::now() < deadline {
                std::thread::yield_now();
            }
            assert_eq!(CALLBACKS.load(Ordering::Acquire), 1);
        }
    }

    #[test]
    fn assign_and_dereference_preserve_pointer_value() {
        unsafe {
            let mut value = 7u64;
            let mut pointer = core::ptr::null_mut();
            rcu_assign_pointer(&mut pointer, &mut value);
            assert_eq!(rcu_dereference(&pointer), core::ptr::addr_of_mut!(value));
        }
    }
}
