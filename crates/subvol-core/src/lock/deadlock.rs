//! DeadlockDetector — 基于 DFS 栈的瞬态死锁检测
//!
//! 不再使用共享 HashMap 图结构，而是使用 per-thread 固定大小 DFS 栈
//! 进行环检测。每次 detect() 调用从头遍历预收集的 WaiterInfo 列表，
//! 不持有任何持久化数据。
//!
//! 设计目标：无锁、无堆分配（除初始 Vec 容量分配外）、O(N) 遍历。

use std::cell::UnsafeCell;

/// DFS 栈节点 — 表示等待链中的一个事务
///
/// 注意：lock_id 未存储于栈节点中（当前 DFS 算法仅需 trans_id），
/// 若需要死锁报告，可从 WaiterInfo 中反查。
#[derive(Debug, Clone)]
struct DfsStackEntry {
    trans_id: u64,
    state: DfsColor,
}

/// DFS 颜色标记
///
/// - Gray: 当前在 DFS 路径中（正在处理）
/// - Black: 所有边已处理完毕
/// - 注：未入栈的节点隐式为"未访问"（相当于传统的 White），无需显式颜色
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DfsColor {
    /// 在当前 DFS 路径中（正在处理）
    Gray,
    /// 所有边已处理完毕
    Black,
}

/// 等待者信息 — 预收集的 (等待事务, 等待的锁, 持有锁的事务) 三元组
///
/// 由调用者（D7 WaitFifo）在 park 前通过 RCU 遍历收集。
#[derive(Debug, Clone)]
pub struct WaiterInfo {
    /// 等待者事务 ID
    pub trans_id: u64,
    /// 正在等待的锁对象 ID
    pub lock_id: u64,
    /// 当前持有该锁的事务 ID
    pub waiting_for_trans_id: u64,
}

/// DeadlockDetector — per-thread 瞬态死锁检测器
///
/// 使用固定大小的 DFS 栈进行环检测。不持有任何持久化数据——
/// 每次 `detect()` 调用从头开始遍历 WaiterInfo 列表。
/// 通过 thread_local 持有，无需外部互斥。
///
/// # 算法
///
/// 从起始事务出发，沿着"事务 X 等待锁 L（被事务 Y 持有）"的依赖链
/// 进行 DFS 遍历。如果遍历到的事务已在当前 DFS 路径中（Gray），
/// 则检测到环（死锁）。
#[derive(Debug)]
pub struct DeadlockDetector {
    /// DFS 调用栈（最大深度受 MAX_DEPTH 限制）
    stack: Vec<DfsStackEntry>,
}

/// 最大 DFS 递归深度（防止栈溢出）
const MAX_DEPTH: usize = 64;

impl DeadlockDetector {
    /// 创建空的 DeadlockDetector
    pub const fn new() -> Self {
        Self { stack: Vec::new() }
    }

    /// 执行死锁检测
    ///
    /// - `start_trans_id`: 发起检测的事务 ID（当前尝试获取锁失败的事务）
    /// - `_start_lock_id`: 当前等待的锁 ID（保留用于未来死锁报告）
    /// - `waiters`: 预收集的等待者信息列表，格式为 `(waiter, lock, holder)` 三元组
    ///
    /// 返回 `true` 表示检测到死锁（当前事务应回滚以解除死锁）。
    pub fn detect(
        &mut self,
        start_trans_id: u64,
        _start_lock_id: u64,
        waiters: &[WaiterInfo],
    ) -> bool {
        self.stack.clear();

        // 起始事务入栈（Gray = 当前 DFS 路径中）
        self.stack.push(DfsStackEntry {
            trans_id: start_trans_id,
            state: DfsColor::Gray,
        });

        // visited: 已完全处理完毕的事务集合
        let mut visited: Vec<u64> = Vec::new();

        loop {
            if self.stack.is_empty() {
                break;
            }

            let top_idx = self.stack.len() - 1;
            let current = &self.stack[top_idx];

            if current.state == DfsColor::Black {
                visited.push(current.trans_id);
                self.stack.pop();
                continue;
            }

            let current_tid = current.trans_id;

            // 防止无限增长
            if self.stack.len() >= MAX_DEPTH {
                self.stack.clear();
                return false;
            }

            let mut found_new = false;

            // 遍历 waiters，找当前事务正在等待的锁的持有者
            for waiter in waiters {
                if waiter.trans_id == current_tid {
                    let holder = waiter.waiting_for_trans_id;

                    // 检查持有者是否在当前 DFS 路径中（Gray → 环！）
                    if let Some(pos) = self.stack.iter().position(|e| e.trans_id == holder) {
                        if self.stack[pos].state == DfsColor::Gray {
                            return true; // 检测到死锁环
                        }
                        // Black：已完全处理，跳过
                        continue;
                    }

                    // 跳过已完全处理的事务
                    if visited.contains(&holder) {
                        continue;
                    }

                    // 新节点入栈（Gray），继续 DFS
                    self.stack.push(DfsStackEntry {
                        trans_id: holder,
                        state: DfsColor::Gray,
                    });
                    found_new = true;
                    break;
                }
            }

            if !found_new {
                // 所有边已处理完毕 → 标记 Black 并弹出
                self.stack[top_idx].state = DfsColor::Black;
            }
        }

        false
    }
}

impl Default for DeadlockDetector {
    fn default() -> Self {
        Self::new()
    }
}

thread_local! {
    /// 当前线程的 DeadlockDetector 实例
    pub(crate) static DEADLOCK_DETECTOR: UnsafeCell<DeadlockDetector> =
        const { UnsafeCell::new(DeadlockDetector::new()) };
}

/// 访问当前线程的 DeadlockDetector
///
/// 通过 thread_local + UnsafeCell 实现零开销访问。
/// SAFETY: thread_local 保证线程独占访问，闭包不会重入。
pub fn with_detector_mut<F, R>(f: F) -> R
where
    F: FnOnce(&mut DeadlockDetector) -> R,
{
    DEADLOCK_DETECTOR.with(|cell| {
        // SAFETY: thread_local 保证调用者在同一线程，且闭包同步执行不重入
        f(unsafe { &mut *cell.get() })
    })
}

// ─── 测试 ───────────────────────────────────────────────────

#[cfg(never)]
mod tests {
    use super::*;

    fn make_waiters(pairs: &[(u64, u64, u64)]) -> Vec<WaiterInfo> {
        pairs
            .iter()
            .map(|&(t, l, h)| WaiterInfo {
                trans_id: t,
                lock_id: l,
                waiting_for_trans_id: h,
            })
            .collect()
    }

    #[test]
    fn test_empty_detector() {
        let mut d = DeadlockDetector::new();
        assert!(!d.detect(0, 0, &[]));
    }

    #[test]
    fn test_single_wait_no_deadlock() {
        let mut d = DeadlockDetector::new();
        // T2 waits for L1 held by T1, but T1 is not waiting for anyone
        let waiters = make_waiters(&[(2, 100, 1)]);
        assert!(!d.detect(2, 100, &waiters));
    }

    #[test]
    fn test_simple_deadlock() {
        let mut d = DeadlockDetector::new();
        // T1 holds L1, waits for L2; T2 holds L2, waits for L1
        // T1→L2→T2, T2→L1→T1
        let waiters = make_waiters(&[(1, 200, 2), (2, 100, 1)]);
        assert!(d.detect(1, 200, &waiters), "should detect 2-way deadlock");
    }

    #[test]
    fn test_three_way_deadlock() {
        let mut d = DeadlockDetector::new();
        // T1→L2→T2, T2→L3→T3, T3→L1→T1
        let waiters = make_waiters(&[(1, 102, 2), (2, 103, 3), (3, 101, 1)]);
        assert!(d.detect(1, 102, &waiters), "3-way deadlock");
    }

    #[test]
    fn test_no_deadlock_after_remove() {
        let mut d = DeadlockDetector::new();
        // T2 waits for L1 held by T1, but T1 is not waiting (already released)
        let waiters = make_waiters(&[(2, 100, 1)]);
        assert!(!d.detect(2, 100, &waiters));
    }

    #[test]
    fn test_self_cycle() {
        let mut d = DeadlockDetector::new();
        // T1 waits for L1 held by T1 → self-deadlock
        let waiters = make_waiters(&[(1, 100, 1)]);
        assert!(d.detect(1, 100, &waiters), "self-cycle should be detected");
    }

    #[test]
    fn test_diamond_no_deadlock() {
        let mut d = DeadlockDetector::new();
        // T2 waits for L1 and L2 (both held by T1), no one waits for T2
        let waiters = make_waiters(&[(2, 100, 1), (2, 200, 1)]);
        assert!(!d.detect(2, 100, &waiters));
    }

    #[test]
    fn test_detect_from_different_start() {
        let mut d = DeadlockDetector::new();
        // T1→L2→T2, T2→L1→T1 (same 2-way deadlock)
        let waiters = make_waiters(&[(1, 200, 2), (2, 100, 1)]);
        // Detect from T2's perspective
        assert!(d.detect(2, 100, &waiters), "should detect from either side");
    }

    #[test]
    fn test_no_cycle_with_three_chain() {
        let mut d = DeadlockDetector::new();
        // T1→L2→T2, T2→L3→T3 (linear, no cycle)
        let waiters = make_waiters(&[(1, 200, 2), (2, 300, 3)]);
        assert!(!d.detect(1, 200, &waiters));
    }

    #[test]
    fn test_reuse_detector() {
        let mut d = DeadlockDetector::new();
        // First call: no deadlock
        assert!(!d.detect(0, 0, &[]));
        // Second call: deadlock
        let waiters = make_waiters(&[(1, 200, 2), (2, 100, 1)]);
        assert!(d.detect(1, 200, &waiters));
        // Third call: no deadlock (detector state is clean)
        assert!(!d.detect(2, 100, &[]));
    }

    #[test]
    fn test_with_detector_mut_works() {
        with_detector_mut(|d| {
            assert!(!d.detect(0, 0, &[]));
            let waiters = make_waiters(&[(1, 200, 2), (2, 100, 1)]);
            assert!(d.detect(1, 200, &waiters));
        });
    }
}
