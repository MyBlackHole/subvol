use std::cell::UnsafeCell;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex as StdMutex;
use std::thread::{self, Thread, ThreadId};

/* ── 状态位布局（与 bcachefs six.h 完全一致）── */

const SIX_LOCK_HELD_read_SHIFT: u32 = 0;
const SIX_LOCK_HELD_read_MASK: u32 = (1 << 26) - 1;
const SIX_LOCK_HELD_intent: u32 = 1 << 26;
const SIX_LOCK_HELD_write: u32 = 1 << 27;
const SIX_LOCK_WAITING_read: u32 = 1 << 28;
const SIX_LOCK_WAITING_intent: u32 = 1 << 29;
const SIX_LOCK_WAITING_write: u32 = 1 << 30;
const SIX_LOCK_NOSPIN: u32 = 1 << 31;

/* ── 锁类型 ── */

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SixLockType {
    Read = 0,
    Intent = 1,
    Write = 2,
}

/* ── 等待者（与 bcachefs six_lock_waiter 对应）── */

#[derive(Clone, Debug)]
pub struct SixLockWaiter {
    pub trans_start_time: u64,
    pub thread: Option<Thread>,
    pub lock_want: SixLockType,
    pub lock_acquired: bool,
    pub slot_idx: u16,
}

/* ── 锁计数 ── */

#[derive(Clone, Copy, Debug, Default)]
pub struct SixLockCount {
    pub n: [u32; 3],
}

/* ── 返回类型 ── */

pub type SixLockResult = i32;

/// should_sleep 回调类型：在 park 前调用，返回 0=继续等待，非0=中止
pub type SixLockShouldSleepFn = Box<dyn Fn(&SixLock, &SixLockWaiter) -> i32>;

/* ── thread_local should_sleep ── */

thread_local! {
    static THREAD_SHOULD_SLEEP: UnsafeCell<Option<SixLockShouldSleepFn>> =
        const { UnsafeCell::new(None) };
}

pub fn sx_set_thread_should_sleep(f: Option<SixLockShouldSleepFn>) {
    THREAD_SHOULD_SLEEP.with(|cell| {
        unsafe { *cell.get() = f };
    });
}

fn call_should_sleep(lock: &SixLock, waiter: &SixLockWaiter) -> i32 {
    THREAD_SHOULD_SLEEP.with(|cell| {
        let f = unsafe { &*cell.get() };
        match f {
            Some(cb) => cb(lock, waiter),
            None => 0,
        }
    })
}

/* ── 锁结构 ── */

pub struct SixLock {
    state: AtomicU32,
    seq: AtomicU64, // u64 以适配现有代码（bcachefs 中为 u32）
    intent_lock_recurse: AtomicU32,
    write_lock_recurse: AtomicU32,
    owner_tid: AtomicU32,

    wait_lock: StdMutex<()>,
    waiters: StdMutex<VecDeque<SixLockWaiter>>,
}

impl SixLock {
    pub fn new() -> Self {
        Self {
            state: AtomicU32::new(0),
            seq: AtomicU64::new(0),
            intent_lock_recurse: AtomicU32::new(0),
            write_lock_recurse: AtomicU32::new(0),
            owner_tid: AtomicU32::new(0),
            wait_lock: StdMutex::new(()),
            waiters: StdMutex::new(VecDeque::new()),
        }
    }
}

/* ── 锁值表（与 bcachefs six.c l[] 一致）── */

struct LockVals {
    lock_val: u32,
    lock_fail: u32,
    held_mask: u32,
    unlock_wakeup: SixLockType,
}

const L: [LockVals; 3] = [
    LockVals {
        lock_val: 1 << SIX_LOCK_HELD_read_SHIFT,
        lock_fail: SIX_LOCK_HELD_write,
        held_mask: SIX_LOCK_HELD_read_MASK,
        unlock_wakeup: SixLockType::Write,
    },
    LockVals {
        lock_val: SIX_LOCK_HELD_intent,
        lock_fail: SIX_LOCK_HELD_intent,
        held_mask: SIX_LOCK_HELD_intent,
        unlock_wakeup: SixLockType::Intent,
    },
    LockVals {
        lock_val: SIX_LOCK_HELD_write,
        lock_fail: SIX_LOCK_HELD_read_MASK,
        held_mask: SIX_LOCK_HELD_write,
        unlock_wakeup: SixLockType::Read,
    },
];

/* ── 序列号 ── */

impl SixLock {
    pub fn six_lock_seq(&self) -> u64 {
        self.seq.load(Ordering::Acquire)
    }

    pub fn six_lock_intent_recurse_if_owner(&self) -> bool {
        if self.state.load(Ordering::Relaxed) & SIX_LOCK_HELD_intent != 0
            && self.owner_tid.load(Ordering::Acquire) == cur_tid()
        {
            self.intent_lock_recurse.fetch_add(1, Ordering::Relaxed);
            true
        } else {
            false
        }
    }
}

/* ── owner 辅助 ── */

fn cur_tid() -> u32 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    thread::current().id().hash(&mut h);
    h.finish() as u32
}

fn six_set_owner(lock: &SixLock, typ: SixLockType, old: u32) {
    if typ != SixLockType::Intent {
        return;
    }
    if old & SIX_LOCK_HELD_intent == 0 {
        lock.owner_tid.store(cur_tid(), Ordering::Release);
    }
}

fn six_clear_owner(lock: &SixLock, typ: SixLockType) {
    if typ == SixLockType::Intent || typ == SixLockType::Write {
        lock.owner_tid.store(0, Ordering::Release);
    }
}

/* ── do_trylock（核心 CAS，与 six.c __do_six_trylock 对应）── */

fn do_trylock(lock: &SixLock, typ: SixLockType) -> bool {
    let lv = &L[typ as usize];
    let mut old = lock.state.load(Ordering::Relaxed);
    loop {
        if old & lv.lock_fail != 0 {
            return false;
        }
        match lock.state.compare_exchange_weak(
            old,
            old + lv.lock_val,
            Ordering::Acquire,
            Ordering::Relaxed,
        ) {
            Ok(_) => {
                six_set_owner(lock, typ, old);
                return true;
            }
            Err(v) => old = v,
        }
    }
}

/* ── 唤醒 ── */

fn six_lock_wakeup(lock: &SixLock, lock_type: SixLockType) {
    let want_bit = SIX_LOCK_WAITING_read << (lock_type as u32);
    if lock.state.load(Ordering::Relaxed) & want_bit == 0 {
        return;
    }
    let _wl = lock.wait_lock.lock().unwrap();

    if lock_type == SixLockType::Read {
        let mut ws = lock.waiters.lock().unwrap();
        let mut woken = false;
        for entry in ws.iter_mut() {
            if entry.lock_want == SixLockType::Read && !entry.lock_acquired {
                if do_trylock(lock, SixLockType::Read) {
                    entry.lock_acquired = true;
                    if let Some(ref t) = entry.thread {
                        t.unpark();
                    }
                    woken = true;
                }
            }
        }
        if woken {
            lock.state.fetch_and(!want_bit, Ordering::Relaxed);
        }
    } else {
        let mut ws = lock.waiters.lock().unwrap();
        let idx = ws
            .iter()
            .position(|e| e.lock_want == lock_type && !e.lock_acquired);
        if let Some(i) = idx {
            if do_trylock(lock, lock_type) {
                ws[i].lock_acquired = true;
                if let Some(ref t) = ws[i].thread {
                    t.unpark();
                }
            }
        } else {
            lock.state.fetch_and(!want_bit, Ordering::Relaxed);
        }
    }
}

/* ── do_unlock_type ── */

fn do_unlock_type(lock: &SixLock, typ: SixLockType) {
    let lv = &L[typ as usize];
    if typ == SixLockType::Read {
        lock.state.fetch_sub(lv.lock_val, Ordering::Release);
    } else {
        lock.state
            .fetch_sub(lv.lock_val | SIX_LOCK_NOSPIN, Ordering::Release);
    }
    six_lock_wakeup(lock, lv.unlock_wakeup);
}

/* ========== 公共 API ========== */

/* ── 加锁（返回 bool，现有代码用作非阻塞 trylock）── */

impl SixLock {
    pub fn six_lock_read(&self) -> bool {
        do_trylock(self, SixLockType::Read)
    }

    pub fn six_lock_intent(&self) -> bool {
        do_trylock(self, SixLockType::Intent)
    }

    pub fn six_lock_write(&self) -> bool {
        do_trylock(self, SixLockType::Write)
    }

    pub fn six_lock_type(&self, typ: SixLockType) -> bool {
        do_trylock(self, typ)
    }

    /// six_lock_ip_waiter — 带 waiter 慢路径加锁（死锁检测用）
    pub fn six_lock_ip_waiter(&self, typ: SixLockType, waiter: &mut SixLockWaiter) -> i32 {
        if do_trylock(self, typ) {
            return 0;
        }
        self.six_lock_slowpath_waiter(typ, waiter)
    }

    pub fn six_lock_contended(&self, typ: SixLockType, waiter: &mut SixLockWaiter) -> i32 {
        self.six_lock_slowpath_waiter(typ, waiter)
    }
}

/* ── 慢路径 ── */

impl SixLock {
    fn six_lock_slowpath_waiter(&self, typ: SixLockType, waiter: &mut SixLockWaiter) -> i32 {
        let want_bit = SIX_LOCK_WAITING_read << (typ as u32);
        loop {
            {
                let _wl = self.wait_lock.lock().unwrap();
                self.state.fetch_or(want_bit, Ordering::Relaxed);

                if do_trylock(self, typ) {
                    self.state.fetch_and(!want_bit, Ordering::Relaxed);
                    six_lock_wakeup(self, SixLockType::Read);
                    six_lock_wakeup(self, SixLockType::Intent);
                    six_lock_wakeup(self, SixLockType::Write);
                    return 0;
                }
                self.waiters.lock().unwrap().push_back(waiter.clone());
            }
            let ret = call_should_sleep(self, waiter);
            if ret != 0 {
                let mut ws = self.waiters.lock().unwrap();
                ws.retain(|e| {
                    e.thread
                        .as_ref()
                        .map_or(true, |t| t.id() != thread::current().id())
                });
                return ret;
            }
            thread::park();
            if waiter.lock_acquired {
                return 0;
            }
            if do_trylock(self, typ) {
                let mut ws = self.waiters.lock().unwrap();
                ws.retain(|e| {
                    e.thread
                        .as_ref()
                        .map_or(true, |t| t.id() != thread::current().id())
                });
                return 0;
            }
        }
    }
}

/* ── trylock ── */

impl SixLock {
    pub fn six_trylock_type(&self, typ: SixLockType) -> bool {
        do_trylock(self, typ)
    }
    pub fn six_trylock_read(&self) -> bool {
        do_trylock(self, SixLockType::Read)
    }
    pub fn six_trylock_intent(&self) -> bool {
        do_trylock(self, SixLockType::Intent)
    }
    pub fn six_trylock_write(&self) -> bool {
        do_trylock(self, SixLockType::Write)
    }
}

/* ── relock ── */

impl SixLock {
    pub fn six_relock_type(&self, typ: SixLockType, seq: u64) -> bool {
        if self.six_lock_seq() != seq {
            return false;
        }
        if !do_trylock(self, typ) {
            return false;
        }
        if self.six_lock_seq() != seq {
            do_unlock_type(self, typ);
            return false;
        }
        true
    }

    pub fn six_relock_read(&self, seq: u64) -> bool {
        self.six_relock_type(SixLockType::Read, seq)
    }
    pub fn six_relock_intent(&self, seq: u64) -> bool {
        self.six_relock_type(SixLockType::Intent, seq)
    }
    pub fn six_relock_write(&self, seq: u64) -> bool {
        self.six_relock_type(SixLockType::Write, seq)
    }
}

/* ── unlock ── */

impl SixLock {
    pub fn six_unlock_type(&self, typ: SixLockType) {
        match typ {
            SixLockType::Read => self.six_unlock_read(),
            SixLockType::Intent => self.six_unlock_intent(),
            SixLockType::Write => self.six_unlock_write(),
        }
    }

    pub fn six_unlock_read(&self) {
        do_unlock_type(self, SixLockType::Read);
    }

    pub fn six_unlock_intent(&self) {
        if self.intent_lock_recurse.load(Ordering::Relaxed) > 0 {
            self.intent_lock_recurse.fetch_sub(1, Ordering::Relaxed);
            return;
        }
        do_unlock_type(self, SixLockType::Intent);
        six_clear_owner(self, SixLockType::Intent);
    }

    pub fn six_unlock_write(&self) {
        if self.write_lock_recurse.load(Ordering::Relaxed) > 0 {
            self.write_lock_recurse.fetch_sub(1, Ordering::Relaxed);
            return;
        }
        self.seq.fetch_add(1, Ordering::Release);
        do_unlock_type(self, SixLockType::Write);
        six_clear_owner(self, SixLockType::Write);
    }
}

/* ── downgrade / tryupgrade / convert ── */

impl SixLock {
    pub fn six_lock_downgrade(&self) {
        self.six_lock_increment(SixLockType::Read);
        self.six_unlock_intent();
    }

    pub fn six_lock_tryupgrade(&self) -> bool {
        let mut old = self.state.load(Ordering::Relaxed);
        loop {
            if old & SIX_LOCK_HELD_intent != 0 {
                return false;
            }
            let mut new = old;
            new = (new & !SIX_LOCK_HELD_read_MASK)
                | ((new & SIX_LOCK_HELD_read_MASK).wrapping_sub(1));
            new |= SIX_LOCK_HELD_intent;
            match self
                .state
                .compare_exchange_weak(old, new, Ordering::Acquire, Ordering::Relaxed)
            {
                Ok(_) => {
                    six_set_owner(self, SixLockType::Intent, old);
                    return true;
                }
                Err(v) => old = v,
            }
        }
    }

    pub fn six_trylock_convert(&self, from: SixLockType, to: SixLockType) -> bool {
        if to == SixLockType::Write || from == SixLockType::Write {
            return false;
        }
        if to == from {
            return true;
        }
        if to == SixLockType::Read {
            self.six_lock_downgrade();
            true
        } else {
            self.six_lock_tryupgrade()
        }
    }
}

/* ── increment ── */

impl SixLock {
    pub fn six_lock_increment(&self, typ: SixLockType) {
        match typ {
            SixLockType::Read => {
                self.state.fetch_add(1, Ordering::Relaxed);
            }
            SixLockType::Intent | SixLockType::Write => {
                self.intent_lock_recurse.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/* ── wakeup_all ── */

impl SixLock {
    pub fn six_lock_wakeup_all(&self) {
        six_lock_wakeup(self, SixLockType::Read);
        six_lock_wakeup(self, SixLockType::Intent);
        six_lock_wakeup(self, SixLockType::Write);
        let ws = self.waiters.lock().unwrap();
        for entry in ws.iter() {
            if let Some(ref t) = entry.thread {
                t.unpark();
            }
        }
    }
}

/* ── counts / readers_add ── */

impl SixLock {
    pub fn six_lock_counts(&self) -> SixLockCount {
        let s = self.state.load(Ordering::Relaxed);
        SixLockCount {
            n: [
                s & SIX_LOCK_HELD_read_MASK,
                if s & SIX_LOCK_HELD_intent != 0 { 1 } else { 0 }
                    + self.intent_lock_recurse.load(Ordering::Relaxed),
                if s & SIX_LOCK_HELD_write != 0 { 1 } else { 0 },
            ],
        }
    }

    pub fn six_lock_readers_add(&self, nr: i32) {
        if nr >= 0 {
            self.state.fetch_add(nr as u32, Ordering::Relaxed);
        } else {
            self.state.fetch_sub((-nr) as u32, Ordering::Relaxed);
        }
    }
}

/* ── 死锁检测辅助 ── */

impl SixLock {
    pub fn sx_collect_wait_fifo_waiter_info(
        &self,
        lock_id: u64,
        current_trans_id: u64,
    ) -> Vec<crate::lock::deadlock::WaiterInfo> {
        let mut out = Vec::new();
        let ws = self.waiters.lock().unwrap();
        let state = self.state.load(Ordering::Relaxed);
        let holder = if state & SIX_LOCK_HELD_write != 0 || state & SIX_LOCK_HELD_intent != 0 {
            self.owner_tid.load(Ordering::Relaxed) as u64
        } else {
            lock_id
        };
        for entry in ws.iter() {
            if entry.lock_acquired {
                continue;
            }
            out.push(crate::lock::deadlock::WaiterInfo {
                trans_id: entry.trans_start_time,
                lock_id,
                waiting_for_trans_id: holder,
            });
        }
        out
    }
}

/* ── exit ── */

pub fn six_lock_exit(_lock: &SixLock) {}
