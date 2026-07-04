//! WaitFifo — RCU 定长槽位等待队列
//!
//! 对应 bcachefs fs/util/six.h 中的 six_lock_wait_fifo。
//! 定长槽位数组，O(1) 插入/删除，RCU 无锁遍历。
//! 非阻塞数据结构——所有操作通过 RcuBox·compare_and_update 完成。
//!
//! RCU 设计要点：
//! - 每个槽位是 `RcuBox<Option<Box<WaiterBox>>>`
//! - 读取在 `rscs`（read-side critical section）内进行
//! - 写入通过 `compare_and_update` 原子替换
//! - 旧值的回收在 RCU grace period 后自动进行
//!
//! 线程安全：RcuBox 是 Send+Sync，WaitFifo 因此也是 Send+Sync。

use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::Arc;
use std::thread;

use urcu::boxed::RcuBox;
use urcu::Rcu;

// RcuThread 不在此处导入 — 由 with_rcu() 内部处理线程注册

use super::six::SixLockType;

/// 等待者元数据
///
/// 存储在 WaitFifo 槽位中，每个槽位是 `RcuBox<Option<Box<WaiterBox>>>`。
#[derive(Debug)]
#[repr(C)]
pub struct WaiterBox {
    pub trans_id: u64,                               // 事务 ID（死锁检测用）
    pub lock_type: SixLockType,                      // 需要的锁类型
    pub seq: u64,                                    // 入队序列号（竞争时序用）
    pub thread: Option<thread::Thread>,              // 等待线程句柄（park/unpark 用）
    pub lock_acquired: bool, // handoff 标记：解锁方在 unpark 前设置此标记，告知等待者锁已授予它
    pub lock_acquired_flag: Option<Arc<AtomicBool>>, // NEW: 带外 handoff 信号，waker 通过此信号通知 waiter 锁已获取
    pub percpu_slot: u32, // NEW: percpu 读线程的槽号，waker 用此槽号替 reader 递增计数
}

/// WaitFifo —— RCU 友好的定长槽位等待队列
///
/// 设计要点：
/// - 固定槽位数组，每个槽位是 `RcuBox<Option<Box<WaiterBox>>>`
/// - 插入：`compare_and_update` 原子替换 `None → Some(Box::new(waiter))`
/// - 删除：`compare_and_update` 原子替换 `Some(waiter) → None`
/// - 遍历：RCU 保护下线性扫描，只读非空槽
/// - 扩容：分配新 fifo → 迁移 → 原子替换
///
/// 线程安全：RcuBox 内部使用原子指针 + RCU 回收，无需外部互斥锁。
pub struct WaitFifo {
    slots: Box<[RcuBox<Option<Box<WaiterBox>>>]>,
    next_free: AtomicU16,
    size: u16,
}

impl WaitFifo {
    /// 创建指定容量的 WaitFifo
    ///
    /// 需要一个 `&Rcu` 句柄来初始化每个 RcuBox。
    pub fn new(size: u16, rcu: &Rcu) -> Self {
        assert!(size > 0, "WaitFifo size must be > 0");
        let mut slots = Vec::with_capacity(size as usize);
        for _ in 0..size {
            slots.push(RcuBox::empty(rcu));
        }
        Self {
            slots: slots.into_boxed_slice(),
            next_free: AtomicU16::new(0),
            size,
        }
    }

    /// 插入等待者（O(1) 平均）
    ///
    /// 使用 `next_free` hint 快速找到空闲槽。
    /// 在 RCU read-side critical section 内用 `compare_and_update`
    /// 原子地将空槽从 `None` 替换为 `Some(Box::new(waiter))`。
    /// 返回插入的槽位索引，或 `None` 表示队列满。
    pub fn push(
        &self,
        trans_id: u64,
        lock_type: SixLockType,
        seq: u64,
        thread: Option<thread::Thread>,
        percpu_slot: u32,
        lock_acquired_flag: Option<Arc<AtomicBool>>,
    ) -> Option<u16> {
        super::six::with_rcu(|_rcu, t| {
            t.rscs(|rscs| {
                let start = self.next_free.load(Ordering::Relaxed);
                for offset in 0..self.size {
                    let idx = (start + offset) % self.size;
                    let slot = &self.slots[idx as usize];
                    let current = slot.read(rscs);
                    // Slot 空闲：RcuRef 从未写入（null ptr）或值已被移除（None）
                    let slot_free = current.as_ref().and_then(|v| v.as_ref()).is_none();
                    if !slot_free {
                        continue;
                    }

                    let new_val = Some(Box::new(WaiterBox {
                        trans_id,
                        lock_type,
                        seq,
                        thread: thread.clone(),
                        lock_acquired: false,
                        lock_acquired_flag: lock_acquired_flag.clone(),
                        percpu_slot,
                    }));
                    match slot.compare_and_update(current, new_val, |_, _| ControlFlow::Break(())) {
                        Ok((_, _old)) => {
                            self.next_free
                                .store((idx + 1) % self.size, Ordering::Relaxed);
                            return Some(idx);
                        }
                        Err(_) => {
                            // 被其他线程抢先，重试下一槽位
                            continue;
                        }
                    }
                }
                None // 队列满
            })
        })
    }

    /// 移除指定线程的等待者（O(n) 扫描）
    ///
    /// 在 RCU rscs 内扫描所有槽位，找到 thread 匹配的 WaiterBox。
    /// 使用 `compare_and_update` 原子清空。
    /// 返回被移除的 WaiterBox（如果有）。
    pub fn remove_by_thread(&self, thread_id: thread::ThreadId) -> Option<Box<WaiterBox>> {
        let mut to_drop: Option<RcuBox<Option<Box<WaiterBox>>>> = None;

        super::six::with_rcu(|_rcu, t| {
            t.rscs(|rscs| {
                'outer: for slot in self.slots.iter() {
                    let current = slot.read(rscs);
                    if let Some(Some(ref waiter)) = current.as_ref() {
                        if waiter
                            .thread
                            .as_ref()
                            .map(|t| t.id() == thread_id)
                            .unwrap_or(false)
                        {
                            match slot
                                .compare_and_update(current, None, |_, _| ControlFlow::Continue(()))
                            {
                                Ok((_, old_value)) => {
                                    to_drop = Some(old_value);
                                    break 'outer;
                                }
                                Err(_new_curr) => {
                                    // CAS 失败（被其他线程抢先），继续扫描
                                    continue;
                                }
                            }
                        }
                    }
                }
            })
        });

        // `to_drop` 在 rscs 外被 drop，避免 RcuBox::drop 在 rscs 内调用 call_rcu
        to_drop.and_then(|old| old.into_inner().flatten())
    }

    /// 移除指定事务的等待者（O(n) 均摊）
    ///
    /// 只移除 trans_id 匹配的槽位。
    /// 返回被移除的 WaiterBox（如果有）。
    pub fn remove(&self, trans_id: u64) -> Option<Box<WaiterBox>> {
        let mut to_drop: Option<RcuBox<Option<Box<WaiterBox>>>> = None;

        super::six::with_rcu(|_rcu, t| {
            t.rscs(|rscs| {
                'outer: for slot in self.slots.iter() {
                    let current = slot.read(rscs);
                    if let Some(Some(ref waiter)) = current.as_ref() {
                        if waiter.trans_id == trans_id {
                            match slot
                                .compare_and_update(current, None, |_, _| ControlFlow::Continue(()))
                            {
                                Ok((_, old_value)) => {
                                    to_drop = Some(old_value);
                                    break 'outer;
                                }
                                Err(_new_curr) => {
                                    continue;
                                }
                            }
                        }
                    }
                }
            })
        });

        to_drop.and_then(|old| old.into_inner().flatten())
    }

    /// 按 index 清除指定槽位（O(1)）
    ///
    /// 对应 bcachefs six_lock_wait_fifo_remove。
    /// 由 wait_lock 保护，无并发访问，直接使用 `update(None)`。
    pub fn remove_by_index(&self, idx: usize) {
        if idx >= self.slots.len() {
            return;
        }
        let slot = &self.slots[idx];
        let _old = slot.update(None);
    }

    /// 当前队列深度（非空槽位数）
    pub fn len(&self) -> usize {
        super::six::with_rcu(|_rcu, t| {
            t.rscs(|rscs| {
                self.slots
                    .iter()
                    .filter(|s| s.read(rscs).as_ref().and_then(|v| v.as_ref()).is_some())
                    .count()
            })
        })
    }

    /// 返回所有槽位的引用（用于 RCU 遍历）
    pub(crate) fn slots(&self) -> &[RcuBox<Option<Box<WaiterBox>>>] {
        &self.slots
    }

    /// 队列是否为空
    pub fn is_empty(&self) -> bool {
        super::six::with_rcu(|_rcu, t| {
            t.rscs(|rscs| {
                self.slots
                    .iter()
                    .all(|s| s.read(rscs).as_ref().and_then(|v| v.as_ref()).is_none())
            })
        })
    }

    /// 扩容（分配新 fifo，迁移数据，原子替换）
    pub fn resize(&mut self, new_size: u16, rcu: &Rcu) {
        assert!(new_size > self.size, "new_size must be > current size");
        let old_vec = Vec::from(std::mem::take(&mut self.slots));
        let mut new_vec: Vec<RcuBox<Option<Box<WaiterBox>>>> =
            Vec::with_capacity(new_size as usize);

        // 迁移现有 slot
        for slot in old_vec {
            new_vec.push(slot);
        }
        // 补充新空槽
        for _ in self.slots.len()..new_size as usize {
            new_vec.push(RcuBox::empty(rcu));
        }

        self.slots = new_vec.into_boxed_slice();
        self.size = new_size;
    }
}

/// 使用 `update(None)` 清空所有槽位，旧值在 RCU grace period 后自动回收。
impl Drop for WaitFifo {
    fn drop(&mut self) {
        for slot in self.slots.iter() {
            let _old = slot.update(None);
            // _old 在此处 drop，调度 deferred reclamation
        }
    }
}

impl std::fmt::Debug for WaitFifo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WaitFifo")
            .field("size", &self.size)
            .field("next_free", &self.next_free.load(Ordering::Relaxed))
            .field("slots_len", &self.slots.len())
            .finish()
    }
}

unsafe impl Send for WaitFifo {}
unsafe impl Sync for WaitFifo {}

// ─── 测试 ───────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use urcu::Rcu;

    /// 验证 RcuBox 在 rscs 内 drop 不会死锁
    #[test]
    fn test_rcu_box_drop_semantics() {
        super::super::six::with_rcu(|rcu, thread| {
            let boxed: RcuBox<Option<Box<WaiterBox>>> = RcuBox::empty(rcu);

            // 先在 rscs 外推入一个值
            let waiter = Box::new(WaiterBox {
                trans_id: 1,
                lock_type: SixLockType::Read,
                seq: 100,
                thread: None,
                lock_acquired: false,
                lock_acquired_flag: None,
                percpu_slot: 0,
            });
            boxed.update(Some(waiter));

            // 在 rscs 内 update 并 drop 旧值 —— 不应死锁
            thread.rscs(|rscs| {
                let current = boxed.read(rscs);
                assert!(current.as_ref().and_then(|v| v.as_ref()).is_some());
                let old = boxed.update(None);
                // 在 rscs 内 drop 旧 RcuBox -> call_rcu 不应死锁
                drop(old);
                let current = boxed.read(rscs);
                assert!(current.as_ref().and_then(|v| v.as_ref()).is_none());
            });

            rcu.barrier();
        });
    }

    #[test]
    fn test_push_pop() {
        let rcu = Rcu::init();
        let fifo = WaitFifo::new(4, &rcu);
        assert!(fifo.is_empty());

        let idx = fifo
            .push(1, SixLockType::Read, 100, None, 0, None)
            .expect("push failed");
        assert_eq!(idx, 0);
        assert!(!fifo.is_empty());

        let removed = fifo.remove(1);
        assert!(removed.is_some());
        assert_eq!(removed.unwrap().trans_id, 1);
        assert!(fifo.is_empty());
    }

    #[test]
    fn test_push_full() {
        let rcu = Rcu::init();
        let fifo = WaitFifo::new(2, &rcu);

        assert!(fifo.push(0, SixLockType::Read, 0, None, 0, None).is_some());
        assert!(fifo.push(1, SixLockType::Read, 1, None, 0, None).is_some());
        // 第三个 push 应该失败
        assert!(fifo.push(2, SixLockType::Read, 2, None, 0, None).is_none());
    }

    #[test]
    fn test_push_scans_multiple_slots_in_one_rcu_section() {
        let rcu = Rcu::init();
        let fifo = WaitFifo::new(4, &rcu);

        assert!(fifo.push(1, SixLockType::Read, 1, None, 0, None).is_some());
        assert!(fifo.push(2, SixLockType::Read, 2, None, 0, None).is_some());
        assert!(fifo.push(3, SixLockType::Read, 3, None, 0, None).is_some());
        assert!(fifo.push(4, SixLockType::Read, 4, None, 0, None).is_some());

        // 此时 next_free 已回到 0。移除中间 slot，确保后续 push
        // 需要先越过已占用的 slot 0，再复用 slot 1。
        fifo.remove_by_index(1);
        let idx = fifo
            .push(4, SixLockType::Read, 4, None, 0, None)
            .expect("push should reuse a free slot after scanning");
        assert_eq!(idx, 1);
    }

    #[test]
    fn test_len() {
        let rcu = Rcu::init();
        let fifo = WaitFifo::new(4, &rcu);

        assert_eq!(fifo.len(), 0);
        fifo.push(1, SixLockType::Write, 1, None, 0, None);
        assert_eq!(fifo.len(), 1);
        fifo.push(2, SixLockType::Write, 2, None, 0, None);
        assert_eq!(fifo.len(), 2);
        fifo.remove(1);
        assert_eq!(fifo.len(), 1);
    }

    #[test]
    fn test_remove_nonexistent() {
        let rcu = Rcu::init();
        let fifo = WaitFifo::new(4, &rcu);

        fifo.push(42, SixLockType::Read, 1, None, 0, None);
        assert!(fifo.remove(99).is_none());
        assert!(fifo.remove(42).is_some());
    }

    #[test]
    fn test_resize() {
        let rcu = Rcu::init();
        let mut fifo = WaitFifo::new(2, &rcu);

        fifo.push(1, SixLockType::Read, 1, None, 0, None);
        fifo.resize(8, &rcu);
        assert_eq!(fifo.size, 8);
        let removed = fifo.remove(1);
        assert!(removed.is_some());
    }

    #[test]
    fn test_concurrent_push_remove() {
        use std::sync::Arc;
        use std::thread;

        let rcu = Rcu::init();
        let fifo = Arc::new(WaitFifo::new(16, &rcu));
        let mut handles = vec![];

        // 4 个线程各 push 4 个 waiter
        // 每个线程通过 with_rcu() 自动注册
        for t in 0..4 {
            let f = fifo.clone();
            handles.push(thread::spawn(move || {
                for i in 0..4 {
                    let id = (t * 4 + i) as u64;
                    let _ = f.push(id, SixLockType::Read, id, None, 0, None);
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        // 验证不 panic
        let len = fifo.len();
        println!("len: {}", len);
    }

    #[test]
    fn test_remove_by_index() {
        let rcu = Rcu::init();
        let fifo = WaitFifo::new(4, &rcu);

        fifo.push(1, SixLockType::Read, 100, None, 0, None);
        fifo.push(2, SixLockType::Write, 200, None, 0, None);
        assert_eq!(fifo.len(), 2);

        // 按 index 删除
        fifo.remove_by_index(0);
        assert_eq!(fifo.len(), 1);

        // 验证 slot 1 还在
        let removed = fifo.remove(2);
        assert!(removed.is_some());
        assert_eq!(removed.unwrap().trans_id, 2);

        // index 越界不应 panic
        fifo.remove_by_index(99);
    }
}
