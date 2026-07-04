//! SixLock — 3 状态读写锁（atomic bitfield + percpu reader）
//!
//! 对应 bcachefs fs/util/six.h + six.c。
//! "SIX" 不是 6 个状态，而是 6 个操作（lock/unlock × read/intent/write）。
//! 实际只有 3 种锁类型：
//!
//! - **Read**: 多个可共享，阻塞写锁
//! - **Intent**: 意向锁，意向之间互斥，但不阻塞读
//! - **Write**: 完全独占
//!
//! ## state 位布局 (AtomicU32)
//!
//! ```text
//! bit [0:25]  read_lock_count   (26 bits) — 当前持有读锁的线程数
//! bit [26]    intent_lock       (1 bit)   — 是否有意向锁被持有
//! bit [27]    write_lock        (1 bit)   — 是否有写锁被持有
//! bit [28]    waiting_read      (1 bit)   — 有线程在等待读锁
//! bit [29]    waiting_intent    (1 bit)   — 有线程在等待意向锁
//! bit [30]    waiting_write     (1 bit)   — 有线程在等待写锁
//! bit [31]    nospin            (1 bit)   — 禁止自旋，直接睡眠
//! ```
//!
//! ## Percpu Reader 模式
//!
//! 读者计数不通过原子操作更新 state，而是通过 percpu 变量：
//! 1. percpu_reader++（无锁）
//! 2. 全屏障 (Acquire fence)
//! 3. 检查 state.write_lock：
//!    - 无写锁 → 成功返回
//!    - 有写锁 → 回滚 percpu_reader，走慢路径（atomic CAS）

use std::cell::{OnceCell, UnsafeCell};
use std::sync::atomic::{fence, AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;

use spin::Mutex;

use urcu::{Rcu, RcuRSCS, RcuThread};

use super::wait_fifo::{WaitFifo, WaiterBox};

// 当前线程的读锁计数（用于 try_lock_write 排除自身读锁）
// 仅在非 percpu 路径（readers.is_none()）时使用。
// try_lock_read 成功时 +1，unlock_read 时 -1。
// try_lock_write 校验时从总 reader count 中减去此值，
// 使得持有读锁的线程可以升级到写锁。
thread_local! {
    static THREAD_READ_CNT: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

// RCU 句柄（每个线程首次访问时延迟初始化）
// 每个线程只需注册一次。`with_rcu` 提供安全的闭包式访问。
thread_local! {
    static RCU_HANDLE: OnceCell<(Rcu, RcuThread)> = const { OnceCell::new() };
}

// 在当前线程的 RCU read-side critical section 内执行闭包
// 自动初始化 RCU 库并注册当前线程（仅一次）。
pub(crate) fn with_rcu<F, T>(f: F) -> T
where
    F: FnOnce(&Rcu, &RcuThread) -> T,
{
    RCU_HANDLE.with(|cell| {
        let (rcu, thread) = cell.get_or_init(|| {
            let rcu = Rcu::init();
            let thread = RcuThread::register(&rcu);
            (rcu, thread)
        });
        f(rcu, thread)
    })
}

// ─── 位域常量 ───────────────────────────────────────────────

const READ_COUNT_MASK: u32 = 0x03FF_FFFF; // bits 0-25 (26 bits)
const INTENT_BIT: u32 = 0x0400_0000; // bit 26
const WRITE_BIT: u32 = 0x0800_0000; // bit 27
const WAITING_READ_BIT: u32 = 0x1000_0000; // bit 28
const WAITING_INTENT_BIT: u32 = 0x2000_0000; // bit 29; 对应 bcachefs SIX_LOCK_WAITING_intent
const WAITING_WRITE_BIT: u32 = 0x4000_0000; // bit 30; 对应 C SIX_LOCK_WAITING_write = 1U << (28 + SIX_LOCK_write) 其中 SIX_LOCK_write=2
const NOSPIN_BIT: u32 = 0x8000_0000; // bit 31

/// 自旋重试次数（对应 bcachefs six_lock_spin() 的 ~1024 PAUSE 循环）
const SPIN_COUNT: u32 = 1024;

/// 锁类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum SixLockType {
    Read = 0,
    Intent = 1,
    Write = 2,
}

/// 锁获取结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SixLockResult {
    /// 成功获取锁
    Acquired,
    /// 获取失败（trylock 路径）
    Busy,
    /// 死锁检测触发（需要事务重启）
    Deadlock,
}

/// 锁冲突矩阵 — 对应 bcachefs lock_type_conflicts()
///
/// ```text
/// read(0) + read(0) = 0    → 不冲突
/// read(0) + intent(1) = 1  → 不冲突
/// intent(1) + intent(1) = 2 → 冲突
/// intent(1) + write(2) = 3 → 冲突
/// write(2) + write(2) = 4  → 冲突
/// ```
pub(crate) fn lock_conflicts(held: SixLockType, want: SixLockType) -> bool {
    (held as u8 + want as u8) > 1
}

// ─── SixLock 相关类型（对应 bcachefs six.h） ─────────────────

/// 等待者条目 —— 对应 bcachefs struct six_lock_waiter
///
/// 嵌入上层的事务/锁跟踪结构体中，作为 lock waitlist 入口。
/// `trans_start_time` 用于 waitlist 排序（最早的事务先获取锁）。
#[derive(Debug)]
#[repr(C)]
pub struct SixLockWaiter {
    /// 事务开始时间戳（用于 waitlist 排序和死锁检测游标）
    pub trans_start_time: u64,
    /// 等待线程句柄
    pub thread: Option<thread::Thread>,
    /// 期望获取的锁类型
    pub lock_want: SixLockType,
    /// 锁是否已获取 —— 由唤醒方通过 Release 屏障设置
    pub lock_acquired: bool,
    /// 在 wait_fifo 中的槽位索引（用于 O(1) 自移除）
    pub slot_idx: u16,
}

/// 锁持有计数 —— 对应 bcachefs struct six_lock_count
#[derive(Debug, Clone, Copy, Default)]
#[repr(C)]
pub struct SixLockCount {
    /// n[0]=read, n[1]=intent, n[2]=write 的持有计数
    pub n: [u32; 3],
}

/// `should_sleep_fn` 回调签名
///
/// 对应 bcachefs:
/// ```c
/// typedef int (*six_lock_should_sleep_fn)(struct six_lock *, struct six_lock_waiter *);
/// ```
///
/// 参数: `&SixLock`, `&SixLockWaiter`
/// 返回: 0 = 允许 sleep 继续等待；非 0 = 错误码，中止加锁并返回该值
pub type SixLockShouldSleepFn = dyn Fn(&SixLock, &SixLockWaiter) -> i32 + Send + Sync;

/// 线程局部 should_sleep 回调 — 用于 btree 层注入死锁检测
///
/// 对应 bcachefs 在 `bch2_btree_node_lock` 路径中将 `bch2_six_check_for_deadlock`
/// 作为 should_sleep_fn 传给 `six_lock_ip_waiter`。subvol 通过 thread_local
/// 实现等效机制：事务在获取锁前注册回调，lock_slowpath 在 park 循环中调用它。
use std::cell::RefCell;
thread_local! {
    static THREAD_SHOULD_SLEEP: RefCell<Option<Box<dyn Fn(&SixLock, &SixLockWaiter) -> i32>>> = const { RefCell::new(None) };
}

/// 注册当前线程的 should_sleep 回调（用于 btree 事务死锁检测）
/// 回调仅在当前线程调用，无需 Send + Sync
pub(crate) fn sx_set_thread_should_sleep(f: Option<Box<dyn Fn(&SixLock, &SixLockWaiter) -> i32>>) {
    THREAD_SHOULD_SLEEP.with(|c| *c.borrow_mut() = f);
}

// ─── 线程 ID 分配（用于 percpu reader slot） ────────────────

static NEXT_THREAD_SLOT: AtomicU32 = AtomicU32::new(0);

thread_local! {
    static THREAD_SLOT: u32 = NEXT_THREAD_SLOT.fetch_add(1, Ordering::Relaxed);
}

fn current_thread_slot() -> u32 {
    THREAD_SLOT.with(|&s| s)
}

// ─── SixLock ────────────────────────────────────────────────

/// SixLock 主结构
///
/// 三种锁状态编码在单个 AtomicU32 位域中。
/// 可选 percpu reader 模式：读者用独立槽位计数，避免原子操作。
pub struct SixLock {
    state: AtomicU32,
    seq: AtomicU64,
    // Percpu reader 模式
    // 当启用时，读者通过 readers[slot] 计数，不操作 state.read_count
    readers: Option<Box<[AtomicU32]>>,
    // 持有 intent 锁的线程 id（用于重入检测）
    intent_owner: UnsafeCell<Option<thread::ThreadId>>,
    intent_recurse: UnsafeCell<u32>,
    // 持有写锁的线程 id
    write_owner: UnsafeCell<Option<thread::ThreadId>>,
    write_recurse: UnsafeCell<u32>,
    // 等待队列（Phase C1: spin/sleep 路径使用，push 等待者 + 设置 waiting bit）
    // 使用 RcuBox 而非 Mutex 提供遍历保护
    wait_fifo: WaitFifo,
    // 等待队列自旋锁 — 对应 bcachefs raw_spinlock_t wait_lock
    // push_waiter / wakeup_lock_type / remove_self_from_fifo 三者共用，
    // 确保 FIFO push/remove 与 WAITING bit 管理的原子性。
    wait_lock: Mutex<()>,
}

// SixLock 的 Send/Sync: AtomicU32 是 Send+Sync，UnsafeCell 是 !Sync 但被保护
unsafe impl Send for SixLock {}
unsafe impl Sync for SixLock {}

impl SixLock {
    /// 创建新的 SixLock（标准模式）
    pub fn new() -> Self {
        let rcu = Rcu::init();
        Self {
            state: AtomicU32::new(0),
            seq: AtomicU64::new(0),
            readers: None,
            intent_owner: UnsafeCell::new(None),
            intent_recurse: UnsafeCell::new(0),
            write_owner: UnsafeCell::new(None),
            write_recurse: UnsafeCell::new(0),
            wait_fifo: WaitFifo::new(16, &rcu),
            wait_lock: Mutex::new(()),
        }
    }

    /// 创建支持 percpu reader 的 SixLock
    ///
    /// `num_slots` = 预估的最大并发读者数（建议 >= CPU 核数 * 2）
    pub fn with_percpu(num_slots: u32) -> Self {
        assert!(num_slots > 0, "num_slots must be > 0");
        let readers: Vec<AtomicU32> = (0..num_slots).map(|_| AtomicU32::new(0)).collect();
        let rcu = Rcu::init();
        Self {
            state: AtomicU32::new(0),
            seq: AtomicU64::new(0),
            readers: Some(readers.into_boxed_slice()),
            intent_owner: UnsafeCell::new(None),
            intent_recurse: UnsafeCell::new(0),
            write_owner: UnsafeCell::new(None),
            write_recurse: UnsafeCell::new(0),
            wait_fifo: WaitFifo::new(16, &rcu),
            wait_lock: Mutex::new(()),
        }
    }

    /// 当前序列号 — 别名 `six_lock_seq`（每次写锁释放时递增，用于 relock 验证）
    fn lock_seq(&self) -> u64 {
        self.seq.load(Ordering::Acquire)
    }

    /// bcachefs 风格别名：`six_lock_seq`
    pub fn six_lock_seq(&self) -> u64 {
        self.lock_seq()
    }

    // ── 内部操作 ──

    /// 读取当前 state
    fn read_state(&self) -> u32 {
        self.state.load(Ordering::Acquire)
    }

    /// 判断是否有写锁被持有
    fn has_write_lock(&self, state: u32) -> bool {
        state & WRITE_BIT != 0
    }

    /// 判断是否有 intent 锁被持有
    fn has_intent_lock(&self, state: u32) -> bool {
        state & INTENT_BIT != 0
    }

    /// 读取锁持有者计数
    fn read_count(&self, state: u32) -> u32 {
        state & READ_COUNT_MASK
    }

    // ── 读锁 ──

    /// 尝试获取读锁（快速路径）
    ///
    /// Percpu 模式：
    ///   1. percpu_reader[slot]++
    ///   2. Acquire fence
    ///   3. 检查 write_lock bit
    ///
    /// 标准模式：
    ///   1. CAS state.read_count + 1
    ///   2. 检查 write_lock bit（CAS 时会失败如果有写锁）
    ///
    /// 对应 bcachefs __do_six_trylock(Read) six.c:122-214, read 分支 six.c:159-185。
    /// 只把已持有的 write lock 视为直接冲突；waiting write bit 由写者持锁预设，
    /// 不是读 fast path 的额外阻断条件。
    pub fn six_trylock_read(&self) -> bool {
        if let Some(ref readers) = self.readers {
            // Percpu 快速路径
            let slot = current_thread_slot() as usize % readers.len();
            readers[slot].fetch_add(1, Ordering::Relaxed);
            fence(Ordering::Acquire);
            let state = self.read_state();
            if !self.has_write_lock(state) {
                return true;
            }
            // 有写锁，回滚
            readers[slot].fetch_sub(1, Ordering::Relaxed);
            fence(Ordering::Acquire);
            // S1: spurious wakeup — 临时增加的 percpu 读者计数可能让写者 drain 失败，
            // 回滚后需要唤醒已入队的写者
            let after = self.read_state();
            if after & WAITING_WRITE_BIT != 0 {
                self.wakeup_lock_type(after, SixLockType::Write);
            }
            false
        } else {
            // 标准原子路径
            loop {
                let state = self.read_state();
                // 只在写锁已持有时拒绝新读者；waiting bit 不单独阻断 read fast path。
                if self.has_write_lock(state) {
                    return false;
                }
                if self
                    .state
                    .compare_exchange_weak(state, state + 1, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
                {
                    THREAD_READ_CNT.with(|c| c.set(c.get() + 1));
                    return true;
                }
                // CAS 失败，重试
                std::hint::spin_loop();
            }
        }
    }

    /// 释放读锁
    ///
    /// 对应 bcachefs do_six_unlock_type(Read) six.c:771-795（read 分支 six.c:778-783）。
    pub fn six_unlock_read(&self) {
        if let Some(ref readers) = self.readers {
            let slot = current_thread_slot() as usize % readers.len();
            readers[slot].fetch_sub(1, Ordering::Release);
        } else {
            self.state.fetch_sub(1, Ordering::Release);
            THREAD_READ_CNT.with(|c| c.set(c.get() - 1));
        }
        // 读锁释放 → 可能可以唤醒 write waiter
        let state = self.read_state();
        self.wakeup_lock_type(state, SixLockType::Write);
    }

    /// bcachefs 风格别名：`six_unlock_ip(lk, SIX_LOCK_read, ip)`
    pub fn six_unlock_ip_read(&self, ip: usize) {
        self.unlock_ip(SixLockType::Read, ip);
    }

    // ── Intent 锁 ──

    /// 尝试获取 intent 锁（intent 之间互斥，但不阻塞读）
    ///
    /// 使用 CAS 循环（对应 C __do_six_trylock() 的 atomic_try_cmpxchg_acquire 循环）：
    /// 1. 读 state
    /// 2. 检查 INTENT_BIT 冲突
    /// 3. CAS 尝试设置 INTENT_BIT
    /// 4. 如果 CAS 失败（并发 state 变化），回退到步骤 1 重试
    ///
    /// 注意：不使用 fetch_or + 回滚模式，因为回滚可能错误清除其他线程已设置的 INTENT_BIT。
    ///
    /// 对应 bcachefs __do_six_trylock(Intent) six.c:122-214, intent 分支 six.c:159-169。
    pub fn six_trylock_intent(&self) -> bool {
        // 先检查重入：当前线程已持有 intent 锁（通过 intent_owner 判断）
        let owner = unsafe { *self.intent_owner.get() };
        if owner == Some(thread::current().id()) {
            unsafe {
                *self.intent_recurse.get() += 1;
            }
            return true;
        }

        let mut state = self.read_state();
        loop {
            // 冲突检查：只有 intent 已持有时失败
            if state & INTENT_BIT != 0 {
                return false;
            }
            // CAS 原子设置 INTENT_BIT，仅当 state 未变化时成功
            match self.state.compare_exchange_weak(
                state,
                state | INTENT_BIT,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    unsafe {
                        *self.intent_owner.get() = Some(thread::current().id());
                    }
                    return true;
                }
                Err(current) => {
                    state = current;
                    // 重试循环：state 被其他线程修改（如 read_count 变化），
                    // 重新检查冲突并重试 CAS
                }
            }
        }
    }

    /// 释放 intent 锁
    ///
    /// 对应 bcachefs do_six_unlock_type(Intent) six.c:771-795（intent 分支 six.c:775-776, 783-791）。
    /// 重入处理对应 six_unlock_ip six.c:823-827。
    pub fn six_unlock_intent(&self) {
        let recurse = unsafe { *self.intent_recurse.get() };
        if recurse > 0 {
            unsafe {
                *self.intent_recurse.get() = recurse - 1;
            }
            return;
        }
        unsafe {
            *self.intent_owner.get() = None;
        }
        self.state
            .fetch_and(!(INTENT_BIT | NOSPIN_BIT), Ordering::Release);
        // Intent 释放 → 可能可以唤醒 intent waiter
        let state = self.read_state();
        self.wakeup_lock_type(state, SixLockType::Intent);
    }

    /// bcachefs 风格别名：`six_unlock_ip(lk, SIX_LOCK_intent, ip)`
    pub fn six_unlock_ip_intent(&self, ip: usize) {
        self.unlock_ip(SixLockType::Intent, ip);
    }

    // ── 写锁 ──

    /// 尝试获取写锁（独占，必须 read_count == 0）
    ///
    /// Percpu 模式对齐 bcachefs：
    /// 1. CAS 先预设 WRITE_BIT
    /// 2. CAS 成功后只 drain 其他 slot，跳过自身 slot
    ///    自身 percpu 读者是该线程持有的读锁，不阻塞写锁升级。
    ///
    /// 对应 bcachefs __do_six_trylock(Write) six.c:122-214, write 分支 six.c:186-205。
    /// bcachefs 要求 write 获取者已经是 intent owner；write 的 lock_fail 仍只检查 read，
    /// 不把调用者自己持有的 INTENT_BIT 视为冲突位。
    pub fn six_trylock_write(&self) -> bool {
        debug_assert_eq!(
            unsafe { *self.intent_owner.get() },
            Some(thread::current().id()),
            "write lock requires intent ownership"
        );
        let state = self.read_state();
        if self.has_write_lock(state) {
            // 重入检测
            let owner = unsafe { *self.write_owner.get() };
            if owner == Some(thread::current().id()) {
                unsafe {
                    *self.write_recurse.get() += 1;
                }
                return true;
            }
            return false;
        }
        // write 只与 read 冲突；intent 由上层路径顺序约束，不作为 trylock 冲突位
        if (state & WRITE_BIT) != 0 {
            return false;
        }

        if let Some(ref readers) = self.readers {
            // percpu 模式：CAS 预设 WRITE_BIT，一次性检查所有槽位
            // 对应 bcachefs __do_six_trylock(Write) six.c:186-205 — pcpu_read_count()
            // 做一次性快照，不旋转等待。若槽位非零则撤消 WRITE_BIT 返回 false，
            // 由上层 spin_lock / slowpath 处理等待。
            let ok = self
                .state
                .compare_exchange(
                    state,
                    state | WRITE_BIT,
                    Ordering::Acquire,
                    Ordering::Relaxed,
                )
                .is_ok();
            if ok {
                for reader in readers.iter() {
                    if reader.load(Ordering::Acquire) > 0 {
                        // 撤消 WRITE_BIT — 对应 six.c:201 atomic_sub_return(HELD_write)
                        self.state.fetch_and(!WRITE_BIT, Ordering::Release);
                        return false;
                    }
                }
                unsafe {
                    *self.write_owner.get() = Some(thread::current().id());
                }
            }
            ok
        } else {
            // 非 percpu 模式：直接检查读者计数（不自排除）
            // 对应 bcachefs __do_six_trylock(Write) six.c:159-164
            // 在非 percpu 模式下，__do_six_trylock 的 write 类型走 intent 逻辑分支，
            // 只检查 old & SIX_LOCK_HELD_read，不排除任何读者。
            // 排除自身读者由调用方（如 bch2_btree_node_lock_write_contended
            // locking.c:965-972）在调用 six_trylock_write 前通过
            // six_lock_readers_add(-N) 完成。
            let total_reads = state & READ_COUNT_MASK;
            if total_reads != 0 {
                return false;
            }
            let ok = self
                .state
                .compare_exchange(
                    state,
                    state | WRITE_BIT,
                    Ordering::Acquire,
                    Ordering::Relaxed,
                )
                .is_ok();
            if ok {
                unsafe {
                    *self.write_owner.get() = Some(thread::current().id());
                }
            }
            ok
        }
    }

    /// 在 WRITE_BIT 已预设的慢路径中使用的 trylock。
    ///
    /// 对应 bcachefs `__do_six_trylock(..., try=false)` 在 write 锁上的行为：
    /// 不检查 WRITE_BIT（因为 slowpath 已预设），只检查读者计数是否为零。
    /// WRITE_BIT 已预设意味着 writer 已在等待队列中，此时只需确认读者已全部退出。
    fn try_lock_write_preset(&self) -> bool {
        self.try_lock_write_preset_for(thread::current().id())
    }

    /// Waker 替 write waiter 声明 write lock
    ///
    /// 对应 bcachefs __do_six_trylock(lock, SIX_LOCK_write, waiter->task, false) six.c:163-165, 186-205。
    /// WRITE_BIT 已被慢路径预设。只做一次性快照检查读者计数，不旋转等待。
    /// bcachefs 中 !try 路径不做 CAS（WRITE_BIT 已预设），smp_mb 后一次性检查 pcpu_read_count。
    /// 若读者未清零则返回 false，由上层 park/wake 循环处理等待。
    fn try_lock_write_preset_for(&self, tid: thread::ThreadId) -> bool {
        debug_assert!(self.has_write_lock(self.read_state()));
        debug_assert_eq!(
            unsafe { *self.intent_owner.get() },
            Some(tid),
            "write lock requires intent ownership"
        );

        if let Some(ref readers) = self.readers {
            for reader in readers.iter() {
                if reader.load(Ordering::Acquire) > 0 {
                    return false;
                }
            }
        } else {
            let state = self.read_state();
            if state & READ_COUNT_MASK != 0 {
                return false;
            }
        }

        // 设 owner 为 WAITER，不是 waker
        unsafe {
            *self.write_owner.get() = Some(tid);
        }
        true
    }

    /// Waker 替 intent waiter 声明 intent lock
    ///
    /// 对应 bcachefs __do_six_trylock(lock, SIX_LOCK_intent, waiter->task, false)。
    /// 通过 CAS 设置 INTENT_BIT，设 owner 为 waiter 的 tid。
    fn try_lock_intent_for(&self, tid: thread::ThreadId) -> bool {
        let mut state = self.read_state();
        loop {
            if state & INTENT_BIT != 0 {
                return false;
            }
            match self.state.compare_exchange_weak(
                state,
                state | INTENT_BIT,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    unsafe {
                        *self.intent_owner.get() = Some(tid);
                    }
                    return true;
                }
                Err(current) => state = current,
            }
        }
    }

    /// Waker 替 read waiter 声明 read lock（按 waiter 的 percpu slot）
    ///
    /// 对应 bcachefs __do_six_trylock(lock, SIX_LOCK_read, waiter->task, false)。
    /// 非 percpu：state.fetch_add(1)；percpu：readers[waiter_slot].fetch_add(1)。
    /// 不检查 write/intent 状态——wakeup_lock_type 的快速检查已确保写锁不会在此
    /// 期间被获取（或 reader 不会处于 WAITING 状态）。
    fn try_lock_read_for(&self, slot_idx: u32) -> bool {
        if let Some(ref readers) = self.readers {
            let idx = slot_idx as usize % readers.len();
            readers[idx].fetch_add(1, Ordering::Relaxed);
            fence(Ordering::Acquire);
        } else {
            self.state.fetch_add(1, Ordering::Acquire);
        }
        true
    }

    // ── 自旋方法 ──

    /// 内部自旋等待读锁可用（检查 nospin bit，循环 SPIN_COUNT 次）
    ///
    /// 先检查 nospin bit：如果置位则立即返回 false。
    /// 然后循环调用 try_lock_read()，每次迭代间用 spin_loop() 减少 busy-wait 开销。
    /// 超过 SPIN_COUNT 次后让步 CPU（thread::yield_now），减少 CPU busy-wait 消耗。
    fn spin_lock_read_internal(&self) -> bool {
        if self.read_state() & NOSPIN_BIT != 0 {
            return false;
        }
        for _ in 0..SPIN_COUNT {
            if self.six_trylock_read() {
                return true;
            }
            std::hint::spin_loop();
        }
        // Phase C1: 自旋超时后让步 CPU，让 OS 调度其他线程
        std::thread::yield_now();
        if self.six_trylock_read() {
            return true;
        }
        false
    }

    /// 内部自旋等待 intent 锁可用
    ///
    /// 超过 SPIN_COUNT 次后让步 CPU（thread::yield_now），减少 CPU busy-wait 消耗。
    fn spin_lock_intent_internal(&self) -> bool {
        if self.read_state() & NOSPIN_BIT != 0 {
            return false;
        }
        for _ in 0..SPIN_COUNT {
            if self.six_trylock_intent() {
                return true;
            }
            std::hint::spin_loop();
        }
        // Phase C1: 自旋超时后让步 CPU，让 OS 调度其他线程
        std::thread::yield_now();
        if self.six_trylock_intent() {
            return true;
        }
        false
    }

    /// 内部自旋等待写锁可用（要求 read_count == 0, write == 0；intent 由上层路径约束）
    ///
    /// 超过 SPIN_COUNT 次后让步 CPU（thread::yield_now），减少 CPU busy-wait 消耗。
    fn spin_lock_write_internal(&self) -> bool {
        if self.read_state() & NOSPIN_BIT != 0 {
            return false;
        }
        for _ in 0..SPIN_COUNT {
            if self.six_trylock_write() {
                return true;
            }
            std::hint::spin_loop();
        }
        // Phase C1: 自旋超时后让步 CPU，让 OS 调度其他线程
        std::thread::yield_now();
        if self.six_trylock_write() {
            return true;
        }
        false
    }

    // ── 阻塞/等待锁方法（try → spin → sleep） ──

    /// 获取读锁（try → spin → sleep 分级等待）
    ///
    /// 1. try_lock_read 快速路径
    /// 2. spin_lock_read_internal 自旋 SPIN_COUNT 次
    /// 3. sleep：入队 WaitFifo + thread::park() 阻塞
    ///    wake 后尝试获取锁，成功则移除自己并返回 true。
    pub fn six_lock_read(&self) -> bool {
        if self.six_trylock_read() {
            return true;
        }
        if self.spin_lock_read_internal() {
            return true;
        }
        // Sleep 路径：委托 lock_slowpath 统一慢路径（对齐 bcachefs __six_lock_slowpath）
        let mut wait = SixLockWaiter {
            trans_start_time: 0,
            thread: None,
            lock_want: SixLockType::Read,
            lock_acquired: false,
            slot_idx: 0,
        };
        self.lock_slowpath(SixLockType::Read, &mut wait, None) == 0
    }

    /// 获取 intent 锁（try → spin → sleep 分级等待）
    pub fn six_lock_intent(&self) -> bool {
        if self.six_trylock_intent() {
            return true;
        }
        if self.spin_lock_intent_internal() {
            return true;
        }
        // Sleep 路径：委托 lock_slowpath 统一慢路径
        let mut wait = SixLockWaiter {
            trans_start_time: 0,
            thread: None,
            lock_want: SixLockType::Intent,
            lock_acquired: false,
            slot_idx: 0,
        };
        self.lock_slowpath(SixLockType::Intent, &mut wait, None) == 0
    }

    /// 获取写锁（try → spin → sleep 分级等待，完全独占）
    ///
    /// Sleep 路径委托 lock_slowpath 统一慢路径。WRITE_BIT 预设 + WAITING_WRITE_BIT 预设
    /// + trylock 重试都在 lock_slowpath 内部，对齐 bcachefs __six_lock_slowpath:
    /// atomic_add(HELD_write) → WAITING_write → trylock。
    pub fn six_lock_write(&self) -> bool {
        if self.six_trylock_write() {
            return true;
        }
        if self.spin_lock_write_internal() {
            return true;
        }
        // Sleep 路径：委托 lock_slowpath 统一慢路径
        let mut wait = SixLockWaiter {
            trans_start_time: 0,
            thread: None,
            lock_want: SixLockType::Write,
            lock_acquired: false,
            slot_idx: 0,
        };
        self.lock_slowpath(SixLockType::Write, &mut wait, None) == 0
    }

    /// 释放写锁
    ///
    /// 对应 bcachefs six_unlock_ip(Write) six.c:812-839 + do_six_unlock_type(Write) six.c:771-795。
    /// seq++ 对齐 six_unlock_ip six.c:835-836。
    /// 唤醒 Read 路径：do_six_unlock_type 调用 six_lock_wakeup(lock, state, SIX_LOCK_read) six.c:794。
    pub fn six_unlock_write(&self) {
        debug_assert_eq!(
            unsafe { *self.intent_owner.get() },
            Some(thread::current().id()),
            "write unlock requires intent ownership"
        );
        debug_assert_eq!(
            unsafe { *self.write_owner.get() },
            Some(thread::current().id()),
            "write unlock requires write ownership"
        );
        let recurse = unsafe { *self.write_recurse.get() };
        if recurse > 0 {
            unsafe {
                *self.write_recurse.get() = recurse - 1;
            }
            return;
        }
        unsafe {
            *self.write_owner.get() = None;
        }
        self.seq.fetch_add(1, Ordering::Release);
        self.state
            .fetch_and(!(WRITE_BIT | NOSPIN_BIT), Ordering::Release);
        // 写锁释放 → 可能可以唤醒 read / intent waiter
        let state = self.read_state();
        self.wakeup_lock_type(state, SixLockType::Read);
    }

    /// bcachefs 风格别名：`six_unlock_ip(lk, SIX_LOCK_write, ip)`
    pub fn six_unlock_ip_write(&self, ip: usize) {
        self.unlock_ip(SixLockType::Write, ip);
    }

    // ── 升级操作 ──

    fn try_upgrade_read_to_intent(&self) -> bool {
        if let Some(ref readers) = self.readers {
            // Percpu 模式：decrement percpu reader, CAS set intent bit
            let slot = current_thread_slot() as usize % readers.len();
            debug_assert!(
                readers[slot].load(Ordering::Relaxed) > 0,
                "current thread must have a percpu reader"
            );
            readers[slot].fetch_sub(1, Ordering::Relaxed);
            fence(Ordering::Acquire);

            let state = self.read_state();
            if state & INTENT_BIT != 0 {
                // 有人持有 intent → 回滚
                readers[slot].fetch_add(1, Ordering::Relaxed);
                return false;
            }

            // 设置 intent bit
            let ok = self
                .state
                .compare_exchange(
                    state,
                    state | INTENT_BIT,
                    Ordering::Acquire,
                    Ordering::Relaxed,
                )
                .is_ok();
            if ok {
                unsafe {
                    *self.intent_owner.get() = Some(thread::current().id());
                }
                return true;
            }
            // CAS 失败 → 回滚 percpu reader
            readers[slot].fetch_add(1, Ordering::Relaxed);
            false
        } else {
            // 标准模式：CAS read_count - 1, set intent bit
            loop {
                let state = self.read_state();
                if state & INTENT_BIT != 0 {
                    return false;
                }
                if self.read_count(state) == 0 {
                    return false; // 没有读者（说明我们不持有读锁）
                }
                if self
                    .state
                    .compare_exchange_weak(
                        state,
                        (state - 1) | INTENT_BIT, // read_count -= 1, intent_bit = 1
                        Ordering::Acquire,
                        Ordering::Relaxed,
                    )
                    .is_ok()
                {
                    unsafe {
                        *self.intent_owner.get() = Some(thread::current().id());
                    }
                    // 读锁已释放（state.read_count - 1），同步递减 THREAD_READ_CNT
                    // 防止 try_lock_write 在排除自身读者时错误地忽略其他线程的读者
                    THREAD_READ_CNT.with(|c| c.set(c.get() - 1));
                    return true;
                }
                std::hint::spin_loop();
            }
        }
    }

    // ── 升级自旋方法 ──

    /// 自旋等待从读锁升级为 intent 锁
    ///
    /// 要求调用者已持有读锁。自旋等待其他 intent/write 持有者释放。
    /// 比 try_upgrade_read_to_intent 多一次 SPIN_COUNT 重试。
    /// 超过 SPIN_COUNT 次后让步 CPU（thread::yield_now），减少 CPU busy-wait 消耗。
    fn upgrade_read_to_intent(&self) -> bool {
        if self.try_upgrade_read_to_intent() {
            return true;
        }
        for _ in 0..SPIN_COUNT {
            if self.try_upgrade_read_to_intent() {
                return true;
            }
            std::hint::spin_loop();
        }
        // Phase C1: 自旋超时后让步 CPU
        std::thread::yield_now();
        if self.try_upgrade_read_to_intent() {
            return true;
        }
        false
    }

    // ── Relock API（序列号验证重入） ──

    /// 尝试重入读锁，验证序列号未变化
    ///
    /// 对应 bcachefs six_relock_read()（宏展开为 six_relock_ip six.c:470-482）。
    /// 锁的序列号在写锁释放时递增。如果当前序列号与 `expected_seq` 一致，
    /// 说明自解锁以来没有写操作，数据仍然有效。
    ///
    /// 用法：
    /// ```text
    /// let seq = lock.six_lock_seq();
    /// lock.six_unlock_read();
    /// some_blocking_operation();
    /// if lock.six_relock_read(seq) {
    ///     // 数据未变，安全继续
    /// }
    /// ```
    pub fn six_relock_read(&self, expected_seq: u64) -> bool {
        self.relock_ip(SixLockType::Read, expected_seq, 0)
    }

    /// 尝试重入 intent 锁，验证序列号未变化
    ///
    /// 对应 bcachefs six_relock_intent()。
    /// 语义同 relock_read，但使用 intent 锁。
    pub fn six_relock_intent(&self, expected_seq: u64) -> bool {
        self.relock_ip(SixLockType::Intent, expected_seq, 0)
    }

    /// 尝试重入写锁，验证序列号未变化（对应 bcachefs six_relock_write）
    ///
    /// 写锁重入在 C 中也有定义，但实践中写锁不会轻易释放后重入。
    pub fn six_relock_write(&self, expected_seq: u64) -> bool {
        self.relock_ip(SixLockType::Write, expected_seq, 0)
    }

    // ── 状态查询 ──

    /// 当前是否有写锁被持有
    pub(crate) fn is_write_locked(&self) -> bool {
        self.read_state() & WRITE_BIT != 0
    }

    /// 当前是否有 intent 锁被持有
    pub(crate) fn is_intent_locked(&self) -> bool {
        self.read_state() & INTENT_BIT != 0
    }

    /// 当前线程是否持有写锁
    pub(crate) fn is_write_locked_by_current(&self) -> bool {
        unsafe { *self.write_owner.get() == Some(thread::current().id()) }
    }

    /// 当前线程是否持有 intent 锁
    pub(crate) fn is_intent_locked_by_current(&self) -> bool {
        unsafe { *self.intent_owner.get() == Some(thread::current().id()) }
    }

    /// 当前读者数量
    fn reader_count(&self) -> u32 {
        let state = self.read_state();
        let atomic_count = self.read_count(state);
        if let Some(ref readers) = self.readers {
            let percpu_count: u32 = readers.iter().map(|r| r.load(Ordering::Relaxed)).sum();
            atomic_count + percpu_count
        } else {
            atomic_count
        }
    }

    // ── 销毁 / 清理 ──

    /// 释放锁占用的资源（对应 bcachefs six_lock_exit）
    ///
    /// 释放 percpu readers 和 wait_fifo 中的等待者。
    /// 在 Rust 中，Drop 会处理大部分清理工作，此方法提供显式控制。
    pub fn six_lock_exit(&mut self) {
        self.readers = None;
        // wait_fifo 的空槽清理在其 Drop 中完成
    }

    // ── 通用 trylock（对应 bcachefs six_trylock_ip） ──

    /// 通用 trylock —— 按类型尝试获取锁（对应 bcachefs six_trylock_ip）
    ///
    /// `ip` 参数用于 lockdep，在 Rust 中保留以匹配 C API 签名但未使用。
    pub fn six_trylock_ip(&self, type_: SixLockType, _ip: usize) -> bool {
        match type_ {
            SixLockType::Read => self.six_trylock_read(),
            SixLockType::Intent => self.six_trylock_intent(),
            SixLockType::Write => self.six_trylock_write(),
        }
    }

    /// bcachefs 风格别名：`six_trylock_type`
    pub fn six_trylock_type(&self, type_: SixLockType) -> bool {
        self.six_trylock_ip(type_, 0)
    }

    // ── 通用 unlock（对应 bcachefs six_unlock_ip） ──

    /// 通用 unlock —— 按类型释放锁（对应 bcachefs six_unlock_ip six.c:812-839）
    ///
    /// `ip` 参数用于 lockdep，在 Rust 中保留以匹配 C API 签名但未使用。
    /// 注意：C 版有 recurse 检查和 seq++（写锁），分别由各 unlock_* 实现。
    fn unlock_ip(&self, type_: SixLockType, _ip: usize) {
        match type_ {
            SixLockType::Read => self.six_unlock_read(),
            SixLockType::Intent => self.six_unlock_intent(),
            SixLockType::Write => self.six_unlock_write(),
        }
    }

    /// bcachefs 风格别名：`six_unlock_ip`
    pub fn six_unlock_ip(&self, type_: SixLockType, ip: usize) {
        self.unlock_ip(type_, ip);
    }

    // ── 通用 relock（对应 bcachefs six_relock_ip） ──

    /// 通用 relock —— 验证序列号后重新加锁（对应 bcachefs six_relock_ip six.c:470-482）
    ///
    /// 返回 true 表示加锁成功且序列号未变化，false 表示序列号已变化或加锁失败。
    fn relock_ip(&self, type_: SixLockType, seq: u64, ip: usize) -> bool {
        if self.lock_seq() != seq {
            return false;
        }
        if !self.six_trylock_ip(type_, ip) {
            return false;
        }
        // 双检：获取锁后再次验证 seq
        if self.lock_seq() != seq {
            self.unlock_ip(type_, ip);
            return false;
        }
        true
    }

    /// bcachefs 风格别名：`six_relock_ip`
    pub fn six_relock_ip(&self, type_: SixLockType, seq: u64, ip: usize) -> bool {
        self.relock_ip(type_, seq, ip)
    }

    /// bcachefs 风格别名：`six_lock_type`
    pub fn six_lock_type(&self, type_: SixLockType) -> bool {
        match type_ {
            SixLockType::Read => self.six_lock_read(),
            SixLockType::Intent => self.six_lock_intent(),
            SixLockType::Write => self.six_lock_write(),
        }
    }

    // ── 通用阻塞加锁（对应 bcachefs six_lock_ip_waiter） ──

    /// 最通用的阻塞加锁函数（对应 bcachefs six_lock_ip_waiter）
    ///
    /// 完整的 try → spin → sleep 三级等待。
    /// `wait` 是栈上分配的 SixLockWaiter，需要由调用者提供。
    /// `should_sleep` 是可选的回调，在 park 前调用；返回 0=继续等待，非 0=中止。
    ///
    /// 返回 0 表示加锁成功，非 0 表示 `should_sleep` 返回的错误码。
    pub fn six_lock_ip_waiter(
        &self,
        type_: SixLockType,
        wait: &mut SixLockWaiter,
        should_sleep: Option<&SixLockShouldSleepFn>,
        _ip: usize,
    ) -> i32 {
        // 快速路径：trylock
        if self.six_trylock_ip(type_, 0) {
            return 0;
        }
        // 自旋路径
        match type_ {
            SixLockType::Read => {
                if self.spin_lock_read_internal() {
                    return 0;
                }
            }
            SixLockType::Intent => {
                if self.spin_lock_intent_internal() {
                    return 0;
                }
            }
            SixLockType::Write => {
                if self.spin_lock_write_internal() {
                    return 0;
                }
            }
        }
        // Sleep 路径
        self.lock_slowpath(type_, wait, should_sleep)
    }

    /// 跳过初始 trylock，直接进入加锁慢路径（对应 bcachefs six_lock_contended）
    ///
    /// 调用者已在外部做过 trylock 并观测到锁被争用。
    /// 避免在已知锁被争用时浪费一次 CAS 操作。
    pub fn six_lock_contended(
        &self,
        type_: SixLockType,
        wait: &mut SixLockWaiter,
        should_sleep: Option<&SixLockShouldSleepFn>,
        _ip: usize,
    ) -> i32 {
        self.lock_slowpath(type_, wait, should_sleep)
    }

    /// 简化版阻塞加锁（对应 bcachefs six_lock_waiter six.h:366-369）
    ///
    /// 不传 IP，自动填入 _THIS_IP_（在 Rust 中为 0）。
    pub fn six_lock_waiter(
        &self,
        type_: SixLockType,
        wait: &mut SixLockWaiter,
        should_sleep: Option<&SixLockShouldSleepFn>,
    ) -> i32 {
        self.six_lock_ip_waiter(type_, wait, should_sleep, 0)
    }

    /// 最简阻塞加锁（对应 bcachefs six_lock_ip six.h:385-391）
    ///
    /// 栈上创建 SixLockWaiter，调用 six_lock_ip_waiter。
    pub fn six_lock_ip(
        &self,
        type_: SixLockType,
        should_sleep: Option<&SixLockShouldSleepFn>,
        ip: usize,
    ) -> i32 {
        let mut wait = SixLockWaiter {
            trans_start_time: 0,
            thread: None,
            lock_want: type_,
            lock_acquired: false,
            slot_idx: 0,
        };
        self.six_lock_ip_waiter(type_, &mut wait, should_sleep, ip)
    }

    /// 加锁慢路径 —— push waiter + park/wake 循环
    fn lock_slowpath(
        &self,
        type_: SixLockType,
        wait: &mut SixLockWaiter,
        should_sleep: Option<&SixLockShouldSleepFn>,
    ) -> i32 {
        wait.thread = Some(thread::current());
        wait.lock_want = type_;
        wait.lock_acquired = false;

        // 写锁需要预设 WRITE_BIT + WAITING_WRITE_BIT 防止读者饥饿
        if type_ == SixLockType::Write {
            // S4: 一次性预设 WRITE_BIT + WAITING_WRITE_BIT，减少原子往返并缩小竞态窗口
            self.state
                .fetch_or(WRITE_BIT | WAITING_WRITE_BIT, Ordering::SeqCst);
            // S6: 双检用 try_lock_write_preset 而非 six_trylock_ip（try_lock_write 会检查
            // WRITE_BIT 是否已设置，但我们自己刚设了 WRITE_BIT，导致 try_lock_write 总是
            // 返回 false）。try_lock_write_preset 只检查读者计数，与 bcachefs
            // __do_six_trylock(try=false) 对齐。
            let ok = self.try_lock_write_preset();
            if ok {
                self.clear_waiting_bit(type_);
                wait.lock_acquired = true;
                return 0;
            }
        }

        // 创建带外 handoff 信号
        let flag = Arc::new(AtomicBool::new(false));
        // 入队 WaitFifo
        let waiter_box = WaiterBox {
            trans_id: wait.trans_start_time,
            lock_type: type_,
            seq: self.seq.load(Ordering::Relaxed),
            thread: Some(thread::current()),
            lock_acquired: false,
            lock_acquired_flag: Some(flag.clone()),
            percpu_slot: current_thread_slot(),
        };
        // push_waiter_with_recheck 在 wait_lock 内设 WAITING bit → trylock 重试 → 入队
        // 注意：should_sleep_fn 在入队之后、park 循环内调用（对齐 bcachefs __six_lock_slowpath）
        // 内置的 trylock 替代了之前的独立 six_trylock_ip + push_waiter 两步，闭合 C1 竞态窗口
        if self.push_waiter_with_recheck(&waiter_box) {
            // push_waiter_with_recheck 内 try_lock_read 增了 state.read_count
            // 但未增线程本地 THREAD_READ_CNT，此处补偿
            if type_ == SixLockType::Read && self.readers.is_none() {
                THREAD_READ_CNT.with(|c| c.set(c.get() + 1));
            }
            wait.lock_acquired = true;
            return 0;
        }

        // 对应 bcachefs __six_lock_slowpath six.c:624: schedule() 让步 CPU
        std::thread::yield_now();

        loop {
            // park 前调用 should_sleep_fn
            // 优先使用传入的 should_sleep，其次使用 thread-local btree 死锁检测
            let ret = if let Some(ref sleep_fn) = should_sleep {
                sleep_fn(self, wait)
            } else {
                THREAD_SHOULD_SLEEP.with(|c| {
                    if let Some(ref f) = *c.borrow() {
                        f(self, wait)
                    } else {
                        0
                    }
                })
            };
            if ret != 0 {
                // S5: 对应 bcachefs __six_lock_slowpath should_sleep 错误路径
                // wait_lock 保护下原子检查 waker 是否已替我们声明锁
                let _lock = self.wait_lock.lock();
                let acquired = flag.load(Ordering::Acquire);
                if !acquired {
                    self.wait_fifo.remove_by_thread(thread::current().id());
                    if self.wait_fifo.is_empty() {
                        if self.read_state() & WAITING_READ_BIT != 0 {
                            self.state.fetch_and(!WAITING_READ_BIT, Ordering::Release);
                        }
                        if self.read_state() & WAITING_INTENT_BIT != 0 {
                            self.state.fetch_and(!WAITING_INTENT_BIT, Ordering::Release);
                        }
                        if self.read_state() & WAITING_WRITE_BIT != 0 {
                            self.state.fetch_and(!WAITING_WRITE_BIT, Ordering::Release);
                        }
                    }
                }
                drop(_lock);
                if acquired {
                    self.unlock_ip(type_, 0);
                } else if type_ == SixLockType::Write {
                    self.state.fetch_and(!WRITE_BIT, Ordering::Release);
                    let s = self.read_state();
                    self.wakeup_lock_type(s, SixLockType::Read);
                }
                wait.lock_acquired = false;
                return ret;
            }
            thread::park();
            // O(1) 带外检查：waker 已替我们声明锁（通过 lock_acquired_flag）
            if flag.load(Ordering::Acquire) {
                // waker 已替我们声明锁，跳过 trylock
                // 读锁路径：waker 增了 state 但未增线程本地计数 THREAD_READ_CNT
                if type_ == SixLockType::Read && self.readers.is_none() {
                    THREAD_READ_CNT.with(|c| c.set(c.get() + 1));
                }
                self.remove_self_from_fifo();
                wait.lock_acquired = true;
                return 0;
            }
            // 非 handoff wake：使用 per-type 正确 trylock（对齐 bcachefs __six_lock_slowpath
            // 的 trylock 行为）。Write 路径必须用 try_lock_write_preset（因为 WRITE_BIT 已预设，
            // try_lock_write 会错误地认为写锁已被其他线程持有）。
            let try_ok = match type_ {
                SixLockType::Read => self.six_trylock_read(),
                SixLockType::Intent => self.six_trylock_intent(),
                SixLockType::Write => self.try_lock_write_preset(),
            };
            if try_ok {
                if type_ == SixLockType::Read && self.readers.is_none() {
                    THREAD_READ_CNT.with(|c| c.set(c.get() + 1));
                }
                self.remove_self_from_fifo();
                wait.lock_acquired = true;
                return 0;
            }
        }
    }

    // ── 锁转换 API（对应 bcachefs six_lock_downgrade 等） ──

    /// 将 intent 锁降级为读锁（对应 bcachefs six_lock_downgrade）
    ///
    /// 调用者必须已持有 intent 锁。
    /// 降级后调用者持有读锁。
    fn lock_downgrade(&self) {
        // 对应 C: six_lock_increment(lock, SIX_LOCK_read) + six_unlock_intent(lock)
        self.lock_increment(SixLockType::Read);
        self.six_unlock_intent();
    }

    /// bcachefs 风格别名：`six_lock_downgrade`
    pub fn six_lock_downgrade(&self) {
        self.lock_downgrade();
    }

    /// 尝试将读锁升级为 intent 锁（对应 bcachefs six_lock_tryupgrade）
    ///
    /// 调用者必须已持有读锁。
    /// 返回 true 表示升级成功，调用者现在持有 intent 锁。
    fn lock_tryupgrade(&self) -> bool {
        self.try_upgrade_read_to_intent()
    }

    /// bcachefs 风格别名：`six_lock_tryupgrade`
    pub fn six_lock_tryupgrade(&self) -> bool {
        self.lock_tryupgrade()
    }

    /// 通用锁类型转换（对应 bcachefs six_six_trylock_convert）
    ///
    /// 支持 read↔intent 之间的转换（不含 write）。
    /// `from` 和 `to` 必须不同且均不为 write。
    pub fn six_trylock_convert(&self, from: SixLockType, to: SixLockType) -> bool {
        debug_assert!(
            from != SixLockType::Write && to != SixLockType::Write,
            "six_trylock_convert does not support write locks"
        );
        if to == from {
            return true;
        }
        if to == SixLockType::Read {
            // intent → read
            self.lock_downgrade();
            true
        } else {
            // read → intent
            self.lock_tryupgrade()
        }
    }

    // ── 重入计数 API（对应 bcachefs six_lock_increment） ──

    /// 增加已持有锁的引用计数（对应 bcachefs six_lock_increment）
    ///
    /// 用于上层提供重入语义：当已知锁已被当前线程以 `type_` 类型持有时，
    /// 调此方法增加计数，后续需要相应次数的 unlock 才能完全释放。
    ///
    /// 对于 Read：增加 reader count（percpu 或 atomic）
    /// 对于 Intent：增加 intent_recurse
    /// 对于 Write：先增加 write_recurse，再按 C 的 fallthrough
    /// 同时增加 intent_recurse
    fn lock_increment(&self, type_: SixLockType) {
        match type_ {
            SixLockType::Read => {
                if let Some(ref readers) = self.readers {
                    let slot = current_thread_slot() as usize % readers.len();
                    readers[slot].fetch_add(1, Ordering::Relaxed);
                } else {
                    self.state.fetch_add(1, Ordering::Relaxed);
                    THREAD_READ_CNT.with(|c| c.set(c.get() + 1));
                }
            }
            SixLockType::Intent => unsafe {
                *self.intent_recurse.get() += 1;
            },
            SixLockType::Write => unsafe {
                *self.write_recurse.get() += 1;
                *self.intent_recurse.get() += 1;
            },
        }
    }

    /// bcachefs 风格别名：`six_lock_increment`
    pub fn six_lock_increment(&self, type_: SixLockType) {
        self.lock_increment(type_);
    }

    // ── 等待者管理 API ──

    /// 唤醒指定类型等待者（对应 bcachefs __six_lock_wakeup six.c:316-410）
    ///
    /// 结构严格对齐：
    /// 1. Read: 逐 waiter 调 trylock，ret <= 0 → goto out（受 BC1 级联支持）
    /// 2. Intent/Write: 找最老 waiter，ret <= 0 → goto out
    /// 3. 成功则统一从 FIFO 删除 + 通过 lock_acquired_flag 做带外 handoff
    /// 4. 统一清除 WAITING bit
    /// 5. ret < 0 时级联到下一个锁类型
    /// 6. shrink FIFO
    fn __wakeup_lock_type(&self, mut lock_type: SixLockType, rscs: &RcuRSCS) {
        loop {
            let ret = self.__wakeup_one_type(lock_type, rscs);
            if ret >= 0 {
                break;
            }
            // ret < 0 → 级联：ret = -1 - lock_type → lock_type = -ret - 1
            lock_type = match -ret - 1 {
                0 => SixLockType::Read,
                1 => SixLockType::Intent,
                2 => SixLockType::Write,
                _ => break,
            };
        }
    }

    /// 对应 bcachefs __six_lock_wakeup 主体：单轮唤醒 + WAITING bit 清除
    /// 返回：>0 = 成功无级联, 0 = 已处理终止, <0 = 需级联（-lock_type - 1）
    fn __wakeup_one_type(&self, lock_type: SixLockType, rscs: &RcuRSCS) -> i32 {
        use SixLockType::*;
        let mut ret: i32 = 0;

        // ── Read: 唤醒所有读者 ──
        if lock_type == Read {
            for (i, slot) in self.wait_fifo.slots().iter().enumerate() {
                let opt = slot.read(rscs);
                if let Some(Some(ref waiter)) = opt.as_ref() {
                    if !matches!(waiter.lock_type, Read) {
                        continue;
                    }
                    // 对应 six.c:334 — __do_six_trylock(Read, try=false)
                    if !self.try_lock_read_for(waiter.percpu_slot) {
                        // 对应 six.c:335-336 — ret <= 0 goto out
                        // try_lock_read_for 失败 → Write 锁在持有 → 级联 Write
                        ret = -1 - Write as i32;
                        break;
                    }
                    let flag = waiter.lock_acquired_flag.clone();
                    let thread = waiter.thread.clone();
                    let _ = opt;
                    self.wait_fifo.remove_by_index(i);
                    if let Some(ref f) = flag {
                        f.store(true, Ordering::Release); // smp_store_release
                    }
                    if let Some(ref t) = thread {
                        t.unpark(); // wake_up_process
                    }
                }
            }
        } else {
            // ── Intent / Write: 唤醒最老等待者 ──
            let mut oldest_idx = None;
            let mut oldest_trans_id = u64::MAX;
            let mut n_matches = 0u32;

            for (i, slot) in self.wait_fifo.slots().iter().enumerate() {
                let opt = slot.read(rscs);
                if let Some(Some(ref waiter)) = opt.as_ref() {
                    if waiter.lock_type != lock_type {
                        continue;
                    }
                    n_matches += 1;
                    // 对应 six.c:374 — time_before64 选最早事务
                    if waiter.trans_id < oldest_trans_id {
                        oldest_trans_id = waiter.trans_id;
                        oldest_idx = Some(i);
                    }
                }
            }

            if let Some(idx) = oldest_idx {
                let slot = &self.wait_fifo.slots()[idx];
                let opt = slot.read(rscs);
                if let Some(Some(ref waiter)) = opt.as_ref() {
                    let tid = waiter.thread.as_ref().map(|t| t.id()).unwrap();
                    let acquired = match lock_type {
                        Write => self.try_lock_write_preset_for(tid),
                        Intent => self.try_lock_intent_for(tid),
                        _ => unreachable!(),
                    };
                    // 对应 six.c:381-383 — ret <= 0 goto out
                    if !acquired {
                        ret = 0;
                    } else {
                        let flag = waiter.lock_acquired_flag.clone();
                        let thread = waiter.thread.clone();
                        let _ = opt;
                        self.wait_fifo.remove_by_index(idx);
                        if let Some(ref f) = flag {
                            f.store(true, Ordering::Release);
                        }
                        if let Some(ref t) = thread {
                            t.unpark();
                        }
                        n_matches -= 1;
                        // 对应 six.c:397-398 — n_matches > 1 保留 WAITING bit, goto shrink
                        if n_matches > 1 {
                            return 1;
                        }
                    }
                }
            }
        }

        // 对应 six.c:402 — six_clear_bitmask(WAITING)
        let bit = match lock_type {
            Read => WAITING_READ_BIT,
            Intent => WAITING_INTENT_BIT,
            Write => WAITING_WRITE_BIT,
        };
        self.state.fetch_and(!bit, Ordering::Release);

        ret
    }

    /// 唤醒指定类型等待者（对应 bcachefs six_lock_wakeup）
    ///
    /// 封装 wait_lock 获取 + 双检 + RCU 临界区，委派到 __wakeup_lock_type。
    fn wakeup_lock_type(&self, state: u32, lock_type: SixLockType) {
        let waiting_bit = match lock_type {
            SixLockType::Read => WAITING_READ_BIT,
            SixLockType::Intent => WAITING_INTENT_BIT,
            SixLockType::Write => WAITING_WRITE_BIT,
        };

        if state & waiting_bit == 0 {
            return;
        }

        // 对应 bcachefs six_lock_wakeup six.c:416-417:
        // 写锁唤醒时若读者仍活跃，直接跳过（reader 最终释放时会再次触发唤醒）
        // 防止 wait_lock 内 try_lock_write_preset_for 因读者活跃失败后错误清 WAITING bit
        if lock_type == SixLockType::Write && (state & READ_COUNT_MASK) != 0 {
            return;
        }

        let _lock = self.wait_lock.lock();

        if self.read_state() & waiting_bit == 0 {
            return;
        }

        with_rcu(|_rcu, thread| {
            thread.rscs(|rscs| {
                self.__wakeup_lock_type(lock_type, rscs);
            })
        });
    }

    /// 唤醒所有等待者（对应 bcachefs six_lock_wakeup_all six.c:969-995）
    ///
    /// 1. 逐个类型唤醒（每个独立获取/释放 wait_lock）
    /// 2. 对剩余 waiter（trylock 失败的）做无条件 unpark
    pub fn six_lock_wakeup_all(&self) {
        // 1. 逐个类型唤醒 (每个独立获取/释放 wait_lock)
        self.wakeup_lock_type(self.read_state(), SixLockType::Read);
        self.wakeup_lock_type(self.read_state(), SixLockType::Intent);
        self.wakeup_lock_type(self.read_state(), SixLockType::Write);

        // 2. 对剩余 waiter（trylock 失败的）做无条件 unpark
        //    这些 waiter 醒来后 flag 为 false，会回到 park 循环
        let _lock = self.wait_lock.lock();
        with_rcu(|_rcu, thread| {
            thread.rscs(|rscs| {
                for slot in self.wait_fifo.slots() {
                    let opt = slot.read(rscs);
                    if let Some(Some(ref waiter)) = opt.as_ref() {
                        if let Some(ref t) = waiter.thread {
                            t.unpark();
                        }
                    }
                }
            })
        });
    }

    /// 收集 wait_fifo 中的等待者信息（用于死锁检测）
    ///
    /// 对应 bcachefs `bch2_check_for_deadlock` Phase 1: waitlist 快照
    /// (locking.c:565-571)。遍历 wait_fifo，对每个 WAITING 条目返回
    /// (waiter_trans_id, lock_id, holder_trans_id) 三元组。
    /// holder 通过检查 lock 的当前持有者推断。
    pub(crate) fn sx_collect_wait_fifo_waiter_info(
        &self,
        lock_id: u64,
        holder_trans_id: u64,
    ) -> Vec<crate::lock::deadlock::WaiterInfo> {
        let _lock = self.wait_lock.lock();
        with_rcu(|_rcu, thread| {
            let mut out = Vec::new();
            thread.rscs(|rscs| {
                for slot in self.wait_fifo.slots() {
                    let opt = slot.read(rscs);
                    if let Some(Some(ref waiter)) = opt.as_ref() {
                        out.push(crate::lock::deadlock::WaiterInfo {
                            trans_id: waiter.trans_id,
                            lock_id,
                            waiting_for_trans_id: holder_trans_id,
                        });
                    }
                }
            });
            out
        })
    }

    /// 返回各锁类型的当前持有计数（对应 bcachefs six_lock_counts six.c:1004-1016）
    pub fn six_lock_counts(&self) -> SixLockCount {
        let state = self.read_state();
        SixLockCount {
            n: [
                if self.readers.is_some() {
                    // percpu 模式：从所有 slot 汇总
                    self.reader_count()
                } else {
                    // 标准模式：直接从 state 读取
                    self.read_count(state)
                },
                if self.has_intent_lock(state) { 1 } else { 0 }
                    + unsafe { *self.intent_recurse.get() },
                if self.has_write_lock(state) { 1 } else { 0 },
            ],
        }
    }

    /// 直接操作读者计数（对应 bcachefs six_lock_readers_add six.c:1039-1048）
    ///
    /// 用于上层实现重入：当同时持有读锁和 intent 锁时，
    /// 写锁获取需要暂时减去自身读锁计数。
    /// 调用者需确保计数不会变为负数。
    pub fn six_lock_readers_add(&self, nr: i32) {
        if let Some(ref readers) = self.readers {
            let slot = current_thread_slot() as usize % readers.len();
            if nr >= 0 {
                readers[slot].fetch_add(nr as u32, Ordering::Relaxed);
            } else {
                let prev = readers[slot].fetch_sub((-nr) as u32, Ordering::Relaxed);
                debug_assert!(prev >= (-nr) as u32, "six_lock_readers_add underflow");
            }
        } else {
            // atomic_add 支持有符号加法（负数值通过 wrapping 实现）
            if nr < 0 {
                let prev = self.read_count(self.state.load(Ordering::Relaxed));
                debug_assert!(
                    prev >= (-nr) as u32,
                    "six_lock_readers_add underflow non-percpu"
                );
            }
            self.state.fetch_add(nr as u32, Ordering::Relaxed);
            THREAD_READ_CNT.with(|c| c.set(c.get().wrapping_add(nr as u32)));
        }
    }

    /// nospin 标志是否已设置
    pub(crate) fn is_nospin(&self) -> bool {
        self.read_state() & NOSPIN_BIT != 0
    }

    /// 设置 nospin bit（跳过自旋，直接休眠）
    pub(crate) fn set_nospin(&self) {
        self.state.fetch_or(NOSPIN_BIT, Ordering::Relaxed);
    }

    /// 清除 nospin bit
    pub(crate) fn clear_nospin(&self) {
        self.state.fetch_and(!NOSPIN_BIT, Ordering::Relaxed);
    }

    /// 设置对应的等待标志位
    fn set_waiting_bit(&self, lock_type: SixLockType) {
        match lock_type {
            SixLockType::Read => {
                self.state.fetch_or(WAITING_READ_BIT, Ordering::Relaxed);
            }
            SixLockType::Intent => {
                self.state.fetch_or(WAITING_INTENT_BIT, Ordering::Relaxed);
            }
            SixLockType::Write => {
                self.state.fetch_or(WAITING_WRITE_BIT, Ordering::Relaxed);
            }
        }
    }

    /// 清除对应的等待标志位
    fn clear_waiting_bit(&self, lock_type: SixLockType) {
        match lock_type {
            SixLockType::Read => {
                self.state.fetch_and(!WAITING_READ_BIT, Ordering::Release);
            }
            SixLockType::Intent => {
                self.state.fetch_and(!WAITING_INTENT_BIT, Ordering::Release);
            }
            SixLockType::Write => {
                self.state.fetch_and(!WAITING_WRITE_BIT, Ordering::Release);
            }
        }
    }

    /// 推送等待者到 WaitFifo（带 wait_lock 保护的 trylock 重试）
    ///
    /// 对应 bcachefs `__six_lock_slowpath` 的 wait_lock 内重试协议（C1 fix）：
    ///
    /// 1. 持 `wait_lock` 设 WAITING bit
    /// 2. 在 wait_lock 内 trylock 重试（关闭 unlock → push_waiter 间的竞态窗口）
    /// 3. 若重试成功：清 WAITING bit，返回 `true`（锁已获取，未入队）
    /// 4. 若重试失败：入 FIFO，返回 `false`（等待者已入队，WAITING bit 已设）
    ///
    /// 调用者职责：
    /// - 返回 `true`：锁已获取，不应再 park（如读锁路径需同步 THREAD_READ_CNT）
    /// - 返回 `false`：等待者已入队，应进入 park+flag 循环
    fn push_waiter_with_recheck(&self, waiter: &WaiterBox) -> bool {
        let _lock = self.wait_lock.lock();

        // Step 1: 先设 WAITING bit（bcachefs 协议：在 wait_lock 内设，防止 unlock 漏唤醒）
        self.set_waiting_bit(waiter.lock_type);

        // Step 2: wait_lock 内 trylock 重试（对应 bcachefs __do_six_trylock(try=false)）
        // 检查锁是否在初始 trylock 失败后已被释放
        let acquired = match waiter.lock_type {
            SixLockType::Read => self.six_trylock_read(),
            SixLockType::Intent => self.six_trylock_intent(),
            SixLockType::Write => self.try_lock_write_preset(),
        };

        if acquired {
            // Step 3: 锁已可用，无需入队
            self.clear_waiting_bit(waiter.lock_type);
            return true;
        }

        // Step 4: 入 FIFO（WAITING bit 保持设置）
        if self
            .wait_fifo
            .push(
                waiter.trans_id,
                waiter.lock_type,
                waiter.seq,
                waiter.thread.clone(),
                waiter.percpu_slot,
                waiter.lock_acquired_flag.clone(),
            )
            .is_none()
        {
            // FIFO 满（不应发生），清 WAITING bit
            self.clear_waiting_bit(waiter.lock_type);
        }
        false
    }

    /// 推送等待者到 WaitFifo（无 trylock 重试，仅用于 FIFO 行为测试）
    ///
    /// 与 `push_waiter_with_recheck` 的区别：本方法不尝试 wait_lock 内重试，
    /// 直接将 waiter 入队并设 WAITING bit。仅用于 FIFO 测试用例验证入队/出队逻辑。
    #[cfg(test)]
    fn push_waiter_test(&self, waiter: &WaiterBox) -> bool {
        let _lock = self.wait_lock.lock();
        let pushed = self
            .wait_fifo
            .push(
                waiter.trans_id,
                waiter.lock_type,
                waiter.seq,
                waiter.thread.clone(),
                waiter.percpu_slot,
                waiter.lock_acquired_flag.clone(),
            )
            .is_some();
        if pushed {
            self.set_waiting_bit(waiter.lock_type);
        }
        pushed
    }

    /// 从 WaitFifo 中移除当前线程
    ///
    /// 在 park+loop 成功获取锁后调用，清理 fifo 中的等待记录。
    /// wait_lock 保护 FIFO remove 与 WAITING bit 清理的原子性。
    fn remove_self_from_fifo(&self) {
        let _lock = self.wait_lock.lock();
        self.wait_fifo.remove_by_thread(thread::current().id());
        if self.wait_fifo.is_empty() {
            if self.read_state() & WAITING_READ_BIT != 0 {
                self.clear_waiting_bit(SixLockType::Read);
            }
            if self.read_state() & WAITING_INTENT_BIT != 0 {
                self.clear_waiting_bit(SixLockType::Intent);
            }
            if self.read_state() & WAITING_WRITE_BIT != 0 {
                self.clear_waiting_bit(SixLockType::Write);
            }
        }
    }

    /// 当前锁状态的调试描述
    pub(crate) fn debug_state(&self) -> String {
        let state = self.read_state();
        format!(
            "SixLock{{ readers={}, intent={}, write={}, waiting_r={}, waiting_i={}, waiting_w={}, nospin={} }}",
            self.read_count(state),
            self.has_intent_lock(state),
            self.has_write_lock(state),
            (state & WAITING_READ_BIT) != 0,
            (state & WAITING_INTENT_BIT) != 0,
            (state & WAITING_WRITE_BIT) != 0,
            (state & NOSPIN_BIT) != 0,
        )
    }
}

impl Default for SixLock {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for SixLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SixLock")
            .field("state", &self.read_state())
            .field("seq", &self.lock_seq())
            .field("readers_count", &self.reader_count())
            .finish()
    }
}

// ─── 测试 ───────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    // ── 基本功能测试 ──

    #[test]
    fn test_read_lock_basic() {
        let lock = SixLock::new();
        assert!(lock.six_trylock_read());
        assert!(lock.six_trylock_read()); // 读锁可共享
        assert!(lock.six_trylock_intent());
        // 对应 bcachefs __do_six_trylock(Write) six.c:159-164
        // six_trylock_write 不自排除读者——调用方需通过 lock_readers_add(-N) 临时排除。
        // 验证：持有读锁时直接调 six_trylock_write 应失败（自己读锁阻塞自己）。
        assert!(!lock.six_trylock_write());
        // 正确做法：调用方先用 lock_readers_add 排除自身读者
        let readers = lock.reader_count();
        assert_eq!(readers, 2);
        lock.six_lock_readers_add(-(readers as i32));
        assert!(lock.six_trylock_write()); // reader_count 临时清零后写锁成功
        lock.six_lock_readers_add(readers as i32); // 恢复读者计数
        lock.six_unlock_write();
        lock.six_unlock_intent();
        lock.six_unlock_read();
        lock.six_unlock_read();
        assert!(lock.six_trylock_intent());
        assert!(lock.six_trylock_write()); // 读者释放后写锁成功
        lock.six_unlock_write();
        lock.six_unlock_intent();
    }

    #[test]
    fn test_write_lock_exclusive() {
        let lock = SixLock::new();
        assert!(lock.six_trylock_intent());
        assert!(lock.six_trylock_write());
        assert!(!lock.six_trylock_read()); // 写锁阻塞读（同线程）

        // 从另一个线程测试写锁排他
        let same = Arc::new(lock);
        let l = same.clone();
        let h = thread::spawn(move || {
            assert!(
                !l.six_trylock_intent(),
                "other thread should not get intent lock"
            );
        });
        h.join().unwrap();
        same.six_unlock_write();
        same.six_unlock_intent();
        assert!(same.six_trylock_read()); // 写释放后可读
        same.six_unlock_read();
    }

    #[test]
    fn test_intent_lock() {
        let lock = Arc::new(SixLock::new());
        assert!(lock.six_trylock_intent());
        assert!(lock.six_trylock_read()); // intent 不阻塞读
        lock.six_unlock_read();

        // 从另一个线程测试 intent 之间互斥
        let l = lock.clone();
        let h = thread::spawn(move || {
            assert!(
                !l.six_trylock_intent(),
                "other thread should not get intent lock"
            );
        });
        h.join().unwrap();

        assert!(lock.six_trylock_write());
        lock.six_unlock_write();
        lock.six_unlock_intent();
        assert!(lock.six_trylock_intent());
        assert!(lock.six_trylock_write()); // intent 释放后写成功
        lock.six_unlock_write();
        lock.six_unlock_intent();
    }

    #[test]
    fn test_intent_blocks_other_write_owner() {
        let lock = Arc::new(SixLock::new());
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let holder = lock.clone();

        let h = thread::spawn(move || {
            assert!(holder.six_trylock_intent());
            ready_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            holder.six_unlock_intent();
        });

        ready_rx.recv().unwrap();
        assert!(!lock.six_trylock_intent());
        release_tx.send(()).unwrap();
        h.join().unwrap();
    }

    #[test]
    fn test_write_trylock_with_intent_owner() {
        let lock = SixLock::new();
        assert!(lock.six_trylock_intent());
        assert!(lock.six_trylock_write());
        lock.six_unlock_write();
        lock.six_unlock_intent();
    }

    #[test]
    #[should_panic(expected = "write lock requires intent ownership")]
    fn test_write_trylock_requires_intent_owner() {
        SixLock::new().six_trylock_write();
    }

    #[test]
    fn test_intent_reentrant() {
        let lock = SixLock::new();
        assert!(lock.six_trylock_intent());
        assert!(lock.six_trylock_intent()); // 同线程重入
        lock.six_unlock_intent();
        assert!(lock.is_intent_locked()); // 还有一层
        lock.six_unlock_intent();
        assert!(!lock.is_intent_locked());
    }

    #[test]
    fn test_write_reentrant() {
        let lock = SixLock::new();
        assert!(lock.six_trylock_intent());
        assert!(lock.six_trylock_write());
        assert!(lock.six_trylock_write()); // 同线程写锁重入
        lock.six_unlock_write();
        assert!(lock.is_write_locked()); // 还有一层
        lock.six_unlock_write();
        assert!(!lock.is_write_locked());
        lock.six_unlock_intent();
    }

    // ── 升级/降级测试 ──

    #[test]
    fn test_intent_try_lock_write() {
        let lock = Arc::new(SixLock::new());
        assert!(lock.six_trylock_intent());

        // 其他线程持有读锁时不能升级
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let reader_lock = lock.clone();
        let reader = thread::spawn(move || {
            assert!(reader_lock.six_trylock_read());
            ready_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            reader_lock.six_unlock_read();
        });

        ready_rx.recv().unwrap();
        assert!(!lock.six_trylock_write());
        release_tx.send(()).unwrap();
        reader.join().unwrap();

        // 无读者时可以升级
        assert!(lock.six_trylock_write());
        assert!(lock.is_write_locked());
        lock.six_unlock_write();

        // 写释放后 intent 还在
        assert!(lock.is_intent_locked());
        lock.six_unlock_intent();
    }

    #[test]
    fn test_write_unlock_preserves_intent() {
        let lock = SixLock::new();
        assert!(lock.six_trylock_intent());
        assert!(lock.six_trylock_write());
        lock.six_unlock_write();
        assert!(lock.is_intent_locked());
        assert!(!lock.is_write_locked());
        // 降级后读锁可获取
        assert!(lock.six_trylock_read());
        lock.six_unlock_read();
        lock.six_unlock_intent();
    }

    #[test]
    fn test_downgrade_intent_to_read() {
        let lock = SixLock::new();
        assert!(lock.six_trylock_intent());
        lock.lock_downgrade();
        assert!(!lock.is_intent_locked());
        // 现在持有读锁，可以和其他读者共享
        let r1 = lock.six_trylock_read();
        assert!(r1);
        lock.six_unlock_read();
        lock.six_unlock_read();
    }

    // ── 锁冲突矩阵测试 ──

    #[test]
    fn test_lock_conflict_matrix() {
        assert!(!lock_conflicts(SixLockType::Read, SixLockType::Read));
        assert!(!lock_conflicts(SixLockType::Read, SixLockType::Intent));
        assert!(lock_conflicts(SixLockType::Read, SixLockType::Write));
        assert!(!lock_conflicts(SixLockType::Intent, SixLockType::Read));
        assert!(lock_conflicts(SixLockType::Intent, SixLockType::Intent));
        assert!(lock_conflicts(SixLockType::Intent, SixLockType::Write));
        assert!(lock_conflicts(SixLockType::Write, SixLockType::Read));
        assert!(lock_conflicts(SixLockType::Write, SixLockType::Intent));
        assert!(lock_conflicts(SixLockType::Write, SixLockType::Write));
    }

    // ── Percpu reader 测试 ──

    #[test]
    fn test_percpu_read_lock() {
        let lock = SixLock::with_percpu(8);
        assert!(lock.six_trylock_read());
        // percpu 模式下，read_count 应该反映 percpu + atomic
        assert!(lock.reader_count() > 0);
        lock.six_unlock_read();
        assert_eq!(lock.reader_count(), 0);
    }

    #[test]
    fn test_percpu_write_drain() {
        let lock = SixLock::with_percpu(4);
        assert!(lock.six_trylock_read());
        lock.six_unlock_read();
        assert!(lock.six_trylock_intent());
        assert!(lock.six_trylock_write()); // percpu readers drained
        lock.six_unlock_write();
        lock.six_unlock_intent();
    }

    // ── 并发测试 ──

    #[test]
    fn test_concurrent_readers() {
        let lock = Arc::new(SixLock::new());
        let mut handles = vec![];

        for _ in 0..8 {
            let l = lock.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..100 {
                    assert!(l.six_trylock_read());
                    // 模拟一些工作
                    std::hint::spin_loop();
                    l.six_unlock_read();
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        // 验证没有持锁泄漏
        assert_eq!(lock.reader_count(), 0);
        assert!(lock.six_trylock_intent());
        assert!(lock.six_trylock_write());
        lock.six_unlock_write();
        lock.six_unlock_intent();
    }

    #[test]
    fn test_read_write_contention() {
        let lock = Arc::new(SixLock::new());
        let mut handles = vec![];

        // 一个写线程
        let l = lock.clone();
        handles.push(thread::spawn(move || {
            for _ in 0..50 {
                while !l.six_trylock_intent() {
                    std::hint::spin_loop();
                }
                loop {
                    if l.six_trylock_write() {
                        std::hint::spin_loop();
                        l.six_unlock_write();
                        l.six_unlock_intent();
                        break;
                    }
                    std::hint::spin_loop();
                }
            }
        }));

        // 多个读线程
        for _ in 0..4 {
            let l = lock.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..200 {
                    loop {
                        if l.six_trylock_read() {
                            std::hint::spin_loop();
                            l.six_unlock_read();
                            break;
                        }
                        std::hint::spin_loop();
                    }
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(lock.reader_count(), 0);
    }

    #[test]
    fn test_seq_increment_on_write_unlock() {
        let lock = SixLock::new();
        let s1 = lock.lock_seq();
        assert!(lock.six_trylock_intent());
        assert!(lock.six_trylock_write());
        lock.six_unlock_write();
        lock.six_unlock_intent();
        let s2 = lock.lock_seq();
        assert!(s2 > s1, "seq should increment after write unlock");
    }

    /// 对应本地 six.c:948-953：write increment 必须 fall through
    /// 到 intent increment，使两条 linked path 各自的 write+intent unlock 成对。
    #[test]
    fn test_six_lock_increment_write_also_increments_intent() {
        let lock = SixLock::new();
        assert!(lock.six_trylock_intent());
        assert!(lock.six_trylock_write());

        lock.six_lock_increment(SixLockType::Write);
        assert_eq!(lock.six_lock_counts().n, [0, 2, 1]);

        lock.six_unlock_write();
        lock.six_unlock_intent();
        assert!(lock.is_write_locked());
        assert!(lock.is_intent_locked());

        lock.six_unlock_write();
        lock.six_unlock_intent();
        assert!(!lock.is_write_locked());
        assert!(!lock.is_intent_locked());
    }

    // ── 升级 API 测试 ──

    #[test]
    fn test_upgrade_read_to_intent() {
        let lock = SixLock::new();
        assert!(lock.six_trylock_read());
        // 持有读锁时可以升级为 intent
        assert!(lock.try_upgrade_read_to_intent());
        assert!(lock.is_intent_locked_by_current());
        // intent 已持有，读锁已释放（read_count 已递减）
        // raw write trylock 不把 intent 当作直接冲突位
        assert!(lock.six_trylock_write());
        lock.six_unlock_write();
        lock.six_unlock_intent();
    }

    #[test]
    fn test_upgrade_read_to_intent_ignores_write_bit() {
        let lock = SixLock::new();
        assert!(lock.six_trylock_read());
        lock.state.fetch_or(WRITE_BIT, Ordering::SeqCst);
        assert!(lock.try_upgrade_read_to_intent());
        lock.six_unlock_intent();
    }

    #[test]
    fn test_upgrade_read_to_intent_fail_when_conflict() {
        let lock = Arc::new(SixLock::new());
        // 本线程持有读锁
        assert!(lock.six_trylock_read());
        // 另一线程持有 intent 锁（应该阻止升级）
        let l = lock.clone();
        let h = thread::spawn(move || {
            assert!(l.six_trylock_intent());
        });
        h.join().unwrap();
        // 别人持有 intent，升级应该失败
        assert!(!lock.try_upgrade_read_to_intent());
        // 读锁仍然在
        lock.six_unlock_read();
        // 释放对方的 intent
        // （对方线程已结束，但 intent bit 还在——这是设计约束）
        // 实际使用中 intent 由持有者释放
    }

    #[test]
    fn test_upgrade_read_to_intent_percpu() {
        let lock = SixLock::with_percpu(8);
        assert!(lock.six_trylock_read());
        assert!(lock.try_upgrade_read_to_intent());
        assert!(lock.is_intent_locked_by_current());
        lock.six_unlock_intent();
    }

    #[test]
    fn test_upgrade_read_to_intent_not_holding_read() {
        let lock = SixLock::new();
        // 没有持有读锁时，升级应该失败（但 debug_assert 会在 debug 模式 panic）
        // release 模式下返回 false（因为 read_count == 0）
        assert!(!lock.try_upgrade_read_to_intent());
    }

    // ── Relock API 测试 ──

    #[test]
    fn test_relock_read_success() {
        let lock = SixLock::new();
        assert!(lock.six_trylock_read());
        let seq = lock.lock_seq();
        lock.six_unlock_read();
        // 没有写操作，relock 应该成功
        assert!(lock.six_relock_read(seq));
        lock.six_unlock_read();
    }

    #[test]
    fn test_relock_read_fail_after_write() {
        let lock = SixLock::new();
        assert!(lock.six_trylock_read());
        let seq = lock.lock_seq();
        lock.six_unlock_read();

        // 中间发生写操作
        assert!(lock.six_trylock_intent());
        assert!(lock.six_trylock_write());
        lock.six_unlock_write();
        lock.six_unlock_intent();

        // seq 已变化，relock 应该失败
        assert!(!lock.six_relock_read(seq));
    }

    #[test]
    fn test_relock_read_fail_with_wrong_seq() {
        let lock = SixLock::new();
        // 从未获取过锁，seq 为 0
        assert!(!lock.six_relock_read(42));
    }

    #[test]
    fn test_relock_intent_success() {
        let lock = SixLock::new();
        assert!(lock.six_trylock_intent());
        let seq = lock.lock_seq();
        lock.six_unlock_intent();
        // 没有写操作，relock 应该成功
        assert!(lock.six_relock_intent(seq));
        lock.six_unlock_intent();
    }

    #[test]
    fn test_relock_intent_fail_after_write() {
        let lock = SixLock::new();
        assert!(lock.six_trylock_intent());
        let seq = lock.lock_seq();
        lock.six_unlock_intent();

        // 中间发生写操作
        assert!(lock.six_trylock_intent());
        assert!(lock.six_trylock_write());
        lock.six_unlock_write();
        lock.six_unlock_intent();

        assert!(!lock.six_relock_intent(seq));
    }

    #[test]
    fn test_relock_read_fail_when_six_lock_contended() {
        let lock = Arc::new(SixLock::new());
        assert!(lock.six_trylock_read());
        let seq = lock.lock_seq();
        lock.six_unlock_read();

        // 另一线程获取写锁，导致 seq 变化
        let l = lock.clone();
        let h = thread::spawn(move || {
            assert!(l.six_trylock_intent());
            assert!(l.six_trylock_write());
            l.six_unlock_write();
            l.six_unlock_intent();
        });
        h.join().unwrap();

        assert!(!lock.six_relock_read(seq));
    }

    // ── DeadlockDetector 适配测试 ──

    fn make_waiters(pairs: &[(u64, u64, u64)]) -> Vec<crate::lock::deadlock::WaiterInfo> {
        pairs
            .iter()
            .map(|&(t, l, h)| crate::lock::deadlock::WaiterInfo {
                trans_id: t,
                lock_id: l,
                waiting_for_trans_id: h,
            })
            .collect()
    }

    #[test]
    fn test_detector_complex_cycle() {
        use crate::lock::deadlock::DeadlockDetector;

        let mut d = DeadlockDetector::new();
        // 4 个事务形成环：T1→L2→T2→L3→T3→L4→T4→L1→T1
        let waiters = make_waiters(&[(1, 102, 2), (2, 103, 3), (3, 104, 4), (4, 101, 1)]);
        assert!(d.detect(1, 102, &waiters), "should detect 4-way deadlock");
    }

    #[test]
    fn test_detector_multi_cycle() {
        use crate::lock::deadlock::DeadlockDetector;

        let mut d = DeadlockDetector::new();
        // 两个独立的死循环
        // Cycle 1: T1→L2→T2→L1→T1
        // Cycle 2: T3→L4→T4→L3→T3
        let waiters = make_waiters(&[(1, 102, 2), (2, 101, 1), (3, 104, 4), (4, 103, 3)]);
        assert!(d.detect(1, 102, &waiters), "first cycle");
        assert!(d.detect(3, 104, &waiters), "second cycle");
    }

    // ── 压力测试 ──

    #[test]
    fn stress_test_read_heavy_contention() {
        let lock = Arc::new(SixLock::new());
        let mut handles = vec![];

        for _ in 0..16 {
            let l = lock.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..500 {
                    loop {
                        if l.six_trylock_read() {
                            std::hint::spin_loop();
                            l.six_unlock_read();
                            break;
                        }
                        std::hint::spin_loop();
                    }
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(lock.reader_count(), 0);
        assert!(lock.six_trylock_intent());
        assert!(lock.six_trylock_write());
        lock.six_unlock_write();
        lock.six_unlock_intent();
    }

    #[test]
    fn stress_test_write_heavy_contention() {
        let lock = Arc::new(SixLock::new());
        let mut handles = vec![];

        for _ in 0..8 {
            let l = lock.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..50 {
                    while !l.six_trylock_intent() {
                        std::hint::spin_loop();
                    }
                    loop {
                        if l.six_trylock_write() {
                            std::hint::spin_loop();
                            l.six_unlock_write();
                            l.six_unlock_intent();
                            break;
                        }
                        std::hint::spin_loop();
                    }
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(lock.reader_count(), 0);
    }

    #[test]
    fn stress_test_mixed_read_write_intent() {
        let lock = Arc::new(SixLock::new());
        let mut handles = vec![];

        // 4 个写线程
        for _ in 0..4 {
            let l = lock.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..30 {
                    while !l.six_trylock_intent() {
                        std::hint::spin_loop();
                    }
                    loop {
                        if l.six_trylock_write() {
                            std::hint::spin_loop();
                            l.six_unlock_write();
                            l.six_unlock_intent();
                            break;
                        }
                        std::hint::spin_loop();
                    }
                }
            }));
        }

        // 8 个读线程
        for _ in 0..8 {
            let l = lock.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..100 {
                    loop {
                        if l.six_trylock_read() {
                            std::hint::spin_loop();
                            l.six_unlock_read();
                            break;
                        }
                        std::hint::spin_loop();
                    }
                }
            }));
        }

        // 2 个 intent 线程
        for _ in 0..2 {
            let l = lock.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..30 {
                    loop {
                        if l.six_trylock_intent() {
                            if l.six_trylock_write() {
                                std::hint::spin_loop();
                                l.six_unlock_write();
                            }
                            l.six_unlock_intent();
                            break;
                        }
                        std::hint::spin_loop();
                    }
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(lock.reader_count(), 0);
    }

    #[test]
    fn stress_test_percpu_heavy_load() {
        let lock = Arc::new(SixLock::with_percpu(16));
        let mut handles = vec![];

        for _ in 0..16 {
            let l = lock.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..200 {
                    loop {
                        if l.six_trylock_read() {
                            std::hint::spin_loop();
                            l.six_unlock_read();
                            break;
                        }
                        std::hint::spin_loop();
                    }
                }
            }));
        }

        // 2 个写线程穿插写入
        for _ in 0..2 {
            let l = lock.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..20 {
                    while !l.six_trylock_intent() {
                        std::hint::spin_loop();
                    }
                    loop {
                        if l.six_trylock_write() {
                            std::hint::spin_loop();
                            l.six_unlock_write();
                            l.six_unlock_intent();
                            break;
                        }
                        std::hint::spin_loop();
                    }
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(lock.reader_count(), 0);
    }

    #[test]
    fn stress_test_detector_integration() {
        use crate::lock::deadlock::DeadlockDetector;

        let mut d = DeadlockDetector::new();
        // Phase 1: T2→L1→T1 → no deadlock
        let waiters_phase1 = make_waiters(&[(2, 100, 1)]);
        assert!(!d.detect(2, 100, &waiters_phase1), "no cycle yet");
        // Phase 2: T2→L1→T1, T1→L2→T2 → AB-BA deadlock
        let waiters_phase2 = make_waiters(&[(2, 100, 1), (1, 200, 2)]);
        assert!(d.detect(2, 100, &waiters_phase2), "AB-BA deadlock detected");
    }

    // ══════════════════════════════════════════════════════════════════
    // Phase C1: 自旋/等待/通知 测试
    // ══════════════════════════════════════════════════════════════════

    /// S1: spin_read 在写锁释放后成功获取读锁（同线程，spin 适用于微秒级等待）
    #[test]
    fn test_spin_read_succeeds() {
        let lock = SixLock::new();
        assert!(lock.six_trylock_intent());
        assert!(lock.six_trylock_write());
        lock.six_unlock_write();
        lock.six_unlock_intent();
        assert!(lock.spin_lock_read_internal());
        lock.six_unlock_read();
    }

    /// S2: spin_write 在持有读锁时失败（six_trylock_write 不自排除），需调用方先排除读者
    #[test]
    fn test_spin_write_fails_if_readers() {
        let lock = SixLock::new();
        // 同线程持有读锁 → spin_lock_write 不自排除读者，应失败
        assert!(lock.six_trylock_read());
        assert!(lock.six_trylock_intent());
        assert!(!lock.spin_lock_write_internal()); // 持有读锁时 write 自旋失败
        lock.six_unlock_intent();
        lock.six_unlock_read();

        // 调用方排除自身读者后 → 成功
        assert!(lock.six_trylock_read());
        assert!(lock.six_trylock_intent());
        let readers = lock.reader_count();
        assert_eq!(readers, 1);
        lock.six_lock_readers_add(-(readers as i32));
        assert!(lock.spin_lock_write_internal());
        lock.six_lock_readers_add(readers as i32);
        lock.six_unlock_write();
        lock.six_unlock_intent();
        lock.six_unlock_read();

        // 无读者 → 成功
        assert!(lock.six_trylock_intent());
        assert!(lock.spin_lock_write_internal());
        lock.six_unlock_write();
        lock.six_unlock_intent();
    }

    /// S3: 自旋在 SPIN_COUNT 次后超时返回 false（锁被其他线程持续持有）
    #[test]
    fn test_spin_timeout() {
        let lock = Arc::new(SixLock::new());
        assert!(lock.six_trylock_intent());
        assert!(lock.six_trylock_write());
        assert!(!lock.spin_lock_read_internal());
        lock.six_unlock_write();
        lock.six_unlock_intent();
    }

    /// S4: nospin bit 置位后自旋立即返回 false
    #[test]
    fn test_nospin_skips_spin() {
        let lock = SixLock::new();
        assert!(lock.six_trylock_intent());
        assert!(lock.six_trylock_write());
        lock.set_nospin();
        assert!(lock.is_nospin());
        assert!(!lock.spin_lock_read_internal());
        assert!(!lock.spin_lock_intent_internal());
        assert!(!lock.spin_lock_write_internal());
        lock.clear_nospin();
        assert!(!lock.is_nospin());
        lock.six_unlock_write();
        lock.six_unlock_intent();
    }

    /// 对应本地 six.c:783-789：intent/write unlock 把 NOSPIN
    /// 与当前 held bit 一起从 state 中减去。
    #[test]
    fn test_non_read_unlock_clears_nospin() {
        let intent = SixLock::new();
        assert!(intent.six_trylock_intent());
        intent.set_nospin();
        intent.six_unlock_intent();
        assert!(!intent.is_nospin());

        let write = SixLock::new();
        assert!(write.six_trylock_intent());
        assert!(write.six_trylock_write());
        write.set_nospin();
        write.six_unlock_write();
        assert!(!write.is_nospin());
        write.six_unlock_intent();
    }

    /// S5: lock_read 阻塞直到写锁释放后成功获取
    #[test]
    fn test_lock_read_blocks_and_succeeds() {
        let lock = Arc::new(SixLock::new());
        let l = lock.clone();
        let h = thread::spawn(move || {
            assert!(l.six_trylock_intent());
            assert!(l.six_trylock_write());
            thread::sleep(std::time::Duration::from_millis(10));
            l.six_unlock_write();
            l.six_unlock_intent();
        });
        thread::sleep(std::time::Duration::from_millis(5));
        // lock_read 会在写锁释放后成功获取（阻塞等待）
        assert!(lock.six_lock_read());
        lock.six_unlock_read();
        h.join().unwrap();
    }

    /// S6: lock_write 独占（同线程读锁不能和写锁共存）
    #[test]
    fn test_lock_write_exclusive() {
        let lock = SixLock::new();
        assert!(lock.six_lock_intent());
        assert!(lock.six_lock_write());
        assert!(!lock.six_trylock_read());
        lock.six_unlock_write();
        lock.six_unlock_intent();
    }

    /// S7: upgrade_read_to_intent 同线程直接升级
    #[test]
    fn test_upgrade_read_to_intent_same_thread() {
        let lock = SixLock::new();
        assert!(lock.six_trylock_read());
        assert!(lock.six_lock_tryupgrade());
        assert!(lock.is_intent_locked_by_current());
        lock.six_unlock_intent();
    }

    /// S8: intent 持有时直接尝试写锁
    #[test]
    fn test_intent_try_lock_write_same_thread() {
        let lock = SixLock::new();
        assert!(lock.six_trylock_intent());
        assert!(lock.six_trylock_write());
        assert!(lock.is_write_locked());
        lock.six_unlock_write();
        assert!(lock.is_intent_locked());
        lock.six_unlock_intent();
    }

    /// S9: push_waiter_test 直接入队 waiter 到 WaitFifo（无 trylock 重试）
    #[test]
    fn test_waiter_fifo_integration() {
        let lock = SixLock::new();
        assert!(lock.six_trylock_intent());
        assert!(lock.six_trylock_write());
        assert!(lock.push_waiter_test(&WaiterBox {
            trans_id: 0,
            lock_type: SixLockType::Read,
            seq: 0,
            thread: Some(thread::current()),
            lock_acquired: false,
            lock_acquired_flag: None,
            percpu_slot: 0,
        }));
        assert_eq!(
            lock.wait_fifo.len(),
            1,
            "push_waiter_test should add a waiter"
        );
        // 通过 remove_by_thread 验证 waiter 元数据
        let removed = lock.wait_fifo.remove_by_thread(thread::current().id());
        assert!(removed.is_some(), "waiter should be removable");
        assert_eq!(removed.unwrap().lock_type, SixLockType::Read);
        lock.six_unlock_write();
        lock.six_unlock_intent();
    }

    /// S10: 多重 push_waiter_test 累积多个 waiter
    #[test]
    fn test_waiter_fifo_multiple_pushes() {
        let lock = SixLock::new();
        assert!(lock.six_trylock_intent());
        assert!(lock.six_trylock_write());
        let waiter = WaiterBox {
            trans_id: 0,
            lock_type: SixLockType::Read,
            seq: 0,
            thread: Some(thread::current()),
            lock_acquired: false,
            lock_acquired_flag: None,
            percpu_slot: 0,
        };
        assert!(lock.push_waiter_test(&waiter));
        assert!(lock.push_waiter_test(&waiter));
        let len = lock.wait_fifo.len();
        assert!(len >= 2, "multiple pushes should add waiters (got {})", len);
        lock.six_unlock_write();
        lock.six_unlock_intent();
    }

    /// S11: wakeup_lock_type 在有等待者时不 panic
    #[test]
    fn test_wakeup_lock_type_no_panic() {
        let lock = SixLock::new();
        lock.wakeup_lock_type(lock.read_state(), SixLockType::Read);
        lock.wakeup_lock_type(lock.read_state(), SixLockType::Write);
        assert!(lock.six_trylock_intent());
        assert!(lock.six_trylock_write());
        // 用 push_waiter_test 添加 waiter 后再 wakeup
        assert!(lock.push_waiter_test(&WaiterBox {
            trans_id: 0,
            lock_type: SixLockType::Read,
            seq: 0,
            thread: Some(thread::current()),
            lock_acquired: false,
            lock_acquired_flag: None,
            percpu_slot: 0,
        }));
        let state = lock.read_state();
        lock.wakeup_lock_type(state, SixLockType::Read); // should not panic
        lock.six_unlock_write();
        lock.six_unlock_intent();
        assert!(lock.six_lock_read());
        lock.six_unlock_read();
    }

    /// S12: 同线程 lock_write 重入
    #[test]
    fn test_lock_write_reentrant() {
        let lock = SixLock::new();
        assert!(lock.six_lock_intent());
        assert!(lock.six_lock_write());
        assert!(lock.six_lock_write());
        assert!(lock.is_write_locked());
        lock.six_unlock_write();
        assert!(lock.is_write_locked());
        lock.six_unlock_write();
        assert!(!lock.is_write_locked());
        lock.six_unlock_intent();
    }

    /// S13: lock_intent 重入
    #[test]
    fn test_lock_intent_reentrant() {
        let lock = SixLock::new();
        assert!(lock.six_lock_intent());
        assert!(lock.six_lock_intent());
        lock.six_unlock_intent();
        assert!(lock.is_intent_locked());
        lock.six_unlock_intent();
        assert!(!lock.is_intent_locked());
    }

    /// S14: 写锁持有期间 waiting bit 被设置，释放后 lock_read 可获取
    #[test]
    fn test_waiting_bits_after_lock_release() {
        let lock = Arc::new(SixLock::new());
        let l = lock.clone();
        // 另一个线程持写锁
        let h = thread::spawn(move || {
            assert!(l.six_trylock_intent());
            assert!(l.six_trylock_write());
            // 等待主线程 lock_read 阻塞，此时 waiting bit 应已设置
            thread::sleep(std::time::Duration::from_millis(10));
            let state = l.read_state();
            assert!(
                (state & WAITING_READ_BIT) != 0,
                "waiting_read bit should be set during lock_read contention"
            );
            l.six_unlock_write();
            l.six_unlock_intent();
        });
        thread::sleep(std::time::Duration::from_millis(5));
        // lock_read 阻塞直到写锁释放（内部自动 push waiter）
        assert!(lock.six_lock_read());
        assert_eq!(
            lock.wait_fifo.len(),
            0,
            "fifo should be empty after self-removal"
        );
        lock.six_unlock_read();
        h.join().unwrap();
    }

    /// S14b: waiting_write bit 本身不能阻断读 fast path
    ///
    /// bcachefs 的 read fast path 只和持有中的 write lock 冲突；
    /// waiting bit 只是写者慢路径的状态，不应单独让新读者失败。
    #[test]
    fn test_try_lock_read_ignores_waiting_write_bit() {
        let lock = SixLock::new();
        lock.state.fetch_or(WAITING_WRITE_BIT, Ordering::Release);

        assert!(lock.six_trylock_read());
        lock.six_unlock_read();
    }

    /// S15: lock_write 阻塞直到读锁释放后成功获取
    #[test]
    fn test_lock_write_blocks_and_succeeds() {
        let lock = Arc::new(SixLock::new());
        let l = lock.clone();
        // 读线程持读锁 50ms
        let h = thread::spawn(move || {
            assert!(l.six_trylock_read());
            thread::sleep(std::time::Duration::from_millis(50));
            l.six_unlock_read();
        });
        thread::sleep(std::time::Duration::from_millis(5));
        // lock_write 应该阻塞直到读者释放
        assert!(lock.six_lock_intent());
        assert!(lock.six_lock_write());
        lock.six_unlock_write();
        lock.six_unlock_intent();
        h.join().unwrap();
    }

    /// S16: lock_intent 阻塞直到 intent 释放后成功获取
    #[test]
    fn test_lock_intent_blocks_and_succeeds() {
        let lock = Arc::new(SixLock::new());
        let l = lock.clone();
        let h = thread::spawn(move || {
            assert!(l.six_trylock_intent());
            thread::sleep(std::time::Duration::from_millis(50));
            l.six_unlock_intent();
        });
        thread::sleep(std::time::Duration::from_millis(5));
        assert!(lock.six_lock_intent());
        lock.six_unlock_intent();
        h.join().unwrap();
    }

    /// S17: wakeup_lock_type 正确 unpark 等待的读线程
    #[test]
    fn test_wakeup_lock_type_wakes_reader() {
        let lock = Arc::new(SixLock::new());
        let l = lock.clone();
        // 写线程持锁后释放，验证读线程被唤醒
        let h = thread::spawn(move || {
            assert!(l.six_trylock_intent());
            assert!(l.six_trylock_write());
            thread::sleep(std::time::Duration::from_millis(10));
            l.six_unlock_write();
            l.six_unlock_intent();
        });
        thread::sleep(std::time::Duration::from_millis(5));
        assert!(lock.six_lock_read());
        lock.six_unlock_read();
        h.join().unwrap();
    }

    /// S18: wakeup_lock_type 正确 unpark 等待的写线程
    #[test]
    fn test_wakeup_lock_type_wakes_writer() {
        let lock = Arc::new(SixLock::new());
        let l = lock.clone();
        // 读者持锁后释放，验证写线程被唤醒
        let h = thread::spawn(move || {
            assert!(l.six_trylock_read());
            thread::sleep(std::time::Duration::from_millis(10));
            l.six_unlock_read();
        });
        thread::sleep(std::time::Duration::from_millis(5));
        assert!(lock.six_lock_intent());
        assert!(lock.six_lock_write());
        lock.six_unlock_write();
        lock.six_unlock_intent();
        h.join().unwrap();
    }

    /// D1: wakeup_lock_type 链式重入死锁检测
    ///
    /// 8 个读线程 + 2 个写线程同时用 blocking lock 路径争用同一把锁。
    /// 验证 wait_lock Mutex 在 wakeup_lock_type→unpark→acquire→unlock→wakeup_lock_type
    /// 链式调用中不会死锁。
    ///
    /// 关键路径：
    /// 1. 写线程 unlock_write → wakeup_lock_type(Read) → wait_lock.lock → snapshot → unlock
    /// 2. 读线程被 unpark → lock_read 成功 → unlock_read → wakeup_lock_type(Write) → wait_lock.lock
    /// 3. 若 wait_lock 在步骤 1 未释放，步骤 2 死锁——但我们先 unlock 再 unpark，所以安全
    #[test]
    fn stress_deadlock_read_write_chain() {
        let lock = Arc::new(SixLock::new());
        let mut handles = vec![];
        let ready = Arc::new(std::sync::Barrier::new(11)); // 10 workers + main

        // 8 个读线程：lock_read → unlock_read 循环
        for _ in 0..8 {
            let l = lock.clone();
            let b = ready.clone();
            handles.push(thread::spawn(move || {
                b.wait(); // 同步启动
                for _ in 0..100 {
                    assert!(l.six_lock_read(), "reader should acquire lock");
                    std::hint::spin_loop(); // 模拟短工作
                    l.six_unlock_read();
                }
            }));
        }

        // 2 个写线程：lock_write → unlock_write 循环
        for _ in 0..2 {
            let l = lock.clone();
            let b = ready.clone();
            handles.push(thread::spawn(move || {
                b.wait();
                for _ in 0..25 {
                    assert!(l.six_lock_intent(), "writer should acquire intent lock");
                    assert!(l.six_lock_write(), "writer should acquire lock");
                    std::hint::spin_loop();
                    l.six_unlock_write();
                    l.six_unlock_intent();
                }
            }));
        }

        ready.wait(); // 所有线程同时开始
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        for h in handles {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                panic!("DEADLOCK DETECTED: thread did not finish within 10s");
            }
            h.join().unwrap();
        }
    }

    /// D2: 多写线程阻塞唤醒链死锁检测
    ///
    /// 4 个写线程同时争用写锁。锁只有一个，其他三个必须通过 sleep 路径
    /// park 等待。释放时 wakeup_lock_type 唤醒一个，该线程 unlock 后再次唤醒下一个。
    ///
    /// 验证 write→write 阻塞唤醒链不因 wait_lock 死锁。
    #[test]
    fn stress_deadlock_write_chain() {
        let lock = Arc::new(SixLock::new());
        let mut handles = vec![];
        let ready = Arc::new(std::sync::Barrier::new(5)); // 4 workers + main

        for _ in 0..4 {
            let l = lock.clone();
            let b = ready.clone();
            handles.push(thread::spawn(move || {
                b.wait();
                for _ in 0..50 {
                    assert!(l.six_lock_intent(), "writer should acquire intent lock");
                    assert!(l.six_lock_write(), "writer should acquire lock");
                    std::hint::spin_loop();
                    l.six_unlock_write();
                    l.six_unlock_intent();
                }
            }));
        }

        ready.wait();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        for h in handles {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                panic!("DEADLOCK DETECTED: write chain did not finish within 10s");
            }
            h.join().unwrap();
        }
    }

    /// D3: wakeup_lock_type snapshot/remove 并发压力测试
    ///
    /// 1 个写线程持锁，8 个读线程全部在 fifo 中等待。
    /// 写线程释放时 wakeup_lock_type(Read) 对所有读线程 unpark。
    /// 8 个读线程同时 wake → try_lock_read → remove_self_from_fifo。
    /// 验证 wait_lock 保护下的并发 remove_by_thread 不会死锁或 panic。
    #[test]
    fn stress_deadlock_burst_wake() {
        let lock = Arc::new(SixLock::new());
        let mut handles = vec![];
        let ready = Arc::new(std::sync::Barrier::new(10)); // 8 readers + 1 writer + main

        // 8 个读线程
        for _ in 0..8 {
            let l = lock.clone();
            let b = ready.clone();
            handles.push(thread::spawn(move || {
                b.wait();
                // lock_read 会阻塞直到写锁释放
                assert!(
                    l.six_lock_read(),
                    "reader should acquire lock after burst wake"
                );
                // 微延迟避免所有读者同时 release
                thread::sleep(std::time::Duration::from_micros(100));
                l.six_unlock_read();
            }));
        }

        // 写线程：持锁，释放（触发 burst wake）
        //
        // 写线程先用 lock_write 确保获取锁（与读者 Barrier 同时启动，读者可能抢先）。
        // 获取后释放，触发所有在读等待者的 burst wake。
        let l = lock.clone();
        let b = ready.clone();
        let writer = thread::spawn(move || {
            b.wait();
            for _ in 0..20 {
                // 用 lock_write 阻塞获取（ready Barrier 后读者可能已抢先持锁）
                assert!(l.six_lock_intent(), "writer should acquire intent lock");
                assert!(l.six_lock_write(), "writer should acquire lock");
                // 等读者全进 fifo
                thread::sleep(std::time::Duration::from_millis(5));
                l.six_unlock_write(); // ← burst wake: 所有在读等待者被 unpark
                l.six_unlock_intent();
                thread::sleep(std::time::Duration::from_millis(10));
            }
        });

        ready.wait();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        for h in handles {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                panic!("DEADLOCK DETECTED: burst wake did not finish within 10s");
            }
            h.join().unwrap();
        }
        writer.join().unwrap();
    }

    /// S19: 多个读者同时等待写锁释放后全部获取读锁
    #[test]
    fn test_multiple_readers_block_then_all_succeed() {
        let lock = Arc::new(SixLock::new());
        let mut handles = vec![];
        // 持写锁
        assert!(lock.six_trylock_intent());
        assert!(lock.six_trylock_write());
        // 5 个读线程各调用 lock_read（都会阻塞）
        for _ in 0..5 {
            let l = lock.clone();
            handles.push(thread::spawn(move || {
                assert!(l.six_lock_read());
                l.six_unlock_read();
            }));
        }
        thread::sleep(std::time::Duration::from_millis(10));
        // 释放写锁 → 所有读线程应被唤醒
        lock.six_unlock_write();
        lock.six_unlock_intent();
        for h in handles {
            h.join().unwrap();
        }
    }
}
