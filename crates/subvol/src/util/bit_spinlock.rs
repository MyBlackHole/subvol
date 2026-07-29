use std::sync::atomic::{AtomicUsize, Ordering};

pub unsafe fn bit_spin_lock(nr: usize, addr: *const AtomicUsize) {
    let mask = 1usize << (nr % usize::BITS as usize);
    loop {
        let old = (*addr).fetch_or(mask, Ordering::Acquire);
        if old & mask == 0 {
            return;
        }
        std::hint::spin_loop();
        std::thread::yield_now();
    }
}

pub unsafe fn bit_spin_wake(_nr: usize, _addr: *const AtomicUsize) {}

pub unsafe fn bit_spin_unlock(nr: usize, addr: *const AtomicUsize) {
    let mask = !(1usize << (nr % usize::BITS as usize));
    (*addr).fetch_and(mask, Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn bit_lock_uses_low_bit_and_releases_with_ordering() {
        let word = Arc::new(AtomicUsize::new(0));
        unsafe {
            bit_spin_lock(0, &*word);
            assert_eq!(word.load(Ordering::Acquire) & 1, 1);
            bit_spin_unlock(0, &*word);
            bit_spin_wake(0, &*word);
            assert_eq!(word.load(Ordering::Acquire) & 1, 0);
        }
    }
}
