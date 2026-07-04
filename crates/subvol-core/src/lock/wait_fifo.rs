use std::sync::Mutex;
use std::thread;

use super::six::SixLockType;

#[derive(Debug)]
pub struct WaiterBox {
    pub trans_id: u64,
    pub lock_type: SixLockType,
    pub seq: u64,
    pub thread: Option<thread::Thread>,
    pub lock_acquired: bool,
    pub lock_acquired_flag: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    pub percpu_slot: u32,
}

pub struct WaitFifo {
    inner: Mutex<Vec<WaiterBox>>,
}

impl WaitFifo {
    pub fn new(_size: u16, _rcu: &()) -> Self {
        Self {
            inner: Mutex::new(Vec::new()),
        }
    }

    pub fn push(
        &self,
        trans_id: u64,
        lock_type: SixLockType,
        seq: u64,
        thread: Option<thread::Thread>,
        percpu_slot: u32,
        lock_acquired_flag: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) -> Option<u16> {
        let mut slots = self.inner.lock().unwrap();
        let idx = slots.len() as u16;
        slots.push(WaiterBox {
            trans_id,
            lock_type,
            seq,
            thread,
            lock_acquired: false,
            lock_acquired_flag,
            percpu_slot,
        });
        Some(idx)
    }

    pub fn remove_by_thread(&self, thread_id: thread::ThreadId) -> Option<Box<WaiterBox>> {
        let mut slots = self.inner.lock().unwrap();
        let idx = slots.iter().position(|w| {
            w.thread
                .as_ref()
                .map(|t| t.id() == thread_id)
                .unwrap_or(false)
        });
        idx.map(|i| Box::new(slots.remove(i)))
    }

    pub fn remove(&self, trans_id: u64) -> Option<Box<WaiterBox>> {
        let mut slots = self.inner.lock().unwrap();
        let idx = slots.iter().position(|w| w.trans_id == trans_id);
        idx.map(|i| Box::new(slots.remove(i)))
    }

    pub fn remove_by_index(&self, idx: usize) {
        let mut slots = self.inner.lock().unwrap();
        if idx < slots.len() {
            slots.remove(idx);
        }
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.lock().unwrap().is_empty()
    }

    pub fn slots(&self) -> &[()] {
        &[]
    }

    pub fn resize(&mut self, _new_size: u16, _rcu: &()) {}
}

impl std::fmt::Debug for WaitFifo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WaitFifo")
            .field("len", &self.len())
            .finish()
    }
}

unsafe impl Send for WaitFifo {}
unsafe impl Sync for WaitFifo {}
