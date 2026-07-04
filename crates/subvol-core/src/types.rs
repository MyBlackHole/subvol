//! subvol 公共类型定义

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;

use crate::btree::key::Bpos;

/// 扇区移位（512B = 2^9）— bcachefs 内部基本单位
pub const SECTOR_SHIFT: u8 = 9;
/// 扇区大小（字节）
pub const SECTOR_SIZE: u64 = 512;

/// 卷 ID
pub type VolumeId = u64;

/// 物理块地址
///
/// raw: 物理块编号
/// ver: 版本号（S3 后端：{raw}.{ver} 版本化 key）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BlockAddr {
    pub raw: u64,
    pub ver: u16,
}

impl BlockAddr {
    pub const fn new(raw: u64) -> Self {
        Self { raw, ver: 0 }
    }

    pub const fn with_ver(raw: u64, ver: u16) -> Self {
        Self { raw, ver }
    }
}

/// 块大小（默认 4KB，创建时固定）
pub type BlockSize = u32;

/// 校验和类型 — bcachefs 对齐: `enum bch_csum_type` (opts.h)
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum CsumType {
    None,
    Crc32c,
}

impl Default for CsumType {
    fn default() -> Self {
        Self::Crc32c
    }
}

/// 卷容量（字节）
pub type Capacity = u64;

/// 后端类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BackendType {
    S3,
    Nfs,
}

/// 健康状态
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HealthStatus {
    Healthy,
    Degraded { reason: &'static str },
    Unreachable { reason: String },
}

/// 后端存储错误
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("block not found: addr={0:?}")]
    BlockNotFound(BlockAddr),

    #[error("checksum mismatch: expected {expected:#x}, got {actual:#x}")]
    ChecksumMismatch { expected: u32, actual: u32 },

    #[error("extent is poisoned")]
    ExtentPoisoned,

    #[error("backend unreachable: {0}")]
    Unreachable(String),

    #[error("invalid block size: {0}")]
    InvalidBlockSize(u64),

    #[error("volume {0} not found")]
    VolumeNotFound(VolumeId),

    #[error("invalid data: {0}")]
    InvalidData(String),

    #[error("serialization error: {0}")]
    Serialization(#[from] bincode::Error),

    #[error("{0} not found")]
    NotFound(String),

    #[error("address space exhausted (max raw addr: {max_raw_addr})")]
    AddressSpaceExhausted { max_raw_addr: u64 },

    #[error("transaction lock conflict at bpos={0}")]
    TransactionLockConflict(Bpos),

    #[error("transaction restart limit exceeded: {0} restarts")]
    TransactionRestartLimit(u64),

    #[error("transaction error: {0}")]
    Transaction(String),

    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    #[error("metadata bucket inconsistency: {0}")]
    MetadataBucketInconsistency(String),

    /// 节点数据损坏（header magic/version/format 不兼容等）
    /// 对应 bcachefs btree_node_bad_magic / btree_node_bad_version
    #[error("corrupt btree node data: {0}")]
    CorruptData(String),

    #[error("journal error: {0}")]
    JournalError(String),

    /// btree 节点空间不足 — 对应 bcachefs -BCH_ERR_btree_insert_btree_node_full
    #[error("btree node full")]
    BtreeNodeFull,

    /// journal reclaim 路径死锁（水位线低于 Reclaim 时被 journal 阻塞）
    /// 对应 bcachefs journal_reclaim_would_deadlock
    #[error("journal reclaim would deadlock")]
    JournalReclaimWouldDeadlock,

    #[error("watermark too low: request={request:?}, current={current:?}")]
    WatermarkTooLow {
        request: Watermark,
        current: Watermark,
    },

    /// 资源已存在（如 Volume 状态转换时目标状态已被占用）
    #[error("already exists: {0}")]
    AlreadyExists(&'static str),

    /// 恢复过程错误（如 passes_failing 超出重试阈值）
    #[error("recovery error: {0}")]
    Recovery(&'static str),

    /// Recovery 请求回退到指定 pass（对应 bcachefs -BCH_ERR_restart_recovery）
    ///
    /// 当 pass 检测到状态不一致时调用 bch2_rewind_recovery() 返回此错误，
    /// 通知外层调度器重新从指定 pass 运行。
    #[error("rewind recovery to pass {0}")]
    RewindRecovery(usize),

    /// 配额超限 — 对应 bcachefs -QUOTA_EXCEEDED
    #[error("quota exceeded: {message}")]
    QuotaExceeded { message: String },
}

/// 对应 bcachefs `cl->error` 的 cmpxchg 原子错误收集。
///
/// bcachefs 中 `SET_ERROR` 宏用 `cmpxchg(&cl->error, 0, err)` 设置首个错误；
/// 多个并发 IO 完成回调中只有第一个写入生效，后续被忽略。
///
/// subvol 用 `AtomicBool` 替代 cmpxchg 的原子比较交换，
/// `UnsafeCell<Option<StorageError>>` 存储错误值（设置后不再修改）。
#[derive(Debug)]
pub(crate) struct AtomicFirstError {
    set: AtomicBool,
    error: UnsafeCell<Option<StorageError>>,
}

// Sync: AtomicFirstError 写一次（set_first 用 swap 确保仅一个 writer 进入），
// 读一次（take 在 writer 全部结束后调用）。UnsafeCell 的 &self 写不安全
// 但被 AtomicBool swap 保护，符合 bcachefs cmpxchg 语义。
unsafe impl Sync for AtomicFirstError {}

impl AtomicFirstError {
    pub fn new() -> Self {
        Self {
            set: AtomicBool::new(false),
            error: UnsafeCell::new(None),
        }
    }

    /// 尝试设为第一个错误 — 对应 bcachefs `cmpxchg(&cl->error, 0, err)`
    pub fn set_first(&self, err: StorageError) {
        if !self.set.swap(true, Ordering::AcqRel) {
            unsafe { *self.error.get() = Some(err) };
        }
    }

    /// 取出错误（如果有）
    pub fn take(&self) -> Option<StorageError> {
        unsafe { (*self.error.get()).take() }
    }
}

/// 单写入者-单读取者原子单元格 — 对齐 bcachefs 的裸字段模式。
///
/// bcachefs 中很多数据传递路径使用裸字段（如 bio 的 bi_private + end_io），
/// 写入者（如 block device 完成回调）和读取者（如 end_io 回调）之间通过
/// 回调调用顺序建立 happens-before，不需要 Mutex。
///
/// subvol 中 `AtomicCell` 用 `AtomicBool` 记录"已写入"状态，
/// `UnsafeCell<Option<T>>` 存储数据。写入一次、读取一次，零锁开销。
#[derive(Debug)]
pub(crate) struct AtomicCell<T> {
    set: AtomicBool,
    value: UnsafeCell<Option<T>>,
}

// Sync: AtomicCell 的写入者和读取者之间存在 happens-before 关系
// （写入后信号同步 → 读取），符合 bcachefs 回调链的语义。
unsafe impl<T: Send> Sync for AtomicCell<T> {}

impl<T> AtomicCell<T> {
    pub fn new() -> Self {
        Self {
            set: AtomicBool::new(false),
            value: UnsafeCell::new(None),
        }
    }

    /// 存储值 — 对应 bcachefs 裸字段写入
    ///
    /// 假定只有一个写入者。多次调用时第一次成功，后续被忽略。
    pub fn store(&self, value: T) {
        if !self.set.swap(true, Ordering::Release) {
            unsafe { *self.value.get() = Some(value) };
        }
    }

    /// 取出值（如果有）— 对应 bcachefs 裸字段读取
    ///
    /// 在 store 完成后调用一次。
    pub fn take(&self) -> Option<T> {
        unsafe { (*self.value.get()).take() }
    }
}

impl From<AllocError> for StorageError {
    fn from(e: AllocError) -> Self {
        match e {
            AllocError::ReserveExhausted {
                group_id: _group_id,
                bucket_count,
            } => StorageError::AddressSpaceExhausted {
                max_raw_addr: bucket_count,
            },
            AllocError::AddressSpaceExhausted { max_raw_addr } => {
                StorageError::AddressSpaceExhausted { max_raw_addr }
            }
            AllocError::OpenBucketExhausted => {
                StorageError::AddressSpaceExhausted { max_raw_addr: 0 }
            }
            AllocError::IncompatibleTarget { data_type, target } => {
                StorageError::Transaction(format!(
                    "incompatible target group {} for data type {:?}",
                    target, data_type
                ))
            }
            AllocError::Serialization(e) => StorageError::Serialization(e),
            AllocError::QuotaExceeded(msg) => StorageError::QuotaExceeded { message: msg },
        }
    }
}

/// 分配器专用错误 — 对应 bcachefs alloc 层错误
///
/// 与 StorageError 分离，避免将 alloc 层错误（如桶耗尽）与
/// 通用存储错误（IO/checksum）混淆。调用方可从 AllocError 恢复
///（降级水位线、重试、或等待回收），而 StorageError 更可能是 fatal 的。
#[derive(Debug, thiserror::Error)]
pub enum AllocError {
    /// 预留空间耗尽（原 AddressSpaceExhausted）
    #[error("reserve exhausted: all {bucket_count} buckets in group {group_id} used")]
    ReserveExhausted {
        /// 分配组 ID
        group_id: u32,
        /// 该组 bucket 总数
        bucket_count: u64,
    },

    /// 全局地址空间耗尽（所有 AG 均无可用桶）
    #[error("address space exhausted (max raw addr: {max_raw_addr})")]
    AddressSpaceExhausted {
        /// 可用 block 上限
        max_raw_addr: u64,
    },

    /// Open bucket 池满
    #[error("open bucket pool exhausted")]
    OpenBucketExhausted,

    /// 分配请求的数据类型与 target group 不兼容
    #[error("data type {data_type:?} not compatible with target group {target}")]
    IncompatibleTarget {
        /// 请求的数据类型
        data_type: super::alloc::BchDataType,
        /// 目标 group
        target: u32,
    },

    /// 事务序列化失败（bincode）
    #[error("serialization error: {0}")]
    Serialization(#[from] bincode::Error),

    /// 配额超限 — 对应 bcachefs -QUOTA_EXCEEDED
    #[error("quota exceeded: {0}")]
    QuotaExceeded(String),
}

/// bcachefs 对齐的水位线总数（Watermark 变体数）
pub const WATERMARK_NR: usize = 7;

// ═══════════════════════════════════════════════════════════
// Watermark 水位线系统（bcachefs BCH_WATERMARKS 对齐）
// ═══════════════════════════════════════════════════════════

/// bcachefs 对齐的水位线级别（参考 `fs/alloc/types.h` BCH_WATERMARKS）
///
/// 用于 journal 和 alloc 的准入控制：
/// - 低枚举值 = 高需求操作（大 I/O），需要更多空闲空间
/// - 高枚举值 = 关键操作（btree split/reclaim），可以极少空间运行
///
/// Journal 水位线随利用率上升，阻止低水位线操作通过。
/// Alloc 水位线决定预留 bucket 数——高需求操作需更多空闲 bucket。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum Watermark {
    /// 最高需求——条带写入（大 IO，最贪婪）
    Stripe = 0,
    /// 正常用户写入
    Normal = 1,
    /// GC 碎片整理
    CopyGC = 2,
    /// btree 节点写入
    Btree = 3,
    /// btree 节点 GC
    BtreeCopyGC = 4,
    /// 回收操作（journal/alloc reclaim）
    Reclaim = 5,
    /// 最低需求——btree 分裂/合并（最紧急，必须成功）
    InteriorUpdate = 6,
}

impl Watermark {
    pub const BITS: u8 = 3;
    pub const MASK: u8 = 0b111;

    /// 从 u8 转换（低位 3 位有效）
    pub fn from_bits(bits: u8) -> Self {
        match bits & Self::MASK {
            0 => Watermark::Stripe,
            1 => Watermark::Normal,
            2 => Watermark::CopyGC,
            3 => Watermark::Btree,
            4 => Watermark::BtreeCopyGC,
            5 => Watermark::Reclaim,
            _ => Watermark::InteriorUpdate,
        }
    }

    /// 转为 u8
    pub fn to_bits(self) -> u8 {
        self as u8
    }

    /// 返回该水位线预留的 bucket 数
    ///
    /// 对齐 bcachefs `bch2_dev_buckets_reserved()` (buckets.h)。
    /// 使用 switch fallthrough 语义：低枚举值（高需求）累积更多预留。
    pub fn reserved_buckets_with_btree_reserve(
        self,
        total_buckets: u64,
        btree_reserve: u64,
    ) -> u64 {
        let nb = total_buckets.max(1);
        let mut reserved = 0u64;
        let wm = self as u8;
        if wm <= Watermark::Stripe as u8 {
            reserved += (nb / 64).max(1).min(nb);
        }
        if wm <= Watermark::Normal as u8 {
            reserved += (nb / 64).max(1).min(nb);
        }
        if wm <= Watermark::CopyGC as u8 {
            reserved += btree_reserve;
        }
        if wm <= Watermark::Btree as u8 {
            reserved += btree_reserve;
        }
        // BtreeCopyGC / Reclaim / InteriorUpdate: 无预留（break）
        // 确保预留不超过总桶数，且至少留 1 个空闲桶用于正常分配
        reserved.min(nb.saturating_sub(1))
    }

    /// 兼容包装：使用默认配置推导的 btree reserve。
    pub fn reserved_buckets(self, total_buckets: u64) -> u64 {
        self.reserved_buckets_with_btree_reserve(total_buckets, default_btree_reserve_buckets())
    }

    /// 根据 journal 利用率选择水位线
    ///
    /// 利用率越高，水位线越高（准入越严格）。
    /// bcachefs 在 `journal_space_available()` 中动态调整 `j->watermark`。
    pub fn from_journal_utilization(util_pct: f64) -> Self {
        if util_pct >= 0.90 {
            Watermark::InteriorUpdate // 几乎满 → 仅最紧急操作
        } else if util_pct >= 0.80 {
            Watermark::Reclaim
        } else if util_pct >= 0.70 {
            Watermark::Btree
        } else if util_pct >= 0.50 {
            Watermark::Normal
        } else {
            Watermark::Stripe // 充足空间 → 全部放行
        }
    }

    /// 根据 allocator 利用率选择水位线
    pub fn from_alloc_utilization(util_pct: f64) -> Self {
        if util_pct >= 0.95 {
            Watermark::InteriorUpdate
        } else if util_pct >= 0.85 {
            Watermark::Reclaim
        } else if util_pct >= 0.75 {
            Watermark::Btree
        } else if util_pct >= 0.60 {
            Watermark::CopyGC
        } else {
            Watermark::Stripe
        }
    }

    /// 检查当前水位线是否允许请求通过
    ///
    /// 返回 `true` 如果 `request >= self`（请求的水位线 >= 当前阈值）。
    /// bcachefs 语义：`(flags & WATERMARK_MASK) >= j->watermark` 才通过。
    pub fn allows(self, request: Watermark) -> bool {
        (request as u8) >= (self as u8)
    }
}

/// 后台任务句柄 — tokio 版的 kthread_should_stop/kthread_stop
///
/// 对齐 bcachefs 的 kthread 生命周期管理：
/// - kthread_create → spawn（返回句柄）
/// - wake_up_process → （tokio 自动调度）
/// - kthread_should_stop → is_cancelled()
/// - kthread_stop → cancel() + join()
///
/// 后台任务需要定期调用 or 在循环条件中检查 `is_cancelled()`
/// 以实现及时关闭。
pub struct BgTaskHandle {
    pub name: &'static str,
    cancelled: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

const DEFAULT_BTREE_NODE_RESERVE: u64 = 60;

const fn default_btree_reserve_buckets() -> u64 {
    let ratio = crate::alloc::DEFAULT_BUCKET_SIZE / crate::alloc::DEFAULT_BTREE_NODE_SIZE as u64;
    let ratio = if ratio == 0 { 1 } else { ratio };
    (DEFAULT_BTREE_NODE_RESERVE + ratio - 1) / ratio
}

impl BgTaskHandle {
    /// 创建一个后台任务
    ///
    /// `f` 是后台逻辑函数，通过接收 `should_stop: Arc<AtomicBool>`
    /// 参数来检查是否被取消。任务函数应在每次循环迭代中检查
    /// `should_stop.load(Ordering::Acquire)`，若为 true 则退出。
    pub fn spawn<F>(name: &'static str, f: F) -> Self
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let handle = tokio::spawn(f);
        Self {
            name,
            cancelled: Arc::new(AtomicBool::new(false)),
            handle,
        }
    }

    /// 创建一个可取消的后台任务
    ///
    /// `mk_future` 接收 `should_stop: Arc<AtomicBool>` 参数，
    /// 任务应定期检查 `should_stop` 来响应取消请求。
    pub fn spawn_cancellable<F, M>(name: &'static str, mk_future: M) -> Self
    where
        M: FnOnce(Arc<AtomicBool>) -> F,
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let cancelled = Arc::new(AtomicBool::new(false));
        let should_stop = cancelled.clone();
        let handle = tokio::spawn(mk_future(should_stop));
        Self {
            name,
            cancelled,
            handle,
        }
    }

    /// 获取停止标志的克隆（用于传递给子任务）
    pub fn stop_flag(&self) -> Arc<AtomicBool> {
        self.cancelled.clone()
    }

    /// 触发取消（对应 kthread_stop）
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    /// 等待任务完全退出（对应 kthread_stop + put_task_struct）
    pub async fn join(self) {
        let _ = self.handle.await;
    }

    /// 检查任务是否已被取消（对应 kthread_should_stop）
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::{default_btree_reserve_buckets, Watermark};

    #[test]
    fn watermark_reserved_buckets_uses_default_btree_reserve() {
        assert_eq!(default_btree_reserve_buckets(), 15);
        assert_eq!(
            Watermark::Stripe.reserved_buckets(4096),
            4096 / 64 + 4096 / 64 + 15 + 15
        );
        assert_eq!(
            Watermark::Normal.reserved_buckets(4096),
            4096 / 64 + 15 + 15
        );
        assert_eq!(Watermark::CopyGC.reserved_buckets(4096), 15 + 15);
        assert_eq!(Watermark::Btree.reserved_buckets(4096), 15);
        assert_eq!(Watermark::BtreeCopyGC.reserved_buckets(4096), 0);
        assert_eq!(Watermark::Reclaim.reserved_buckets(4096), 0);
        assert_eq!(Watermark::InteriorUpdate.reserved_buckets(4096), 0);
    }
}
