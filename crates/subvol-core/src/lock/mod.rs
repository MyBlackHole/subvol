//! 锁模块 — SixLock 3 状态读写锁 + WaitFifo 等待队列 + DeadlockDetector 死锁检测
//!
//! 对应 bcachefs 的 six.{h,c} + fs/util/six.h 设计。

pub mod deadlock;
pub mod six;
pub mod wait_fifo;

pub use deadlock::DeadlockDetector;
pub use six::{
    SixLock, SixLockCount, SixLockResult, SixLockShouldSleepFn, SixLockType, SixLockWaiter,
};
pub use wait_fifo::{WaitFifo, WaiterBox};
