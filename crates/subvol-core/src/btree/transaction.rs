//! BtreeTrans — bcachefs 对齐的事务（iter 容器 + journal commit + 自动重启）
//!
/// 注意：bcachefs 中没有 ACID 事务。btree_transaction 是多个 btree_iter
/// 的容器，负责管理锁顺序、提供重启机制、以及将修改提交到 journal。
///
/// ## Journal 集成（Phase 2）
///
/// BtreeTrans 维护一个 journal 列表，记录事务内的所有 btree 修改操作。
/// 调用者（Volume 层）在事务提交后 drain journal，将条目写入 WAL：
///
/// ```text
/// trans.bch2_trans_begin();
/// btree.insert(key, val, &mut trans);
/// trans.bch2_trans_commit()?;
/// for entry in trans.drain_journal() {
///     let wal_entry = WalEntry::new_btree_node_entry(seq, node_addr, entry.key, entry.value, entry.op);
///     wal.append(&wal_entry).await?;
/// }
/// ```
///
/// ## 自动重启（Phase A）
///
/// `bch2_trans_commit()` 使用自动重启循环（restart loop），在锁冲突等场景下自动释放锁、
/// 重置 iter、然后重试。重启通过 `needs_restart` 标志触发，由事务内部的
/// `try_lock_all()` 或外部操作（如 iter 锁升级失败）设置。
///
/// 重启计数由 `restart_count` 追踪，超过 `MAX_RESTARTS` 阈值时返回
/// `StorageError::TransactionRestartLimit`。
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::alloc::{BchFsUsageBase, DiskReservation};
use crate::btree::io::bch2_btree_node_prep_for_write;
use crate::btree::iter::{BtreeIter, IterFlags};
use crate::btree::key::{BchVal, Bpos, BtreeEntry, BtreeKey, ExtentValue, KeyType, KeyValue};
use crate::btree::node::{
    bch2_btree_node_iter_advance, bch2_btree_node_iter_init, bch2_btree_node_iter_init_from_start,
    bch2_btree_node_iter_peek, BtreeNode, BtreeNodeIter,
};
use crate::btree::op::BtreeOp;
use crate::btree::types::{
    BtreeNodeLockedType, BtreePath, BtreePathError, BtreePathLevel, BtreePathNode, BtreeRoot,
    NodeCache, PathIdx, BTREE_ITER_INITIAL, BTREE_ITER_MAX, BTREE_MAX_DEPTH, PATH_IDX_INVALID,
    ROOT_CACHE_ADDR,
};
use crate::btree::writer::NoopWriter;
use crate::btree::Btree;
use crate::btree::BtreeId;
use crate::io::{submit_bio_write, BioRequest};
use crate::journal::{
    crc32c, JournalRes, JsetEntryHeader, JsetEntryType, JsetHeader, RawJsetEntry, CSUM_TYPE_NONE,
    JOURNAL_MAGIC, JSET_BLOCK_SIZE, JSET_VERSION,
};
use crate::lock::deadlock::{with_detector_mut, WaiterInfo};
use crate::lock::six::{SixLockType, SixLockWaiter};

use crate::types::Watermark;
use crate::BchVol;
use crate::StorageError;

/// 对齐 bcachefs `trans->locking` 原始指针的包装
///
/// 对应 bcachefs `struct btree_bkey_cached_common *locking` (types.h:849)。
/// 在事务锁获取期间指向目标节点，仅在同一线程或 should_sleep 回调中访问。
/// `*const ()` 默认不是 Send，但此指针的访问模式是线程安全的：
///   1. 在 `btree_node_lock_nopath` 中设置
///   2. 在锁等待回调中读取
///   3. 在锁获取返回前清空
#[repr(transparent)]
struct LockingPtr(*const ());
// SAFETY: locking 指针仅在锁获取期间有效，且在同一线程或
// should_sleep 回调（由 six_lock 的 waker 在同一线程上调用）中访问。
unsafe impl Send for LockingPtr {}
unsafe impl Sync for LockingPtr {}

/// 最大重启次数（防止无限循环）
const MAX_RESTARTS: u32 = 1024;

/// 事务重启触发条件 — 对应 bcachefs `BCH_ERR_transaction_restart_*` 错误码
///
/// 扩展覆盖 bcachefs 核心 restart 场景（commit.c:1381-1523 + btree_types.h）：
///
/// | 变体 | bcachefs 对应 | 触发场景 |
/// |------|---------------|---------|
/// | LockConflict | restart_would_deadlock | 锁获取失败 |
/// | NodeSplit | restart_btree_node_split | btree 节点分裂 |
/// | KeyCacheMiss | restart_key_cache_raced | key_cache 未命中 |
/// | TriggerNeedsLock | (trans_trigger 失败) | 触发器需要重试 |
/// | NodeReadRequired | (节点重读) | 节点需要从磁盘读取 |
/// | WouldDeadlock | restart_would_deadlock_write | 死锁检测 |
/// | WriteOverflow | restart_write_overflow | btree 节点空间不足 |
/// | SplitWithInteriorUpdates | restart_split_with_interior_updates | 分裂时存在内部更新 |
/// | PathUpgradeFailed | (路径升级失败) | 无法升级到写锁 |
/// | JournalReclaimWouldDeadlock | journal_reclaim_would_deadlock | reclaim 路径死锁 |
/// | JournalOverwritesChanged | restart_journal_overwrites_changed | journal 覆盖键变化 |
/// | TraverseAll | restart_traverse_all | 遍历所有 nodes |
/// | Relock | restart_relock | 重新获取锁 |
/// | RelockPath | restart_relock_path | 重新获取指定路径锁 |
/// | Upgrade | restart_upgrade | 锁升级失败 |
/// | FaultInject | restart_fault_inject | 故障注入测试 |
/// | Nested | restart_nested | 嵌套事务重启 |
/// | LockWaitlistAlloc | restart_lock_waitlist_alloc | 等待列表分配失败 |
/// | MemoryRealloced | restart_mem_realloced | 内存重分配（路径表扩容） |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RestartReason {
    /// 锁获取失败（锁冲突）
    /// 对应 bcachefs `BCH_ERR_transaction_restart_would_deadlock`
    LockConflict,
    /// btree 节点分裂导致路径失效
    /// 对应 bcachefs `BCH_ERR_transaction_restart_btree_node_split`
    NodeSplit,
    /// key_cache miss 需要 IO
    /// 对应 bcachefs `BCH_ERR_transaction_restart_key_cache_raced`
    KeyCacheMiss,
    /// 触发器需要额外的锁
    TriggerNeedsLock,
    /// btree 节点需要重新读取
    NodeReadRequired,
    /// 死锁检测 — 锁顺序违反导致死锁风险
    /// 对应 bcachefs `BCH_ERR_transaction_restart_would_deadlock_write`
    WouldDeadlock,
    /// btree 节点空间不足（写溢出）
    /// 对应 bcachefs `BCH_ERR_transaction_restart_write_overflow`
    WriteOverflow,
    /// 分裂时存在内部更新，需完整重启
    /// 对应 bcachefs `BCH_ERR_transaction_restart_split_with_interior_updates`
    SplitWithInteriorUpdates,
    /// 无法将路径升级到写锁
    PathUpgradeFailed,
    /// journal reclaim 路径死锁 — 水位线低于 Reclaim 且被阻塞
    /// 对应 bcachefs `journal_reclaim_would_deadlock`
    JournalReclaimWouldDeadlock,
    /// journal 事务名覆盖键变化，需重新获取 journal res
    /// 对应 bcachefs `BCH_ERR_transaction_restart_journal_overwrites_changed`
    JournalOverwritesChanged,
    /// 遍历所有 nodes — 路径表顺序变化需从头遍历
    /// 对应 bcachefs `BCH_ERR_transaction_restart_traverse_all`
    TraverseAll,
    /// 重新获取锁 — 当前节点锁被释放需重获
    /// 对应 bcachefs `BCH_ERR_transaction_restart_relock`
    Relock,
    /// 重新获取指定路径锁
    /// 对应 bcachefs `BCH_ERR_transaction_restart_relock_path`
    RelockPath,
    /// 锁升级失败 — 无法从当前级别升级到目标级别
    /// 对应 bcachefs `BCH_ERR_transaction_restart_upgrade`
    Upgrade,
    /// 故障注入测试
    /// 对应 bcachefs `BCH_ERR_transaction_restart_fault_inject`
    FaultInject,
    /// 嵌套事务重启
    /// 对应 bcachefs `BCH_ERR_transaction_restart_nested`
    Nested,
    /// 等待列表分配失败
    /// 对应 bcachefs `BCH_ERR_transaction_restart_lock_waitlist_alloc`
    LockWaitlistAlloc,
    /// 内存重分配 — 路径表扩容导致指针失效
    /// 对应 bcachefs `BCH_ERR_transaction_restart_mem_realloced`
    MemoryRealloced,
    /// traverse-all 递归进入。
    /// 对应本地 `BCH_ERR_transaction_restart_in_traverse_all`。
    InTraverseAll,
    /// btree 节点空间不足 — 对应 bcachefs `BCH_ERR_btree_insert_btree_node_full`
    BtreeNodeFull,
    /// journal reclaim 等待 — 对应 bcachefs `BCH_ERR_btree_insert_need_journal_reclaim`
    NeedJournalReclaim,
}

/// `bch2_btree_path_traverse_one()`/`_all()` 的错误类别。
///
/// 对应本地 bcachefs `iter.c:1315-1323`：transaction restart 与 ENOMEM
/// 必须回到 `retry_all`，其余错误必须从 `err` 出口原样返回。
#[derive(Debug)]
pub enum BtreePathTraverseError {
    Restart(RestartReason),
    OutOfMemory,
    Storage(StorageError),
}

/// btree 事务中的单个更新条目 — 对齐 bcachefs `struct btree_insert_entry` (types.h:673-730)
///
/// 记录 btree 修改操作及其上下文（层级、触发器状态、old key 等）。
/// 替代旧版裸元组 `(BtreeId, BtreeKey, BchVal, BtreeOp)`。
#[derive(Debug, Clone)]
pub struct BtreeTransEntry {
    /// 操作类型（Insert/Delete/Whiteout）
    /// 对应 bcachefs BTREE_UPDATE_* flags
    pub op: BtreeOp,
    /// 目标 btree 实例
    pub btree_id: BtreeId,
    /// 目标层级（0 = leaf）
    /// 对应 bcachefs `level:3`
    pub level: u8,
    /// 是否更新键缓存
    /// 对应 bcachefs `cached:1`
    pub cached: bool,
    /// 新键
    pub key: BtreeKey,
    /// 新值（Insert/Whiteout 时有效，Delete 时为空值）
    pub value: BchVal,
    /// 原始值（用于非 extent 数据如 snapshot 的序列化数据）。
    /// 当为 Some 时，写入使用 insert_entry_raw；value 字段被忽略。
    pub raw_value: Option<Vec<u8>>,
    /// 原始键（被覆盖/删除的旧键，用于触发器 overwrite 比较）
    /// 对应 bcachefs `old_k` / `old_v`
    pub old_key: Option<BtreeKey>,
    /// 原始值
    pub old_value: Option<BchVal>,
    /// 被覆盖/删除的原始序列化值（extent trigger 需要完整 pointer 集合）
    pub old_raw_value: Option<Vec<u8>>,
    /// insert 触发器是否已运行
    /// 对应 bcachefs `insert_trigger_run:1`
    pub insert_trigger_run: bool,
    /// overwrite 触发器是否已运行
    /// 对应 bcachefs `overwrite_trigger_run:1`
    pub overwrite_trigger_run: bool,
    /// 排序顺序 — 对应 bcachefs `sort_order` (types.h:688)
    /// 用于锁获取排序，确保 Alloc→Freespace 等依赖顺序
    pub sort_order: u8,
    /// 所属 iter 索引（替代 bcachefs 的 path 引用）
    pub iter_idx: usize,
    /// 所属 path 索引 — 对应 bcachefs `path: btree_path_idx_t` (types.h:720)
    pub path_idx: PathIdx,
    /// 被覆盖/删除的旧条目在 btree 中的 u64s 大小
    ///
    /// 用于计算 u64s_delta = new_u64s - old_btree_u64s
    /// 在 foreground merge 检查中使用。0 表示无旧条目（新插入）。
    pub old_btree_u64s: u16,
}

/// 对应 bcachefs `btree_trigger_order()` (types.h:1363-1373)
/// Alloc 最高优先级，确保其锁在 Freespace 之前获取
pub fn btree_trigger_order(btree_id: BtreeId) -> u8 {
    match btree_id {
        BtreeId::Alloc => u8::MAX,
        BtreeId::Stripes => u8::MAX - 1,
        _ => btree_id as u8,
    }
}

/// B-tree 事务 — 对应 bcachefs `btree_transaction`
///
/// 不是 ACID 事务，而是 iter 容器 + journal 累积器 + 重启管理器。核心职责：
/// 1. 持有多个 BtreeIter，管理它们的锁
/// 2. begin/commit 控制 iter 生命周期
/// 3. lock ordering 保证（避免死锁）
/// 4. 自动重启循环（锁冲突时自动 retry）
/// 5. 累积 btree 修改操作到 journal（Phase 2 WAL 集成）
pub struct BtreeTrans<'ctx> {
    /// 事务 volume 上下文（bcachefs `trans->c` 对应）
    ctx_vol: Option<&'ctx BchVol>,
    /// 事务持有的 iterators
    iters: Vec<BtreeIter>,
    /// 每个 iter 对应的 BtreeId（与 iters 并行）
    iter_types: Vec<BtreeId>,

    // ── bcachefs 对齐：路径池（trans->paths[]）──
    /// 路径数组 — 对应 bcachefs `trans->paths` (types.h:797)
    ///
    /// 路径归 trans 所有，BtreeIter 通过 `path_idx: PathIdx` 索引。
    paths: Box<Vec<Option<Box<BtreePath>>>>,
    /// 当前路径数组容量（最多 1024）
    nr_paths: PathIdx,
    /// 历史最大 path 索引 — 对应 bcachefs `trans->nr_paths_max`。
    nr_paths_max: PathIdx,
    /// 排序后的路径索引 — 对应 bcachefs `trans->sorted`
    sorted: Vec<PathIdx>,
    /// 路径是否已排序 — 对应 bcachefs `trans->paths_sorted`
    paths_sorted: bool,

    /// 事务开始后的 journal 序列号
    journal_seq: u64,
    /// 事务局部磁盘使用量变化，对应本地 `trans->fs_usage_delta`
    /// (`fs/alloc/buckets.c:572-600`)。
    fs_usage_delta: BchFsUsageBase,
    accounting_undo: Option<UsageAccountingUndo>,
    /// 当前事务的磁盘预留，对应本地 `trans->disk_res`。
    disk_res: Option<DiskReservation>,
    /// 额外磁盘预留扇区，对应本地 `trans->extra_disk_res`。
    extra_disk_res: u64,
    /// 是否已提交
    committed: bool,
    /// 节点缓存（无 vol 模式时的 fallback；有 vol 时从 vol 派生）
    cache: Option<Arc<NodeCache>>,
    // ── Phase B2: WAL pin 集成 ──
    /// 当前事务持有的 WAL pin ID（由 Volume 层在写 WAL 时设置，None = 未 pin）
    wal_pin_id: Option<u64>,
    /// Phase 2: btree 修改 journal — `Vec<BtreeTransEntry>`
    ///
    /// 调用者在 insert/delete 后调用 `trans_update` / `trans_delete`
    /// 记录修改操作。每个条目包含操作类型、btree 类型、层级、新旧键值、
    /// 触发器状态和 iter 索引。
    /// 事务 commit/rollback 后通过 `drain_journal` 取出。
    journal: Vec<BtreeTransEntry>,
    // ── Phase A: 自动重启 ──
    /// 重启计数器（每次 full restart 递增）
    restart_count: u32,
    /// 标记本次提交是否需要重启
    needs_restart: bool,
    /// 最近一次重启的原因
    restart_reason: Option<RestartReason>,
    /// 操作水位线（对应 bcachefs `BCH_WATERMARK_*`）
    ///
    /// 决定事务在资源竞争时的行为。`Reclaim` 及以上水位线的事务
    /// 在提交时跳过阻塞等待（避免 journal reclaim deadlock）。
    watermark: Watermark,
    /// 写锁已持有标志 — 对应 bcachefs `trans->write_locked`
    ///
    /// 在 `try_lock_all()` 成功获取写锁后设为 true，
    /// 在 `bch2_trans_unlock_write()` 或 `bch2_trans_unlock()` 后重置为 false。
    write_locked: bool,
    // ── REQ-4: 提交钩子 ──
    /// 提交钩子列表 — 对应 bcachefs `btree_trans_commit_hook` (commit.c:198-230)
    ///
    /// 在事务提交的 Transactional 触发器之后、写锁获取之前执行。
    /// 用于注入额外操作（如 open bucket 日志记录）。
    commit_hooks:
        Vec<Box<dyn for<'a> FnMut(&mut BtreeTrans<'a>) -> Result<(), StorageError> + Send>>,
    // ── REQ-2: 死锁检测 ──
    /// 必须中止标志 — 死锁检测到环后设置，should_sleep_fn 读此标志返回错误
    /// 对应 bcachefs `trans->lock_must_abort` (locking.c:14-17)
    lock_must_abort: bool,
    /// 必须成功标志 — reclaim/高位水位操作设置，锁阻塞时直接返回错误
    /// 对应 bcachefs `trans->lock_may_not_fail` (locking.c:47-51)
    lock_may_not_fail: bool,
    // ── bcachefs 对齐字段：锁相位 & CPU 迁移 ──
    /// 事务是否「已锁定」 — 对应 bcachefs `trans->locked` (locking.h:115)
    ///
    /// 在获取写锁/重锁时设为 true（trans_set_locked），在释放锁时设为 false
    /// （trans_set_unlocked）。subvol 不使用 lockdep，该字段仅作为相位跟踪标记。
    locked: bool,
    /// 对应本地 `trans->btree_cache_cannibalize_locked`。
    btree_cache_cannibalize_locked: bool,
    /// 对应本地 `trans->in_traverse_all`。
    in_traverse_all: bool,
    /// 对应本地 `trans->memory_allocation_failure`。
    memory_allocation_failure: bool,
    /// 对应本地 `trans->notrace_relock_fail`。
    notrace_relock_fail: bool,
    /// 上次解锁的指令地址 — 对应 bcachefs `trans->last_unlock_ip`
    /// subvol 中固定为 0（无实用值，仅结构对齐）
    last_unlock_ip: usize,
    /// 是否禁用了 CPU 迁移（bcachefs 在持锁期间 pin 到 CPU 以保持 cache 热度）
    ///
    /// bcachefs 对应：`trans->migrate_disabled` (locking.h:99-104)
    /// subvol 语义：在 async 运行时不控制 CPU 迁移，该字段仅用作锁相位标记。
    migrate_disabled: bool,
    /// 绑定的 shard CPU（bcachefs inode 分配 shard 对应的 CPU 编号）
    ///
    /// bcachefs 对应：`trans->shard_cpu` (locking.h:99)
    /// >= 0 时 trans_maybe_disable_migrate 检查当前 CPU 是否匹配。
    /// subvol 默认 -1（不检查），由调用者在绑定 shard 模式时设置。
    shard_cpu: i32,
    /// 是否持有 SRCU 读锁 — 对应 bcachefs `trans->srcu_held`
    ///
    /// bcachefs 在 btree 节点查找时用 SRCU 保护节点生命周期；
    /// subvol 使用 Arc 管理节点引用，不需要 SRCU。该字段仅用于相位对齐。
    srcu_held: bool,
    /// SRCU 读锁索引 — 对应 bcachefs `trans->srcu_idx` (types.h:845)
    srcu_idx: u64,
    /// SRCU 加锁时间戳 — 对应 bcachefs `trans->srcu_lock_time` (types.h:843)
    srcu_lock_time: u64,
    /// 持锁期间排队的 btree 节点写 IO。
    ///
    /// 对应本地 bcachefs `trans->queued_write_bios`
    /// (`types.h:857-864`)；只能在所有 path 锁释放后提交。
    queued_write_bios: Vec<BioRequest>,
    // ── REQ-3: 锁顺序/死锁检测（锁获取相关） ──
    /// 当前正在获取锁的节点指针 — 对应 bcachefs `trans->locking` (locking.h:414)
    ///
    /// 在 `btree_node_lock_nopath` 中设置，锁获取完成后清空。
    /// six_lock 的 should_sleep 回调通过此指针读取节点信息做 `node_reuse_race` 检测。
    /// subvol 使用 Arc + `BtreeNode` 引用计数管理生命周期，不需要 reuse_race 检测。
    /// 原始指针对齐 bcachefs 的 `struct btree_bkey_cached_common *locking`。
    locking: LockingPtr,
    /// 锁获取时的 key hash — 对应 bcachefs `trans->locking_hash_val` (types.h:848)
    ///
    /// 用于 `node_reuse_race()` 检测节点是否被回收重用。
    /// subvol 中当前未使用，仅结构对齐。
    locking_hash_val: u64,
    /// 根 btree ID — 对应 bcachefs `trans->locking_root_id` (types.h:846)
    locking_root_id: u32,
    /// 内嵌锁等待者 — 对应 bcachefs `trans->locking_wait` (types.h:855)
    ///
    /// bcachefs 在 `btree_node_lock_nopath` 中将此 waiter 传递给 `six_lock_ip_waiter`，
    /// 复用同一 waiter 条目，避免每次锁操作都栈分配。
    /// `trans_start_time` 用于 waitlist 排序和死锁检测游标。
    locking_wait: SixLockWaiter,
    /// 路径分配位图 — 对应 bcachefs `trans->paths_allocated` (iter.c:2148-2177)
    ///
    /// 第 `i` 位为 1 表示 `paths[i]` 已被分配。
    /// 使用 `trailing_zeros` 实现 O(1) 空闲槽位查找。
    paths_allocated: Vec<u64>,
}

/// `bch_fs_usage_base` 的字段选择器，供事务触发器累加 delta。
#[derive(Debug, Clone, Copy)]
pub(crate) enum UsageField {
    Hidden,
    Btree,
    Data,
    Cached,
    Reserved,
}

#[derive(Clone, Copy, Default)]
struct UsageAccountingUndo {
    usage: BchFsUsageBase,
    sectors_available_subtracted: u64,
    reservation_consumed: u64,
}

/// 路径索引位图迭代器 — 对应 bcachefs `trans_for_each_path_idx_from` (iter.h:242-245)
///
/// 在动态位图上扫描已分配的路径槽位。
pub struct PathBitmapIter<'a> {
    bits: &'a [u64],
    nr_paths: usize,
    pos: usize,
}

impl<'a> PathBitmapIter<'a> {
    fn new(bits: &'a [u64], nr_paths: usize, start: usize) -> Self {
        Self {
            bits,
            nr_paths,
            pos: start,
        }
    }
}

impl Iterator for PathBitmapIter<'_> {
    type Item = PathIdx;

    fn next(&mut self) -> Option<PathIdx> {
        // 对应本地 bcachefs iter.h:242-245 的 find_next_bit 循环。
        while self.pos < self.nr_paths {
            let word_idx = self.pos / u64::BITS as usize;
            let bit_idx = self.pos % u64::BITS as usize;
            let shifted = self.bits[word_idx] >> bit_idx;

            if shifted != 0 {
                let idx = self.pos + shifted.trailing_zeros() as usize;
                self.pos = idx + 1;
                return Some(idx as PathIdx);
            }

            self.pos = (word_idx + 1) * u64::BITS as usize;
        }

        None
    }
}

/// userspace 单调事务时间游标，对应本地 `local_clock()` 用于
/// `six_lock_waiter.trans_start_time` 的排序语义。
static NEXT_TRANS_START_TIME: AtomicU64 = AtomicU64::new(1);

/// 内部默认字段（新建 trans 时共用）
fn trans_defaults() -> BtreeTrans<'static> {
    BtreeTrans {
        iters: Vec::new(),
        iter_types: Vec::new(),
        paths: Box::new(
            std::iter::repeat_with(|| None)
                .take(BTREE_ITER_INITIAL)
                .collect(),
        ),
        nr_paths: BTREE_ITER_INITIAL as PathIdx,
        nr_paths_max: 0,
        sorted: Vec::new(),
        paths_sorted: false,
        journal_seq: 0,
        fs_usage_delta: BchFsUsageBase::default(),
        accounting_undo: None,
        disk_res: None,
        extra_disk_res: 0,
        committed: false,
        cache: None,
        wal_pin_id: None,
        journal: Vec::new(),
        restart_count: 0,
        needs_restart: false,
        restart_reason: None,
        watermark: Watermark::Normal,
        write_locked: false,
        lock_must_abort: false,
        lock_may_not_fail: false,
        locked: false,
        btree_cache_cannibalize_locked: false,
        in_traverse_all: false,
        memory_allocation_failure: false,
        notrace_relock_fail: false,
        last_unlock_ip: 0,
        migrate_disabled: false,
        shard_cpu: -1,
        srcu_held: false,
        srcu_idx: 0,
        srcu_lock_time: 0,
        queued_write_bios: Vec::new(),
        locking: LockingPtr(std::ptr::null()),
        locking_hash_val: 0,
        locking_root_id: 0,
        locking_wait: SixLockWaiter {
            trans_start_time: 0,
            thread: None,
            lock_want: SixLockType::Read,
            lock_acquired: false,
            slot_idx: 0,
        },
        commit_hooks: Vec::new(),
        ctx_vol: None,
        // 本地 bcachefs iter.c:4112-4113：path 0 永久保留为 sentinel。
        paths_allocated: vec![1],
    }
}

/// 对应本地 bcachefs `bch2_trans_put()` (`iter.c:4189-4203`) 的
/// Rust 生命周期出口：先执行 long unlock，再释放 update 持有的 path 引用。
impl Drop for BtreeTrans<'_> {
    fn drop(&mut self) {
        Self::sx_unregister_deadlock_detection();
        self.bch2_trans_unlock_long();

        let path_indices: Vec<PathIdx> = self
            .journal
            .iter()
            .filter(|entry| entry.path_idx != PATH_IDX_INVALID)
            .map(|entry| entry.path_idx)
            .collect();
        for path_idx in path_indices {
            self.__btree_path_put(path_idx, true);
        }
        self.journal.clear();
    }
}

impl<'ctx> BtreeTrans<'ctx> {
    /// 写事务构造 — 对应 bcachefs 事务构造/初始化路径
    ///
    /// journal 从 BchVol 获取，写事务会继续写入 WAL。
    pub fn new(vol: &'ctx BchVol) -> Self {
        let mut t = trans_defaults();
        t.ctx_vol = Some(vol);
        // bcachefs: __bch2_trans_get — srcu_read_lock at end (iter.c:4134-4137)
        t.bch2_trans_srcu_lock();
        t
    }

    /// 读事务构造 — 适用于只需读 btree 的操作（list / get / count 等）
    pub fn new_ro(vol: &'ctx BchVol) -> Self {
        let mut t = trans_defaults();
        t.ctx_vol = Some(vol);
        t.bch2_trans_srcu_lock();
        t
    }

    /// 无 journal 事务（测试 / recovery 早期使用）
    ///
    /// 可读可写，但不写入 WAL。
    pub fn new_nojournal(vol: &'ctx BchVol) -> Self {
        let mut t = trans_defaults();
        t.ctx_vol = Some(vol);
        t.bch2_trans_srcu_lock();
        t
    }

    /// 内部构造：仅绑定 cache（无 vol），用于 `Btree::with_transaction`
    pub(crate) fn new_with_cache(cache: Arc<NodeCache>) -> Self {
        let mut t = trans_defaults();
        t.cache = Some(cache);
        t
    }

    /// 设置事务水位线（返回 self 以便链式调用）
    ///
    /// 水位线决定事务在资源竞争时的行为。`Reclaim` 及以上操作
    /// 跳过阻塞等待（避免 journal reclaim 死锁）。
    pub fn set_watermark(&mut self, wm: Watermark) -> &mut Self {
        self.watermark = wm;
        self
    }

    /// `lock_must_abort` — 对应 bcachefs `trans->lock_must_abort`
    pub fn lock_must_abort(&self) -> bool {
        self.lock_must_abort
    }

    /// `lock_may_not_fail` — 对应 bcachefs `trans->lock_may_not_fail`
    pub fn lock_may_not_fail(&self) -> bool {
        self.lock_may_not_fail
    }

    /// 设置 lock_must_abort — 死锁检测到环时调用
    pub fn set_lock_must_abort(&mut self) {
        self.lock_must_abort = true;
    }

    /// 设置 lock_may_not_fail — reclaim/高位水位操作调用
    pub fn set_lock_may_not_fail(&mut self) {
        self.lock_may_not_fail = true;
    }

    /// 死锁检测入口 — 对应 bcachefs `bch2_check_for_deadlock` (locking.c:189-310)
    ///
    /// 接收预收集的 WaiterInfo 列表，运行基于 DFS 栈的死锁检测。
    /// 若检测到死锁环，设置 lock_must_abort 并返回 true。
    pub fn bch2_check_for_deadlock(&mut self, waiters: &[WaiterInfo]) -> bool {
        if self.lock_must_abort {
            return true;
        }
        let deadlocked =
            with_detector_mut(|d| d.detect(self.locking_wait.trans_start_time, 0, waiters));
        if deadlocked {
            self.lock_must_abort = true;
        }
        deadlocked
    }

    /// 收集当前事务持有的所有锁上的等待者信息
    ///
    /// 对应 bcachefs `bch2_check_for_deadlock` Phase 2 (locking.c:573-631):
    /// 遍历事务的所有 btree_path，对每个 path level 上已锁定的节点，
    /// 扫描其 wait_fifo 收集等待者信息。
    fn sx_collect_held_lock_waiter_info(&self, out: &mut Vec<WaiterInfo>) {
        for iter in &self.iters {
            let path = self.path_ref(iter.path);
            for level in &path.levels {
                if let BtreePathNode::Node(pl) = level {
                    let lock_id = pl.node.lock.six_lock_seq();
                    // 推断 holder 为当前事务（我们持有这些锁）
                    let holder = self.locking_wait.trans_start_time;
                    let more = pl
                        .node
                        .lock
                        .sx_collect_wait_fifo_waiter_info(lock_id, holder);
                    out.extend(more);
                }
            }
        }
    }

    /// should_sleep 回调 — 对应 bcachefs `bch2_six_check_for_deadlock`
    ///
    /// 在 six_lock 的 park 循环中被调用（对齐 bcachefs `__six_lock_slowpath`
    /// 中的 `should_sleep_fn`）。收集当前锁和事务持有路径上的等待者信息，
    /// 运行 DFS 死锁检测。若检测到环则返回非 0 中止等待。
    ///
    /// 注意：此函数通过 thread_local 机制注册（见 `sx_register_deadlock_detection`），
    /// 因为锁获取发生在 iter traversal 中而非直接通过 BtreeTrans 方法。
    pub fn bch2_six_check_for_deadlock(
        &mut self,
        lock: &crate::lock::six::SixLock,
        _waiter: &crate::lock::six::SixLockWaiter,
    ) -> i32 {
        if self.lock_must_abort {
            return -1;
        }
        let mut waiters: Vec<WaiterInfo> =
            lock.sx_collect_wait_fifo_waiter_info(0, self.locking_wait.trans_start_time);
        self.sx_collect_held_lock_waiter_info(&mut waiters);
        if self.bch2_check_for_deadlock(&waiters) {
            return -1;
        }
        0
    }

    /// 注册当前线程的 should_sleep 死锁检测回调
    pub(crate) fn sx_register_deadlock_detection(&mut self) {
        let self_ptr = self as *mut BtreeTrans as *mut BtreeTrans<'static>;
        crate::lock::six::sx_set_thread_should_sleep(Some(Box::new(move |lock, waiter| unsafe {
            (*self_ptr).bch2_six_check_for_deadlock(lock, waiter)
        })));
    }

    /// 注销当前线程的 should_sleep 回调
    pub(crate) fn sx_unregister_deadlock_detection() {
        crate::lock::six::sx_set_thread_should_sleep(None);
    }

    /// 注册提交钩子 — 对应 bcachefs `bch2_trans_commit_hook` (commit.c:198-230)
    ///
    /// 钩子在本地 bcachefs transactional trigger 之后、try_lock_all 之前执行。
    /// 可用于注入额外操作（如 open bucket 日志记录）。
    /// 支持短路：钩子返回 Err 时中断提交流程。
    pub fn add_commit_hook<F>(&mut self, hook: F)
    where
        F: for<'a> FnMut(&mut BtreeTrans<'a>) -> Result<(), StorageError> + Send + 'static,
    {
        self.commit_hooks.push(Box::new(hook));
    }

    /// 执行所有提交钩子 — 对应 bcachefs `run_hooks` (commit.c:210-222)
    ///
    /// 钩子按注册顺序执行。每个钩子执行后立即从列表中移除（即使失败）。
    /// 任一钩子返回 Err 则中止后续钩子并将错误传播，但已移除的钩子不会重试。
    /// 执行成功后列表为空（与 bcachefs 链表消耗语义一致）。
    fn run_commit_hooks(&mut self) -> Result<(), StorageError> {
        while !self.commit_hooks.is_empty() {
            let mut hook = self.commit_hooks.remove(0);
            (hook)(self)?;
        }
        Ok(())
    }

    /// 获取 btree 只读引用
    pub fn btree(&self, id: BtreeId) -> &Btree {
        self.ctx_vol
            .map(|v| v.btree(id))
            .unwrap_or_else(|| panic!("BtreeTrans has no vol — use new()/new_ro()/new_nojournal()"))
    }

    /// 获取 btree 可变引用（通过 UnsafeCell 内部可变性）
    pub fn btree_mut(&self, id: BtreeId) -> &mut Btree {
        self.ctx_vol
            .map(|v| v.btree_mut(id))
            .unwrap_or_else(|| panic!("BtreeTrans has no vol — use new()/new_ro()/new_nojournal()"))
    }

    /// 获取当前事务绑定的卷
    pub fn vol(&self) -> &BchVol {
        self.ctx_vol
            .unwrap_or_else(|| panic!("BtreeTrans has no vol — use new()/new_ro()/new_nojournal()"))
    }

    /// 将事务触发器产生的 usage delta 累加到 `fs_usage_delta`。
    /// bcachefs 的字段是 `u64`，但事务 delta 允许负值，因此保留二补码累加语义。
    pub(crate) fn fs_usage_add(&mut self, field: UsageField, delta: i64) {
        let dst = match field {
            UsageField::Hidden => &mut self.fs_usage_delta.hidden,
            UsageField::Btree => &mut self.fs_usage_delta.btree,
            UsageField::Data => &mut self.fs_usage_delta.data,
            UsageField::Cached => &mut self.fs_usage_delta.cached,
            UsageField::Reserved => &mut self.fs_usage_delta.reserved,
        };
        *dst = dst.wrapping_add(delta as u64);
    }

    pub(crate) fn fs_usage_delta(&self) -> BchFsUsageBase {
        self.fs_usage_delta
    }

    /// 转移一次 reservation 的唯一所有权给事务。
    pub fn set_disk_reservation(&mut self, reservation: DiskReservation) {
        self.disk_res = Some(reservation);
    }

    pub fn disk_reservation_sectors(&self) -> u64 {
        self.disk_res.as_ref().map_or(0, |r| r.sectors)
    }

    /// 对应本地 `bch2_trans_account_disk_usage_change()`
    /// (`fs/alloc/buckets.c:562-601`)。
    pub(crate) fn bch2_trans_account_disk_usage_change(&mut self) {
        let Some(vol) = self.ctx_vol else {
            return;
        };
        let capacity = unsafe { &mut *vol.capacity.get() };
        let _mark_lock = capacity.mark_lock.write().unwrap();

        let src = self.fs_usage_delta;
        let added = (src.btree as i64)
            .wrapping_add(src.data as i64)
            .wrapping_add(src.reserved as i64);
        let disk_res_sectors = self.disk_reservation_sectors();
        let should_not_have_added = added - disk_res_sectors as i64;

        let sectors_available_subtracted = if should_not_have_added > 0 {
            let requested = should_not_have_added as u64;
            let previous = capacity
                .sectors_available
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |old| {
                    Some(old.saturating_sub(requested))
                })
                .unwrap_or_else(|old| old);
            previous.saturating_sub(previous.saturating_sub(requested))
        } else {
            0
        };

        let accounted = if should_not_have_added > 0 {
            added - should_not_have_added
        } else {
            added
        };
        let mut reservation_consumed = 0;
        if accounted > 0 {
            if let Some(res) = self.disk_res.as_mut() {
                reservation_consumed = (accounted as u64).min(res.sectors);
                res.sectors = res.sectors.wrapping_sub(reservation_consumed);
                capacity.pcpu[0].online_reserved = capacity.pcpu[0]
                    .online_reserved
                    .wrapping_sub(reservation_consumed);
            }
        }

        capacity.pcpu[0].usage.hidden = capacity.pcpu[0].usage.hidden.wrapping_add(src.hidden);
        capacity.pcpu[0].usage.btree = capacity.pcpu[0].usage.btree.wrapping_add(src.btree);
        capacity.pcpu[0].usage.data = capacity.pcpu[0].usage.data.wrapping_add(src.data);
        capacity.pcpu[0].usage.cached = capacity.pcpu[0].usage.cached.wrapping_add(src.cached);
        capacity.pcpu[0].usage.reserved =
            capacity.pcpu[0].usage.reserved.wrapping_add(src.reserved);
        let undo = self.accounting_undo.get_or_insert_default();
        undo.usage.hidden = undo.usage.hidden.wrapping_add(src.hidden);
        undo.usage.btree = undo.usage.btree.wrapping_add(src.btree);
        undo.usage.data = undo.usage.data.wrapping_add(src.data);
        undo.usage.cached = undo.usage.cached.wrapping_add(src.cached);
        undo.usage.reserved = undo.usage.reserved.wrapping_add(src.reserved);
        undo.sectors_available_subtracted = undo
            .sectors_available_subtracted
            .saturating_add(sectors_available_subtracted);
        undo.reservation_consumed = undo
            .reservation_consumed
            .saturating_add(reservation_consumed);
        self.fs_usage_delta = BchFsUsageBase::default();
    }

    pub(crate) fn revert_disk_usage_accounting(&mut self) {
        let Some(undo) = self.accounting_undo.take() else {
            return;
        };
        let Some(vol) = self.ctx_vol else {
            return;
        };
        let capacity = unsafe { &mut *vol.capacity.get() };
        let _mark_lock = capacity.mark_lock.write().unwrap();
        capacity.pcpu[0].usage.hidden = capacity.pcpu[0]
            .usage
            .hidden
            .wrapping_sub(undo.usage.hidden);
        capacity.pcpu[0].usage.btree = capacity.pcpu[0].usage.btree.wrapping_sub(undo.usage.btree);
        capacity.pcpu[0].usage.data = capacity.pcpu[0].usage.data.wrapping_sub(undo.usage.data);
        capacity.pcpu[0].usage.cached = capacity.pcpu[0]
            .usage
            .cached
            .wrapping_sub(undo.usage.cached);
        capacity.pcpu[0].usage.reserved = capacity.pcpu[0]
            .usage
            .reserved
            .wrapping_sub(undo.usage.reserved);
        if undo.sectors_available_subtracted != 0 {
            capacity
                .sectors_available
                .fetch_add(undo.sectors_available_subtracted, Ordering::AcqRel);
        }
        if undo.reservation_consumed != 0 {
            if let Some(res) = self.disk_res.as_mut() {
                res.sectors = res.sectors.saturating_add(undo.reservation_consumed);
            }
            capacity.pcpu[0].online_reserved = capacity.pcpu[0]
                .online_reserved
                .wrapping_add(undo.reservation_consumed);
        }
    }

    /// 获取指定 btree 类型的节点缓存
    fn cache_for(&self, btree_id: BtreeId) -> Arc<NodeCache> {
        if let Some(vol) = self.ctx_vol {
            vol.cache_arc(btree_id)
        } else if let Some(ref cache) = self.cache {
            cache.clone()
        } else {
            panic!("BtreeTrans has no cache — use new()/new_ro() with a vol, or new_with_cache()")
        }
    }

    /// btree point read。
    /// 先查当前事务的 pending journal entries，再 fallback 到 btree。
    pub fn get_entry(&self, btree_id: BtreeId, pos: Bpos) -> Option<BtreeEntry> {
        for entry in self.journal.iter().rev() {
            if entry.btree_id == btree_id && Bpos::from_key(&entry.key) == pos {
                return match entry.op {
                    BtreeOp::Whiteout => None,
                    BtreeOp::Insert if entry.key.key_type == KeyType::Deleted => None,
                    BtreeOp::Insert => {
                        let value = match &entry.raw_value {
                            Some(raw) => KeyValue::Raw(raw.clone()),
                            None => KeyValue::Extent(ExtentValue {
                                paddr: entry.value.paddr.get(),
                                size: 1,
                                ver: entry.value.ver,
                                dev_idx: 0,
                                crc32c: 0,
                                crc_offset_blocks: 0,
                            }),
                        };
                        Some(BtreeEntry {
                            pos,
                            key_type: entry.key.key_type,
                            needs_whiteout: false,
                            value,
                        })
                    }
                    _ => None,
                };
            }
        }
        self.ctx_vol?
            .btree(btree_id)
            .bch2_btree_iter_peek_entry(pos)
    }

    /// btree point read，允许 whiteout 覆盖的条目。
    pub fn get_entry_allow_whiteout(&self, btree_id: BtreeId, pos: Bpos) -> Option<BtreeEntry> {
        for entry in self.journal.iter().rev() {
            if entry.btree_id == btree_id && Bpos::from_key(&entry.key) == pos {
                let value = match &entry.raw_value {
                    Some(raw) => KeyValue::Raw(raw.clone()),
                    None => KeyValue::Extent(ExtentValue {
                        paddr: entry.value.paddr.get(),
                        size: 1,
                        ver: entry.value.ver,
                        dev_idx: 0,
                        crc32c: 0,
                        crc_offset_blocks: 0,
                    }),
                };
                return Some(BtreeEntry {
                    pos,
                    key_type: entry.key.key_type,
                    needs_whiteout: false,
                    value,
                });
            }
        }
        self.ctx_vol?.btree(btree_id).get_entry_allow_whiteout(pos)
    }

    /// 创建一个新的 iter 并加入事务
    ///
    /// 对应 bcachefs `bch2_trans_get_iter()`
    /// `btree_type` 指定该 iter 将用于哪个 btree 实例，用于锁排序。
    pub fn bch2_trans_get_iter(
        &mut self,
        root: &BtreeRoot,
        target: &BtreeKey,
        intent: bool,
        btree_type: BtreeId,
    ) -> &mut BtreeIter {
        let idx = self.get_path(root, target, intent, btree_type, None);
        &mut self.iters[idx]
    }

    /// 获取或创建 path iter，优先复用现有 path
    ///
    /// R1 路径缓存复用：先在已有 iters 中查找匹配的 path。
    /// 精确匹配 (pos == target) 直接返回索引；否则下降新 iter，
    /// 若与已有 iter 在同一个 leaf 中则复用（通过 `Arc::ptr_eq` 比较 leaf 节点地址）。
    ///
    /// 对应本地 bcachefs `bch2_path_get()` (iter.c:2201-2279)，
    /// 追加 `flags` 参数支持 `BTREE_ITER_cached` 和
    /// `BTREE_ITER_nopreserve` 标志。
    ///
    /// 返回 iters 中的索引，调用者通过 `iter_mut(idx)` 访问。
    pub fn get_path(
        &mut self,
        root: &BtreeRoot,
        target: &BtreeKey,
        intent: bool,
        btree_type: BtreeId,
        flags: Option<IterFlags>,
    ) -> usize {
        // 解包标志（对应本地 bcachefs iter.c:2208 的 cached 提取和
        // iter.c:2267 的 nopreserve 检查）
        let (cached, nopreserve) = flags
            .map(|f| (f.cached, f.nopreserve))
            .unwrap_or((false, false));

        // 对应本地 bcachefs bch2_path_get() (iter.c:2201-2279)。
        let target_bpos = target.to_bpos();
        let new_flags = IterFlags {
            intent,
            forward: true,
            with_journal: false,
            cached,
            nopreserve,
        };
        self.btree_trans_sort_paths();
        let reusable = self.sorted.iter().rev().copied().find(|path_idx| {
            let path = self.path_ref(*path_idx);
            // 对应本地 bcachefs iter.c:2233-2237：
            //   trans->paths[path_pos].cached == cached
            //   && trans->paths[path_pos].btree_id == btree_id
            //   && trans->paths[path_pos].level == level
            //   && bch2_btree_path_upgrade_norestart(...)
            // C 版本检查 cached 字段匹配；Rust 版本追加 cached 支持。
            path.btree_id == btree_type
                && path.cached == cached
                && path.level == 0
                && path.pos == target_bpos
        });
        if let Some(path_idx) = reusable {
            let locks_want = u8::from(intent);
            if self.bch2_btree_path_upgrade_norestart(path_idx, locks_want) {
                self.__btree_path_get(path_idx, intent);
                // 对应本地 bcachefs iter.c:2267-2268：
                //   if (!(flags & BTREE_ITER_nopreserve)) path->preserve = true;
                if !nopreserve {
                    self.path_mut(path_idx).preserve = true;
                }
                let iter = BtreeIter::from_existing(
                    target,
                    new_flags,
                    self.cache_for(btree_type),
                    btree_type,
                    path_idx,
                    &mut self.paths,
                );
                let iter_idx = self.iters.len();
                self.iters.push(iter);
                self.iter_types.push(btree_type);
                return iter_idx;
            }
        }

        let cache = self.cache_for(btree_type);
        let path_idx = self.path_alloc(PATH_IDX_INVALID);
        {
            let path = self.path_mut(path_idx);
            path.pos = target_bpos;
            path.btree_id = btree_type;
            path.locks_want = u8::from(intent);
            path.ref_count = 1;
            path.intent_ref = u8::from(intent);
            // 对应本地 bcachefs iter.c:2267-2268：
            //   if (!(flags & BTREE_ITER_nopreserve)) path->preserve = true;
            path.preserve = !nopreserve;
            // 对应本地 bcachefs iter.c:2251：path->cached = cached;
            path.cached = cached;
        }
        let new_iter = BtreeIter::init_with_path(
            root,
            target,
            new_flags,
            &cache,
            btree_type,
            path_idx,
            &mut self.paths,
        );
        let iter_idx = self.iters.len();
        self.iters.push(new_iter);
        self.iter_types.push(btree_type);
        iter_idx
    }

    /// 获取指定位置的 iter（返回 mutable 引用）
    pub fn iter_mut(&mut self, idx: usize) -> Option<&mut BtreeIter> {
        self.iters.get_mut(idx)
    }

    /// 获取指定位置的 iter（只读）
    pub fn iter(&self, idx: usize) -> Option<&BtreeIter> {
        self.iters.get(idx)
    }

    /// 对应本地 bcachefs `bch2_btree_iter_set_pos()` (`btree/iter.h:680-703`)。
    ///
    /// iterator 的 path 由 transaction 所有，因此位置更新必须在这里执行：
    /// 先释放 iterator 的 update path，再按当前 snapshot 重建查询 key 与
    /// leaf path。仅修改 `BtreeIter::pos` 会留下旧 leaf 位置，和 bcachefs
    /// 的 `__bch2_btree_iter_set_pos()` 语义不等价。
    pub fn bch2_btree_iter_set_pos(&mut self, iter_idx: usize, new_pos: Bpos) {
        assert!(iter_idx < self.iters.len(), "invalid btree iterator index");

        let (update_path, intent, snapshot, btree_type) = {
            let iter = &self.iters[iter_idx];
            (
                iter.update_path,
                iter.flags.intent,
                iter.snapshot,
                iter.btree_type,
            )
        };

        // bcachefs `bch2_btree_iter_set_pos()` first drops update_path via
        // `bch2_path_put()` and then clears the iterator's update_path field.
        if update_path != PATH_IDX_INVALID {
            self.path_put(update_path, intent);
            self.iters[iter_idx].update_path = PATH_IDX_INVALID;
        }

        // The local iterator has no BTREE_ITER_all_snapshots flag; its snapshot
        // filter is always authoritative for normal snapshot-scoped iterators.
        let mut pos = new_pos;
        pos.snapshot = snapshot;
        let mut key = BtreeKey::from_bpos(pos, KeyType::Deleted);
        key.size = 0;
        self.iters[iter_idx].pos = key;

        // Rust's iterator path is transaction-owned. Rebuild the existing path
        // from the root so the node iterator, not only the public position field,
        // points at the new lookup position.
        let vol = self
            .ctx_vol
            .expect("BtreeTrans has no volume for iterator set_pos");
        let root = vol.btree(btree_type).root();
        self.iters[iter_idx].restart(root);
        self.iters[iter_idx].snapshot = snapshot;
    }

    // ── bcachefs 对齐：路径池管理 ──

    /// 初始化新 path — 对应本地 bcachefs `btree_path_init()` (iter.c:2075-2086)。
    fn btree_path_init(&mut self, pos: PathIdx, path_idx: PathIdx) {
        self.paths[path_idx as usize] = Some(Box::new(BtreePath {
            levels: std::array::from_fn(|_| BtreePathNode::None),
            pos: Bpos::ZERO,
            btree_id: BtreeId::Extents,
            level: 0,
            ref_count: 0,
            intent_ref: 0,
            should_be_locked: false,
            locks_want: 0,
            nodes_locked: 0,
            sorted_idx: 0,
            preserve: false,
            cached: false,
        }));

        self.btree_path_list_add(pos, path_idx);
        self.paths_sorted = false;
    }

    /// 增量插入 sorted list — 对应本地 bcachefs `btree_path_list_add()`
    /// (iter.c:3632-3655)。
    fn btree_path_list_add(&mut self, pos: PathIdx, path_idx: PathIdx) {
        let sorted_idx = if pos != PATH_IDX_INVALID {
            self.path_ref(pos).sorted_idx as usize + 1
        } else {
            self.sorted.len()
        };

        self.sorted.insert(sorted_idx, path_idx);
        for i in sorted_idx..self.sorted.len() {
            let idx = self.sorted[i];
            self.path_mut(idx).sorted_idx = i as PathIdx;
        }
    }

    /// 扩容 path pool — 对应本地 bcachefs `btree_paths_realloc()`
    /// (iter.c:2088-2145)。
    fn btree_paths_realloc(&mut self, pos: PathIdx) -> PathIdx {
        if self.nr_paths as usize == BTREE_ITER_MAX {
            return PATH_IDX_INVALID;
        }

        let path_idx = self.nr_paths;
        let nr = self.nr_paths as usize * 2;
        self.paths.resize_with(nr, || None);
        self.paths_allocated
            .resize(nr.div_ceil(u64::BITS as usize), 0);
        self.nr_paths = nr as PathIdx;

        if path_idx > self.nr_paths_max {
            self.nr_paths_max = path_idx;
        }

        let word = path_idx as usize / u64::BITS as usize;
        let bit = path_idx as usize % u64::BITS as usize;
        self.paths_allocated[word] |= 1u64 << bit;
        self.btree_path_init(pos, path_idx);
        path_idx
    }

    /// 分配一个新路径 — 对应本地 bcachefs `btree_path_alloc()`
    /// (iter.c:2147-2177)。
    pub fn path_alloc(&mut self, pos: PathIdx) -> PathIdx {
        let mut path_idx = 0usize;

        for word_idx in 0..self.paths_allocated.len() {
            let word = self.paths_allocated[word_idx];
            if word != u64::MAX {
                let bit = (!word).trailing_zeros() as usize;
                path_idx |= bit;

                if path_idx > self.nr_paths_max as usize {
                    self.nr_paths_max = path_idx as PathIdx;
                }

                self.paths_allocated[word_idx] |= 1u64 << bit;
                self.btree_path_init(pos, path_idx as PathIdx);
                return path_idx as PathIdx;
            }

            path_idx += u64::BITS as usize;
        }

        self.btree_paths_realloc(pos)
    }

    /// 获取指定路径的可变引用
    pub fn path_mut(&mut self, path_idx: PathIdx) -> &mut BtreePath {
        self.paths[path_idx as usize]
            .as_mut()
            .map(Box::as_mut)
            .expect("path_alloc 应确保路径有效")
    }

    /// 获取指定路径的只读引用
    pub fn path_ref(&self, path_idx: PathIdx) -> &BtreePath {
        self.paths[path_idx as usize]
            .as_ref()
            .map(Box::as_ref)
            .expect("path 应在 path_alloc 后有效")
    }

    /// 递增路径的引用计数 — 对应 bcachefs `__btree_path_get()` (iter.c)
    ///
    /// `intent=true` 同时递增 intent_ref，标记该 iter 需要 intent 锁。
    fn __btree_path_get(&mut self, path_idx: PathIdx, intent: bool) {
        assert!((path_idx as usize) < self.nr_paths as usize);
        let word = path_idx as usize / u64::BITS as usize;
        let bit = path_idx as usize % u64::BITS as usize;
        assert_ne!(self.paths_allocated[word] & (1u64 << bit), 0);

        let path = self.path_mut(path_idx);
        assert_ne!(path.ref_count, u8::MAX, "path {path_idx} refcount overflow");
        path.ref_count += 1;
        if intent {
            path.intent_ref += 1;
        }
    }

    pub fn path_get(&mut self, path_idx: PathIdx, intent: bool) {
        self.__btree_path_get(path_idx, intent);
    }

    /// 对应本地 bcachefs `__btree_path_put()` (`iter.h:158-174`)。
    fn __btree_path_put(&mut self, path_idx: PathIdx, intent: bool) -> bool {
        assert!((path_idx as usize) < self.nr_paths as usize);
        let word = path_idx as usize / u64::BITS as usize;
        let bit = path_idx as usize % u64::BITS as usize;
        assert_ne!(self.paths_allocated[word] & (1u64 << bit), 0);

        let path = self.path_mut(path_idx);
        assert_ne!(path.ref_count, 0, "path {path_idx} refcount underflow");
        assert!(
            !intent || path.intent_ref != 0,
            "path {path_idx} intent refcount underflow"
        );
        path.intent_ref -= u8::from(intent);
        path.ref_count -= 1;
        path.ref_count == 0
    }

    /// 递减路径的引用计数 — 对应 bcachefs `bch2_path_put()` (iter.c:1787-1835)
    ///
    /// 当 ref_count 归零时尝试释放路径（将槽位置为 None）。
    /// 若 `should_be_locked=true` 或 `preserve=true`，先尝试迁移锁到相邻路径。
    /// 无 dup 或 relock 失败时保留路径存活。— D1
    pub fn path_put(&mut self, path_idx: PathIdx, intent: bool) {
        if !self.__btree_path_put(path_idx, intent) {
            return;
        }

        // 对应 C iter.c:1794 — preserve 或 should_be_locked 时触发迁移
        let should_migrate = {
            let path = self.path_ref(path_idx);
            path.should_be_locked || path.preserve
        };

        if should_migrate {
            // C iter.c:1794-1835 — 尝试迁移标志位
            // D1: 无 dup 或 relock 失败时返回 false → 保留路径
            if !self.try_migrate_locks(path_idx) {
                return;
            }
        }

        self.__bch2_path_free(path_idx);
    }

    /// 对应本地 bcachefs `__bch2_path_free()` (iter.c:1748-1753)。
    fn __bch2_path_free(&mut self, path_idx: PathIdx) {
        self.__bch2_btree_path_unlock(path_idx);
        self.path_list_remove(path_idx);
        let word = path_idx as usize / u64::BITS as usize;
        let bit = path_idx as usize % u64::BITS as usize;
        self.paths_allocated[word] &= !(1u64 << bit);
        self.paths[path_idx as usize] = None;
    }

    /// 从 sorted[] 中移除路径 — 对应 bcachefs `btree_path_list_remove()` (iter.c:3615-3630)
    ///
    /// 更新 sorted array 和所有受影响路径的 sorted_idx。
    fn path_list_remove(&mut self, path_idx: PathIdx) {
        let sorted_idx = {
            let path = self.path_ref(path_idx);
            path.sorted_idx as usize
        };
        assert!(sorted_idx < self.sorted.len());
        assert_eq!(self.sorted[sorted_idx], path_idx);
        self.sorted.remove(sorted_idx);
        // 更新 sorted_idx
        for i in sorted_idx..self.sorted.len() {
            let idx = self.sorted[i];
            if let Some(ref mut path) = self.paths[idx as usize] {
                path.sorted_idx = i as PathIdx;
            }
        }
    }

    /// 将路径的 should_be_locked/preserve 锁迁移到 sorted 中相邻的路径。
    ///
    /// 对应 bcachefs iter.c:1794-1835。
    /// 返回 true 表示迁移成功（路径可安全释放），
    /// false 表示无 dup 或 relock 失败（保留路径）。— D1/D4
    fn try_migrate_locks(&mut self, path_idx: PathIdx) -> bool {
        // ── 1. 记录源路径信息 ──
        let (btree_id, level, pos, cached, preserve, should_be_locked) = {
            let p = self.path_ref(path_idx);
            (
                p.btree_id,
                p.level,
                p.pos,
                p.cached,
                p.preserve,
                p.should_be_locked,
            )
        };

        let node_ptr = if should_be_locked {
            let p = self.path_ref(path_idx);
            p.btree_path_node(level as usize)
                .and_then(|node| match node {
                    BtreePathNode::Node(level) => Some(Arc::as_ptr(&level.node)),
                    BtreePathNode::None | BtreePathNode::Error(_) => None,
                })
        } else {
            None
        };

        if !self.paths_sorted {
            return false;
        }

        let sorted_pos = {
            let p = self.path_ref(path_idx);
            if p.sorted_idx as usize >= self.sorted.len() {
                return false;
            }
            p.sorted_idx
        };

        // ── 2. 搜索 dup — 前驱和后继 ──
        // C iter.c:1795-1808 — bcachefs 只检查 prev 和 next 相邻路径
        let mut dup_idx = None;

        // 本地 bcachefs iter.c:1795-1808：只检查紧邻的前驱和后继。
        let prev = (sorted_pos != 0).then(|| self.sorted[sorted_pos as usize - 1]);
        let next = self.sorted.get(sorted_pos as usize + 1).copied();

        if let Some(idx) = prev {
            let is_match = self.paths[idx as usize].as_ref().is_some_and(|p| {
                if preserve {
                    p.btree_id == btree_id && p.cached == cached && p.pos == pos && p.level == level
                } else if should_be_locked {
                    p.btree_id == btree_id
                        && p.level == level
                        && p.btree_path_node(level as usize)
                            .is_some_and(|node| match node {
                                BtreePathNode::Node(level) => node_ptr
                                    .is_some_and(|np| Arc::as_ptr(&level.node) as *const _ == np),
                                BtreePathNode::None | BtreePathNode::Error(_) => false,
                            })
                } else {
                    false
                }
            });
            if is_match {
                dup_idx = Some(idx);
            }
        }

        if dup_idx.is_none() {
            if let Some(idx) = next {
                let is_match = self.paths[idx as usize].as_ref().is_some_and(|p| {
                    if preserve {
                        p.btree_id == btree_id
                            && p.cached == cached
                            && p.pos == pos
                            && p.level == level
                    } else if should_be_locked {
                        p.btree_id == btree_id
                            && p.level == level
                            && p.btree_path_node(level as usize)
                                .is_some_and(|node| match node {
                                    BtreePathNode::Node(level) => node_ptr.is_some_and(|np| {
                                        Arc::as_ptr(&level.node) as *const _ == np
                                    }),
                                    BtreePathNode::None | BtreePathNode::Error(_) => false,
                                })
                    } else {
                        false
                    }
                });
                if is_match {
                    dup_idx = Some(idx);
                }
            }
        }

        // C iter.c:1810-1811 — 无 dup，保留路径
        let Some(target_idx) = dup_idx else {
            return false;
        };

        // ── 3. 尝试 relock dup ──
        // C iter.c:1817-1826
        let needs_relock = should_be_locked
            && self.paths[target_idx as usize]
                .as_ref()
                .is_some_and(|p| !p.should_be_locked);

        if needs_relock && !self.needs_restart() {
            let relock_ok = if self.locked {
                self.bch2_btree_path_relock_norestart(target_idx)
            } else {
                self.path_can_relock(target_idx)
            };
            // C iter.c:1822-1823 — relock 失败，保留路径
            if !relock_ok {
                return false;
            }
            // C iter.c:1825
            if let Some(ref mut p) = self.paths[target_idx as usize] {
                p.should_be_locked = true;
            }
        }

        // ── 4. EBUG_ON 检查 ──
        // C iter.c:1828-1831 — should_be_locked 且有 locked trans 时，
        // dup 的对应 level 必须已锁定
        debug_assert!(
            !should_be_locked
                || self.needs_restart()
                || !self.locked
                || self.paths[target_idx as usize]
                    .as_ref()
                    .and_then(|p| p.btree_path_node(level as usize))
                    .is_some_and(|node| match node {
                        BtreePathNode::Node(level) => {
                            level.lock_state != BtreeNodeLockedType::None
                        }
                        BtreePathNode::None | BtreePathNode::Error(_) => false,
                    }),
            "try_migrate_locks: dup path {} level {} not locked after relock",
            target_idx,
            level,
        );

        // ── 5. 标志位转移 ──
        // C iter.c:1833-1834
        if let Some(ref mut path) = self.paths[path_idx as usize] {
            path.should_be_locked = false;
        }
        if let Some(ref mut tp) = self.paths[target_idx as usize] {
            tp.preserve |= preserve;
        }

        true
    }

    /// 对应本地 bcachefs `bch2_btree_path_upgrade_norestart()` 与
    /// `__bch2_btree_path_upgrade_norestart()`
    /// (`locking.h:601-608`, `locking.c:1290-1307`)。
    ///
    /// 追加 btree_node_lock_increment 回退：当 tryupgrade/relock 失败时，
    /// 检查事务内其他路径是否已持有同一节点的 INTENT 锁，通过
    /// `six_lock_increment()` 重入获取（对应 `locking.c:1230-1234`）。
    fn bch2_btree_path_upgrade_norestart(&mut self, path_idx: PathIdx, new_locks_want: u8) -> bool {
        if new_locks_want <= self.path_ref(path_idx).locks_want {
            return true;
        }

        self.path_mut(path_idx).locks_want = new_locks_want;
        let level = self.path_ref(path_idx).level;
        let mut failed = false;
        for l in level..new_locks_want {
            let (node, lock_state, locked_seq) = match &self.path_ref(path_idx).levels[l as usize] {
                BtreePathNode::Node(level) => {
                    (Arc::clone(&level.node), level.lock_state, level.locked_seq)
                }
                BtreePathNode::None | BtreePathNode::Error(_) => {
                    failed = true;
                    break;
                }
            };

            let l_usize = l as usize;
            let mut locked = match lock_state {
                BtreeNodeLockedType::Intent | BtreeNodeLockedType::Write => true,
                BtreeNodeLockedType::Read => node.lock.six_lock_tryupgrade(),
                BtreeNodeLockedType::None => node.lock.six_relock_intent(locked_seq),
            };

            // ── 回退：btree_node_lock_increment（对应 locking.c:1230-1234） ──
            // C 的 bch2_btree_node_upgrade 中：tryupgrade/relock 失败后，
            // 如果 lock_seq 匹配且其他路径已持有 INTENT，通过 increment 重入，
            // 然后释放当前路径的锁（如果原本持有 read 锁）。
            if !locked
                && self.btree_node_lock_seq_matches(path_idx, l_usize)
                && self.btree_node_lock_increment(path_idx, l_usize, BtreeNodeLockedType::Intent)
            {
                // C: btree_node_unlock(trans, path, level);
                // 释放当前路径在此层级的锁（read 或 none）
                if lock_state.is_locked() {
                    self.btree_node_unlock(path_idx, l_usize);
                }
                locked = true;
            }

            if !locked {
                failed = true;
                break;
            }

            let path = self.path_mut(path_idx);
            if let BtreePathNode::Node(level) = &mut path.levels[l_usize] {
                level.lock_state = BtreeNodeLockedType::Intent;
            }
            path.mark_btree_node_locked_noreset(l_usize, BtreeNodeLockedType::Intent);
        }

        !failed || !self.path_ref(path_idx).should_be_locked
    }

    /// 尝试重锁路径的所有层级，不触发 restart。
    /// 返回 true 表示所有层级成功重锁。
    ///
    /// 对应 C: locking.c:1249-1254 `bch2_btree_path_relock_norestart`
    /// （内部调用 `btree_path_get_locks` → `__bch2_btree_node_relock`）。
    ///
    /// 追加 `btree_node_lock_increment` 回退：当 `six_relock_type` 失败时，
    /// 检查事务内其他路径是否已持有同一节点的同级别锁，通过
    /// `six_lock_increment()` 重入获取（对应 `locking.c:1146-1151`）。
    fn bch2_btree_path_relock_norestart(&mut self, path_idx: PathIdx) -> bool {
        let (level, locks_want) = {
            let p = self.path_ref(path_idx);
            (p.level, p.locks_want)
        };
        let mut l = level;
        loop {
            let l_usize = l as usize;
            let want = if l < locks_want {
                BtreeNodeLockedType::Intent
            } else {
                BtreeNodeLockedType::Read
            };

            let ok = {
                let p = self.path_ref(path_idx);
                match &p.levels[l_usize] {
                    BtreePathNode::Node(level) if l < locks_want => {
                        level.node.lock.six_relock_intent(level.locked_seq)
                    }
                    BtreePathNode::Node(level) => level.node.lock.six_relock_read(level.locked_seq),
                    BtreePathNode::None => break,
                    BtreePathNode::Error(_) => false,
                }
            };

            // ── 回退：btree_node_lock_increment（对应 locking.c:1146-1151） ──
            // C 的 __bch2_btree_node_relock 中：
            //   if (six_relock_type(...) ||
            //       (btree_node_lock_seq_matches(...) &&
            //        btree_node_lock_increment(...)))
            //       mark_btree_node_locked(...); return true;
            let mut relocked = ok;
            if !relocked
                && self.btree_node_lock_seq_matches(path_idx, l_usize)
                && self.btree_node_lock_increment(path_idx, l_usize, want)
            {
                relocked = true;
            }

            if !relocked {
                if self.path_ref(path_idx).should_be_locked && !self.needs_restart {
                    return false;
                }

                self.__bch2_btree_path_unlock(path_idx);
                for failed_level in 0..=l_usize {
                    self.path_mut(path_idx).levels[failed_level] =
                        BtreePathNode::Error(BtreePathError::Relock);
                }
                return false;
            }
            let path = self.path_mut(path_idx);
            if let BtreePathNode::Node(level) = &mut path.levels[l_usize] {
                level.lock_state = want;
            }
            path.mark_btree_node_locked_noreset(l_usize, want);
            l += 1;
            if l >= locks_want {
                break;
            }
        }
        true
    }

    /// 对应本地 bcachefs `btree_node_unlock()` (`locking.h:373-387`)。
    fn btree_node_unlock(&mut self, path_idx: PathIdx, level_idx: usize) {
        let lock_type = self.path_ref(path_idx).btree_node_locked_type(level_idx);
        if lock_type == BtreeNodeLockedType::None {
            return;
        }
        let BtreePathNode::Node(level) = &self.path_ref(path_idx).levels[level_idx] else {
            panic!("locked path has no node at level {level_idx}");
        };
        let node = Arc::clone(&level.node);

        let mut lock_type = lock_type;
        if lock_type == BtreeNodeLockedType::Write {
            node.lock.six_unlock_write();
            let locked_seq = node.lock.six_lock_seq();
            for linked in self.paths.iter_mut().flatten() {
                if let BtreePathNode::Node(linked_level) = &mut linked.levels[level_idx] {
                    if Arc::ptr_eq(&linked_level.node, &node) {
                        linked_level.locked_seq = locked_seq;
                    }
                }
            }
            lock_type = BtreeNodeLockedType::Intent;
        }

        match lock_type {
            BtreeNodeLockedType::Read => node.lock.six_unlock_read(),
            BtreeNodeLockedType::Intent => node.lock.six_unlock_intent(),
            BtreeNodeLockedType::None | BtreeNodeLockedType::Write => unreachable!(),
        }

        let path = self.path_mut(path_idx);
        let BtreePathNode::Node(level) = &mut path.levels[level_idx] else {
            panic!("locked path has no node at level {level_idx}");
        };
        level.lock_state = BtreeNodeLockedType::None;
        path.mark_btree_node_locked_noreset(level_idx, BtreeNodeLockedType::None);
    }

    /// 重入锁递增 — 对应本地 bcachefs `btree_node_lock_increment()`
    /// (`locking.c:873-889`)。
    ///
    /// 遍历事务所有路径，检查是否有其他路径在指定层级和节点上
    /// 持有 >= want 级别的锁。如果有，通过 `six_lock_increment()`
    /// 递增引用计数（重入），避免当前路径重复等待阻塞。
    ///
    /// 典型场景：btree split 时 parent 路径已锁住节点 A（INTENT），
    /// child 路径也需要节点 A 同一层级的锁 — 通过重入获取相同的 INTENT 锁。
    fn btree_node_lock_increment(
        &self,
        path_idx: PathIdx,
        level: usize,
        want: BtreeNodeLockedType,
    ) -> bool {
        let node = match &self.path_ref(path_idx).levels[level] {
            BtreePathNode::Node(lvl) => Arc::clone(&lvl.node),
            _ => return false,
        };

        // 遍历所有路径，查找同一节点的更高类型锁
        for (idx, slot) in self.paths.iter().enumerate() {
            if idx as PathIdx == path_idx {
                continue;
            }
            let Some(other) = slot.as_ref() else {
                continue;
            };
            let other_lvl = match &other.levels[level] {
                BtreePathNode::Node(lvl) => lvl,
                _ => continue,
            };

            if !Arc::ptr_eq(&other_lvl.node, &node) {
                continue;
            }
            if other.btree_node_locked_type(level) as u8 >= want as u8 {
                let lock_type = match want {
                    BtreeNodeLockedType::Read => SixLockType::Read,
                    BtreeNodeLockedType::Intent => SixLockType::Intent,
                    BtreeNodeLockedType::Write => SixLockType::Write,
                    BtreeNodeLockedType::None => return false,
                };
                node.lock.six_lock_increment(lock_type);
                return true;
            }
        }
        false
    }

    /// 检查锁序列是否匹配 — 对应本地 bcachefs `btree_node_lock_seq_matches()`
    /// (`iter.h:201-205`)。
    fn btree_node_lock_seq_matches(&self, path_idx: PathIdx, level: usize) -> bool {
        match &self.path_ref(path_idx).levels[level] {
            BtreePathNode::Node(lvl) => lvl.node.lock.six_lock_seq() == lvl.locked_seq,
            _ => false,
        }
    }

    /// 对应本地 bcachefs `__bch2_btree_path_unlock()`
    /// (`locking.h:389-394`)。
    fn __bch2_btree_path_unlock(&mut self, path_idx: PathIdx) {
        while self.path_ref(path_idx).nodes_locked != 0 {
            let level_idx = self.path_ref(path_idx).nodes_locked.trailing_zeros() as usize >> 1;
            self.btree_node_unlock(path_idx, level_idx);
        }
    }

    /// 检查路径的所有层级是否可重锁（locked_seq 未变）。
    /// 返回 true 表示所有层级在 unlock 后未被修改。
    /// 对应 C: iter.c:1757-1775 `bch2_btree_path_can_relock`
    fn path_can_relock(&self, path_idx: PathIdx) -> bool {
        let (level, locks_want) = {
            let p = self.path_ref(path_idx);
            (p.level, p.locks_want)
        };
        let mut l = level;
        while l < locks_want {
            let ok = {
                let p = self.path_ref(path_idx);
                let lvl = match &p.levels[l as usize] {
                    BtreePathNode::Node(level) => level,
                    BtreePathNode::None => break,
                    BtreePathNode::Error(_) => return false,
                };
                lvl.lock_state == BtreeNodeLockedType::None
                    || lvl.node.lock.six_lock_seq() == lvl.locked_seq
            };
            if !ok {
                return false;
            }
            l += 1;
        }
        true
    }

    /// 显式设置路径的 should_be_locked — 对应 bcachefs `btree_path_set_should_be_locked()` (locking.h:626-639)
    ///
    /// 在成功获取路径的所有锁后调用，标记此路径在 unlock+relock 周期中需要重锁。
    pub fn btree_path_set_should_be_locked(&mut self, path_idx: PathIdx) {
        if let Some(ref path) = self.paths[path_idx as usize] {
            // C locking.h:628 — EBUG_ON(!btree_node_locked(path, path->level))
            // D3: 检查特定层级而非 nodes_locked != 0
            debug_assert!(
                path.btree_path_node(path.level as usize)
                    .is_some_and(|node| match node {
                        BtreePathNode::Node(level) => {
                            level.lock_state != BtreeNodeLockedType::None
                        }
                        BtreePathNode::None | BtreePathNode::Error(_) => false,
                    }),
                "btree_path_set_should_be_locked: path {} level {} not locked",
                path_idx,
                path.level,
            );
            if let Some(ref mut p) = self.paths[path_idx as usize] {
                p.should_be_locked = true;
            }
        }
    }

    /// 设置 iter 所引用 path 的 should_be_locked。
    pub fn iter_set_should_be_locked(&mut self, iter_idx: usize, value: bool) {
        let Some(iter) = self.iters.get(iter_idx) else {
            return;
        };
        self.path_mut(iter.path).should_be_locked = value;
    }

    /// 对应本地 bcachefs `bch2_btree_node_relock()` (`locking.h:273-292`)。
    fn bch2_btree_node_relock(&mut self, path_idx: PathIdx, level_idx: usize) -> bool {
        let lock_type = self.path_ref(path_idx).btree_node_locked_type(level_idx);
        if lock_type != BtreeNodeLockedType::None {
            return true;
        }

        let (node, locked_seq, want_intent) = match &self.path_ref(path_idx).levels[level_idx] {
            BtreePathNode::Node(level) => (
                Arc::clone(&level.node),
                level.locked_seq,
                level_idx < self.path_ref(path_idx).locks_want as usize,
            ),
            BtreePathNode::None | BtreePathNode::Error(_) => return false,
        };
        let want = if want_intent {
            BtreeNodeLockedType::Intent
        } else {
            BtreeNodeLockedType::Read
        };

        let relocked = if want_intent {
            node.lock.six_relock_intent(locked_seq)
        } else {
            node.lock.six_relock_read(locked_seq)
        };

        // ── 回退：btree_node_lock_increment（对应 locking.c:1146-1151） ──
        let mut success = relocked;
        if !success
            && self.btree_node_lock_seq_matches(path_idx, level_idx)
            && self.btree_node_lock_increment(path_idx, level_idx, want)
        {
            success = true;
        }

        if !success {
            return false;
        }

        let path = self.path_mut(path_idx);
        if let BtreePathNode::Node(level) = &mut path.levels[level_idx] {
            level.lock_state = want;
        }
        path.mark_btree_node_locked_noreset(level_idx, want);
        true
    }

    /// 对应本地 bcachefs `__btree_path_set_level_up()`
    /// (`locking.h:642-648`)。
    fn __btree_path_set_level_up(&mut self, path_idx: PathIdx, level_idx: usize) {
        self.btree_node_unlock(path_idx, level_idx);
        self.path_mut(path_idx).levels[level_idx] = BtreePathNode::Error(BtreePathError::Up);
    }

    /// 对应本地 bcachefs `btree_path_up_until_good_node()`
    /// (`iter.c:1360-1395`)；当前调用的 `check_pos` 固定为 0。
    fn btree_path_up_until_good_node(&mut self, path_idx: PathIdx, _check_pos: i32) -> u8 {
        let mut level = self.path_ref(path_idx).level as usize;

        'again: loop {
            while level < BTREE_MAX_DEPTH
                && !matches!(self.path_ref(path_idx).levels[level], BtreePathNode::None)
                && !self.bch2_btree_node_relock(path_idx, level)
            {
                self.__btree_path_set_level_up(path_idx, level);
                level += 1;
            }

            let locks_want = self.path_ref(path_idx).locks_want as usize;
            let mut i = level + 1;
            while i < locks_want
                && i < BTREE_MAX_DEPTH
                && !matches!(self.path_ref(path_idx).levels[i], BtreePathNode::None)
            {
                if !self.bch2_btree_node_relock(path_idx, i) {
                    while level <= i {
                        self.__btree_path_set_level_up(path_idx, level);
                        level += 1;
                    }
                    continue 'again;
                }
                i += 1;
            }
            return level as u8;
        }
    }

    /// 对应本地 bcachefs `bch2_btree_path_level_init()`。
    fn bch2_btree_path_level_init(
        &mut self,
        path_idx: PathIdx,
        level_idx: usize,
        node: Arc<BtreeNode>,
    ) {
        let pos = self.path_ref(path_idx).pos;
        let mut node_iter = BtreeNodeIter::default();
        bch2_btree_node_iter_init(&mut node_iter, &node, &pos);

        let mut offset = 0u16;
        if bch2_btree_node_iter_peek(&mut node_iter, &node).is_some() {
            let wanted = node_iter.data[0].k;
            let mut scan = BtreeNodeIter::default();
            bch2_btree_node_iter_init_from_start(&mut scan, &node);
            let mut index = 1u16;
            while bch2_btree_node_iter_peek(&mut scan, &node).is_some() {
                if scan.data[0].k == wanted {
                    offset = index;
                    break;
                }
                index += 1;
                bch2_btree_node_iter_advance(&mut scan, &node);
            }
        }

        let mut path_level = BtreePathLevel::new(node);
        path_level.offset = offset;
        path_level.iter = node_iter;
        path_level.locked_seq = path_level.node.lock.six_lock_seq();
        self.path_mut(path_idx).levels[level_idx] = BtreePathNode::Node(path_level);
    }

    /// 对应本地 bcachefs `btree_path_lock_root()` (`iter.c:930-1001`)。
    /// 返回 1 表示 root 深度低于调用者要求的深度。
    fn btree_path_lock_root(
        &mut self,
        path_idx: PathIdx,
        depth_want: u8,
        root: &BtreeRoot,
    ) -> Result<i32, BtreePathTraverseError> {
        debug_assert_eq!(self.path_ref(path_idx).nodes_locked, 0);

        self.path_mut(path_idx).level = root.depth;
        if root.depth < depth_want {
            let path = self.path_mut(path_idx);
            path.level = depth_want;
            for level in depth_want as usize..BTREE_MAX_DEPTH {
                path.levels[level] = BtreePathNode::None;
            }
            return Ok(1);
        }

        let level_idx = root.depth as usize;
        let want_intent = level_idx < self.path_ref(path_idx).locks_want as usize;
        let locked = if want_intent {
            root.node.lock.six_lock_intent()
        } else {
            root.node.lock.six_lock_read()
        };
        if !locked {
            self.needs_restart = true;
            self.restart_reason = Some(RestartReason::LockConflict);
            return Err(BtreePathTraverseError::Restart(RestartReason::LockConflict));
        }

        {
            let path = self.path_mut(path_idx);
            for level in 0..level_idx {
                path.levels[level] = BtreePathNode::Error(BtreePathError::LockRoot);
            }
            for level in level_idx + 1..BTREE_MAX_DEPTH {
                path.levels[level] = BtreePathNode::None;
            }
        }
        self.bch2_btree_path_level_init(path_idx, level_idx, Arc::clone(&root.node));
        let lock_type = if want_intent {
            BtreeNodeLockedType::Intent
        } else {
            BtreeNodeLockedType::Read
        };
        let path = self.path_mut(path_idx);
        if let BtreePathNode::Node(level) = &mut path.levels[level_idx] {
            level.lock_state = lock_type;
            level.block_addr = ROOT_CACHE_ADDR;
        }
        path.mark_btree_node_locked_noreset(level_idx, lock_type);
        Ok(0)
    }

    /// 对应本地 bcachefs `btree_path_down()` (`iter.c:1216-1260`)。
    fn btree_path_down(
        &mut self,
        path_idx: PathIdx,
        _flags: IterFlags,
    ) -> Result<i32, BtreePathTraverseError> {
        let parent_level = self.path_ref(path_idx).level as usize;
        debug_assert_ne!(
            self.path_ref(path_idx).btree_node_locked_type(parent_level),
            BtreeNodeLockedType::None
        );

        let (parent, mut node_iter, child_idx) = match &self.path_ref(path_idx).levels[parent_level]
        {
            BtreePathNode::Node(level) => {
                (Arc::clone(&level.node), level.iter.clone(), level.offset)
            }
            BtreePathNode::None | BtreePathNode::Error(_) => unreachable!(),
        };
        let Some(raw) = bch2_btree_node_iter_peek(&mut node_iter, &parent) else {
            return Err(BtreePathTraverseError::Storage(StorageError::InvalidData(
                "btree parent contains no child pointer".into(),
            )));
        };
        let raw_offset = raw.as_ptr() as usize - parent.data.as_ptr() as usize;
        let (key, value) = parent.read_packed_entry(raw_offset);
        if self.path_ref(path_idx).pos > key.to_bpos() {
            return Err(BtreePathTraverseError::Storage(StorageError::InvalidData(
                "btree child pointer does not cover lookup position".into(),
            )));
        }

        let level_idx = parent_level - 1;
        let want_intent = level_idx < self.path_ref(path_idx).locks_want as usize;
        if self.path_ref(path_idx).btree_node_locked_type(parent_level) == BtreeNodeLockedType::Read
        {
            self.btree_node_unlock(path_idx, parent_level);
        }

        let child_addr = value.paddr;
        let cache = self.cache_for(self.path_ref(path_idx).btree_id);
        let child = cache.get_or_create(child_addr, level_idx as u8);
        let locked = if want_intent {
            child.lock.six_lock_intent()
        } else {
            child.lock.six_lock_read()
        };
        if !locked {
            self.needs_restart = true;
            self.restart_reason = Some(RestartReason::LockConflict);
            return Err(BtreePathTraverseError::Restart(RestartReason::LockConflict));
        }

        self.path_mut(path_idx).level = level_idx as u8;
        self.bch2_btree_path_level_init(path_idx, level_idx, child);
        let lock_type = if want_intent {
            BtreeNodeLockedType::Intent
        } else {
            BtreeNodeLockedType::Read
        };
        let path = self.path_mut(path_idx);
        if let BtreePathNode::Node(level) = &mut path.levels[level_idx] {
            level.lock_state = lock_type;
            level.block_addr = child_addr;
            level.child_idx = child_idx;
        }
        path.mark_btree_node_locked_noreset(level_idx, lock_type);
        Ok(0)
    }

    /// 对应本地 bcachefs `bch2_btree_path_traverse_one()`
    /// (`iter.c:1490-1590`)。
    pub fn bch2_btree_path_traverse_one(
        &mut self,
        path_idx: PathIdx,
        flags: IterFlags,
    ) -> Result<(), BtreePathTraverseError> {
        let depth_want = self.path_ref(path_idx).level;
        let fallback_root = self
            .path_ref(path_idx)
            .levels
            .iter()
            .enumerate()
            .rev()
            .find_map(|(level, node)| match node {
                BtreePathNode::Node(level_node) => {
                    Some((level as u8, Arc::clone(&level_node.node)))
                }
                BtreePathNode::None | BtreePathNode::Error(_) => None,
            });

        if self.needs_restart {
            return Err(BtreePathTraverseError::Restart(
                self.restart_reason.unwrap_or(RestartReason::TraverseAll),
            ));
        }

        if !self.srcu_held {
            self.bch2_trans_srcu_lock();
        }

        if self.bch2_btree_path_relock_norestart(path_idx) {
            return Ok(());
        }

        if self.path_ref(path_idx).should_be_locked {
            self.needs_restart = true;
            self.restart_reason = Some(RestartReason::RelockPath);
            return Err(BtreePathTraverseError::Restart(RestartReason::RelockPath));
        }

        if self.path_ref(path_idx).cached {
            self.__bch2_btree_path_unlock(path_idx);
            self.path_mut(path_idx).levels[depth_want as usize] =
                BtreePathNode::Error(BtreePathError::Cached);
            return Err(BtreePathTraverseError::Storage(StorageError::InvalidData(
                "cached btree path has no btree_bkey_cached representation".into(),
            )));
        }

        if depth_want as usize >= BTREE_MAX_DEPTH {
            return Ok(());
        }

        let btree_type = self.path_ref(path_idx).btree_id;

        let root = if let Some(vol) = self.ctx_vol {
            let (root, _) = vol.btree(btree_type).root_and_cache();
            root.clone()
        } else {
            let Some((depth, root_node)) = fallback_root else {
                return Err(BtreePathTraverseError::Storage(StorageError::NotFound(
                    "btree root".into(),
                )));
            };
            BtreeRoot {
                node: root_node,
                depth,
            }
        };

        let level = self.btree_path_up_until_good_node(path_idx, 0);
        self.path_mut(path_idx).level = level;
        let max_level = level;

        while self.path_ref(path_idx).level > depth_want {
            let current_level = self.path_ref(path_idx).level as usize;
            let ret = if current_level < BTREE_MAX_DEPTH
                && matches!(
                    self.path_ref(path_idx).levels[current_level],
                    BtreePathNode::Node(_)
                ) {
                self.btree_path_down(path_idx, flags)
            } else {
                self.btree_path_lock_root(path_idx, depth_want, &root)
            };

            match ret {
                Ok(0) => {}
                Ok(1) => return Ok(()),
                Ok(_) => unreachable!(),
                Err(err) => {
                    self.__bch2_btree_path_unlock(path_idx);
                    let path = self.path_mut(path_idx);
                    path.level = depth_want;
                    path.levels[depth_want as usize] = BtreePathNode::Error(BtreePathError::Down);
                    return Err(err);
                }
            }
        }

        if max_level > self.path_ref(path_idx).level {
            let level = self.path_ref(path_idx).level as usize;
            let node = match &self.path_ref(path_idx).levels[level] {
                BtreePathNode::Node(path_level) => Arc::clone(&path_level.node),
                BtreePathNode::None | BtreePathNode::Error(_) => return Ok(()),
            };
            let copied: Vec<BtreePathNode> = ((level + 1)..max_level as usize)
                .map(|copy_level| self.path_ref(path_idx).levels[copy_level].clone())
                .collect();
            for linked in self.paths.iter_mut().flatten() {
                let same_node = matches!(
                    &linked.levels[level],
                    BtreePathNode::Node(linked_level)
                        if Arc::ptr_eq(&linked_level.node, &node)
                );
                if !same_node || linked.pos < node.min_key || linked.pos > node.max_key {
                    continue;
                }
                for (copy_level, source) in ((level + 1)..max_level as usize).zip(copied.iter()) {
                    linked.levels[copy_level] = source.clone();
                }
            }
        }

        Ok(())
    }

    /// 重新遍历所有 transaction paths（重启后调用）。
    ///
    /// 对应 bcachefs `bch2_btree_path_traverse_all()` (iter.c:1264-1340)。
    /// 在 `bch2_trans_unlock()` + `bch2_trans_begin()` 之后调用，为每个 iter 重新建立
    /// 从 root 到 leaf 的完整路径并获取读锁（或 intent 锁）。
    ///
    /// 前提：iter.path[0].node 必须有效（root 节点 Arc 在 unlock 后仍存活）。
    fn bch2_btree_path_traverse_all(&mut self) -> Result<(), BtreePathTraverseError> {
        if self.in_traverse_all {
            return Err(BtreePathTraverseError::Restart(
                RestartReason::InTraverseAll,
            ));
        }

        self.in_traverse_all = true;

        // 对应 C iter.c:1282 — retry_all: 标签
        'retry_all: loop {
            // C iter.c:1283-1284 — retry_all 入口消费本次 restart 状态。
            self.needs_restart = false;
            self.restart_reason = None;

            // C iter.c:1286-1287 — 清除所有路径的 should_be_locked
            for slot in self.paths.iter_mut() {
                if let Some(ref mut path) = slot {
                    path.should_be_locked = false;
                }
            }

            // C iter.c:1289 — 排序（死锁预防）
            self.btree_trans_sort_paths();

            // C iter.c:1291-1292 — 排序完成后统一解锁，再按排序顺序重遍历。
            self.bch2_trans_unlock();
            self.trans_set_locked(false);

            // 对应本地 iter.c:1294-1301：内存分配失败时等待并
            // 获取 btree cache cannibalize lock，然后再开始有序遍历。
            if self.memory_allocation_failure {
                let cache = self
                    .cache
                    .clone()
                    .or_else(|| self.iters.first().map(|iter| Arc::clone(&iter.cache)));
                if let Some(cache) = cache {
                    while cache.cache().bch2_btree_cache_cannibalize_lock() {
                        std::thread::yield_now();
                    }
                    self.btree_cache_cannibalize_locked = true;
                }
            }

            // C iter.c:1305-1327：必须按动态 sorted 长度遍历；遍历可插入新 path。
            let mut sorted_pos = 0usize;
            while sorted_pos < self.sorted.len() {
                let path_idx = self.sorted[sorted_pos];
                let should_traverse = {
                    let path = self.path_ref(path_idx);
                    path.nodes_locked == 0
                        && !matches!(path.levels[path.level as usize], BtreePathNode::None)
                };
                if !should_traverse {
                    sorted_pos += 1;
                    continue;
                }

                self.__btree_path_get(path_idx, false);
                let ret = self.bch2_btree_path_traverse_one(path_idx, IterFlags::default());
                self.__btree_path_put(path_idx, false);

                match ret {
                    Ok(()) => {}
                    Err(
                        BtreePathTraverseError::Restart(_) | BtreePathTraverseError::OutOfMemory,
                    ) => continue 'retry_all,
                    Err(err) => {
                        if self.btree_cache_cannibalize_locked {
                            if let Some(cache) = self
                                .cache
                                .clone()
                                .or_else(|| self.iters.first().map(|iter| Arc::clone(&iter.cache)))
                            {
                                cache.cache().bch2_btree_cache_cannibalize_unlock();
                            }
                            self.btree_cache_cannibalize_locked = false;
                        }
                        self.in_traverse_all = false;
                        return Err(err);
                    }
                }
            }

            break;
        }

        if self.btree_cache_cannibalize_locked {
            if let Some(cache) = self
                .cache
                .clone()
                .or_else(|| self.iters.first().map(|iter| Arc::clone(&iter.cache)))
            {
                cache.cache().bch2_btree_cache_cannibalize_unlock();
            }
            self.btree_cache_cannibalize_locked = false;
        }
        self.in_traverse_all = false;
        Ok(())
    }

    // ── bcachefs 对齐：路径迭代 API（iter.h:242-346）──

    /// 遍历已分配的路径索引 — 对应 `trans_for_each_path_idx_from` (iter.h:242-245)
    ///
    /// 使用 `paths_allocated` 位图高效跳过空闲槽位。
    pub fn path_idx_iter(&self) -> PathBitmapIter<'_> {
        PathBitmapIter::new(&self.paths_allocated, self.nr_paths as usize, 1)
    }

    /// 从指定索引开始遍历路径索引
    pub fn path_idx_iter_from(&self, start: PathIdx) -> PathBitmapIter<'_> {
        PathBitmapIter::new(
            &self.paths_allocated,
            self.nr_paths as usize,
            start as usize,
        )
    }

    /// 遍历所有已分配路径的引用 — 对应 `trans_for_each_path` (iter.h:280)
    pub fn iter_paths(&self) -> impl Iterator<Item = (PathIdx, &BtreePath)> + '_ {
        self.paths
            .iter()
            .enumerate()
            .filter_map(|(idx, slot)| slot.as_ref().map(|p| (idx as PathIdx, p.as_ref())))
    }

    /// 遍历所有已分配路径的可变引用
    pub fn iter_paths_mut(&mut self) -> impl Iterator<Item = (PathIdx, &mut BtreePath)> + '_ {
        self.paths
            .iter_mut()
            .enumerate()
            .filter_map(|(idx, slot)| slot.as_mut().map(|p| (idx as PathIdx, p.as_mut())))
    }

    /// 按 sorted 顺序遍历路径 — 对应 `trans_for_each_path_inorder` (iter.h:314)
    ///
    /// 调用前需确保 `btree_trans_sort_paths()` 已执行。
    pub fn iter_paths_inorder(&self) -> impl Iterator<Item = (PathIdx, &BtreePath)> + '_ {
        self.sorted
            .iter()
            .filter_map(|&idx| self.paths[idx as usize].as_ref().map(|p| (idx, p.as_ref())))
    }

    /// 逆序遍历路径 — 对应 `trans_for_each_path_inorder_reverse` (iter.h:321)
    pub fn iter_paths_inorder_reverse(&self) -> impl Iterator<Item = (PathIdx, &BtreePath)> + '_ {
        self.sorted
            .iter()
            .rev()
            .filter_map(|&idx| self.paths[idx as usize].as_ref().map(|p| (idx, p.as_ref())))
    }

    /// 遍历带有指定 btree_id 的路径 — 对应 `trans_for_each_path_with_node` (iter.h:346)
    pub fn iter_paths_with_node(
        &self,
        btree_id: BtreeId,
    ) -> impl Iterator<Item = (PathIdx, &BtreePath)> + '_ {
        self.paths
            .iter()
            .enumerate()
            .filter_map(move |(idx, slot)| {
                slot.as_ref()
                    .filter(|p| {
                        p.btree_id == btree_id
                            && p.levels
                                .iter()
                                .any(|level| matches!(level, BtreePathNode::Node(_)))
                    })
                    .map(|p| (idx as PathIdx, p.as_ref()))
            })
    }

    /// 获取指定 iter 的 btree type
    pub fn iter_type(&self, idx: usize) -> BtreeId {
        self.iter_types
            .get(idx)
            .copied()
            .unwrap_or(BtreeId::Extents)
    }

    /// 提交事务 — 对应 bcachefs `__bch2_trans_commit()` (commit.c:1381-1523)
    ///
    /// ## bcachefs 对齐的入口流程
    ///
    /// ### Pre-loop 检查（`__bch2_trans_commit` line 1387-1487）
    /// 1. `bch2_trans_verify_not_unlocked_or_in_restart` — 验证事务状态
    /// 2. `trans_maybe_inject_restart` — 故障注入（跳过）
    /// 3. `bch2_trans_has_updates` — 无更新则快速返回（line 1394-1395）
    /// 4. Watermark throttle（line 1397-1403）
    /// 5. `bch2_trans_commit_run_triggers` — 事务性触发器（line 1405-1407）
    ///
    /// ### Retry 循环（`retry:` label at line 1490）
    /// ```text
    /// retry:
    ///   do_bch2_trans_commit()  // 写锁 + 原子触发器 + 键插入
    ///   if (ret) goto err
    ///   goto out
    /// err:
    ///   bch2_trans_commit_error()
    ///   if (ret) goto out
    ///   goto retry
    /// out_reset:
    ///   downgrade + reset_updates
    /// ```
    ///
    /// ### 三阶段触发器
    /// - `Transactional` — 在 retry 循环内（锁获取后）执行，失败可回滚触发重启
    /// - `Atomic` — committed 标记后执行，失败传播错误（不可回滚）
    /// - `Gc` — committed 标记后执行，错误仅日志记录（best-effort）
    ///
    /// 上下文通过 `self.ctx_vol` 访问，调用前必须先初始化 vol 绑定。
    /// 若未设置 vol，仅执行锁管理，跳过触发器管线。

    /// 获取 journal 保留空间 — 对应 bcachefs `bch2_trans_journal_res_get` (commit.c:49-70)
    ///
    /// 封装 `journal.bch2_journal_res_get`，自动注入事务的水位线。
    /// 在事务没有 vol 的情况下返回 `None`。
    pub fn bch2_trans_journal_res_get(&self, req_u64s: u32) -> Result<JournalRes, StorageError> {
        let Some(vol) = self.ctx_vol else {
            return Err(StorageError::Transaction("no vol".into()));
        };
        let journal = vol.journal_ref();
        journal
            .bch2_journal_res_get(self.watermark, req_u64s)
            .map_err(|e| StorageError::JournalError(e.to_string()))
    }

    pub fn __bch2_trans_commit(&mut self) -> Result<(), StorageError> {
        let saved_restart_count = self.restart_count;

        // ── Pre-loop: has_updates 检查 ──
        // bcachefs line 1394-1395: if (!bch2_trans_has_updates(trans)) goto out_reset
        // 无 journal 条目且无 iter → 空事务，直接返回 Ok（无需 commit）
        if self.journal.is_empty() && self.iters.is_empty() {
            return Ok(());
        }

        // ── Pre-loop: Reclaim 水位线检查 ──
        // bcachefs interior.c:1432-1442: watermark < BCH_WATERMARK_reclaim
        // 当操作水位线 >= Reclaim，不应在 commit 路径中阻塞等待——否则自死锁。
        // Reclaim=5, InteriorUpdate=6 — 数字越大越接近 reclaim
        let is_reclaim = self.watermark.to_bits() >= Watermark::Reclaim.to_bits();

        // ── Pre-loop: Transactional 阶段触发器 ──
        // bcachefs 在进入 retry label 之前运行 transactional triggers，
        // 因此它们不会因为后续 lock retry 被重复执行。
        //
        // 注意：这里仍然允许触发器设置 needs_restart；只要它发生在锁竞争
        // 之前，就按 bcachefs 的 restart 语义处理一次，然后再进入锁重试。
        self.bch2_trans_commit_run_triggers()?;

        if self.needs_restart {
            if is_reclaim {
                self.restart_count += 1;
                return Err(StorageError::TransactionRestartLimit(
                    self.restart_count.into(),
                ));
            }
            if self.bch2_trans_begin() > MAX_RESTARTS {
                return Err(StorageError::TransactionRestartLimit(
                    self.restart_count.into(),
                ));
            }
        }

        // ── Pre-loop: 提交钩子 ──
        // 对应 bcachefs `run_hooks` (commit.c:215-222)，在 write_locked 之前执行。
        // 钩子注入的额外操作（如 open bucket 日志）在写锁获取前完成。
        if !self.commit_hooks.is_empty() {
            self.run_commit_hooks()?;
        }

        // ── Main retry loop ──
        // 对应 bcachefs `retry:` label（commit.c:1490）
        loop {
            // ── Phase 0a: Reclaim bail ──
            // 如果获得锁前就需要重启，reclaim 操作直接失败（避免 reclaim 死锁）
            if is_reclaim && self.needs_restart {
                self.restart_count += 1;
                return Err(StorageError::TransactionRestartLimit(
                    self.restart_count.into(),
                ));
            }

            // ── Phase 0b: 按 journal 自然顺序获取写锁 ──
            // 对应 bcachefs `bch2_trans_lock_write_inlined()` (commit.c:141-159)
            // + 路径升级检查 (commit.c:1432-1436)
            //
            self.btree_trans_sort_paths();
            self.try_lock_all();

            if !self.needs_restart {
                self.bch2_trans_record_locked_seqs();
            }

            if self.needs_restart {
                if is_reclaim {
                    self.restart_count += 1;
                    return Err(StorageError::TransactionRestartLimit(
                        self.restart_count.into(),
                    ));
                }
                self.bch2_trans_unlock();
                if self.bch2_trans_begin() > MAX_RESTARTS {
                    return Err(StorageError::TransactionRestartLimit(
                        self.restart_count.into(),
                    ));
                }
                continue;
            }

            // ── Phase 1: 标记已提交（继续持有 write lock） ──
            // 对应 bcachefs committed 标记；unlock_updates_write 在
            // atomic trigger 和 btree materialize 完成后执行。
            self.committed = true;
            if self.fs_usage_delta.hidden
                | self.fs_usage_delta.btree
                | self.fs_usage_delta.data
                | self.fs_usage_delta.cached
                | self.fs_usage_delta.reserved
                != 0
            {
                self.bch2_trans_account_disk_usage_change();
            }
            // ── Phase 2: Atomic 阶段触发器（不可回滚） ──
            // bcachefs: run_one_mem_trigger 在 do_bch2_trans_commit 内部，
            // 在 journal_res_get 之后、key insert 之前执行
            if let Err(e) = self.run_atomic_triggers() {
                self.bch2_trans_unlock_write();
                return Err(e);
            }
            // Rust alloc extent trigger is registered in the Atomic phase and
            // therefore produces its delta here; publish it before journal
            // insertion while the transaction still owns its reservation.
            if self.fs_usage_delta.hidden
                | self.fs_usage_delta.btree
                | self.fs_usage_delta.data
                | self.fs_usage_delta.cached
                | self.fs_usage_delta.reserved
                != 0
            {
                self.bch2_trans_account_disk_usage_change();
            }

            // 对应 bcachefs `bch2_trans_unlock_updates_write()`，必须在
            // atomic trigger 完成后释放 write lock。
            self.bch2_trans_unlock_write();

            // ── 成功退出 ──
            // 对应 bcachefs `out_reset:` (commit.c:1512-1515):
            //   1) if (!ret) bch2_trans_downgrade(trans);
            //   2) bch2_trans_reset_updates(trans);
            //
            // bcachefs 的 bch2_trans_reset_updates 不会清除 committed flag
            // （committed 仅供 debug 断言使用）。subvol 在成功路径保留 committed=true
            // 供外部调用者检查；begin() 在 caller 下一次操作前由 rustart 循环调用。
            self.restart_count = saved_restart_count;
            self.bch2_trans_downgrade();
            return Ok(());
        }
    }

    /// 运行 Transactional 阶段触发器，失败时设置 needs_restart
    ///
    /// 对应 bcachefs `bch2_trans_commit_run_triggers()` (commit.c:598-647)。
    /// 在 retry 循环的 Phase 0b 运行（try_lock_all 之前），对齐 bcachefs 的触发顺序。
    fn bch2_trans_commit_run_triggers(&mut self) -> Result<(), StorageError> {
        if self.ctx_vol.is_none() {
            return Ok(());
        }
        if self.journal.is_empty() {
            return Ok(());
        }

        let mut sort_id_start = 0;
        while sort_id_start < self.journal.len() {
            let sort_id = self.journal[sort_id_start].sort_order;
            let mut idx;

            loop {
                let mut trans_trigger_run = false;
                idx = sort_id_start;

                while idx < self.journal.len() && self.journal[idx].sort_order <= sort_id {
                    if self.journal[idx].sort_order < sort_id {
                        sort_id_start = idx;
                        idx += 1;
                        continue;
                    }

                    let entry = self.journal[idx].clone();
                    if !matches!(entry.btree_id, BtreeId::Subvolumes | BtreeId::Extents)
                        || (entry.insert_trigger_run && entry.overwrite_trigger_run)
                    {
                        idx += 1;
                        continue;
                    }

                    let key_bytes = bincode::serialize(&entry.key).unwrap_or_default();
                    let mut new_bytes = if entry.key.key_type == KeyType::Deleted {
                        None
                    } else if let Some(raw) = &entry.raw_value {
                        Some(raw.clone())
                    } else {
                        Some(bincode::serialize(&entry.value).unwrap_or_default())
                    };
                    let mut old_bytes = entry.old_raw_value.clone().or_else(|| {
                        entry
                            .old_value
                            .as_ref()
                            .map(|v| bincode::serialize(v).unwrap_or_default())
                    });

                    if !entry.overwrite_trigger_run && !entry.insert_trigger_run {
                        self.journal[idx].overwrite_trigger_run = true;
                        self.journal[idx].insert_trigger_run = true;
                    } else if !entry.overwrite_trigger_run {
                        self.journal[idx].overwrite_trigger_run = true;
                        new_bytes = None;
                    } else if !entry.insert_trigger_run {
                        self.journal[idx].insert_trigger_run = true;
                        old_bytes = None;
                    }

                    let result = match entry.btree_id {
                        BtreeId::Subvolumes => crate::subvol::bch2_subvolume_trigger(
                            self,
                            entry.btree_id,
                            &key_bytes,
                            old_bytes.as_deref(),
                            new_bytes.as_deref(),
                        ),
                        BtreeId::Extents => crate::alloc::bch2_trigger_extent(
                            self,
                            entry.btree_id,
                            &key_bytes,
                            old_bytes.as_deref(),
                            new_bytes.as_deref(),
                        ),
                        BtreeId::Alloc => crate::alloc::bch2_trigger_alloc(
                            self,
                            entry.btree_id,
                            &key_bytes,
                            old_bytes.as_deref(),
                            new_bytes.as_deref(),
                        ),
                        _ => Ok(()),
                    };
                    if let Err(e) = result {
                        // bcachefs: transactional trigger failure → restart
                        self.needs_restart = true;
                        self.restart_reason = Some(RestartReason::TriggerNeedsLock);
                        return Err(e);
                    }
                    trans_trigger_run = true;
                    idx += 1;
                }

                if !trans_trigger_run {
                    break;
                }
            }

            sort_id_start = idx;
        }
        Ok(())
    }

    /// 运行 Atomic 阶段触发器，错误直接传播（不可回滚）
    ///
    /// 对应 bcachefs `run_one_mem_trigger` with `BTREE_TRIGGER_atomic` (commit.c:1153-1159)。
    /// bcachefs 中在 journal_res_get + commit_hooks 之后执行。
    fn run_atomic_triggers(&mut self) -> Result<(), StorageError> {
        if self.ctx_vol.is_none() {
            return Ok(());
        }
        let mut idx = 0;
        while idx < self.journal.len() {
            let entry = self.journal[idx].clone();
            let key_bytes = bincode::serialize(&entry.key).unwrap_or_default();
            let mut new_bytes = if entry.key.key_type == KeyType::Deleted {
                None
            } else if let Some(raw) = &entry.raw_value {
                Some(raw.clone())
            } else {
                Some(bincode::serialize(&entry.value).unwrap_or_default())
            };
            let old_bytes = entry
                .old_raw_value
                .clone()
                .or_else(|| entry.old_value.as_ref().map(|v| bincode::serialize(v).unwrap_or_default()));

            if entry.btree_id == BtreeId::Alloc && self.journal_seq != 0 {
                if let Some(bytes) = new_bytes.as_mut() {
                    if let Ok(mut alloc) = crate::alloc::btree::deserialize_alloc_entry(bytes) {
                        let old_empty = old_bytes
                            .as_deref()
                            .and_then(|old| crate::alloc::btree::deserialize_alloc_entry(old).ok())
                            .map(|old| {
                                crate::alloc::bucket::data_type_is_empty(
                                    crate::alloc::BchDataType::from_raw(old.data_type)
                                        .unwrap_or(crate::alloc::BchDataType::Free),
                                )
                            })
                            .unwrap_or(true);
                        let old_type = old_bytes
                            .as_deref()
                            .and_then(|old| crate::alloc::btree::deserialize_alloc_entry(old).ok())
                            .and_then(|old| crate::alloc::BchDataType::from_raw(old.data_type))
                            .unwrap_or(crate::alloc::BchDataType::Free);
                        let new_type = crate::alloc::BchDataType::from_raw(alloc.data_type)
                            .unwrap_or(crate::alloc::BchDataType::Free);
                        let new_empty = crate::alloc::bucket::data_type_is_empty(new_type);
                        if alloc.journal_seq_nonempty > self.journal_seq {
                            alloc.journal_seq_nonempty = self.journal_seq;
                        }
                        if old_empty && !new_empty && alloc.journal_seq_nonempty == 0 {
                            alloc.journal_seq_nonempty = self.journal_seq;
                        }
                        if old_type != crate::alloc::BchDataType::NeedDiscard
                            && new_type == crate::alloc::BchDataType::NeedDiscard
                        {
                            alloc.journal_seq_empty = self.journal_seq;
                            if alloc.journal_seq_nonempty == alloc.journal_seq_empty {
                                alloc.journal_seq_nonempty = 0;
                                alloc.journal_seq_empty = 0;
                            }
                        }

                        let trigger_key = bincode::deserialize::<BtreeKey>(&key_bytes).ok();
                        if let Some(trigger_key) = trigger_key {
                            let dev = trigger_key.to_bpos().inode as u8;
                            let bucket_idx = trigger_key.get_vaddr();
                            let old_gen = old_bytes
                                .as_deref()
                                .and_then(|old| {
                                    crate::alloc::btree::deserialize_alloc_entry(old).ok()
                                })
                                .map(|old| old.gen)
                                .unwrap_or(0);
                            if old_gen != alloc.gen {
                                if let Some(ca) = self
                                    .ctx_vol
                                    .and_then(|vol| vol.device_rcu_noerror(dev))
                                {
                                    if let Some(vol) = self.ctx_vol {
                                        vol.allocator().for_each_bucket_mut(
                                            &ca,
                                            |global_bi, _bucket, generation| {
                                                if global_bi == bucket_idx {
                                                    *generation = alloc.gen;
                                                }
                                            },
                                        );
                                    }
                                }
                            }
                            if !crate::alloc::bucket::data_type_is_empty(old_type)
                                && new_type == crate::alloc::BchDataType::Free
                            {
                                if let Some(ca) = self
                                    .ctx_vol
                                    .and_then(|vol| vol.device_rcu_noerror(dev))
                                {
                                    ca.alloc_wake_counter
                                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                }
                            }

                            if old_type == crate::alloc::BchDataType::NeedDiscard
                                && new_type != crate::alloc::BchDataType::NeedDiscard
                            {
                                if let Some(old) = old_bytes.as_deref().and_then(|old| {
                                    crate::alloc::btree::deserialize_alloc_entry(old).ok()
                                }) {
                                    let bucket = (u64::from(dev) << 48)
                                        | (bucket_idx & ((1_u64 << 48) - 1));
                                    self.bch2_trans_delete(
                                        BtreeId::NeedDiscard,
                                        0,
                                        false,
                                        BtreeKey::from_bpos(
                                            Bpos::new(old.journal_seq_empty, bucket, 0),
                                            KeyType::Set,
                                        ),
                                        0,
                                    );
                                }
                            }
                            if old_type != crate::alloc::BchDataType::NeedDiscard
                                && new_type == crate::alloc::BchDataType::NeedDiscard
                            {
                                let bucket = (u64::from(dev) << 48)
                                    | (bucket_idx & ((1_u64 << 48) - 1));
                                self.bch2_trans_update_raw(
                                    BtreeId::NeedDiscard,
                                    0,
                                    false,
                                    BtreeKey::from_bpos(
                                        Bpos::new(alloc.journal_seq_empty, bucket, 0),
                                        KeyType::Set,
                                    ),
                                    Vec::new(),
                                    0,
                                );
                            }
                        }
                        let encoded = crate::alloc::btree::serialize_alloc_entry(&alloc);
                        *bytes = encoded.clone();
                        self.journal[idx].raw_value = Some(encoded);
                    }
                }
            }
            let result = Ok(());
            if let Err(e) = result {
                // bcachefs trans_commit_fatal_err: 原子阶段失败 → 不可恢复
                eprintln!(
                    "Atomic trigger failed for {:?} op {:?}: {}",
                    entry.btree_id, entry.op, e
                );
                self.revert_disk_usage_accounting();
                return Err(e);
            }
            idx += 1;
        }
        Ok(())
    }

    // ── bcachefs 对齐方法：do_bch2_trans_commit 三步拆分 ──

    /// 获取写锁 — 对应 bcachefs `bch2_trans_lock_write()` (commit.c:128)
    ///
    /// bcachefs 对齐的 3 步锁获取流程：
    /// 1. `btree_trans_sort_paths` — 排序 transaction 的权威 path pool
    /// 2. `try_lock_all` — 尝试所有 leaf 的 intent→write 升级
    /// 3. `bch2_trans_record_locked_seqs` — 记录锁序列号（仅成功时）
    pub fn bch2_trans_lock_write(&mut self) -> Result<(), StorageError> {
        let is_reclaim = self.watermark.to_bits() >= Watermark::Reclaim.to_bits();

        // bcachefs 对齐：trans_set_locked(trans, false) (locking.c:1514)
        //           内部调用 trans_maybe_disable_migrate (locking.h:125)
        self.trans_set_locked(false);
        self.btree_trans_sort_paths();
        self.try_lock_all();

        if !self.needs_restart {
            self.bch2_trans_record_locked_seqs();
        }

        if self.needs_restart {
            if is_reclaim {
                self.restart_count += 1;
                return Err(StorageError::TransactionRestartLimit(
                    self.restart_count.into(),
                ));
            }
            self.bch2_trans_unlock();
            if self.bch2_trans_begin() > MAX_RESTARTS {
                return Err(StorageError::TransactionRestartLimit(
                    self.restart_count.into(),
                ));
            }
        }
        Ok(())
    }

    /// 释放写锁并降级为 intent 锁 — 对应 bcachefs `bch2_trans_unlock_updates_write()` (commit.c:147)
    ///
    /// 对应 bcachefs unlock_updates_write (commit.c:147-164)。
    /// 遍历所有 path levels，将 write lock 降级为 intent lock，清空 write_locked 标志。
    pub fn bch2_trans_unlock_updates_write(&mut self) {
        self.bch2_trans_unlock_write();
    }

    /// 错误处理分支 — 对应 bcachefs `__bch2_trans_commit_error()` (commit.c:788-855)
    ///
    /// 处理提交过程中的可恢复/不可恢复错误：
    ///
    /// | 错误类型 | 处理方式 | bcachefs 对应 |
    /// |---------|---------|--------------|
    /// | TransactionRestartLimit | 直接传播 | — |
    /// | BtreeNodeFull | try split + restart | btree_insert_btree_node_full → bch2_btree_split_leaf + restart |
    /// | JournalBlocked | reclaim 检查 + 传播 | journal_res_blocked → journal_reclaim_would_deadlock |
    /// | NeedJournalReclaim | journal reclaim 等待 | btree_insert_need_journal_reclaim → wait reclaim |
    /// | 其他 Transaction 错误 | 设为 restart 信号，unlock+begin+retry | btree_trans_restart |
    /// | 其他所有错误 | 直接传播 | default: BUG_ON |
    ///
    /// 返回 `Ok(())` 表示错误已处理，调用者应继续 retry 循环。
    /// 返回 `Err(e)` 表示错误不可恢复，调用者应终止。
    fn __bch2_trans_commit_error(
        &mut self,
        err: StorageError,
        is_reclaim: bool,
    ) -> Result<(), StorageError> {
        match &err {
            // 重启限制：直接传播（已无重试空间）
            StorageError::TransactionRestartLimit(_) => Err(err),

            // BtreeNodeFull: 节点空间不足
            // 对应 bcachefs commit.c:811-825:
            //   -BCH_ERR_btree_insert_btree_node_full
            //   → bch2_btree_split_leaf() + restart
            // 注意: bcachefs 在错误路径中直接调用 split，Rust 版本
            // 将 split 委托给 btree::insert_with_transaction（内含分裂逻辑），
            // 因此这里只需设置重启原因；next restart 时插入路径会自动
            // 触发节点分裂。
            StorageError::BtreeNodeFull => {
                if is_reclaim {
                    return Err(err);
                }
                self.needs_restart = true;
                self.restart_reason = Some(RestartReason::BtreeNodeFull);
                Ok(())
            }

            // Journal 阻塞：检查 reclaim 死锁
            // 对应 bcachefs commit.c:795-808:
            //   -BCH_ERR_journal_res_blocked
            //   → reclaim 水位线检查 → journal_reclaim_would_deadlock
            //   → drop_locks_do + bch2_trans_journal_res_get
            StorageError::JournalError(msg) if msg.contains("blocked") || msg.contains("full") => {
                if is_reclaim {
                    self.needs_restart = true;
                    self.restart_reason = Some(RestartReason::JournalReclaimWouldDeadlock);
                    return Err(StorageError::JournalReclaimWouldDeadlock);
                }
                // 非 reclaim：unlock + begin + retry
                // 对应 bcachefs drop_locks_do + journal_res_get (commit.c:804-807)
                self.needs_restart = true;
                self.restart_reason = Some(RestartReason::LockConflict);
                Ok(())
            }

            // Journal reclaim 等待
            // 对应 bcachefs commit.c:828-850:
            //   -BCH_ERR_btree_insert_need_journal_reclaim
            //   → bch2_trans_unlock + event_inc_trace
            //   → wait_event_freezable_timeout(journal.reclaim_wait)
            //   → bch2_trans_relock(trans)
            StorageError::JournalError(msg)
                if msg.contains("reclaim") || msg.contains("flushing") =>
            {
                if is_reclaim {
                    return Err(err);
                }
                // bcachefs: 等待 journal reclaim 完成
                // Rust 等价：设置 needs_restart + 标记 reclaim 等待
                // journal_reclaim_wait_done 检查在 Vol 层的 journal 锁获取中完成
                self.needs_restart = true;
                self.restart_reason = Some(RestartReason::NeedJournalReclaim);
                Ok(())
            }

            // 不可恢复错误：直接传播
            _ => Err(err),
        }
    }

    /// 回滚更新 — 对应 bcachefs `bch2_trans_reset_updates()` (update.h:557-571)
    ///
    /// **不放锁，不清除 iters。** 仅重置 journal 和 committed 状态。
    /// bcachefs 语义：`bch2_trans_reset_updates()` 在成功和失败路径中都会调用，
    /// 清除 `nr_updates`、`journal_entries`、`hooks` 等，但**不**重置 `restart_count`。
    /// restart_count 仅在成功提交时由调用者重置（saved_restart_count 模式）。
    ///
    /// 如需完全清理（释放锁 + 清除 iters），请调用
    /// `bch2_trans_unlock()` 后清除 iter。
    pub fn rollback(&mut self) {
        self.revert_disk_usage_accounting();
        self.journal.clear();
        self.fs_usage_delta = BchFsUsageBase::default();
        self.committed = false;
        self.needs_restart = false;
        self.restart_reason = None;
    }

    // ─── Phase A: 锁排序 + 自动重启 ──────────────────────────

    /// 收集所有 path levels 为 BtreePath 列表
    ///
    /// 遍历每个 iter 的每个 path level，创建对应的 BtreePath。
    /// 获取 journal 条目的写锁 — 对应 bcachefs `bch2_trans_lock_write_inlined()` (commit.c:141-159)
    ///
    /// 遍历 journal 条目（自然追加顺序，与 bcachefs `trans_for_each_update` 对齐），
    /// 对引用的 leaf 节点做 intent→write 升级。路径已持有 intent/read 锁
    ///（来自遍历时的 `lock_read()` / `lock_intent()` 阻塞获取）。
    ///
    /// bcachefs 不需要 `sort_locks()` 的原因：
    /// 1. 路径锁在遍历时已按 (btree_id, pos, level) 自然顺序获取（树下降路径确定）
    /// 2. 写锁升级顺序由 `trans->updates[]` 追加顺序决定（`bch2_trans_update` 调用顺序）
    /// 3. SIX lock 内置死锁检测（`bch2_six_check_for_deadlock`）兜底 ABBA 场景
    ///
    /// 对应 bcachefs `btree_insert_entry_cmp()` (update.c:25-32)
    /// 排序键: (sort_order, cached, -level, pos)
    /// 确保 Alloc → Freespace 等依赖顺序，避免锁顺序反转
    ///
    /// 注意: BtreeKey 为 `#[repr(C, packed)]`，直接字段访问会创建对齐引用
    ///（UB），必须使用 `ptr::addr_of!().read_unaligned()`。
    fn btree_insert_entry_cmp(a: &BtreeTransEntry, b: &BtreeTransEntry) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        use std::ptr::addr_of;

        match (&a.sort_order).cmp(&b.sort_order) {
            Ordering::Equal => {}
            ord => return ord,
        }
        match (&a.cached).cmp(&b.cached) {
            Ordering::Equal => {}
            ord => return ord,
        }
        // -level: higher level first
        match (&b.level).cmp(&a.level) {
            Ordering::Equal => {}
            ord => return ord,
        }
        // 使用 bpos_cmp 对齐 bcachefs bpos_cmp(l->k->k.p, r->k->k.p)
        // BtreeKey 缺少 inode 字段（subvol 中始终为 0），不影响排序结果
        let a_bpos = Bpos {
            inode: 0,
            offset: unsafe { addr_of!(a.key.vaddr).read_unaligned() },
            snapshot: unsafe { addr_of!(a.key.snapshot_id).read_unaligned() },
        };
        let b_bpos = Bpos {
            inode: 0,
            offset: unsafe { addr_of!(b.key.vaddr).read_unaligned() },
            snapshot: unsafe { addr_of!(b.key.snapshot_id).read_unaligned() },
        };
        crate::btree::key::bpos_cmp(a_bpos, b_bpos)
    }

    /// subvol 的 SixLock 暂无在线死锁检测，但 intent→write 直接走 `try_lock_write()`
    ///（spin + yield，不 sleep），因此 ABBA 场景双方都失败→重启，
    /// 不存在真死锁。
    fn try_lock_all(&mut self) {
        // 按 bcachefs btree_insert_entry_cmp 排序 journal 条目
        self.journal.sort_by(Self::btree_insert_entry_cmp);

        // 收集需要锁升级的条目信息（预收集避免升级循环中跨方法借用冲突）
        struct LockUpgrade {
            _journal_idx: usize, // sorted journal 中的索引
            iter_idx: usize,
            path_idx: PathIdx,
            level: usize,
            node: Arc<BtreeNode>,
            lock_state: BtreeNodeLockedType,
        }

        let mut upgrades: Vec<LockUpgrade> = Vec::new();
        for i in 0..self.journal.len() {
            let entry = &self.journal[i];
            if i > 0 {
                let prev = &self.journal[i - 1];
                if prev.iter_idx == entry.iter_idx && prev.level == entry.level {
                    continue;
                }
            }
            let iter_idx = entry.iter_idx;
            let level = entry.level as usize;

            if iter_idx >= self.iters.len() || level >= BTREE_MAX_DEPTH {
                continue;
            }
            let path_idx = self.iters[iter_idx].path;
            let BtreePathNode::Node(path_level) = &self.path_ref(path_idx).levels[level] else {
                continue;
            };
            let lock_state = path_level.lock_state;
            if lock_state == BtreeNodeLockedType::Write {
                // 已有 write 锁（多个条目共享同一 leaf），无需升级
                continue;
            }

            let node = path_level.node.clone();

            upgrades.push(LockUpgrade {
                _journal_idx: i,
                iter_idx,
                path_idx,
                level,
                node,
                lock_state,
            });
        }

        // 统一执行锁升级，失败时倒序回滚已升级的 write 锁
        for pos in 0..upgrades.len() {
            let upgrade = &upgrades[pos];
            let ok = match upgrade.lock_state {
                BtreeNodeLockedType::Intent => {
                    // 对应 bcachefs bch2_btree_node_lock_write_contended locking.c:965-972
                    // six_trylock_write 不自排除读者，需临时减去 intent 节点上的自身读锁计数。
                    // 因 intent 锁已阻止新读者进入，所有现存读者均来自本事务/线程。
                    let readers = upgrade.node.lock.six_lock_counts().n[0];
                    if readers > 0 {
                        upgrade.node.lock.six_lock_readers_add(-(readers as i32));
                    }
                    let ok = upgrade.node.lock.six_trylock_write();
                    if readers > 0 {
                        upgrade.node.lock.six_lock_readers_add(readers as i32);
                    }
                    ok
                }
                BtreeNodeLockedType::Read => {
                    // six_lock_tryupgrade 内部已递减 reader_count（state - 1），
                    // 升级后 reader_count 归零，six_trylock_write 可直接调用。
                    if upgrade.node.lock.six_lock_tryupgrade() {
                        let remaining = upgrade.node.lock.six_lock_counts().n[0];
                        if remaining > 0 {
                            upgrade.node.lock.six_lock_readers_add(-(remaining as i32));
                        }
                        let ok = upgrade.node.lock.six_trylock_write();
                        if remaining > 0 {
                            upgrade.node.lock.six_lock_readers_add(remaining as i32);
                        }
                        ok
                    } else {
                        false
                    }
                }
                // Write 和 None 已在收集阶段排除
                _ => unreachable!(),
            };

            if !ok {
                // ── 失败回滚 ──
                // 对应 bcachefs trans_lock_write_fail() (commit.c:119-137)
                // 倒序释放已成功升级前 pos 个条目的 write 锁
                self.needs_restart = true;
                self.restart_reason = Some(RestartReason::LockConflict);

                for rp in (0..pos).rev() {
                    let rb = &upgrades[rp];
                    rb.node.lock.six_unlock_write();
                    if let BtreePathNode::Node(level) =
                        &mut self.path_mut(rb.path_idx).levels[rb.level]
                    {
                        level.lock_state = BtreeNodeLockedType::Intent;
                        self.path_mut(rb.path_idx)
                            .mark_btree_node_locked_noreset(rb.level, BtreeNodeLockedType::Intent);
                    }
                }
                return;
            }

            // 成功：更新 lock_state + btree 节点写前准备
            if let BtreePathNode::Node(level) =
                &mut self.path_mut(upgrade.path_idx).levels[upgrade.level]
            {
                level.lock_state = BtreeNodeLockedType::Write;
                let node_ptr = Arc::as_ptr(&level.node) as *mut BtreeNode;
                let node = unsafe { &mut *node_ptr };
                bch2_btree_node_prep_for_write(node);
                self.path_mut(upgrade.path_idx)
                    .mark_btree_node_locked_noreset(upgrade.level, BtreeNodeLockedType::Write);
            }
        }

        self.write_locked = true;
    }

    /// 记录所有 path levels 的 locked_seq
    ///
    /// 在每次成功获取锁后调用，保存每个节点锁的当前序列号。
    /// 序列号在写锁释放时递增，因此 locked_seq 可用于检测节点是否被外部修改：
    /// 重启时若 lock.six_lock_seq() == locked_seq，说明节点未被写操作触及，可跳过重读。
    ///
    /// 对应 bcachefs `bch2_trans_unlock()` (locking.c:1478-1490) 中
    /// `bch2_btree_path_traverse_unlock()` 的 seq 记录时机——在锁释放前记录。
    /// 调用位置：`__bch2_trans_commit()` Phase 0c 中 `try_lock_all()` 成功后立即调用。
    fn bch2_trans_record_locked_seqs(&mut self) {
        for slot in self.paths.iter_mut() {
            let Some(path) = slot else {
                continue;
            };
            for node in &mut path.levels {
                if let BtreePathNode::Node(level) = node {
                    level.locked_seq = level.node.lock.six_lock_seq();
                }
            }
        }
    }

    /// 优化版重启：利用 locked_seq 检测是否需要完整重启
    ///
    /// R2 优化：检查每个 iter 的 path levels 的 seq 是否与加锁时相同。
    /// 如果所有 iters 的 seq 都未变化，说明数据未被外部修改，返回 `None`。
    /// 否则返回 `Some(reason)` 表示需要完整重启。
    ///
    /// 无论检测结果如何，都会释放所有锁并重置状态。
    pub fn restart_optimized(&mut self) -> Option<RestartReason> {
        // 1. 检查是否有任何 iter 的任意 path level seq 与 locked_seq 不符
        //
        // 注意：不仅检查 leaf，还要检查所有中间层级。因为 SixLock
        // 的 seq 是节点级别的——内部节点（split/merge）的变化不会
        // 传播到子节点。如果仅检查 leaf 而跳过内部节点，可能在
        // 树拓扑已修改时错误返回 None（路径失效）。
        let needs_full_restart = self.paths.iter().flatten().any(|path| {
            path.levels.iter().any(|node| match node {
                BtreePathNode::Node(level) => level.node.lock.six_lock_seq() != level.locked_seq,
                BtreePathNode::None | BtreePathNode::Error(_) => false,
            })
        });

        // 2. 取出重启原因（如果有）
        let reason = self.restart_reason.take();

        // 3. 释放所有锁并重置状态
        self.bch2_trans_unlock();
        self.bch2_trans_begin();

        if needs_full_restart {
            // 检测到 seq 变化 → 调用者应执行完整重下降
            Some(reason.unwrap_or(RestartReason::LockConflict))
        } else {
            // 所有 seq 未变 → 调用者可跳过重下降
            None
        }
    }

    // ── bcachefs 对齐方法：锁相位 & 迁移控制 ──

    /// 标记事务为「已锁定」— 对齐 bcachefs `trans_set_locked` (locking.h:115-127)
    ///
    /// bcachefs 相位：
    ///   a. if (!trans->locked) { ... } — 幂等守卫
    ///   b. trans->locked = true; trans->last_unlock_ip = 0;
    ///   c. lock_acquire_exclusive(&trans->dep_map, ...) — lockdep 注解
    ///      （subvol：无 lockdep 框架，结构对齐保留）
    ///   d. PF_MEMALLOC_NOFS — 防止文件系统递归
    ///      （subvol：Rust async 运行时不使用 PF_ 标志位，结构对齐保留）
    ///   e. trans_maybe_disable_migrate(trans) — 可选的 CPU pin
    fn trans_set_locked(&mut self, try_: bool) {
        if !self.locked {
            self.locked = true;
            self.last_unlock_ip = 0;
            // bcachefs: lock_acquire_exclusive — subvol 无 lockdep 等效
            // bcachefs: current->flags |= PF_MEMALLOC_NOFS — subvol 无等效
            self.trans_maybe_disable_migrate();
        }
        // bcachefs: trans_set_locked 会无条件尝试 disable_migrate，
        // 但 only-if-not-locked（上面）。后面三个不用。
        _ = try_; // bcachefs 用 try 区分是否 try-lock，subvol 保留形参
    }

    /// 标记事务为「已解锁」— 对齐 bcachefs `trans_set_unlocked` (locking.h:129-139)
    ///
    /// bcachefs 相位：
    ///   a. if (trans->locked) { ... } — 幂等守卫
    ///   b. trans->locked = false; trans->last_unlock_ip = _RET_IP_;
    ///   c. lock_release — lockdep 对应释放
    ///   d. current->flags &= ~PF_MEMALLOC_NOFS — 恢复 FS 递归标记
    fn trans_set_unlocked(&mut self, unlock_ip: usize) {
        if self.locked {
            self.locked = false;
            self.last_unlock_ip = unlock_ip;
            // bcachefs: lock_release — subvol 无 lockdep 等效
            // bcachefs: current->flags &= ~PF_MEMALLOC_NOFS — subvol 无等效
        }
    }

    /// 在持锁期间 pin 到当前 CPU — 对齐 bcachefs `trans_maybe_disable_migrate` (locking.h:88-104)
    ///
    /// bcachefs 条件（全部满足才禁用迁移）：
    ///   1. `!trans->migrate_disabled` — 尚未禁用
    ///   2. `trans->shard_cpu >= 0` — 已绑定 shard
    ///   3. `trans->shard_cpu == raw_smp_processor_id()` — 当前 CPU 匹配
    ///
    /// subvol：async 运行时不控制 CPU 迁移。per-CPU 锁路径在 `six_trylock_read`
    /// 中通过 `current_thread_slot()` 稳定获取 slot 索引，无需 migrate_disable。
    /// `migrate_disabled` 仅作锁相位跟踪标记。
    fn trans_maybe_disable_migrate(&mut self) {
        // bcachefs: raw_smp_processor_id() — 当前 CPU ID
        // subvol: std::thread::current().id() 不保证与 CPU ID 对应，
        //          shard_cpu >= 0 时作逻辑检查（无实际 migrate_disable 调用）
        if !self.migrate_disabled && self.shard_cpu >= 0 {
            // bcachefs: if (shard_cpu == raw_smp_processor_id())
            //           migrate_disable();
            self.migrate_disabled = true;
        }
    }

    /// 重新启用 CPU 迁移 — 对齐 bcachefs `trans_enable_migrate` (locking.h:107-113)
    fn trans_enable_migrate(&mut self) {
        if self.migrate_disabled {
            // bcachefs: migrate_enable()
            self.migrate_disabled = false;
        }
    }

    /// 验证路径锁状态（DEBUG 断言）— 对齐 bcachefs `bch2_btree_path_verify_locks` (locking.c:1415)
    ///
    /// bcachefs 中在 downgrade 后调用，检查所有 path level 的锁状态是否与
    /// `locks_want` 一致。subvol 降级后不应有 Write 锁，且 intent 锁只应在
    /// `keep_intent` 为 true 的 leaf level 存在。
    #[cfg(debug_assertions)]
    fn bch2_btree_path_verify_locks(&self) {
        for path in self.paths.iter().flatten() {
            if path.nodes_locked == 0 {
                continue;
            }
            for level_idx in 0..BTREE_MAX_DEPTH {
                let want = if level_idx < path.level as usize {
                    BtreeNodeLockedType::None
                } else if level_idx < path.locks_want as usize {
                    BtreeNodeLockedType::Intent
                } else if level_idx == path.level as usize {
                    BtreeNodeLockedType::Read
                } else {
                    BtreeNodeLockedType::None
                };
                let have = match path.btree_node_locked_type(level_idx) {
                    BtreeNodeLockedType::Write => BtreeNodeLockedType::Intent,
                    other => other,
                };
                let is_node = matches!(path.levels[level_idx], BtreePathNode::Node(_));
                debug_assert!(
                    is_node || have == BtreeNodeLockedType::None,
                    "non-node level {level_idx} is locked: {have:?}"
                );
                debug_assert!(
                    !is_node || want == have,
                    "path level={}; locks_want={}; slot={level_idx}; want={want:?}; have={have:?}; nodes_locked={:#x}",
                    path.level,
                    path.locks_want,
                    path.nodes_locked,
                );
                if have != BtreeNodeLockedType::None {
                    let BtreePathNode::Node(level) = &path.levels[level_idx] else {
                        unreachable!();
                    };
                    debug_assert_eq!(level.locked_seq, level.node.lock.six_lock_seq());
                }
            }
        }
    }

    // ── 锁操作 ──

    /// 释放所有当前持有的锁并重置锁状态（用于重启前的清理）
    ///
    /// 对齐 bcachefs `bch2_trans_unlock()` (locking.c:1524-1541)。
    ///
    /// bcachefs 相位：
    ///   1. `trans_set_unlocked(trans, _RET_IP_)` — locked=false + lockdep 释放
    ///      + 恢复 PF_MEMALLOC_NOFS
    ///   2. `__bch2_trans_unlock(trans)` — 释放所有 per-path 锁
    ///   3. `bch2_btree_cache_cannibalize_unlock(trans)` 如果 cannibalize 锁被持有
    ///      （subvol：无 btree_cache_cannibalize_lock 等效机制，该项为架构差异，见 spec ➖ 表）
    ///
    /// **不显式清除 `locked_seq`** — 对齐 bcachefs `__bch2_btree_path_unlock()`
    /// (locking.c:1440-1454)：path-level 释放后 seq 保留，下次遍历时重新获取。
    /// `locked_seq` 由 `restart_optimized()` 用于检测节点是否被外部修改。
    ///
    /// 如果 restart_count 超过阈值（>= 100），在所有锁上设置 nospin bit
    /// 以跳过后续的自旋尝试。
    /// 获取 SRCU 读锁 — 对应 bcachefs `bch2_trans_srcu_lock()` (iter.c:3833-3840)
    ///
    /// bcachefs userspace 中 SRCU 是空操作，`srcu_read_lock` 返回 0；
    /// urcu crate 也不支持 `srcu_read_lock`。此函数仅做结构对齐。
    /// subvol 使用 Arc 管理节点生命周期，不需要 SRCU。
    fn bch2_trans_srcu_lock(&mut self) {
        if !self.srcu_held {
            // bcachefs: trans->srcu_idx = srcu_read_lock(&trans->c->btree.trans.barrier);
            // bcachefs: trans->srcu_lock_time = jiffies;
            self.srcu_idx = 0; // no-op align
            self.srcu_lock_time = 0; // no-op align
            self.srcu_held = true;
        }
    }

    /// 释放写锁但保持 intent/read 锁 — 对应 bcachefs `bch2_trans_unlock_write()`
    ///
    /// 遍历所有 iter 的 path levels，只释放写锁，降级到 intent。
    /// 不释放 intent 或 read 锁。
    ///
    /// bcachefs 对应：locking.c `bch2_trans_unlock_write()` (line 1572-1581)
    pub fn bch2_trans_unlock_write(&mut self) {
        let mut write_locks = Vec::new();
        for path_idx in self.path_idx_iter() {
            let path = self.path_ref(path_idx);
            for level_idx in 0..BTREE_MAX_DEPTH {
                if path.btree_node_locked_type(level_idx) != BtreeNodeLockedType::Write {
                    continue;
                }
                match &path.levels[level_idx] {
                    BtreePathNode::Node(level) => {
                        write_locks.push((path_idx, level_idx, Arc::clone(&level.node)));
                    }
                    BtreePathNode::None | BtreePathNode::Error(_) => {
                        panic!("write-locked path has no node at level {level_idx}");
                    }
                }
            }
        }

        for (path_idx, level_idx, node) in write_locks {
            let old_seq = node.lock.six_lock_seq();
            node.lock.six_unlock_write();
            let new_seq = node.lock.six_lock_seq();
            if new_seq != old_seq {
                for path in self.paths.iter_mut().flatten() {
                    if let BtreePathNode::Node(level) = &mut path.levels[level_idx] {
                        if Arc::ptr_eq(&level.node, &node) {
                            level.locked_seq = new_seq;
                        }
                    }
                }
            }
            let path = self.path_mut(path_idx);
            if let BtreePathNode::Node(level) = &mut path.levels[level_idx] {
                level.lock_state = BtreeNodeLockedType::Intent;
            }
            path.mark_btree_node_locked_noreset(level_idx, BtreeNodeLockedType::Intent);
        }
        self.write_locked = false;
    }

    /// 检查事务是否持有任何锁
    ///
    /// 遍历所有 iter 的 path levels，只要有任一 level 的 lock_state 不为 None 即返回 true。
    ///
    /// 对应 bcachefs bch2_trans_locked locking.c:1622-1631。
    pub fn bch2_trans_locked(&self) -> bool {
        for path in self.paths.iter().flatten() {
            for level in 0..BTREE_MAX_DEPTH {
                if path.btree_node_locked_type(level) != BtreeNodeLockedType::None {
                    return true;
                }
            }
        }
        false
    }

    /// 降级事务中所有 iter 的锁
    ///
    /// 对齐 bcachefs `bch2_trans_downgrade` (locking.c:1427-1438) +
    /// `__bch2_btree_path_downgrade` (locking.c:1386-1423) +
    /// `bch2_btree_path_downgrade` (iter.h:635-641)。
    ///
    /// bcachefs 相位：
    ///   1. `if (trans->restarted) return;` (locking.c:1394-1395)
    ///   2. 对每条 path 计算 `new_locks_want = path->level + !!path->intent_ref`
    ///      (iter.h:638)，subvol 使用 `iter.flags.intent` 等效于 intent_ref
    ///   3. 对 path 中每个 level：
    ///      a. level > path->level → `btree_node_unlock`（完全释放）
    ///      b. level == path->level 且 intent-locked 且 !keep_intent →
    ///         `six_lock_downgrade` + `mark_btree_node_locked_noreset(READ)`
    ///         (locking.c:1407-1410)
    ///   4. `bch2_btree_path_verify_locks(trans, path)` (locking.c:1415)
    ///      — subvol：所有 iters 降级后统一调用 bch2_btree_path_verify_locks() 验证
    pub fn bch2_trans_downgrade(&mut self) {
        // Phase 1: bcachefs if (trans->restarted) return; (locking.c:1394-1395)
        if self.needs_restart {
            return;
        }

        let path_indices: Vec<PathIdx> = self.path_idx_iter().collect();
        for path_idx in path_indices {
            if self.path_ref(path_idx).ref_count == 0 {
                continue;
            }
            let new_locks_want = {
                let path = self.path_ref(path_idx);
                path.level + u8::from(path.intent_ref != 0)
            };
            if self.path_ref(path_idx).locks_want <= new_locks_want {
                continue;
            }
            self.path_mut(path_idx).locks_want = new_locks_want;

            while self.path_ref(path_idx).nodes_locked != 0 {
                let nodes_locked = self.path_ref(path_idx).nodes_locked;
                let highest = (u8::BITS - 1 - nodes_locked.leading_zeros()) as usize >> 1;
                if highest < self.path_ref(path_idx).locks_want as usize {
                    break;
                }
                if highest > self.path_ref(path_idx).level as usize {
                    self.btree_node_unlock(path_idx, highest);
                } else {
                    let lock_type = self.path_ref(path_idx).btree_node_locked_type(highest);
                    if lock_type == BtreeNodeLockedType::Intent {
                        let BtreePathNode::Node(level) =
                            &mut self.path_mut(path_idx).levels[highest]
                        else {
                            panic!("locked path has no node at level {highest}");
                        };
                        level.node.lock.six_lock_downgrade();
                        level.lock_state = BtreeNodeLockedType::Read;
                        self.path_mut(path_idx)
                            .mark_btree_node_locked_noreset(highest, BtreeNodeLockedType::Read);
                    }
                    break;
                }
            }
        }

        // Phase 4: 锁验证（debug 断言）
        // bcachefs: bch2_btree_path_verify_locks(trans, path) (locking.c:1415)
        // 在 bcachefs 中每条 path 降级后独立验证；subvol 在所有 iters 降级后统一验证
        #[cfg(debug_assertions)]
        self.bch2_btree_path_verify_locks();
    }

    /// 长时间解锁事务持有的所有锁，允许 CPU 迁移
    ///
    /// 对齐 bcachefs `bch2_trans_unlock_long` (locking.c:1543-1570)。
    ///
    /// bcachefs 相位：
    ///   1. `bch2_trans_unlock(trans)` — 释放锁 + trans_set_unlocked
    ///   2. `trans_enable_migrate(trans)` — 允许 CPU 迁移
    ///   3. SRCU 释放（如持有）
    pub fn bch2_trans_unlock_long(&mut self) {
        // Phase 1: bch2_trans_unlock(trans) (locking.c:1545)
        //           → trans_set_unlocked(trans) (locking.c:1526) — bch2_trans_unlock 内部调用
        //           → locked=false, lockdep release
        //           → btree_cache_cannibalize_unlock (if held, subvol 无)
        self.bch2_trans_unlock();

        // Phase 2: trans_enable_migrate(trans) (locking.c:1546, locking.h:107-113)
        //           → if migrate_disabled { migrate_enable(); migrate_disabled=false }
        self.trans_enable_migrate();

        // Phase 3: SRCU 释放 — 对齐 locking.c:1548-1569
        if self.srcu_held {
            for slot in self.paths.iter_mut() {
                if let Some(path) = slot {
                    if path.cached && path.btree_node_locked_type(0) == BtreeNodeLockedType::None {
                        path.levels[0] = BtreePathNode::Error(BtreePathError::SrcuReset);
                    }
                }
            }
            self.srcu_held = false;
        }
    }

    /// 请求重启（由外部操作用于触发重启）
    ///
    /// 当 iter 升级锁失败或检测到路径失效时调用。
    pub fn request_restart(&mut self, reason: RestartReason) {
        self.needs_restart = true;
        self.restart_reason = Some(reason);
    }

    /// 重启事务：释放所有锁并进入 `bch2_trans_begin()`
    ///
    /// 对应 bcachefs `btree_trans_restart()` (iter.h:613)，由 retry 入口
    /// 调用 `bch2_trans_unlock()` + `bch2_trans_begin()`。
    /// 设置 restart 标志 + retry 循环中调用 `bch2_trans_begin()` (iter.c:3887-3946)。
    ///
    /// bcachefs 重启为两阶段模式：(1) `btree_trans_restart()` 设置 `trans->restarted` 错误码
    /// 和 `last_restarted_ip`； (2) retry 循环入口调用 `bch2_trans_begin()` 重置事务状态
    /// （restart_count++、清路径标志、resize mem 等）。
    ///
    /// 返回：
    /// - `Some(reason)` — 正常重启，返回触发重启的原因并消费
    /// - `None` — 超过 `MAX_RESTARTS` 阈值，调用者**必须**终止循环
    ///
    /// 正确调用模式见 `lockrestart_do!` 宏。
    pub fn restart(&mut self) -> Option<RestartReason> {
        let reason = self.restart_reason.take();
        self.bch2_trans_unlock();
        self.bch2_trans_begin();
        (self.restart_count <= MAX_RESTARTS)
            .then_some(reason)
            .flatten()
    }

    /// 重启并自动重获锁（D3）
    ///
    /// bcachefs 的 relock 机制通过路径 `should_be_locked` 标志 +
    /// `bch2_btree_path_traverse_all()` (iter.c:1264-1340) 在 `bch2_trans_begin()` 中统一处理。
    ///
    /// subvol 采用"保存锁状态 → 释放 → 重获取"的显式模式，语义上对齐 bcachefs 的
    /// relock 结果，只是用 Rust 的显式状态恢复替代了遍历内隐式重建。
    ///
    /// 两阶段算法：
    /// 1. 保存当前 path 的目标锁状态
    /// 2. 释放所有锁 + 重置 iter（同 `restart()`）
    /// 3. 按保存的锁状态重新尝试获取所有锁
    /// 4. 若任何锁获取失败，设置 `needs_restart`（调用者可再次重启）
    ///
    /// 与 `restart()` 的区别：完成后 path levels 保持与重启前相同的锁状态，
    /// 而非全部为 None。适用于锁释放后需要立即重获的场景。
    pub fn restart_with_relock(&mut self) -> Option<RestartReason> {
        // Phase 1: 保存目标锁状态
        let targets: Vec<(usize, usize, BtreeNodeLockedType)> = self
            .iters
            .iter()
            .enumerate()
            .flat_map(|(iter_idx, iter)| {
                self.path_ref(iter.path)
                    .levels
                    .iter()
                    .enumerate()
                    .filter_map(move |(level_idx, node)| match node {
                        BtreePathNode::Node(level)
                            if level.lock_state != BtreeNodeLockedType::None =>
                        {
                            Some((iter_idx, level_idx, level.lock_state))
                        }
                        BtreePathNode::Node(_) | BtreePathNode::None | BtreePathNode::Error(_) => {
                            None
                        }
                    })
            })
            .collect();

        let reason = self.restart_reason.take();

        // Phase 2: 释放所有锁 + 重置 iter
        self.bch2_trans_unlock();
        self.bch2_trans_begin();
        if self.restart_count > MAX_RESTARTS {
            return None;
        }

        // Phase 3: 重新获取目标锁
        for (iter_idx, level_idx, target) in &targets {
            if *iter_idx >= self.iters.len() || *level_idx >= BTREE_MAX_DEPTH {
                // iter 已不存在或 path 深度变化，跳过（trigger 会处理）
                continue;
            }
            let path_idx = self.iters[*iter_idx].path;
            let BtreePathNode::Node(path_level) = &self.path_ref(path_idx).levels[*level_idx]
            else {
                continue;
            };
            let node = Arc::clone(&path_level.node);
            let ok = match target {
                BtreeNodeLockedType::Read => node.lock.six_lock_read(),
                BtreeNodeLockedType::Intent => node.lock.six_lock_intent(),
                BtreeNodeLockedType::Write => {
                    if !node.lock.six_lock_intent() {
                        false
                    } else if node.lock.six_lock_write() {
                        true
                    } else {
                        node.lock.six_unlock_intent();
                        false
                    }
                }
                BtreeNodeLockedType::None => true,
            };
            if ok {
                let path = self.path_mut(path_idx);
                let BtreePathNode::Node(path_level) = &mut path.levels[*level_idx] else {
                    unreachable!();
                };
                path_level.lock_state = *target;
                path.mark_btree_node_locked_noreset(*level_idx, *target);
            } else {
                self.needs_restart = true;
                // 单次失败即停止 relock，剩余锁由下次重启处理
                return reason;
            }
        }

        reason
    }

    /// 检查是否需要重启
    pub fn needs_restart(&self) -> bool {
        self.needs_restart
    }

    /// 获取重启计数
    pub fn restart_count(&self) -> u32 {
        self.restart_count
    }

    /// 获取最近一次重启的原因
    pub fn restart_reason(&self) -> Option<RestartReason> {
        self.restart_reason
    }

    // ─── Phase A6: 重启触发辅助方法 ──────────────────────────

    /// 触发 NodeSplit 重启 — btree 节点分裂后调用，通知事务路径可能已失效
    pub fn trigger_node_split(&mut self) {
        self.request_restart(RestartReason::NodeSplit);
    }

    /// 触发 KeyCacheMiss 重启 — 缓存中找不到节点时调用
    pub fn trigger_key_cache_miss(&mut self) {
        self.request_restart(RestartReason::KeyCacheMiss);
    }

    /// 触发 NodeReadRequired 重启 — 节点需要重新从磁盘读取时调用
    ///
    /// 对应 bcachefs `BCH_ERR_transaction_restart_lock_node_reused` (errcode.h:145)
    /// bcachefs 通过 `trace_and_count(trans->c, trans_restart_btree_node_reused, ...)` (trace.h)
    /// 记录节点重读事件；subvol 直接用 RestartReason 简化，语义已对齐（触发重试）。
    pub fn trigger_node_read_required(&mut self) {
        self.request_restart(RestartReason::NodeReadRequired);
    }

    /// 触发 TriggerNeedsLock 重启 — 触发器需要额外的锁时调用
    ///
    /// 对应 bcachefs `BCH_ERR_transaction_restart_upgrade` (errcode.h:153) /
    /// `BCH_ERR_transaction_restart_relock` (errcode.h:141)
    /// bcachefs 在 key cache fill (key_cache.c)、路径遍历 (iter.c) 等位置返回 restart
    /// 错误码；subvol 直接用 RestartReason 简化，触发点已在各遍历路径对齐。
    pub fn trigger_needs_lock(&mut self) {
        self.request_restart(RestartReason::TriggerNeedsLock);
    }

    /// 触发死锁重启 — 锁顺序违反导致死锁风险
    /// 对应 bcachefs `BCH_ERR_transaction_restart_would_deadlock_write` (errcode.h:151)
    pub fn trigger_would_deadlock(&mut self) {
        self.request_restart(RestartReason::WouldDeadlock);
    }

    /// 触发写溢出重启 — btree 节点空间不足
    /// 对应 bcachefs `BCH_ERR_transaction_restart_write_overflow`
    pub fn trigger_write_overflow(&mut self) {
        self.request_restart(RestartReason::WriteOverflow);
    }

    /// 触发分裂+内部更新重启
    /// 对应 bcachefs `BCH_ERR_transaction_restart_split_with_interior_updates`
    pub fn trigger_split_with_interior_updates(&mut self) {
        self.request_restart(RestartReason::SplitWithInteriorUpdates);
    }

    /// 触发 TraverseAll 重启 — 路径表顺序变化需从头遍历
    /// 对应 bcachefs `BCH_ERR_transaction_restart_traverse_all`
    pub fn trigger_traverse_all(&mut self) {
        self.request_restart(RestartReason::TraverseAll);
    }

    /// 触发 Relock 重启 — 当前节点锁被释放需重获
    /// 对应 bcachefs `BCH_ERR_transaction_restart_relock`
    pub fn trigger_relock(&mut self) {
        self.request_restart(RestartReason::Relock);
    }

    /// 触发 RelockPath 重启 — 重新获取指定路径锁
    /// 对应 bcachefs `BCH_ERR_transaction_restart_relock_path`
    pub fn trigger_relock_path(&mut self) {
        self.request_restart(RestartReason::RelockPath);
    }

    /// 触发 Upgrade 重启 — 锁升级失败
    /// 对应 bcachefs `BCH_ERR_transaction_restart_upgrade`
    pub fn trigger_upgrade(&mut self) {
        self.request_restart(RestartReason::Upgrade);
    }

    /// 触发 FaultInject 重启 — 故障注入测试
    /// 对应 bcachefs `BCH_ERR_transaction_restart_fault_inject`
    pub fn trigger_fault_inject(&mut self) {
        self.request_restart(RestartReason::FaultInject);
    }

    /// 触发 Nested 重启 — 嵌套事务重启
    /// 对应 bcachefs `BCH_ERR_transaction_restart_nested`
    pub fn trigger_nested(&mut self) {
        self.request_restart(RestartReason::Nested);
    }

    /// 触发 LockWaitlistAlloc 重启 — 等待列表分配失败
    /// 对应 bcachefs `BCH_ERR_transaction_restart_lock_waitlist_alloc`
    pub fn trigger_lock_waitlist_alloc(&mut self) {
        self.request_restart(RestartReason::LockWaitlistAlloc);
    }

    /// 触发 MemoryRealloced 重启 — 内存重分配（路径表扩容）
    /// 对应 bcachefs `BCH_ERR_transaction_restart_mem_realloced`
    pub fn trigger_mem_realloced(&mut self) {
        self.request_restart(RestartReason::MemoryRealloced);
    }

    /// 检查所有 iter 的路径完整性 — 若任何路径可能失效，触发 NodeSplit 重启
    ///
    /// 对应 bcachefs `__bch2_btree_path_verify()` (iter.c:378-396) 的简化轻量版本。
    /// bcachefs 的 verify 更严格：验证每个 level 的节点指针一致性、锁状态、btree_key 位置等，
    /// 仅在 `CONFIG_BCACHEFS_DEBUG` + `bch2_debug_check_iterators` 启用时生效。
    /// subvol 版本始终运行，仅检查路径长度和空路径——足以判断是否需要重启。
    ///
    /// 遍历每个 iter 的 path levels，检查：
    /// - path 是否为空的（空路径表示未正确初始化）
    /// - 当 tree depth 变化时，path 数量可能不匹配
    pub fn check_path_integrity(&mut self, tree_depth: u8) {
        for iter in &self.iters {
            let path = self.path_ref(iter.path);
            let actual_len = path
                .levels
                .iter()
                .filter(|node| matches!(node, BtreePathNode::Node(_)))
                .count();
            if actual_len == 0 {
                self.trigger_node_read_required();
                return;
            }
            let expected_len = (tree_depth as usize) + 1;
            if actual_len != expected_len {
                self.trigger_node_split();
                return;
            }
        }
    }

    /// 检测 iter 路径是否需要重启（通过 had_restart 标志）
    ///
    /// 对应 bcachefs `trans->restarted` 标志检测的 subvol 等效。
    /// bcachefs 使用 `trans->restarted` 单一标志位（由 `btree_trans_restart()` iter.h:613 设置），
    /// 重启循环检查此标志决定是否 retry。subvol 使用每个 iter 粒度的 `had_restart` 标志，
    /// 使调用者可以精确知道哪个 iter 触发了重启。
    ///
    /// 返回 true 表示检测到任何 iter 请求重启。
    pub fn detect_iter_restart_needed(&mut self) -> bool {
        for iter in &mut self.iters {
            if iter.had_restart {
                iter.had_restart = false;
                self.request_restart(RestartReason::LockConflict);
                return true;
            }
        }
        false
    }

    // ─── 原有方法 ────────────────────────────────────────────

    /// 事务持有的 iter 数量
    pub fn iter_count(&self) -> usize {
        self.iters.len()
    }

    /// 是否已提交
    pub fn is_committed(&self) -> bool {
        self.committed
    }

    /// 设置 journal 序列号
    pub fn set_journal_seq(&mut self, seq: u64) {
        self.journal_seq = seq;
    }

    /// 获取 journal 序列号
    pub fn journal_seq(&self) -> u64 {
        self.journal_seq
    }

    // ─── Phase B2: WAL Pin 集成 ─────────────────────────────

    /// 设置当前事务持有的 WAL pin ID（由 Volume 层在写 WAL 时调用）
    pub fn set_wal_pin(&mut self, pin_id: u64) {
        self.wal_pin_id = Some(pin_id);
    }

    /// 清除当前事务持有的 WAL pin ID（由 Volume 层在写 WAL 后调用）
    pub fn clear_wal_pin(&mut self) {
        self.wal_pin_id = None;
    }

    /// 获取当前事务持有的 WAL pin ID
    pub fn wal_pin_id(&self) -> Option<u64> {
        self.wal_pin_id
    }

    // ─── Phase 2 Journal ──────────────────────────────────────

    /// 记录插入操作到 journal（调用者在 btree.insert 成功后调用）
    ///
    /// 对应 bcachefs `bch2_trans_update()` / `bch2_btree_insert()`。
    /// `iter_idx` 是 `bch2_trans_get_iter()` 返回的索引，`level` 默认为 0（leaf）。
    pub fn bch2_trans_update(
        &mut self,
        btree_type: BtreeId,
        level: u8,
        cached: bool,
        key: BtreeKey,
        value: BchVal,
        iter_idx: usize,
    ) {
        let sort_order = btree_trigger_order(btree_type);
        let old_raw_value = self.snapshot_old_raw_value(btree_type, &key);

        self.journal.push(BtreeTransEntry {
            op: BtreeOp::Insert,
            btree_id: btree_type,
            level,
            cached,
            key,
            value,
            raw_value: None,
            old_key: None,
            old_value: None,
            old_raw_value,
            insert_trigger_run: false,
            overwrite_trigger_run: false,
            sort_order,
            iter_idx,
            path_idx: PATH_IDX_INVALID,
            old_btree_u64s: 0,
        });
    }

    fn snapshot_old_raw_value(&self, btree_type: BtreeId, key: &BtreeKey) -> Option<Vec<u8>> {
        let vol = self.ctx_vol?;
        match vol.get_entry_raw(btree_type, Bpos::from_key(key))?.value {
            KeyValue::Raw(bytes) => Some(bytes),
            _ => None,
        }
    }

    /// 记录原始值插入操作到 journal（用于非 extent 数据如 snapshot 序列化数据）
    pub fn bch2_trans_update_raw(
        &mut self,
        btree_type: BtreeId,
        level: u8,
        cached: bool,
        key: BtreeKey,
        raw_value: Vec<u8>,
        iter_idx: usize,
    ) {
        let sort_order = btree_trigger_order(btree_type);
        let old_raw_value = self.snapshot_old_raw_value(btree_type, &key);

        self.journal.push(BtreeTransEntry {
            op: BtreeOp::Insert,
            btree_id: btree_type,
            level,
            cached,
            key,
            value: BchVal::new(0, 0),
            raw_value: Some(raw_value),
            old_key: None,
            old_value: None,
            old_raw_value,
            insert_trigger_run: false,
            overwrite_trigger_run: false,
            sort_order,
            iter_idx,
            path_idx: PATH_IDX_INVALID,
            old_btree_u64s: 0,
        });
    }

    /// 记录删除操作到 journal（调用者在 btree.delete 成功后调用）
    ///
    /// bcachefs 对齐：`bch2_btree_delete_at()` (btree/update.c:1312) 创建 KEY_TYPE_deleted key
    /// 并通过 `bch2_trans_update()` 插入。subvol 统一为 Insert 条目 + KeyType::Deleted 标记。
    pub fn bch2_trans_delete(
        &mut self,
        btree_type: BtreeId,
        level: u8,
        cached: bool,
        mut key: BtreeKey,
        iter_idx: usize,
    ) {
        let sort_order = btree_trigger_order(btree_type);
        let old_raw_value = self.snapshot_old_raw_value(btree_type, &key);
        key.key_type = KeyType::Deleted;
        self.journal.push(BtreeTransEntry {
            op: BtreeOp::Insert,
            btree_id: btree_type,
            level,
            cached,
            key,
            value: BchVal::new(0, 0),
            raw_value: None,
            old_key: None,
            old_value: None,
            old_raw_value,
            insert_trigger_run: false,
            overwrite_trigger_run: false,
            sort_order,
            iter_idx,
            path_idx: PATH_IDX_INVALID,
            old_btree_u64s: 0,
        });
    }

    /// 记录 whiteout 操作到 journal
    pub fn journal_whiteout(
        &mut self,
        btree_type: BtreeId,
        level: u8,
        cached: bool,
        key: BtreeKey,
        value: BchVal,
        iter_idx: usize,
    ) {
        let sort_order = btree_trigger_order(btree_type);
        let old_raw_value = self.snapshot_old_raw_value(btree_type, &key);
        self.journal.push(BtreeTransEntry {
            op: BtreeOp::Whiteout,
            btree_id: btree_type,
            level,
            cached,
            key,
            value,
            raw_value: None,
            old_key: None,
            old_value: None,
            old_raw_value,
            insert_trigger_run: false,
            overwrite_trigger_run: false,
            sort_order,
            iter_idx,
            path_idx: PATH_IDX_INVALID,
            old_btree_u64s: 0,
        });
    }

    /// 取出所有 journal 条目（事务 commit/rollback 后由调用者消费）
    ///
    /// 返回 `Vec<BtreeTransEntry>` 列表。
    /// 调用者应写入 WAL，可根据 `entry.btree_id` 决定写入哪个 journal/bucket。
    pub fn drain_journal(&mut self) -> Vec<BtreeTransEntry> {
        std::mem::take(&mut self.journal)
    }

    /// journal 是否为空
    pub fn journal_is_empty(&self) -> bool {
        self.journal.is_empty()
    }

    /// journal 条目数
    pub fn journal_len(&self) -> usize {
        self.journal.len()
    }

    /// 返回 journal 中所有快照 ID（仅供 `bch2_snapshot_next_id` 使用）
    pub fn journal_snapshot_ids(&self) -> Vec<u32> {
        self.journal
            .iter()
            .filter(|e| e.btree_id == crate::btree::BtreeId::Snapshots)
            .map(|e| e.key.get_snapshot_id())
            .collect()
    }

    /// 应用 journal 条目到 btree 节点（通过 `ctx_vol` 直接写入）。
    /// 用于 snapshot 操作等同步写路径（不运行触发器、不上锁）。
    // ─── bcachefs 对齐方法 ─────────────────────────────────

    /// 重置更新队列（bcachefs 对齐：`bch2_trans_reset_updates()`）
    ///
    /// 对应 update.h:557-571。释放每个更新条目引用的 path，
    /// 清空 journal 条目计数。
    pub fn bch2_trans_reset_updates(&mut self) {
        // 先收集所有 path_idx，避免借用冲突
        let path_indices: Vec<PathIdx> = self
            .journal
            .iter()
            .filter(|e| e.path_idx != PATH_IDX_INVALID)
            .map(|e| e.path_idx)
            .collect();
        for path_idx in path_indices {
            self.path_put(path_idx, true);
        }
        self.journal.clear();
        self.fs_usage_delta = BchFsUsageBase::default();
        self.accounting_undo = None;
    }

    /// 路径排序 — 对应 bcachefs `__bch2_btree_trans_sort_paths()` (iter.c:3575-3613)
    ///
    /// 使用 Cocktail shaker sort（双向冒泡），因为迭代器通常是"几乎有序"的。
    /// 按 (btree_id, cached, pos, -level) 排序，确保锁获取顺序一致。
    fn btree_trans_sort_paths(&mut self) {
        if self.paths_sorted {
            return;
        }

        let n = self.sorted.len();
        if n <= 1 {
            if n == 1 {
                if let Some(ref mut path) = self.paths[self.sorted[0] as usize] {
                    path.sorted_idx = 0;
                }
            }
            self.paths_sorted = true;
            return;
        }

        // 预提取排序键（避免后面与 path 读操作冲突）
        struct SortKey {
            idx: PathIdx,
            btree_id: u8,
            cached: bool,
            pos: (u64, u64, u32),
            level: u8,
        }
        let keys: Vec<SortKey> = self
            .paths
            .iter()
            .enumerate()
            .filter_map(|(idx, slot)| {
                slot.as_ref().map(|p| SortKey {
                    idx: idx as PathIdx,
                    btree_id: p.btree_id as u8,
                    cached: p.cached,
                    pos: (p.pos.inode, p.pos.offset, p.pos.snapshot),
                    level: p.level,
                })
            })
            .collect();

        let key_for = |idx: PathIdx| -> &SortKey { keys.iter().find(|k| k.idx == idx).unwrap() };

        // Cocktail shaker sort (iter.c:3587-3607)
        let mut l = 0i32;
        let mut r = n as i32;
        let mut inc = 1i32;
        loop {
            let mut swapped = false;
            let (start, end): (i32, i32) = if inc > 0 { (l, r - 1) } else { (r - 2, l - 1) };
            let mut i = start;
            while (inc > 0 && i < end) || (inc < 0 && i > end) {
                let a = self.sorted[i as usize];
                let b = self.sorted[(i + 1) as usize];
                let ka = key_for(a);
                let kb = key_for(b);
                if ka
                    .btree_id
                    .cmp(&kb.btree_id)
                    .then_with(|| ka.cached.cmp(&kb.cached))
                    .then_with(|| ka.pos.cmp(&kb.pos))
                    .then_with(|| kb.level.cmp(&ka.level))
                    .is_gt()
                {
                    self.sorted.swap(i as usize, (i + 1) as usize);
                    swapped = true;
                }
                i += inc;
            }
            if !swapped {
                break;
            }
            if inc > 0 {
                r -= 1;
            } else {
                l += 1;
            }
            inc = -inc;
        }

        // 更新 sorted_idx
        for (order, &idx) in self.sorted.iter().enumerate() {
            if let Some(ref mut path) = self.paths[idx as usize] {
                path.sorted_idx = order as PathIdx;
            }
        }

        self.paths_sorted = true;
    }

    /// bcachefs 对齐的 `bch2_trans_begin()` (iter.c:3887-4004)
    ///
    /// 完整的事务重置流程：
    /// 1. 重置更新队列（release updates' path refs + clear nr_updates）
    /// 2. 递增重启计数器
    /// 3. 重置所有路径状态（should_be_locked=false, 释放 ref=0 的路径）
    pub fn bch2_trans_begin(&mut self) -> u32 {
        let restarted = self.needs_restart;

        self.bch2_trans_reset_updates();
        self.restart_count += 1;
        self.committed = false;
        self.journal_seq = 0;
        self.fs_usage_delta = BchFsUsageBase::default();
        self.accounting_undo = None;

        let path_indices: Vec<PathIdx> = self.path_idx_iter().collect();
        for path_idx in path_indices {
            let path = self.path_mut(path_idx);
            path.should_be_locked = false;

            if !restarted && path.btree_id != BtreeId::Subvolumes {
                path.preserve = false;
            }

            if path.ref_count == 0 && !path.preserve {
                self.__bch2_path_free(path_idx);
            } else {
                self.path_mut(path_idx).preserve = false;
            }
        }

        // C iter.c:3970-3971 — 只有新事务尝试才刷新 waiter 排序游标；
        // restart 必须保留原值，使同一事务在 waitlist 中保持原有年龄。
        if !restarted {
            self.locking_wait.trans_start_time =
                NEXT_TRANS_START_TIME.fetch_add(1, Ordering::Relaxed);
        }

        if restarted {
            let _ = self.bch2_btree_path_traverse_all();
            self.notrace_relock_fail = false;
        }

        self.needs_restart = false;
        self.restart_reason = None;
        self.trans_set_locked(false);
        // 注册死锁检测回调 — 后续 lock_slowpath 的 park 循环会调用
        self.sx_register_deadlock_detection();
        self.restart_count
    }

    /// 对应本地 bcachefs `bch2_trans_unlock()`
    /// (`locking.c:1440-1453,1524-1541`)。
    pub fn bch2_trans_unlock(&mut self) {
        self.trans_set_unlocked(0);

        let path_indices: Vec<PathIdx> = self.path_idx_iter().collect();
        for path_idx in path_indices {
            self.__bch2_btree_path_unlock(path_idx);
        }

        if !self.queued_write_bios.is_empty() {
            self.bch2_trans_submit_write_bios();
        }
        if self.btree_cache_cannibalize_locked {
            if let Some(cache) = self
                .cache
                .clone()
                .or_else(|| self.iters.first().map(|iter| Arc::clone(&iter.cache)))
            {
                cache.cache().bch2_btree_cache_cannibalize_unlock();
            }
            self.btree_cache_cannibalize_locked = false;
        }
        self.write_locked = false;
        // 注销死锁检测回调 — 锁已释放，无需再检测
        Self::sx_unregister_deadlock_detection();
    }

    /// 对应本地 bcachefs `bch2_trans_submit_write_bios()`
    /// (`write.c:648-660`)：先从 transaction 摘链，再按链表顺序提交。
    fn bch2_trans_submit_write_bios(&mut self) {
        let mut bios = std::mem::take(&mut self.queued_write_bios);
        while let Some(bio) = bios.pop() {
            submit_bio_write(bio);
        }
    }

    /// 非阻塞重入之前通过 `bch2_trans_unlock()` 释放的锁
    ///
    /// 使用 `six_relock_*` + `locked_seq` 验证，若节点未被外部修改则可快速重入，
    /// 无需完整重启遍历树。错误保留本地 bcachefs
    /// `BCH_ERR_transaction_restart_relock` 的身份。
    ///
    /// 对应 bcachefs `__bch2_trans_relock()` (locking.c:1487-1517)。
    fn __bch2_trans_relock(&mut self, _trace: bool) -> Result<(), RestartReason> {
        // 对应 C: locking.c:1491 — restarted 检查
        if self.needs_restart {
            return Err(self.restart_reason.unwrap_or(RestartReason::Relock));
        }

        for path_idx in 1..self.paths.len() as PathIdx {
            let Some(path) = self.paths[path_idx as usize].as_ref() else {
                continue;
            };
            // 对应 C: locking.c:1500 — 仅重锁 should_be_locked 的路径
            if !path.should_be_locked {
                continue;
            }
            if !self.bch2_btree_path_relock_norestart(path_idx) {
                self.bch2_trans_unlock();
                self.needs_restart = true;
                self.restart_reason = Some(RestartReason::Relock);
                return Err(RestartReason::Relock);
            }
        }
        // 对应 C: locking.c:1514 — trans_set_locked(trans, true)
        self.trans_set_locked(true);
        Ok(())
    }

    /// 对应本地 bcachefs `bch2_trans_relock_notrace()`
    /// (`locking.c:1519-1522`)。
    pub fn bch2_trans_relock_notrace(&mut self) -> Result<(), RestartReason> {
        self.__bch2_trans_relock(false)
    }

    /// 提交事务并写入 journal（bcachefs 对齐：`__bch2_trans_commit()`）
    ///
    /// 执行 bcachefs 完全对齐的 4 阶段事务提交流程:
    /// Phase 1: 预计算 journal 条目大小并保留空间 → `journal_res_get()`
    /// Phase 2: 修改 btree 节点（使用已保留的 seq）
    /// Phase 3: 填充 journal 条目到已保留空间 → `add_entry()`
    /// Phase 4: 释放保留 → `journal_res_put()`（refcount→0 自动触发写）
    ///
    /// bcachefs 顺序保证：
    /// 如果 journal 保留失败，btree 不会被修改。
    /// 如果 btree 修改后崩溃，journal 条目从未写入 bucket，
    /// recovery 不会看到未应用的条目（线性化保证）。
    ///
    /// # Bcachefs Phase 5 对齐说明: `trans_commit_to_journal_replay_pre/post()`
    ///
    /// bcachefs 在 `do_bch2_trans_commit()` (commit.c:1291-1319) 中在
    /// `bch2_trans_lock_write` 和 `bch2_trans_commit_write_locked` 前后执行:
    /// - `trans_commit_to_journal_replay_pre()`: 获取 overwrite_lock + 检查 journal_keys
    /// - `trans_commit_to_journal_replay_post()`: 标记 overwritten + 释放 overwrite_lock
    ///
    /// 提交路径直接按本地 bcachefs transaction lock、journal
    /// reservation 和 btree materialization 顺序执行。
    ///
    pub fn bch2_trans_commit(&mut self) -> Result<u64, StorageError> {
        // ─── Pre-loop: btree 写节流（bcachefs commit.c:1397-1403） ─────
        if self.watermark.to_bits() <= Watermark::Normal.to_bits() && !self.journal.is_empty() {
            let throttle_cache = self.cache_for(BtreeId::Extents);
            while throttle_cache.should_throttle() {
                futures::executor::block_on(throttle_cache.wait_throttle());
            }
        }

        let vol = self
            .ctx_vol
            .ok_or_else(|| StorageError::JournalError("no transaction context".into()))?;

        if self.journal.is_empty() {
            // 无 journal 条目 → 只运行触发器管线
            self.__bch2_trans_commit()?;
            return Ok(0);
        }

        // 对应本地 bcachefs `__bch2_trans_commit()`：事务性触发器和提交
        // hook 在 retry 前执行；atomic trigger 必须延后到 journal
        // reservation 之后的 write-lock 区域。
        self.bch2_trans_commit_run_triggers()?;
        if !self.commit_hooks.is_empty() {
            self.run_commit_hooks()?;
        }

        // ─── Phase 1a: 按 BtreeId 分组 journal 条目 ───────────────────
        let mut groups: HashMap<BtreeId, Vec<BtreeEntry>> = HashMap::new();
        for je in self.journal.iter() {
            let key_type = if je.op == BtreeOp::Whiteout {
                KeyType::Whiteout
            } else {
                je.key.key_type
            };
            let value = if let Some(raw) = &je.raw_value {
                crate::btree::key::KeyValue::Raw(raw.clone())
            } else {
                crate::btree::key::KeyValue::Extent(crate::btree::key::ExtentValue {
                    paddr: je.value.paddr.get(),
                    size: 1,
                    ver: je.value.ver,
                    dev_idx: 0,
                    crc32c: 0,
                    crc_offset_blocks: 0,
                })
            };
            let group_entry = BtreeEntry {
                pos: Bpos::from_key(&je.key),
                key_type,
                needs_whiteout: false,
                value,
            };
            groups.entry(je.btree_id).or_default().push(group_entry);
        }

        // 构建 JsetEntry 列表（每个 btree 一组）
        // write buffer btree 使用 WriteBufferKeys 类型，非 wb btree 使用 BtreeKeys
        let mut jset_entries: Vec<RawJsetEntry> = match groups
            .into_iter()
            .map(|(bt, entries)| {
                let entries_bytes =
                    bincode::serialize(&entries).map_err(|e| StorageError::Serialization(e))?;
                let entry_type = if matches!(
                    bt,
                    BtreeId::Accounting
                        | BtreeId::Lru
                        | BtreeId::NeedDiscard
                        | BtreeId::Backpointers
                        | BtreeId::DeletedInodes
                        | BtreeId::ReconcileWork
                        | BtreeId::ReconcileHipri
                        | BtreeId::ReconcilePending
                        | BtreeId::ReconcileWorkPhys
                        | BtreeId::ReconcileHipriPhys
                        | BtreeId::StripeBackpointers
                ) {
                    JsetEntryType::WriteBufferKeys as u8
                } else {
                    JsetEntryType::BtreeKeys as u8
                };
                RawJsetEntry::new(bt as u8, entry_type, entries_bytes, 0)
            })
            .collect::<Result<Vec<_>, StorageError>>()
        {
            Ok(entries) => entries,
            Err(err) => {
                self.revert_disk_usage_accounting();
                return Err(err);
            }
        };

        // 对应本地 `do_bch2_trans_commit()`：journal reservation、journal
        // 写入和 btree materialize 必须在同一 transaction write lock 生命周期内。
        let seq = self.do_bch2_trans_commit(vol, &mut jset_entries)?;

        // The complete journal record is now published; committed usage is
        // no longer eligible for transaction-local rollback.
        self.accounting_undo = None;

        Ok(seq)
    }

    /// 对应 bcachefs `bch2_trans_commit_write_locked()` (commit.c:1059-1285)
    ///
    /// 在 write lock 保护下执行（需在调用前持有 write lock）：
    /// 1. journal reservation
    /// 2. 原子触发器和 journal entry 构建
    /// 3. journal 写入
    /// 4. btree materialize
    ///
    /// 返回 `(JournalRes, seq)`，调用者负责 `journal_res_put` 和 `unlock_write`。
    pub fn bch2_trans_commit_write_locked(
        &mut self,
        vol: &BchVol,
        jset_entries: &mut Vec<RawJsetEntry>,
    ) -> Result<(JournalRes, u64), StorageError> {
        let ctx_journal = vol.journal_ref();
        let last_seq = ctx_journal.last_seq_ondisk.load(Ordering::Acquire);

        // ─── Phase 1b: 计算 journal 空间需求 ─────────────────
        let data_size = std::mem::size_of::<JsetHeader>()
            + jset_entries
                .iter()
                .map(|e| std::mem::size_of::<JsetEntryHeader>() + e.payload.len())
                .sum::<usize>();
        let block_size = JSET_BLOCK_SIZE as usize;
        let pad = (block_size - (data_size % block_size)) % block_size;
        let base_u64s = (data_size + pad).div_ceil(8);
        let alloc_updates = self
            .journal
            .iter()
            .filter(|entry| entry.btree_id == BtreeId::Alloc)
            .count();
        let extra_u64s = alloc_updates
            .saturating_mul(2)
            .saturating_mul(
                (std::mem::size_of::<JsetEntryHeader>()
                    + bincode::serialized_size(&vec![BtreeEntry {
                        pos: Bpos::new(0, 0, 0),
                        key_type: KeyType::Set,
                        needs_whiteout: false,
                        value: KeyValue::Raw(Vec::new()),
                    }])
                    .unwrap_or(0) as usize)
                    .div_ceil(8),
            )
            .saturating_add(block_size.div_ceil(8));
        let req_u64s = base_u64s.saturating_add(extra_u64s) as u32;

        // ─── Phase 1c: 保留 journal 空间（bcachefs step 1） ─────
        let mut res = match ctx_journal.bch2_journal_res_get(Watermark::Normal, req_u64s) {
            Ok(res) => res,
            Err(e) => {
                return Err(StorageError::JournalError(e.to_string()));
            }
        };
        let seq = res.seq;
        self.journal_seq = seq;
        let atomic_journal_start = self.journal.len();

        self.committed = true;
        if self.fs_usage_delta.hidden
            | self.fs_usage_delta.btree
            | self.fs_usage_delta.data
            | self.fs_usage_delta.cached
            | self.fs_usage_delta.reserved
            != 0
        {
            self.bch2_trans_account_disk_usage_change();
        }
        if let Err(err) = self.run_atomic_triggers() {
            ctx_journal.bch2_journal_res_put(&res);
            self.revert_disk_usage_accounting();
            return Err(err);
        }
        let atomic_journal = self.journal[atomic_journal_start..].to_vec();
        for entry in &atomic_journal {
            let entries = vec![BtreeEntry {
                pos: Bpos::from_key(&entry.key),
                key_type: entry.key.key_type,
                needs_whiteout: false,
                value: KeyValue::Raw(entry.raw_value.clone().unwrap_or_default()),
            }];
            let entries_bytes = match bincode::serialize(&entries) {
                Ok(b) => b,
                Err(e) => {
                    ctx_journal.bch2_journal_res_put(&res);
                    self.revert_disk_usage_accounting();
                    return Err(StorageError::Serialization(e));
                }
            };
            match RawJsetEntry::new(entry.btree_id as u8, JsetEntryType::WriteBufferKeys as u8, entries_bytes, 0) {
                Ok(e) => jset_entries.push(e),
                Err(e) => {
                    ctx_journal.bch2_journal_res_put(&res);
                    self.revert_disk_usage_accounting();
                    return Err(e);
                }
            }
        }
        let data_size = std::mem::size_of::<JsetHeader>()
            + jset_entries
                .iter()
                .map(|e| std::mem::size_of::<JsetEntryHeader>() + e.payload.len())
                .sum::<usize>();
        let pad = (block_size - (data_size % block_size)) % block_size;
        if self.fs_usage_delta.hidden
            | self.fs_usage_delta.btree
            | self.fs_usage_delta.data
            | self.fs_usage_delta.cached
            | self.fs_usage_delta.reserved
            != 0
        {
            self.bch2_trans_account_disk_usage_change();
        }

        // ─── Phase 1d: flush dirty key cache with real journal_seq ──
        vol.flush_cache_dirty_keys(seq);

        // ─── Phase 2: 逐条写入 journal（bcachefs trans_for_each_update 对齐） ──
        let mut hdr = JsetHeader {
            magic: JOURNAL_MAGIC,
            seq,
            last_seq,
            crc32: 0,
            entry_count: jset_entries.len() as u32,
            version: JSET_VERSION as u32,
            flags: CSUM_TYPE_NONE as u32,
            pad: [0u8; 24],
        };

        let header_bytes = unsafe {
            std::slice::from_raw_parts(
                &hdr as *const JsetHeader as *const u8,
                std::mem::size_of::<JsetHeader>(),
            )
        };
        let mut crc = crc32c(header_bytes, 0);
        for entry in jset_entries.iter() {
            let ehdr_bytes = unsafe {
                std::slice::from_raw_parts(
                    &entry.hdr as *const JsetEntryHeader as *const u8,
                    std::mem::size_of::<JsetEntryHeader>(),
                )
            };
            crc = crc32c(ehdr_bytes, crc);
            if !entry.payload.is_empty() {
                crc = crc32c(&entry.payload, crc);
            }
        }

        hdr.crc32 = crc;
        let header_bytes = unsafe {
            std::slice::from_raw_parts(
                &hdr as *const JsetHeader as *const u8,
                std::mem::size_of::<JsetHeader>(),
            )
        };
        ctx_journal.bch2_journal_add_raw(&mut res, header_bytes);

        for entry in jset_entries.iter() {
            let payload_u64s = entry.payload.len().div_ceil(8) as u32;
            ctx_journal.bch2_journal_add_entry(
                &mut res,
                entry.hdr.entry_type,
                entry.hdr.btree_type,
                entry.hdr.level,
                payload_u64s,
                &entry.payload,
            );
        }

        if pad > 0 {
            ctx_journal.bch2_journal_add_raw(&mut res, &vec![0u8; pad]);
        }

        // ─── Phase 3: 修改 btree 节点（bcachefs commit.c:1258-1267） ─────
        for entry in self.journal.iter() {
            if matches!(
                entry.btree_id,
                BtreeId::Accounting
                    | BtreeId::Lru
                    | BtreeId::NeedDiscard
                    | BtreeId::Backpointers
                    | BtreeId::DeletedInodes
                    | BtreeId::ReconcileWork
                    | BtreeId::ReconcileHipri
                    | BtreeId::ReconcilePending
                    | BtreeId::ReconcileWorkPhys
                    | BtreeId::ReconcileHipriPhys
                    | BtreeId::StripeBackpointers
            ) {
                continue;
            }
            if let Some(ref raw_bytes) = entry.raw_value {
                let btree_entry = BtreeEntry::new(
                    Bpos::from_key(&entry.key),
                    entry.key.key_type,
                    KeyValue::Raw(raw_bytes.clone()),
                );
                vol.insert_entry_raw(entry.btree_id, btree_entry, seq);
            } else if entry.key.key_type == KeyType::Deleted {
                let btree = vol.btree_mut(entry.btree_id);
                let delete_key = entry.key;
                btree.bch2_btree_bset_insert_key_wrapper(
                    BtreeEntry::raw(Bpos::from_key(&delete_key), KeyType::Deleted, Vec::new()),
                    seq,
                );
            } else {
                let _ =
                    futures::executor::block_on(vol.btree_mut(entry.btree_id).bch2_btree_insert(
                        &NoopWriter,
                        entry.key,
                        entry.value,
                        seq,
                ));
            }
        }

        Ok((res, seq))
    }

    /// 对齐 bcachefs `do_bch2_trans_commit()` (commit.c:1291-1321)
    ///
    /// 包装器：获取 write lock → 调用 `bch2_trans_commit_write_locked()`
    /// → 释放 journal 保留 → 释放 write lock。
    fn do_bch2_trans_commit(
        &mut self,
        vol: &BchVol,
        jset_entries: &mut Vec<RawJsetEntry>,
    ) -> Result<u64, StorageError> {
        self.bch2_trans_lock_write()?;

        let (res, seq) = self.bch2_trans_commit_write_locked(vol, jset_entries)?;

        vol.journal_ref().bch2_journal_res_put(&res);
        self.bch2_trans_unlock_write();

        Ok(seq)
    }
}

impl<'ctx> Default for BtreeTrans<'ctx> {
    fn default() -> Self {
        Self::new_with_cache(Arc::new(NodeCache::new()))
    }
}

impl<'ctx> std::fmt::Debug for BtreeTrans<'ctx> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BtreeTrans")
            .field("iters", &self.iters.len())
            .field("journal_seq", &self.journal_seq)
            .field("journal", &self.journal.len())
            .field("committed", &self.committed)
            .field("restart_count", &self.restart_count)
            .field("needs_restart", &self.needs_restart)
            .field("wal_pin_id", &self.wal_pin_id)
            .finish()
    }
}

/// 宏：锁重启循环 — 对应 bcachefs `lockrestart_do()`
///
/// 在事务的重启循环中执行闭包 body。当 body 返回 `Err(RestartReason)` 时，
/// 宏自动调用事务的 `request_restart()` + `restart()` 并重试 body。
/// 当重启次数超过 `MAX_RESTARTS` 时，返回 `StorageError::TransactionRestartLimit`。
///
/// # 用法
///
/// ```text
/// lockrestart_do!(trans, {
///     let iter = trans.bch2_trans_get_iter(&root, &key, true, BtreeId::Extents);
///     if iter.is_empty() {
///         return Err(RestartReason::KeyCacheMiss);
///     }
///     // ... perform safe operations ...
///     Ok(())
/// })?;
/// ```
///
/// # 幂等性要求
///
/// **body 可能被多次执行**。所有操作必须满足：
/// - **幂等**：多次执行与一次执行结果一致
/// - **无副作用**：body 内对外部状态的修改（如资源分配）在重启后可能丢失
/// - **资源安全**：如果在 body 中分配了外部资源（如 bucket），必须在 body 退出前回滚
/// - **最佳实践**：body 仅做"检查 + 计算"，真正的写入在 body 返回后由调用者完成
#[macro_export]
macro_rules! lockrestart_do {
    ($trans:expr, $body:block) => {{
        loop {
            match (|| -> Result<_, RestartReason> { $body })() {
                Ok(result) => break Ok(result),
                Err(reason) => {
                    $trans.request_restart(reason);
                    if $trans.restart().is_none() {
                        break Err(StorageError::TransactionRestartLimit(
                            $trans.restart_count().into(),
                        ));
                    }
                }
            }
        }
    }};
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btree::btree::Btree;
    use crate::btree::iter::IterFlags;
    use crate::btree::key::{BchVal, Bpos, BtreeKey, KeyType};
    use crate::btree::node::BtreeNode;
    use crate::btree::types::{BtreePathLevel, BtreeRoot, NodeCache};
    use crate::btree::writer::NoopWriter;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    fn make_root() -> BtreeRoot {
        BtreeRoot {
            node: Arc::new(BtreeNode::new_leaf()),
            depth: 0,
        }
    }

    fn make_transaction() -> BtreeTrans<'static> {
        BtreeTrans::new_with_cache(Arc::new(NodeCache::new()))
    }

    #[test]
    fn test_trans_account_disk_usage_consumes_reservation_in_order() {
        let vol = BchVol::test_trees();
        let capacity = unsafe { &mut *vol.capacity.get() };
        capacity.sectors_available.store(1_000, Ordering::Release);
        capacity.pcpu[0].online_reserved = 100;

        let mut trans = BtreeTrans::new(&vol);
        let reservation = DiskReservation {
            sectors: 100,
            gen: 0,
            nr_replicas: 1,
        };
        trans.set_disk_reservation(reservation);
        trans.fs_usage_add(UsageField::Data, 80);
        trans.bch2_trans_account_disk_usage_change();

        let capacity = unsafe { &*vol.capacity.get() };
        assert_eq!(capacity.pcpu[0].usage.data, 80);
        assert_eq!(capacity.sectors_available.load(Ordering::Acquire), 1_000);
        assert_eq!(trans.disk_reservation_sectors(), 20);
        assert_eq!(capacity.pcpu[0].online_reserved, 20);
        assert_eq!(trans.fs_usage_delta(), BchFsUsageBase::default());
    }

    #[test]
    fn test_trans_account_disk_usage_clamps_unreserved_positive_delta_once() {
        let vol = BchVol::test_trees();
        let capacity = unsafe { &mut *vol.capacity.get() };
        capacity.sectors_available.store(1_000, Ordering::Release);
        capacity.pcpu[0].online_reserved = 10;

        let mut trans = BtreeTrans::new(&vol);
        trans.set_disk_reservation(DiskReservation {
            sectors: 10,
            gen: 0,
            nr_replicas: 1,
        });
        trans.fs_usage_add(UsageField::Data, 25);
        trans.bch2_trans_account_disk_usage_change();

        let capacity = unsafe { &*vol.capacity.get() };
        assert_eq!(capacity.pcpu[0].usage.data, 25);
        assert_eq!(capacity.sectors_available.load(Ordering::Acquire), 985);
        assert_eq!(trans.disk_reservation_sectors(), 0);
        assert_eq!(capacity.pcpu[0].online_reserved, 0);
    }

    #[test]
    fn test_transaction_new() {
        let t = make_transaction();
        assert!(!t.is_committed());
        assert_eq!(t.iter_count(), 0);
        assert_eq!(t.restart_count(), 0);
        assert!(!t.needs_restart());
    }

    #[test]
    fn test_path_zero_is_reserved_sentinel() {
        let mut t = make_transaction();

        assert_eq!(PATH_IDX_INVALID, 0);
        assert!(t.paths[PATH_IDX_INVALID as usize].is_none());
        assert_eq!(t.path_alloc(PATH_IDX_INVALID), 1);
        assert_eq!(t.path_idx_iter().collect::<Vec<_>>(), vec![1]);
    }

    #[test]
    fn test_path_bitmap_tracks_indices_above_128() {
        let mut t = make_transaction();
        let allocated: Vec<PathIdx> = (0..130).map(|_| t.path_alloc(PATH_IDX_INVALID)).collect();

        assert_eq!(allocated.first(), Some(&1));
        assert_eq!(allocated.last(), Some(&130));
        assert_eq!(t.path_idx_iter().collect::<Vec<_>>(), allocated);
    }

    #[test]
    fn test_registered_path_slots_are_indexed_leaf_first() {
        let mut t = make_transaction();
        let key = BtreeKey::new(100, 1, KeyType::Normal);
        let root_level = BtreePathLevel::new(Arc::new(BtreeNode::new(1)));
        let leaf_level = BtreePathLevel::new(Arc::new(BtreeNode::new_leaf()));
        let path_idx = t.path_alloc(PATH_IDX_INVALID);
        {
            let path = t.path_mut(path_idx);
            path.pos = key.to_bpos();
            path.btree_id = BtreeId::Extents;
            path.level = 1;
            path.ref_count = 1;
            path.intent_ref = 0;
            path.levels[0] = BtreePathNode::Node(leaf_level);
            path.levels[1] = BtreePathNode::Node(root_level);
        }
        let iter = BtreeIter::from_existing(
            &key,
            IterFlags::default(),
            t.cache_for(BtreeId::Extents),
            BtreeId::Extents,
            path_idx,
            &mut t.paths,
        );
        t.iters.push(iter);
        t.iter_types.push(BtreeId::Extents);

        let path = t.path_ref(path_idx);
        assert!(matches!(
            path.btree_path_node(0),
            Some(BtreePathNode::Node(level)) if level.node.level == 0
        ));
        assert!(matches!(
            path.btree_path_node(1),
            Some(BtreePathNode::Node(level)) if level.node.level == 1
        ));
        assert!(matches!(path.levels[2], BtreePathNode::None));
        assert!(matches!(path.levels[3], BtreePathNode::None));
    }

    #[test]
    fn test_cached_unlocked_path_keeps_srcu_reset_error_identity() {
        let mut t = make_transaction();
        let path_idx = t.path_alloc(PATH_IDX_INVALID);
        let path = t.path_mut(path_idx);
        path.cached = true;
        path.levels[0] = BtreePathNode::Node(BtreePathLevel::new(Arc::new(BtreeNode::new_leaf())));
        t.srcu_held = true;

        t.bch2_trans_unlock_long();

        assert!(matches!(
            t.path_ref(path_idx).levels[0],
            BtreePathNode::Error(BtreePathError::SrcuReset)
        ));
        assert!(!t.srcu_held);
    }

    #[test]
    fn test_traverse_all_rejects_recursive_entry() {
        let mut t = make_transaction();
        t.in_traverse_all = true;

        assert!(matches!(
            t.bch2_btree_path_traverse_all(),
            Err(BtreePathTraverseError::Restart(
                RestartReason::InTraverseAll
            ))
        ));
    }

    #[test]
    fn test_traverse_all_clears_guard_on_success() {
        let mut t = make_transaction();

        assert!(t.bch2_btree_path_traverse_all().is_ok());
        assert!(!t.in_traverse_all);
    }

    #[test]
    fn test_traverse_one_preserves_relock_path_restart_identity() {
        let mut t = make_transaction();
        let path_idx = t.path_alloc(PATH_IDX_INVALID);
        let node = Arc::new(BtreeNode::new_leaf());
        let mut level = BtreePathLevel::new(node.clone());
        level.locked_seq = node.lock.six_lock_seq().wrapping_sub(1);
        let path = t.path_mut(path_idx);
        path.levels[0] = BtreePathNode::Node(level);
        path.should_be_locked = true;

        assert!(matches!(
            t.bch2_btree_path_traverse_one(path_idx, IterFlags::default()),
            Err(BtreePathTraverseError::Restart(RestartReason::RelockPath))
        ));
        assert_eq!(t.restart_reason, Some(RestartReason::RelockPath));
        assert!(t.needs_restart);
        assert!(t.srcu_held);
    }

    #[test]
    fn test_traverse_one_relock_failure_restarts_from_saved_root() {
        let mut t = make_transaction();
        let path_idx = t.path_alloc(PATH_IDX_INVALID);
        let node = Arc::new(BtreeNode::new_leaf());
        let mut level = BtreePathLevel::new(node.clone());
        level.locked_seq = node.lock.six_lock_seq().wrapping_sub(1);
        t.path_mut(path_idx).levels[0] = BtreePathNode::Node(level);

        assert!(t
            .bch2_btree_path_traverse_one(path_idx, IterFlags::default())
            .is_ok());
        assert!(matches!(
            &t.path_ref(path_idx).levels[0],
            BtreePathNode::Node(level) if level.locked_seq == node.lock.six_lock_seq()
        ));
        assert_ne!(t.path_ref(path_idx).nodes_locked, 0);
    }

    #[test]
    fn test_traverse_one_reuses_parent_and_descends_to_leaf() {
        let cache = Arc::new(NodeCache::new());
        let child_addr = cache.alloc_addr();
        let mut child = BtreeNode::new_leaf();
        child.insert(BtreeKey::new(10, 1, KeyType::Normal), BchVal::new(100, 0));
        let child = Arc::new(child);
        cache.insert(child_addr, Arc::clone(&child));

        let mut root = BtreeNode::new_internal();
        assert!(root.insert(
            BtreeKey::new(100, 1, KeyType::Normal),
            BchVal::new(child_addr, 0),
        ));
        let root = Arc::new(root);

        let mut t = BtreeTrans::new_with_cache(cache);
        let path_idx = t.path_alloc(PATH_IDX_INVALID);
        let path = t.path_mut(path_idx);
        path.pos = BtreeKey::new(10, 1, KeyType::Normal).to_bpos();
        path.level = 0;
        path.levels[0] = BtreePathNode::Error(BtreePathError::Relock);
        path.levels[2] = BtreePathNode::None;
        t.bch2_btree_path_level_init(path_idx, 1, root);

        t.bch2_btree_path_traverse_one(path_idx, IterFlags::default())
            .unwrap();

        assert_eq!(t.path_ref(path_idx).level, 0);
        assert!(matches!(
            &t.path_ref(path_idx).levels[0],
            BtreePathNode::Node(level) if Arc::ptr_eq(&level.node, &child)
        ));
        assert_eq!(
            t.path_ref(path_idx).btree_node_locked_type(0),
            BtreeNodeLockedType::Read
        );
        assert_eq!(
            t.path_ref(path_idx).btree_node_locked_type(1),
            BtreeNodeLockedType::None
        );
    }

    #[test]
    fn test_traverse_one_cached_path_is_not_reported_uptodate() {
        let mut t = make_transaction();
        let path_idx = t.path_alloc(PATH_IDX_INVALID);
        let path = t.path_mut(path_idx);
        path.cached = true;
        path.levels[0] = BtreePathNode::Error(BtreePathError::Cached);

        assert!(matches!(
            t.bch2_btree_path_traverse_one(path_idx, IterFlags::default()),
            Err(BtreePathTraverseError::Storage(StorageError::InvalidData(
                _
            )))
        ));
        assert!(matches!(
            t.path_ref(path_idx).levels[0],
            BtreePathNode::Error(BtreePathError::Cached)
        ));
    }

    #[test]
    fn test_path_sort_updates_incremental_membership_indices() {
        let mut t = make_transaction();
        let first = t.path_alloc(PATH_IDX_INVALID);
        let second = t.path_alloc(PATH_IDX_INVALID);
        let third = t.path_alloc(PATH_IDX_INVALID);
        t.path_mut(first).pos = Bpos::new(30, 0, 0);
        t.path_mut(second).pos = Bpos::new(10, 0, 0);
        t.path_mut(third).pos = Bpos::new(20, 0, 0);

        t.btree_trans_sort_paths();

        assert_eq!(t.sorted, vec![second, third, first]);
        for (sorted_idx, path_idx) in t.sorted.iter().copied().enumerate() {
            assert_eq!(t.path_ref(path_idx).sorted_idx, sorted_idx as PathIdx);
        }
    }

    #[test]
    fn test_path_put_does_not_scan_past_immediate_neighbor() {
        let mut t = make_transaction();
        let source = t.path_alloc(PATH_IDX_INVALID);
        let neighbor = t.path_alloc(PATH_IDX_INVALID);
        let distant_match = t.path_alloc(PATH_IDX_INVALID);
        let shared_node = Arc::new(BtreeNode::new_leaf());

        for (path_idx, inode, node) in [
            (source, 10, shared_node.clone()),
            (neighbor, 20, Arc::new(BtreeNode::new_leaf())),
            (distant_match, 30, shared_node),
        ] {
            let path = t.path_mut(path_idx);
            path.pos = Bpos::new(inode, 0, 0);
            path.level = 0;
            path.levels[0] = BtreePathNode::Node(BtreePathLevel::new(node));
            t.path_get(path_idx, false);
        }
        t.path_mut(source).should_be_locked = true;
        t.btree_trans_sort_paths();

        t.path_put(source, false);

        assert!(t.paths[source as usize].is_some());
        assert_eq!(t.path_ref(source).ref_count, 0);
    }

    #[test]
    fn test_path_pool_stops_at_bcachefs_hard_limit() {
        let mut t = make_transaction();

        for expected in 1..BTREE_ITER_MAX as PathIdx {
            assert_eq!(t.path_alloc(PATH_IDX_INVALID), expected);
        }

        assert_eq!(t.path_alloc(PATH_IDX_INVALID), PATH_IDX_INVALID);
        assert_eq!(t.path_idx_iter().count(), BTREE_ITER_MAX - 1);
    }

    #[test]
    fn test_path_get_put_updates_bcachefs_refcounts() {
        let mut t = make_transaction();
        let path_idx = t.path_alloc(PATH_IDX_INVALID);

        t.path_get(path_idx, true);
        t.path_get(path_idx, false);
        assert_eq!(t.path_ref(path_idx).ref_count, 2);
        assert_eq!(t.path_ref(path_idx).intent_ref, 1);

        t.path_put(path_idx, false);
        assert_eq!(t.path_ref(path_idx).ref_count, 1);
        assert_eq!(t.path_ref(path_idx).intent_ref, 1);

        t.path_put(path_idx, true);
        assert!(t.paths[path_idx as usize].is_none());
    }

    #[test]
    #[should_panic(expected = "refcount overflow")]
    fn test_path_get_rejects_refcount_overflow() {
        let mut t = make_transaction();
        let path_idx = t.path_alloc(PATH_IDX_INVALID);
        t.path_mut(path_idx).ref_count = u8::MAX;
        t.path_get(path_idx, false);
    }

    #[test]
    #[should_panic(expected = "refcount underflow")]
    fn test_path_put_rejects_refcount_underflow() {
        let mut t = make_transaction();
        let path_idx = t.path_alloc(PATH_IDX_INVALID);
        t.path_put(path_idx, false);
    }

    #[test]
    #[should_panic(expected = "intent refcount underflow")]
    fn test_path_put_rejects_intent_refcount_underflow() {
        let mut t = make_transaction();
        let path_idx = t.path_alloc(PATH_IDX_INVALID);
        t.path_get(path_idx, false);
        t.path_put(path_idx, true);
    }

    #[test]
    fn test_transaction_bch2_trans_get_iter() {
        let root = make_root();
        let mut t = make_transaction();
        let key = BtreeKey::new(100, 1, KeyType::Normal);
        t.bch2_trans_get_iter(&root, &key, false, BtreeId::Extents);
        assert_eq!(t.iter_count(), 1);
    }

    #[test]
    fn test_bch2_btree_iter_set_pos_rebuilds_transaction_owned_path() {
        let vol = BchVol::test_trees();
        let root = vol.btree(BtreeId::Extents).root();
        let mut t = BtreeTrans::new_ro(&vol);
        let initial = BtreeKey::new(100, 7, KeyType::Normal);
        t.bch2_trans_get_iter(root, &initial, false, BtreeId::Extents);
        t.iter_mut(0)
            .expect("iterator missing")
            .set_snapshot_filter(7);

        t.bch2_btree_iter_set_pos(0, Bpos::new(200, 300, 0));

        let iter = t.iter(0).expect("iterator missing");
        let iter_pos = Bpos::from_key(&iter.pos);
        assert_eq!(iter_pos, Bpos::new(200, 300, 7));
        assert_eq!(iter.snapshot, 7);
        assert_eq!(t.path_ref(iter.path).pos, iter_pos);
    }

    #[test]
    fn test_transaction_begin_resets() {
        let _root = make_root();
        let mut t = make_transaction();
        let _key = BtreeKey::new(100, 1, KeyType::Normal);
        t.bch2_trans_begin();
        assert_eq!(t.iter_count(), 0);
    }

    #[test]
    fn test_bch2_trans_commit() {
        let root = make_root();
        let mut t = make_transaction();
        let key = BtreeKey::new(100, 1, KeyType::Normal);
        t.bch2_trans_get_iter(&root, &key, false, BtreeId::Extents);
        assert!(t.__bch2_trans_commit().is_ok());
        assert!(t.is_committed());
    }

    #[test]
    fn test_transaction_rollback() {
        let root = make_root();
        let mut t = make_transaction();
        let key = BtreeKey::new(100, 1, KeyType::Normal);
        t.bch2_trans_get_iter(&root, &key, false, BtreeId::Extents);
        t.rollback();
        // rollback 对齐 bch2_trans_reset_updates：不放锁，保留 iters
        assert_eq!(t.iter_count(), 1, "rollback keeps iters (bcachefs aligned)");
        assert!(!t.is_committed(), "rollback clears committed flag");
        assert!(t.journal.is_empty(), "rollback clears journal");
    }

    #[test]
    fn test_transaction_journal_seq() {
        let mut t = make_transaction();
        t.set_journal_seq(42);
        assert_eq!(t.journal_seq(), 42);
    }

    #[test]
    fn test_transaction_multiple_iters() {
        let root = make_root();
        let mut t = make_transaction();
        t.bch2_trans_get_iter(
            &root,
            &BtreeKey::new(100, 1, KeyType::Normal),
            false,
            BtreeId::Extents,
        );
        t.bch2_trans_get_iter(
            &root,
            &BtreeKey::new(200, 1, KeyType::Normal),
            true,
            BtreeId::Subvolumes,
        );
        assert_eq!(t.iter_count(), 2);
    }

    // ─── Phase A: 新测试 ──────────────────────────────────

    /// 测试 (2): 重启触发 — needs_restart 在 request_restart 后正确设置
    #[test]
    fn test_restart_trigger() {
        let mut t = make_transaction();
        assert!(!t.needs_restart());

        t.request_restart(RestartReason::LockConflict);
        assert!(t.needs_restart());
        assert_eq!(t.restart_reason(), Some(RestartReason::LockConflict));
    }

    /// 测试 (3): begin() 清除 needs_restart
    #[test]
    fn test_begin_clears_restart() {
        let mut t = make_transaction();
        t.request_restart(RestartReason::NodeSplit);
        assert!(t.needs_restart());

        t.bch2_trans_begin();
        assert!(!t.needs_restart());
        assert_eq!(t.restart_reason(), None);
    }

    /// 测试 (4): rollback 清除 restart 状态但不释放锁/清除 iters
    ///
    /// 对齐 bcachefs `bch2_trans_reset_updates()` — 不放锁，仅重置更新队列。
    #[test]
    fn test_rollback_clears_restart() {
        let root = make_root();
        let mut t = make_transaction();
        t.bch2_trans_get_iter(
            &root,
            &BtreeKey::new(100, 1, KeyType::Normal),
            false,
            BtreeId::Extents,
        );
        t.request_restart(RestartReason::LockConflict);

        t.rollback();
        assert!(!t.needs_restart());
        assert_eq!(t.restart_reason(), None);
        assert_eq!(t.restart_count(), 0);
        // rollback 对齐 bch2_trans_reset_updates：不放锁，保留 iters
        // 与旧版不同：旧版释放所有锁并清除 iters，新版仅重置更新队列
        assert_eq!(
            t.iter_count(),
            1,
            "rollback should keep iters (bcachefs aligned)"
        );
        assert!(t.journal.is_empty(), "rollback should clear journal");
    }

    /// 测试 (6): iter_type 返回正确的 btree type
    #[test]
    fn test_iter_type() {
        let root = make_root();
        let mut t = make_transaction();

        t.bch2_trans_get_iter(
            &root,
            &BtreeKey::new(100, 1, KeyType::Normal),
            false,
            BtreeId::Extents,
        );
        t.bch2_trans_get_iter(
            &root,
            &BtreeKey::new(200, 1, KeyType::Normal),
            false,
            BtreeId::Snapshots,
        );

        assert_eq!(t.iter_type(0), BtreeId::Extents);
        assert_eq!(t.iter_type(1), BtreeId::Snapshots);
        // 越界返回默认值
        assert_eq!(t.iter_type(99), BtreeId::Extents);
    }

    /// 测试 (7): 未提交事务 restart_count 为 0
    #[test]
    fn test_restart_count_initial() {
        let t = make_transaction();
        assert_eq!(t.restart_count(), 0);
    }

    /// 测试 (8): 不同 btree type 的锁排序 - 同 type 不同 pos
    /// 测试 (9): try_lock_all — 按 journal 顺序升级写锁通过
    #[test]
    fn test_try_lock_all_success() {
        let root = make_root();
        let mut t = make_transaction();

        // 创建 iter（自动获取 leaf 读锁，intent=false）
        let key = BtreeKey::new(100, 1, KeyType::Normal);
        t.bch2_trans_get_iter(&root, &key, false, BtreeId::Extents);

        // 更新路径必须持有 intent ref，对应 bcachefs update iter。
        t.path_mut(t.iters[0].path).intent_ref = 1;
        t.path_mut(t.iters[0].path).locks_want = 1;

        // 添加 journal 条目引用该 iter（iter_idx=0, level=0）
        let val = BchVal::new(42, 0);
        t.bch2_trans_update(BtreeId::Extents, 0, false, key, val, 0);

        // 按 journal 自然顺序升级（Read → Intent → Write）
        t.try_lock_all();

        // 不应触发重启（无竞争）
        assert!(!t.needs_restart());
        // leaf 锁已升级为 Write
        assert!(matches!(
            &t.path_ref(t.iters[0].path).levels[0],
            BtreePathNode::Node(level) if level.lock_state == BtreeNodeLockedType::Write
        ));
    }

    /// 测试 (10): `__bch2_trans_commit()` 成功返回 Ok
    #[test]
    fn test_bch2_trans_commit_returns_ok() {
        let root = make_root();
        let mut t = make_transaction();

        let key = BtreeKey::new(100, 1, KeyType::Normal);
        t.bch2_trans_get_iter(&root, &key, false, BtreeId::Extents);

        let result = t.__bch2_trans_commit();
        assert!(result.is_ok());
        assert!(t.is_committed());
    }

    /// 测试 (11): 带有条目的事务正常提交
    #[test]
    fn test_bch2_trans_commit_with_entries() {
        let root = make_root();
        let mut t = make_transaction();

        let key = BtreeKey::new(100, 1, KeyType::Normal);
        let val = BchVal::new(42, 0);
        t.bch2_trans_get_iter(&root, &key, true, BtreeId::Extents);

        t.bch2_trans_update(BtreeId::Extents, 0, false, key, val, 0);
        assert_eq!(t.journal_len(), 1);

        let result = t.__bch2_trans_commit();
        assert!(result.is_ok());

        // journal 条目仍可 drain
        let journal = t.drain_journal();
        assert_eq!(journal.len(), 1);
    }

    /// 测试 (12): 没有 LockGraph 时锁获取正常工作（事务生命周期基础）
    #[test]
    fn test_bch2_trans_commit_no_lock_graph() {
        let root = make_root();
        let mut t = make_transaction();
        t.bch2_trans_get_iter(
            &root,
            &BtreeKey::new(100, 1, KeyType::Normal),
            false,
            BtreeId::Extents,
        );
        assert!(t.__bch2_trans_commit().is_ok());
    }

    // ─── REQ-4: 提交钩子测试 ─────────────────────────────

    /// 测试提交钩子在 commit 路径中被执行
    #[test]
    fn test_commit_hook_executed() {
        let root = make_root();
        let mut t = make_transaction();
        let key = BtreeKey::new(100, 1, KeyType::Normal);
        t.bch2_trans_get_iter(&root, &key, false, BtreeId::Extents);

        let hook_called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hc = hook_called.clone();
        t.add_commit_hook(move |_self| {
            hc.store(true, std::sync::atomic::Ordering::Release);
            Ok(())
        });
        assert!(t.__bch2_trans_commit().is_ok());
        assert!(
            hook_called.load(std::sync::atomic::Ordering::Acquire),
            "commit hook should have been called"
        );
    }

    /// 测试提交钩子在 commit 后自动清空
    #[test]
    fn test_commit_hooks_cleared_after_commit() {
        let root = make_root();
        let mut t = make_transaction();
        let key = BtreeKey::new(100, 1, KeyType::Normal);
        t.bch2_trans_get_iter(&root, &key, false, BtreeId::Extents);

        t.add_commit_hook(|_| Ok(()));
        assert_eq!(t.commit_hooks.len(), 1, "hook registered");
        assert!(t.__bch2_trans_commit().is_ok());
        assert_eq!(t.commit_hooks.len(), 0, "hooks cleared after commit");
    }

    /// 测试提交钩子的短路语义：Err 的钩子中止后续钩子和提交
    #[test]
    fn test_commit_hook_short_circuit() {
        let root = make_root();
        let mut t = make_transaction();
        let key = BtreeKey::new(100, 1, KeyType::Normal);
        t.bch2_trans_get_iter(&root, &key, false, BtreeId::Extents);

        let error_hook_called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let second_called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let eh = error_hook_called.clone();
        let sc = second_called.clone();
        t.add_commit_hook(move |_| {
            eh.store(true, std::sync::atomic::Ordering::Release);
            Err(StorageError::Transaction("hook error".into()))
        });
        t.add_commit_hook(move |_| {
            sc.store(true, std::sync::atomic::Ordering::Release);
            Ok(())
        });
        let result = t.__bch2_trans_commit();
        assert!(result.is_err(), "hook error should abort commit");
        assert!(
            error_hook_called.load(std::sync::atomic::Ordering::Acquire),
            "first hook should have been called"
        );
        assert!(
            !second_called.load(std::sync::atomic::Ordering::Acquire),
            "second hook should NOT run after error"
        );
    }

    /// 测试多个提交钩子按注册顺序执行
    #[test]
    fn test_commit_hooks_order() {
        let root = make_root();
        let mut t = make_transaction();
        let key = BtreeKey::new(100, 1, KeyType::Normal);
        t.bch2_trans_get_iter(&root, &key, false, BtreeId::Extents);

        let order = Arc::new(std::sync::Mutex::new(Vec::new()));
        let o1 = order.clone();
        t.add_commit_hook(move |_| {
            o1.lock().unwrap().push(1);
            Ok(())
        });
        let o2 = order.clone();
        t.add_commit_hook(move |_| {
            o2.lock().unwrap().push(2);
            Ok(())
        });
        let o3 = order.clone();
        t.add_commit_hook(move |_| {
            o3.lock().unwrap().push(3);
            Ok(())
        });
        assert!(t.__bch2_trans_commit().is_ok());
        assert_eq!(
            *order.lock().unwrap(),
            vec![1, 2, 3],
            "hooks should execute in registration order"
        );
    }

    // ─── REQ-5: 日志快速路径测试 ──────────────────────────

    /// 测试 `bch2_trans_journal_res_get` 在没有 vol 时返回错误
    #[test]
    fn test_trans_journal_res_get_no_vol() {
        let t = make_transaction();
        let result = t.bch2_trans_journal_res_get(64);
        assert!(result.is_err(), "no vol → journal_res_get should fail");
    }

    // ─── Phase A6: 重启触发测试 ──────────────────────────

    /// 测试 (18): trigger_node_split 设置正确的重启原因
    #[test]
    fn test_trigger_node_split_sets_reason() {
        let mut t = make_transaction();
        t.trigger_node_split();
        assert!(t.needs_restart());
        assert_eq!(t.restart_reason(), Some(RestartReason::NodeSplit));
    }

    /// 测试 (19): trigger_key_cache_miss 设置正确的重启原因
    #[test]
    fn test_trigger_key_cache_miss_sets_reason() {
        let mut t = make_transaction();
        t.trigger_key_cache_miss();
        assert!(t.needs_restart());
        assert_eq!(t.restart_reason(), Some(RestartReason::KeyCacheMiss));
    }

    /// 测试 (20): trigger_node_read_required 设置正确的重启原因
    #[test]
    fn test_trigger_node_read_required_sets_reason() {
        let mut t = make_transaction();
        t.trigger_node_read_required();
        assert!(t.needs_restart());
        assert_eq!(t.restart_reason(), Some(RestartReason::NodeReadRequired));
    }

    /// 测试 (21): trigger_needs_lock 设置正确的重启原因
    #[test]
    fn test_trigger_needs_lock_sets_reason() {
        let mut t = make_transaction();
        t.trigger_needs_lock();
        assert!(t.needs_restart());
        assert_eq!(t.restart_reason(), Some(RestartReason::TriggerNeedsLock));
    }

    /// 测试: trigger_would_deadlock 设置正确的重启原因
    #[test]
    fn test_trigger_would_deadlock_sets_reason() {
        let mut t = make_transaction();
        t.trigger_would_deadlock();
        assert!(t.needs_restart());
        assert_eq!(t.restart_reason(), Some(RestartReason::WouldDeadlock));
    }

    #[test]
    fn test_bch2_check_for_deadlock_no_deadlock() {
        let mut t = make_transaction();
        t.bch2_trans_begin();
        let trans_start_time = t.locking_wait.trans_start_time;
        // 单条依赖链，无环
        let waiters = vec![WaiterInfo {
            trans_id: trans_start_time,
            lock_id: 100,
            waiting_for_trans_id: trans_start_time.wrapping_add(1),
        }];
        assert!(!t.bch2_check_for_deadlock(&waiters));
        assert!(!t.lock_must_abort());
    }

    #[test]
    fn test_bch2_check_for_deadlock_detects_cycle() {
        let mut t = make_transaction();
        t.bch2_trans_begin();
        let trans_start_time = t.locking_wait.trans_start_time;
        // 2 路死锁: T1→L2→T2, T2→L1→T1
        // T1 = t.locking_wait.trans_start_time
        let waiters = vec![
            WaiterInfo {
                trans_id: trans_start_time,
                lock_id: 200,
                waiting_for_trans_id: 42,
            },
            WaiterInfo {
                trans_id: 42,
                lock_id: 100,
                waiting_for_trans_id: trans_start_time,
            },
        ];
        assert!(t.bch2_check_for_deadlock(&waiters));
        assert!(t.lock_must_abort());
    }

    #[test]
    fn test_bch2_check_for_deadlock_self_cycle() {
        let mut t = make_transaction();
        t.bch2_trans_begin();
        let trans_start_time = t.locking_wait.trans_start_time;
        // 自环: T1→L1→T1
        let waiters = vec![WaiterInfo {
            trans_id: trans_start_time,
            lock_id: 100,
            waiting_for_trans_id: trans_start_time,
        }];
        assert!(t.bch2_check_for_deadlock(&waiters));
        assert!(t.lock_must_abort());
    }

    #[test]
    fn test_bch2_check_for_deadlock_already_aborted() {
        let mut t = make_transaction();
        t.lock_must_abort = true;
        // 即使没有 waiters，如果 lock_must_abort 已设置，立即返回 true
        assert!(t.bch2_check_for_deadlock(&[]));
    }

    #[test]
    fn test_bch2_check_for_deadlock_3_way_cycle() {
        let mut t = make_transaction();
        t.bch2_trans_begin();
        let trans_start_time = t.locking_wait.trans_start_time;
        // 3 路死锁: T1→L2→T2→L3→T3→L1→T1
        let waiters = vec![
            WaiterInfo {
                trans_id: trans_start_time,
                lock_id: 200,
                waiting_for_trans_id: 42,
            },
            WaiterInfo {
                trans_id: 42,
                lock_id: 300,
                waiting_for_trans_id: 43,
            },
            WaiterInfo {
                trans_id: 43,
                lock_id: 100,
                waiting_for_trans_id: trans_start_time,
            },
        ];
        assert!(t.bch2_check_for_deadlock(&waiters));
        assert!(t.lock_must_abort());
    }

    /// 测试: trigger_write_overflow 设置正确的重启原因
    #[test]
    fn test_trigger_write_overflow_sets_reason() {
        let mut t = make_transaction();
        t.trigger_write_overflow();
        assert!(t.needs_restart());
        assert_eq!(t.restart_reason(), Some(RestartReason::WriteOverflow));
    }

    /// 测试: trigger_split_with_interior_updates 设置正确的重启原因
    #[test]
    fn test_trigger_split_with_interior_updates_sets_reason() {
        let mut t = make_transaction();
        t.trigger_split_with_interior_updates();
        assert!(t.needs_restart());
        assert_eq!(
            t.restart_reason(),
            Some(RestartReason::SplitWithInteriorUpdates)
        );
    }

    /// 测试 (22): check_path_integrity 检测到空路径时触发 NodeReadRequired
    #[test]
    fn test_check_path_integrity_empty_path() {
        let mut t = make_transaction();
        // 空路径的 iter（模拟未初始化的 iter）
        let k = BtreeKey::new(100, 1, KeyType::Normal);
        let root = make_root();
        t.bch2_trans_get_iter(&root, &k, false, BtreeId::Extents);
        // 清空 path 模拟损坏
        if let Some(path_idx) = t.iters.first().map(|iter| iter.path) {
            let path = t.path_mut(path_idx);
            path.levels = std::array::from_fn(|_| BtreePathNode::Error(BtreePathError::Init));
            path.nodes_locked = 0;
        }
        t.check_path_integrity(0);
        assert!(t.needs_restart());
    }

    /// 测试 (23): detect_iter_restart_needed 检测 had_restart 标志
    #[test]
    fn test_detect_iter_restart() {
        let mut t = make_transaction();
        let k = BtreeKey::new(100, 1, KeyType::Normal);
        let root = make_root();
        t.bch2_trans_get_iter(&root, &k, false, BtreeId::Extents);
        // 设置 had_restart
        if let Some(iter) = t.iters.first_mut() {
            iter.had_restart = true;
        }
        assert!(t.detect_iter_restart_needed());
        assert!(t.needs_restart());
    }

    /// 测试 (24): detect_iter_restart_needed 消耗 had_restart 标志
    #[test]
    fn test_detect_iter_restart_consumes_flag() {
        let mut t = make_transaction();
        let k = BtreeKey::new(100, 1, KeyType::Normal);
        let root = make_root();
        t.bch2_trans_get_iter(&root, &k, false, BtreeId::Extents);
        if let Some(iter) = t.iters.first_mut() {
            iter.had_restart = true;
        }
        assert!(t.detect_iter_restart_needed());
        // 第二次调用不应再触发
        assert!(!t.detect_iter_restart_needed());
    }

    /// 测试 (29): drain_journal 返回包含 btree_type 的条目
    #[test]
    fn test_drain_journal_with_btree_type() {
        let mut t = make_transaction();
        let key = BtreeKey::new(100, 1, KeyType::Normal);
        let val = BchVal::new(42, 0);

        t.bch2_trans_update(BtreeId::Extents, 0, false, key, val, 0);
        t.bch2_trans_delete(BtreeId::Subvolumes, 0, false, key, 0);
        assert_eq!(t.journal_len(), 2);

        let journal = t.drain_journal();
        assert_eq!(journal.len(), 2);
        assert_eq!(journal[0].btree_id, BtreeId::Extents);
        assert_eq!(journal[0].op, BtreeOp::Insert);
        assert_eq!(journal[1].btree_id, BtreeId::Subvolumes);
        assert_eq!(
            journal[1].op,
            BtreeOp::Insert,
            "trans_delete now stores as Insert with Deleted key_type"
        );
        assert_eq!(journal[1].key.key_type, KeyType::Deleted);
    }

    /// 测试 (28): 无对应 bkey trigger 时提交为空操作
    #[test]
    fn test_commit_without_matching_trigger() {
        let mut t = make_transaction();
        let key = BtreeKey::new(100, 1, KeyType::Normal);
        let val = BchVal::new(42, 0);

        t.bch2_trans_update(BtreeId::Extents, 0, false, key, val, 0);

        // Extents 的 BchVal 不产生 extent trigger 更新 → commit 应返回 Ok
        let root = make_root();
        t.bch2_trans_get_iter(&root, &key, true, BtreeId::Extents);
        let result = t.__bch2_trans_commit();
        assert!(result.is_ok());
    }

    // ─── P0 Delta: restart() / sort_key level / lockrestart_do! ──

    /// 测试 (32): restart() 返回最近一次的重启原因并消费
    #[test]
    fn test_restart_returns_reason() {
        let mut t = make_transaction();
        t.request_restart(RestartReason::LockConflict);
        let reason = t.restart();
        assert_eq!(reason, Some(RestartReason::LockConflict));
        // restart 消费了 reason
        assert_eq!(t.restart_reason(), None);
        // restart_count 递增
        assert_eq!(t.restart_count(), 1);
    }

    /// 测试 (33): restart() 超过 MAX_RESTARTS 时返回 None
    #[test]
    fn test_restart_none_when_exceeded() {
        let mut t = make_transaction();
        t.restart_count = MAX_RESTARTS; // 设为刚好在阈值
        t.request_restart(RestartReason::LockConflict);
        let reason = t.restart();
        assert!(reason.is_none(), "over MAX_RESTARTS should return None");
        assert_eq!(t.restart_count(), MAX_RESTARTS + 1);
    }

    /// 测试 (34): restart() 调用后 needs_restart 被清除（由 begin() 完成）
    #[test]
    fn test_restart_clears_needs_restart() {
        let mut t = make_transaction();
        t.request_restart(RestartReason::NodeSplit);
        assert!(t.needs_restart());
        let _ = t.restart();
        assert!(!t.needs_restart());
    }

    /// 对应本地 iter.c:3970-3971：new attempt 刷新内嵌 waiter 的
    /// `trans_start_time`，restart attempt 保留原有排队年龄。
    #[test]
    fn test_trans_begin_updates_only_embedded_waiter_start_time() {
        let mut t = make_transaction();

        t.bch2_trans_begin();
        let first = t.locking_wait.trans_start_time;
        assert_ne!(first, 0);

        t.request_restart(RestartReason::NodeSplit);
        t.bch2_trans_begin();
        assert_eq!(t.locking_wait.trans_start_time, first);

        t.bch2_trans_begin();
        assert!(t.locking_wait.trans_start_time > first);
    }

    /// 测试 (36): lockrestart_do! 成功路径 — body 返回 Ok
    #[test]
    fn test_lockrestart_do_success() {
        let mut t = make_transaction();
        let result = lockrestart_do!(t, { Ok(42) });
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 42);
        assert_eq!(t.restart_count(), 0);
    }

    /// 测试 (37): lockrestart_do! 重启重试 — body 第一次返回 Err 后重试成功
    #[test]
    fn test_lockrestart_do_restart_then_ok() {
        let mut t = make_transaction();
        let attempts = std::cell::Cell::new(0u32);

        let result: Result<(), StorageError> = lockrestart_do!(t, {
            let n = attempts.get();
            attempts.set(n + 1);
            if n == 0 {
                return Err(RestartReason::LockConflict);
            }
            Ok(())
        });

        assert!(result.is_ok());
        // 发生了 1 次重启
        assert_eq!(t.restart_count(), 1);
        // body 执行了 2 次
        assert_eq!(attempts.get(), 2);
    }

    /// 测试 (38): lockrestart_do! 超限 — body 持续返回 Err
    #[test]
    fn test_lockrestart_do_max_restarts() {
        let mut t = make_transaction();
        t.restart_count = MAX_RESTARTS; // 一次额外调用即超限

        let result: Result<(), StorageError> =
            lockrestart_do!(t, { Err(RestartReason::LockConflict) });

        assert!(result.is_err());
        match result.unwrap_err() {
            StorageError::TransactionRestartLimit(count) => {
                assert_eq!(count, u64::from(MAX_RESTARTS + 1));
            }
            _ => panic!("expected TransactionRestartLimit error"),
        }
    }

    // ─── P2: locked_seq + get_path + restart_optimized ──

    /// 测试 (39): bch2_trans_record_locked_seqs 记录所有 path levels 的 locked_seq
    ///
    /// 验证：
    /// - commit() 后每个 path level 的 locked_seq 被记录
    /// - 新节点 seq 从 0 开始，locked_seq 应为 0（对应 SixLock 初始值）
    /// - locked_seq 精确等于 lock.six_lock_seq() 在记录时刻的值
    #[test]
    fn test_locked_seq_recorded_on_lock() {
        let root = make_root();
        let mut t = make_transaction();
        let key = BtreeKey::new(100, 1, KeyType::Normal);
        t.bch2_trans_get_iter(&root, &key, false, BtreeId::Extents);
        assert!(t.__bch2_trans_commit().is_ok());
        let path_idx = t.iter(0).unwrap().path;
        for (i, level) in t.path_ref(path_idx).levels.iter().enumerate() {
            // 新节点从未被写过，SixLock::seq() == 0
            if let BtreePathNode::Node(level) = level {
                assert_eq!(
                    level.locked_seq,
                    level.node.lock.six_lock_seq(),
                    "level {} locked_seq should match lock seq",
                    i
                );
            }
        }
    }

    // ── R1: get_path 测试 ──────────────────────────────

    /// 测试 (39): get_path 精确匹配共享权威 path
    ///
    /// bch2_trans_get_iter 后 get_path 同一 key → pos == target → 直接复用。
    #[test]
    fn test_get_path_exact_match() {
        let root = make_root();
        let mut t = make_transaction();
        let key = BtreeKey::new(100, 1, KeyType::Normal);

        t.bch2_trans_get_iter(&root, &key, false, BtreeId::Extents);
        assert_eq!(t.iter_count(), 1);

        let first_path = t.iters[0].path;

        // 本地 bcachefs：新 iter 增加同一 path 的 ref，而不是覆盖已有 iter。
        let idx = t.get_path(&root, &key, false, BtreeId::Extents, None);
        assert_eq!(idx, 1);
        assert_eq!(t.iter_count(), 2);
        assert_eq!(t.iters[idx].path, first_path);
        assert_eq!(t.path_ref(first_path).ref_count, 2);
    }

    /// 测试 (40): get_path 同 leaf 复用
    ///
    /// 同一 btree_type 不同 key 即使位于同一 leaf，也不能共享可变 position；
    /// 本地 `bch2_btree_path_set_pos()` 在 ref > 1 时会先 make_mut。
    #[test]
    fn test_get_path_same_leaf() {
        let root = make_root();
        let mut t = make_transaction();
        let key_a = BtreeKey::new(100, 1, KeyType::Normal);
        let key_b = BtreeKey::new(200, 1, KeyType::Normal);

        t.bch2_trans_get_iter(&root, &key_a, false, BtreeId::Extents);
        assert_eq!(t.iter_count(), 1);

        let first_path = t.iters[0].path;
        let idx = t.get_path(&root, &key_b, false, BtreeId::Extents, None);
        assert_eq!(idx, 1);
        assert_eq!(t.iter_count(), 2);
        assert_ne!(t.iters[idx].path, first_path);
        assert_eq!(t.path_ref(first_path).ref_count, 1);
    }

    /// 测试 (41): get_path 不同 btree_type 创建新 iter
    #[test]
    fn test_get_path_creates_new_when_type_mismatch() {
        let root = make_root();
        let mut t = make_transaction();
        let key = BtreeKey::new(100, 1, KeyType::Normal);

        t.bch2_trans_get_iter(&root, &key, false, BtreeId::Extents);
        assert_eq!(t.iter_count(), 1);

        // 不同 btree_type → 无法匹配 → 创建新 iter
        let idx = t.get_path(&root, &key, false, BtreeId::Subvolumes, None);
        assert_eq!(idx, 1, "type mismatch should create at index 1");
        assert_eq!(t.iter_count(), 2, "type mismatch should create new iter");
    }

    /// 测试 (42): get_path 返回的索引可通过 iter_mut 访问
    #[test]
    fn test_get_path_returns_usable_index() {
        let root = make_root();
        let mut t = make_transaction();
        let key = BtreeKey::new(100, 1, KeyType::Normal);

        let idx = t.get_path(&root, &key, false, BtreeId::Extents, None);
        // 新创建的 iter 索引为 0
        assert_eq!(idx, 0);

        let iter = t.iter_mut(idx).unwrap();
        assert_eq!(iter.pos, key, "iter should be at target position");
    }

    // ── R2: restart_optimized (事务级) 测试 ──────────────────

    /// 测试 (43): restart_optimized seq 未变时返回 None
    ///
    /// commit() 后 locked_seq 已记录，seq 未变化 → 应返回 None。
    #[test]
    fn test_txn_restart_optimized_none_when_seq_unchanged() {
        let root = make_root();
        let mut t = make_transaction();
        let key = BtreeKey::new(100, 1, KeyType::Normal);

        t.bch2_trans_get_iter(&root, &key, false, BtreeId::Extents);
        t.__bch2_trans_commit().unwrap();
        // locked_seq 已记录，节点未被修改 → restart_optimized 应返回 None
        let result = t.restart_optimized();
        assert!(result.is_none(), "should return None when seq unchanged");
    }

    /// 测试 (44): restart_optimized seq 变化时返回 Some
    ///
    /// commit() 后修改节点 seq，restart_optimized 应检测到变化并返回 Some。
    #[test]
    fn test_txn_restart_optimized_some_when_seq_changed() {
        let root = make_root();
        let mut t = make_transaction();
        let key = BtreeKey::new(100, 1, KeyType::Normal);

        t.bch2_trans_get_iter(&root, &key, false, BtreeId::Extents);
        t.__bch2_trans_commit().unwrap();

        // 手动修改 leaf 节点的 seq（模拟外部写操作）
        let leaf = match &t.path_ref(t.iters[0].path).levels[0] {
            BtreePathNode::Node(level) => Arc::clone(&level.node),
            _ => panic!("expected leaf node"),
        };
        leaf.lock.six_lock_intent();
        let readers = leaf.lock.six_lock_counts().n[0];
        if readers > 0 {
            leaf.lock.six_lock_readers_add(-(readers as i32));
        }
        leaf.lock.six_lock_write();
        if readers > 0 {
            leaf.lock.six_lock_readers_add(readers as i32);
        }
        leaf.lock.six_unlock_write();
        leaf.lock.six_unlock_intent();
        // seq 现在为 1

        let result = t.restart_optimized();
        assert!(result.is_some(), "should return Some when seq changed");
    }

    /// 测试 (45): restart_optimized 空事务返回 None
    #[test]
    fn test_txn_restart_optimized_empty() {
        let mut t = make_transaction();
        // 无 iters → needs_full_restart 为 false（iter path 为空）
        let result = t.restart_optimized();
        assert!(result.is_none(), "empty transaction should return None");
    }

    /// 测试 (46): restart_optimized 检查所有 path level — 内部节点变化 leaf 未变
    ///
    /// 创建 depth-1 树（internal root + leaf），bch2_trans_get_iter + commit 后
    /// 修改 internal node seq。仅当 restart_optimized 检查所有层级
    /// 时才能检测到此变化。
    #[test]
    fn test_txn_restart_optimized_internal_changed_leaf_unchanged() {
        use crate::btree::key::BchVal;

        // 创建小节点树，插入足够条目触发多级分裂（depth ≥ 1）
        let b = Btree::new();
        b.set_root_node_size(512);

        let total = 200u64;
        for i in 0..total {
            futures::executor::block_on(b.bch2_btree_insert(
                &NoopWriter,
                BtreeKey::new(i, 1, KeyType::Normal),
                BchVal::new(i * 10, 0),
                0,
            ))
            .unwrap();
        }
        assert!(
            b.depth() >= 1,
            "should have depth >= 1 after {total} inserts (got depth={})",
            b.depth()
        );

        let mut t = BtreeTrans::new_with_cache(b.node_cache_arc());
        t.bch2_trans_begin();
        t.bch2_trans_get_iter(
            b.root(),
            &BtreeKey::new(10, 1, KeyType::Normal),
            false,
            BtreeId::Extents,
        );
        t.__bch2_trans_commit().unwrap();

        // depth ≥ 1 树 → path 应有至少 2 层
        let path = t.path_ref(t.iters[0].path);
        assert!(
            path.levels
                .iter()
                .filter(|node| matches!(node, BtreePathNode::Node(_)))
                .count()
                >= 2,
            "depth-{} tree should have >=2 path levels (got {})",
            b.depth(),
            path.levels
                .iter()
                .filter(|node| matches!(node, BtreePathNode::Node(_)))
                .count()
        );

        // 记录修改前的 locked_seq（path[0] = internal root）
        let internal_locked = match &path.levels[0] {
            BtreePathNode::Node(level) => level.locked_seq,
            _ => panic!("expected internal node"),
        };

        // 修改 internal node 的 seq（模拟拓扑变化）
        let internal = match &path.levels[0] {
            BtreePathNode::Node(level) => Arc::clone(&level.node),
            _ => panic!("expected internal node"),
        };
        internal.lock.six_lock_intent();
        let readers = internal.lock.six_lock_counts().n[0];
        if readers > 0 {
            internal.lock.six_lock_readers_add(-(readers as i32));
        }
        internal.lock.six_lock_write();
        if readers > 0 {
            internal.lock.six_lock_readers_add(readers as i32);
        }
        internal.lock.six_unlock_write();
        internal.lock.six_unlock_intent();

        // 验证内部节点 seq 变化
        assert_ne!(
            internal.lock.six_lock_seq(),
            internal_locked,
            "internal node seq should have changed"
        );

        // restart_optimized 必须检测到内部节点变化
        let result = t.restart_optimized();
        assert!(result.is_some(), "should detect internal node change");
    }

    // ─── bcachefs 提交流程对齐测试 ──────────────────────────────

    /// 验证 rollback() 不重置 restart_count（对齐 bch2_trans_reset_updates）
    /// bcachefs update.h:557-571: reset_updates 不清除 restart_count
    #[test]
    fn test_rollback_keeps_restart_count() {
        let root = make_root();
        let mut t = make_transaction();
        t.bch2_trans_get_iter(
            &root,
            &BtreeKey::new(100, 1, KeyType::Normal),
            false,
            BtreeId::Extents,
        );
        // 模拟重启计数
        t.restart_count = 42;
        t.request_restart(RestartReason::LockConflict);

        t.rollback();

        // restart_count 应保留（bcachefs reset_updates 不清除它）
        assert_eq!(
            t.restart_count, 42,
            "rollback should NOT reset restart_count"
        );
        // rollback 仍清除其他状态
        assert!(!t.needs_restart(), "rollback should clear needs_restart");
        assert_eq!(
            t.restart_reason(),
            None,
            "rollback should clear restart_reason"
        );
        assert!(t.journal.is_empty(), "rollback should clear journal");
    }

    /// 验证 begin() 重置 journal_seq（对齐 bch2_trans_begin 的 reset_updates）
    /// 防止重试循环中误用之前失败的 journal_seq
    #[test]
    fn test_begin_resets_journal_seq() {
        let mut t = make_transaction();
        t.journal_seq = 42; // 模拟之前失败的 commit 设置了 seq
        t.bch2_trans_begin();
        assert_eq!(t.journal_seq, 0, "begin() should reset journal_seq to 0");
    }

    /// 验证 `__bch2_trans_commit()` 在 reclaim 路径 + needs_restart 时返回 RestartLimit
    /// 对应 `__bch2_trans_commit()` Phase 0a: reclaim bail
    #[test]
    fn test_bch2_trans_commit_reclaim_bail_on_restart() {
        let root = make_root();
        let mut t = make_transaction();
        t.watermark = Watermark::Reclaim;
        t.bch2_trans_get_iter(
            &root,
            &BtreeKey::new(100, 1, KeyType::Normal),
            false,
            BtreeId::Extents,
        );
        // push journal entries 使 has_updates 检查通过
        journal_push_entry(
            &mut t,
            BtreeOp::Insert,
            0,
            false,
            BtreeKey::new(100, 1, KeyType::Normal),
            BchVal::new(0, 0),
        );
        t.request_restart(RestartReason::LockConflict);

        let result = t.__bch2_trans_commit();

        assert!(result.is_err(), "reclaim + needs_restart should fail");
        match result {
            Err(StorageError::TransactionRestartLimit(_)) => {} // expected
            Err(e) => panic!("expected TransactionRestartLimit, got: {e:?}"),
            Ok(_) => panic!("expected error"),
        }
        // restart_count 应递增（验证重启计数语义）
        assert_eq!(
            t.restart_count, 1,
            "reclaim bail should increment restart_count"
        );
    }

    /// 验证 `__bch2_trans_commit()` 正常路径下触发器可运行（Phase 0b 在 try_lock_all 之前）
    /// 无 vol 时触发器应被跳过，不会阻塞提交
    #[test]
    fn test_bch2_trans_commit_triggers_skipped_without_vol() {
        let root = make_root();
        let mut t = make_transaction();
        t.bch2_trans_get_iter(
            &root,
            &BtreeKey::new(100, 1, KeyType::Normal),
            true,
            BtreeId::Extents,
        );
        journal_push_entry(
            &mut t,
            BtreeOp::Insert,
            0,
            false,
            BtreeKey::new(100, 1, KeyType::Normal),
            BchVal::new(0, 0),
        );

        let result = t.__bch2_trans_commit();

        // 无 vol → 无触发器 → 应该成功（没有锁冲突）
        assert!(
            result.is_ok(),
            "commit without triggers should succeed: {:?}",
            result
        );
        assert!(t.committed, "commit should mark committed");
    }

    /// 验证 `__bch2_trans_commit()` 在重启限制达到时返回 TransactionRestartLimit
    /// 模拟多次重启使 restart_count 超过 MAX_RESTARTS
    #[test]
    fn test_bch2_trans_commit_restart_limit_exceeded() {
        let root = make_root();
        let mut t = make_transaction();
        t.bch2_trans_get_iter(
            &root,
            &BtreeKey::new(100, 1, KeyType::Normal),
            false,
            BtreeId::Extents,
        );

        // 设置 restart_count 接近上限，让一次 try_lock_all 失败触发限制
        t.restart_count = MAX_RESTARTS;
        // push 一个 journal 条目使 commit 进入 retry 循环
        journal_push_entry(
            &mut t,
            BtreeOp::Insert,
            0,
            false,
            BtreeKey::new(100, 1, KeyType::Normal),
            BchVal::new(0, 0),
        );
        // 设置冲突锁使 try_lock_all 触发 needs_restart
        // 使用 intent 锁抢占目标节点
        for iter in &t.iters {
            for level in &t.path_ref(iter.path).levels {
                if let BtreePathNode::Node(level) = level {
                    level.node.lock.six_lock_intent();
                }
            }
        }
        let result = t.__bch2_trans_commit();

        // 清理：释放 test 中获取的 intent 锁（commit 错误路径不会自动释放）
        for iter in &t.iters {
            for level in &t.path_ref(iter.path).levels {
                if let BtreePathNode::Node(level) = level {
                    level.node.lock.six_unlock_intent();
                }
            }
        }

        assert!(result.is_err(), "should exceed restart limit");
        match result {
            Err(StorageError::TransactionRestartLimit(count)) => {
                assert!(
                    count > u64::from(MAX_RESTARTS),
                    "count should exceed MAX_RESTARTS"
                );
            }
            Err(e) => panic!("expected TransactionRestartLimit, got: {e:?}"),
            Ok(_) => panic!("expected error"),
        }
    }

    /// 辅助函数：快速向 journal push 一条 Insert 条目（测试用）
    fn journal_push_entry(
        t: &mut BtreeTrans,
        op: BtreeOp,
        level: u8,
        cached: bool,
        key: BtreeKey,
        value: BchVal,
    ) {
        t.journal.push(BtreeTransEntry {
            op,
            btree_id: BtreeId::Extents,
            level,
            cached,
            key,
            value,
            raw_value: None,
            old_key: None,
            old_value: None,
            old_raw_value: None,
            insert_trigger_run: false,
            overwrite_trigger_run: false,
            sort_order: 0,
            iter_idx: 0,
            path_idx: PATH_IDX_INVALID,
            old_btree_u64s: 0,
        });
    }

    #[test]
    fn test_btree_insert_entry_cmp_sort_order() {
        let key_a = BtreeKey::new(100, 1, KeyType::Normal);
        let key_b = BtreeKey::new(200, 1, KeyType::Normal);

        // sort_order 不同 → Alloc (255) 排在 Extents (0) 之后
        let e1 = dummy_entry(BtreeId::Extents, 0, false, key_a);
        let e2 = dummy_entry(BtreeId::Alloc, 0, false, key_b);
        assert!(
            BtreeTrans::btree_insert_entry_cmp(&e1, &e2).is_lt(),
            "Extents (sort_order=0) < Alloc (sort_order=255)"
        );

        // 相同 sort_order 时 cached 排序
        let e3 = dummy_entry(BtreeId::Extents, 0, true, key_a);
        let e4 = dummy_entry(BtreeId::Extents, 0, false, key_a);
        assert!(
            BtreeTrans::btree_insert_entry_cmp(&e3, &e4).is_gt(),
            "cached=true > cached=false"
        );

        // 相同 sort_order + cached 时 level 排序（高 level 优先）
        let e5 = dummy_entry(BtreeId::Extents, 1, false, key_a);
        let e6 = dummy_entry(BtreeId::Extents, 0, false, key_a);
        assert!(
            BtreeTrans::btree_insert_entry_cmp(&e5, &e6).is_lt(),
            "level=1 < level=0 (higher level first)"
        );

        // 全部相同时按 key 排序
        let key_c = BtreeKey::new(300, 1, KeyType::Normal);
        let e7 = dummy_entry(BtreeId::Extents, 0, false, key_a);
        let e8 = dummy_entry(BtreeId::Extents, 0, false, key_c);
        assert!(
            BtreeTrans::btree_insert_entry_cmp(&e7, &e8).is_lt(),
            "vaddr=100 < vaddr=300"
        );
    }

    #[test]
    fn test_btree_trigger_order_matches_bcachefs() {
        assert_eq!(btree_trigger_order(BtreeId::Alloc), u8::MAX);
        assert_eq!(btree_trigger_order(BtreeId::Stripes), u8::MAX - 1);
        assert_eq!(btree_trigger_order(BtreeId::Extents), BtreeId::Extents as u8);
    }

    fn dummy_entry(btree_id: BtreeId, level: u8, cached: bool, key: BtreeKey) -> BtreeTransEntry {
        BtreeTransEntry {
            op: BtreeOp::Insert,
            btree_id,
            level,
            cached,
            sort_order: btree_trigger_order(btree_id),
            key,
            value: BchVal::new(0, 0),
            raw_value: None,
            old_key: None,
            old_value: None,
            old_raw_value: None,
            insert_trigger_run: false,
            overwrite_trigger_run: false,
            iter_idx: 0,
            path_idx: PATH_IDX_INVALID,
            old_btree_u64s: 0,
        }
    }

    // ─── Sub-task A: RestartReason 全覆盖测试 ──────────────────

    /// 验证 trigger_traverse_all 设置正确的 restart_reason
    #[test]
    fn test_trigger_traverse_all_sets_reason() {
        let mut t = make_transaction();
        t.trigger_traverse_all();
        assert_eq!(t.restart_reason(), Some(RestartReason::TraverseAll));
        assert!(t.needs_restart());
    }

    #[test]
    fn test_trigger_relock_sets_reason() {
        let mut t = make_transaction();
        t.trigger_relock();
        assert_eq!(t.restart_reason(), Some(RestartReason::Relock));
        assert!(t.needs_restart());
    }

    #[test]
    fn test_trigger_relock_path_sets_reason() {
        let mut t = make_transaction();
        t.trigger_relock_path();
        assert_eq!(t.restart_reason(), Some(RestartReason::RelockPath));
        assert!(t.needs_restart());
    }

    #[test]
    fn test_trigger_upgrade_sets_reason() {
        let mut t = make_transaction();
        t.trigger_upgrade();
        assert_eq!(t.restart_reason(), Some(RestartReason::Upgrade));
        assert!(t.needs_restart());
    }

    #[test]
    fn test_trigger_fault_inject_sets_reason() {
        let mut t = make_transaction();
        t.trigger_fault_inject();
        assert_eq!(t.restart_reason(), Some(RestartReason::FaultInject));
        assert!(t.needs_restart());
    }

    #[test]
    fn test_trigger_nested_sets_reason() {
        let mut t = make_transaction();
        t.trigger_nested();
        assert_eq!(t.restart_reason(), Some(RestartReason::Nested));
        assert!(t.needs_restart());
    }

    #[test]
    fn test_trigger_lock_waitlist_alloc_sets_reason() {
        let mut t = make_transaction();
        t.trigger_lock_waitlist_alloc();
        assert_eq!(t.restart_reason(), Some(RestartReason::LockWaitlistAlloc));
        assert!(t.needs_restart());
    }

    #[test]
    fn test_trigger_mem_realloced_sets_reason() {
        let mut t = make_transaction();
        t.trigger_mem_realloced();
        assert_eq!(t.restart_reason(), Some(RestartReason::MemoryRealloced));
        assert!(t.needs_restart());
    }

    /// 验证所有 21 个 RestartReason 变体的 bincode 序列化往返
    #[test]
    fn test_restart_reason_serialization_roundtrip() {
        let all_variants = [
            RestartReason::LockConflict,
            RestartReason::NodeSplit,
            RestartReason::KeyCacheMiss,
            RestartReason::TriggerNeedsLock,
            RestartReason::NodeReadRequired,
            RestartReason::WouldDeadlock,
            RestartReason::WriteOverflow,
            RestartReason::SplitWithInteriorUpdates,
            RestartReason::PathUpgradeFailed,
            RestartReason::JournalReclaimWouldDeadlock,
            RestartReason::JournalOverwritesChanged,
            RestartReason::TraverseAll,
            RestartReason::Relock,
            RestartReason::RelockPath,
            RestartReason::Upgrade,
            RestartReason::FaultInject,
            RestartReason::Nested,
            RestartReason::LockWaitlistAlloc,
            RestartReason::MemoryRealloced,
            RestartReason::InTraverseAll,
            RestartReason::BtreeNodeFull,
            RestartReason::NeedJournalReclaim,
        ];

        for reason in all_variants {
            let encoded = bincode::serialize(&reason).expect("serialize should succeed");
            let decoded: RestartReason =
                bincode::deserialize(&encoded).expect("deserialize should succeed");
            assert_eq!(reason, decoded, "roundtrip failed for {:?}", reason);
        }
    }

    /// 验证所有变体可通过 request_restart -> restart_reason 正确传递
    #[test]
    fn test_restart_reason_all_variants_requestable() {
        let all_variants = [
            RestartReason::LockConflict,
            RestartReason::NodeSplit,
            RestartReason::KeyCacheMiss,
            RestartReason::TriggerNeedsLock,
            RestartReason::NodeReadRequired,
            RestartReason::WouldDeadlock,
            RestartReason::WriteOverflow,
            RestartReason::SplitWithInteriorUpdates,
            RestartReason::PathUpgradeFailed,
            RestartReason::JournalReclaimWouldDeadlock,
            RestartReason::JournalOverwritesChanged,
            RestartReason::TraverseAll,
            RestartReason::Relock,
            RestartReason::RelockPath,
            RestartReason::Upgrade,
            RestartReason::FaultInject,
            RestartReason::Nested,
            RestartReason::LockWaitlistAlloc,
            RestartReason::MemoryRealloced,
            RestartReason::InTraverseAll,
            RestartReason::BtreeNodeFull,
            RestartReason::NeedJournalReclaim,
        ];

        for reason in all_variants {
            let mut t = make_transaction();
            t.request_restart(reason);
            assert_eq!(
                t.restart_reason(),
                Some(reason),
                "request_restart failed for {:?}",
                reason
            );
            assert!(t.needs_restart(), "needs_restart not set for {:?}", reason);
        }
    }

    // ─── R2: bch2_trans_unlock() 不清除 locked_seq 测试 ───────

    /// 验证 bch2_trans_unlock() 不清除 locked_seq
    /// 对齐 bcachefs __bch2_btree_path_unlock() (locking.c:1440-1454)：
    /// path-level 释放后 seq 保留，供 restart_optimized() 检测节点变化。
    #[test]
    fn test_bch2_trans_unlock_preserves_locked_seq() {
        let root = make_root();
        let mut t = make_transaction();
        t.bch2_trans_get_iter(
            &root,
            &BtreeKey::new(100, 1, KeyType::Normal),
            false,
            BtreeId::Extents,
        );
        t.__bch2_trans_commit().unwrap();

        // 记录 commit 后的 locked_seq
        let seq_before: Vec<u64> = t
            .path_ref(t.iters[0].path)
            .levels
            .iter()
            .filter_map(|l| match l {
                BtreePathNode::Node(level) => Some(level.locked_seq),
                _ => None,
            })
            .collect();

        // bch2_trans_unlock 释放锁但应保留 locked_seq
        t.bch2_trans_unlock();

        let seq_after: Vec<u64> = t
            .path_ref(t.iters[0].path)
            .levels
            .iter()
            .filter_map(|l| match l {
                BtreePathNode::Node(level) => Some(level.locked_seq),
                _ => None,
            })
            .collect();

        assert_eq!(
            seq_before, seq_after,
            "bch2_trans_unlock() should NOT clear locked_seq"
        );
    }

    /// 验证 bch2_trans_unlock 后 locked_seq 仍可用于 seq 比较
    /// 模拟 restart_optimized 的检测逻辑：unlock → 修改 seq → 检测变化
    #[test]
    fn test_bch2_trans_unlock_then_seq_check_detects_change() {
        let root = make_root();
        let mut t = make_transaction();
        t.bch2_trans_get_iter(
            &root,
            &BtreeKey::new(100, 1, KeyType::Normal),
            false,
            BtreeId::Extents,
        );
        t.__bch2_trans_commit().unwrap();

        let locked_before = match &t.path_ref(t.iters[0].path).levels[0] {
            BtreePathNode::Node(level) => level.locked_seq,
            _ => panic!("expected leaf node"),
        };

        // bch2_trans_unlock 释放锁
        t.bch2_trans_unlock();

        // 模拟外部写操作：lock_write + unlock_write 使 seq 递增
        let node = match &t.path_ref(t.iters[0].path).levels[0] {
            BtreePathNode::Node(level) => Arc::clone(&level.node),
            _ => panic!("expected leaf node"),
        };
        node.lock.six_lock_intent();
        let readers = node.lock.six_lock_counts().n[0];
        if readers > 0 {
            node.lock.six_lock_readers_add(-(readers as i32));
        }
        node.lock.six_lock_write();
        if readers > 0 {
            node.lock.six_lock_readers_add(readers as i32);
        }
        node.lock.six_unlock_write();
        node.lock.six_unlock_intent();

        // locked_seq 应仍为旧值（未被 bch2_trans_unlock 清除）
        assert_eq!(
            match &t.path_ref(t.iters[0].path).levels[0] {
                BtreePathNode::Node(level) => level.locked_seq,
                _ => panic!("expected leaf node"),
            },
            locked_before,
            "locked_seq should survive bch2_trans_unlock"
        );
        // 而 node 的实际 seq 已变化
        assert_ne!(
            node.lock.six_lock_seq(),
            locked_before,
            "node seq should have changed after write unlock"
        );
    }

    #[test]
    fn test_bch2_trans_unlock_write_updates_all_linked_path_sequences() {
        let mut t = make_transaction();
        let first = t.path_alloc(PATH_IDX_INVALID);
        let second = t.path_alloc(PATH_IDX_INVALID);
        let node = Arc::new(BtreeNode::new_leaf());
        let old_seq = node.lock.six_lock_seq();

        node.lock.six_lock_intent();
        assert!(node.lock.six_lock_write());

        for path_idx in [first, second] {
            let mut level = BtreePathLevel::new(node.clone());
            level.locked_seq = old_seq;
            t.path_mut(path_idx).levels[0] = BtreePathNode::Node(level);
        }
        {
            let path = t.path_mut(first);
            if let BtreePathNode::Node(level) = &mut path.levels[0] {
                level.lock_state = BtreeNodeLockedType::Write;
            }
            path.mark_btree_node_locked_noreset(0, BtreeNodeLockedType::Write);
        }

        t.bch2_trans_unlock();

        let new_seq = node.lock.six_lock_seq();
        assert_ne!(new_seq, old_seq);
        for path_idx in [first, second] {
            assert!(matches!(
                &t.path_ref(path_idx).levels[0],
                BtreePathNode::Node(level) if level.locked_seq == new_seq
            ));
        }
    }

    #[test]
    fn test_bch2_trans_put_drop_releases_path_locks() {
        let root = make_root();
        let node = root.node.clone();

        {
            let mut t = make_transaction();
            t.bch2_trans_get_iter(
                &root,
                &BtreeKey::new(100, 1, KeyType::Normal),
                false,
                BtreeId::Extents,
            );
            assert_eq!(node.lock.six_lock_counts().n[0], 1);
        }

        assert_eq!(node.lock.six_lock_counts().n[0], 0);
    }

    // ─── R3: bch2_trans_record_locked_seqs() 调用位置验证测试 ─────────────

    /// 验证 bch2_trans_record_locked_seqs 在 try_lock_all 之后调用
    /// 通过比较 commit 前后 locked_seq 的值：如果 bch2_trans_record_locked_seqs
    /// 在 try_lock_all 之前调用，locked_seq 会是旧值（0）。
    /// 在 try_lock_all 之后调用，locked_seq 应等于 lock.six_lock_seq()。
    #[test]
    fn test_bch2_trans_record_locked_seqs_after_try_lock_all() {
        let root = make_root();
        let mut t = make_transaction();
        t.bch2_trans_get_iter(
            &root,
            &BtreeKey::new(100, 1, KeyType::Normal),
            false,
            BtreeId::Extents,
        );

        // commit 前的 locked_seq 应为初始值 0
        assert_eq!(
            match &t.path_ref(t.iters[0].path).levels[0] {
                BtreePathNode::Node(level) => level.locked_seq,
                _ => panic!("expected leaf node"),
            },
            0,
            "locked_seq should be 0 before commit"
        );

        t.__bch2_trans_commit().unwrap();

        // commit 后 locked_seq 应等于 lock.six_lock_seq()（bch2_trans_record_locked_seqs 在 try_lock_all 后调用）
        for level in &t.path_ref(t.iters[0].path).levels {
            if let BtreePathNode::Node(level) = level {
                assert_eq!(
                    level.locked_seq,
                    level.node.lock.six_lock_seq(),
                    "locked_seq should match lock.six_lock_seq() after commit (bch2_trans_record_locked_seqs post try_lock_all)"
                );
            }
        }
    }

    /// 验证 bch2_trans_record_locked_seqs 在节点 seq 已递增的情况下记录正确值
    /// 先做一次 commit 使节点 seq 递增，再开新事务 commit，验证
    /// 第二次 commit 的 locked_seq 反映的是第二次 try_lock_all 后的 seq。
    #[test]
    fn test_bch2_trans_record_locked_seqs_reflects_post_lock_seq() {
        let root = make_root();

        // 第一次事务：commit 使 root 节点 seq 递增
        let mut t1 = make_transaction();
        t1.bch2_trans_get_iter(
            &root,
            &BtreeKey::new(100, 1, KeyType::Normal),
            true,
            BtreeId::Extents,
        );
        journal_push_entry(
            &mut t1,
            BtreeOp::Insert,
            0,
            false,
            BtreeKey::new(100, 1, KeyType::Normal),
            BchVal::new(0, 0),
        );
        t1.__bch2_trans_commit().unwrap();
        // 解锁后 root seq 应已递增
        let _seq_after_first = match &t1.path_ref(t1.iters[0].path).levels[0] {
            BtreePathNode::Node(level) => level.node.lock.six_lock_seq(),
            _ => panic!("expected leaf node"),
        };

        // 第二次事务：验证 locked_seq 记录的是当前 seq（>0）
        let mut t2 = make_transaction();
        t2.bch2_trans_get_iter(
            &root,
            &BtreeKey::new(100, 1, KeyType::Normal),
            false,
            BtreeId::Extents,
        );
        t2.__bch2_trans_commit().unwrap();

        // locked_seq 应等于当前 lock.six_lock_seq()，且应 > 0（因为第一次 commit 递增了 seq）
        // 注意：如果第一次 commit 没有写操作（journal 空），seq 不会递增
        // 所以这个测试依赖 journal_push_entry 使第一次 commit 实际执行写操作
        for level in &t2.path_ref(t2.iters[0].path).levels {
            if let BtreePathNode::Node(level) = level {
                assert_eq!(
                    level.locked_seq,
                    level.node.lock.six_lock_seq(),
                    "locked_seq should match lock.six_lock_seq() in second transaction"
                );
            }
        }
    }

    /// 对应本地 locking.c:1487-1517：relock 失败保留
    /// `transaction_restart_relock` 错误身份，并释放已重获取的锁。
    #[test]
    fn test_bch2_trans_relock_notrace_preserves_error_identity() {
        let root = make_root();
        let mut trans = make_transaction();
        trans.bch2_trans_get_iter(
            &root,
            &BtreeKey::new(100, 1, KeyType::Normal),
            false,
            BtreeId::Extents,
        );
        let path_idx = trans.iters[0].path;
        trans.path_mut(path_idx).should_be_locked = true;
        let node = match &trans.path_ref(path_idx).levels[0] {
            BtreePathNode::Node(level) => Arc::clone(&level.node),
            _ => panic!("expected leaf node"),
        };

        trans.bch2_trans_unlock();
        node.lock.six_lock_intent();
        node.lock.six_lock_write();
        node.lock.six_unlock_write();
        node.lock.six_unlock_intent();

        assert_eq!(
            trans.bch2_trans_relock_notrace(),
            Err(RestartReason::Relock)
        );
        assert_eq!(trans.restart_reason(), Some(RestartReason::Relock));
        assert!(!trans.bch2_trans_locked());
    }
}
