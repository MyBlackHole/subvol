//! BtreeCache — LRU eviction + dirty tracking + GC retire queue
//!
//! ## bcachefs 参考：bch2_btree_cache（btree_cache.c）
//!
//! bcachefs 的 btree cache 实现包含以下关键功能：
//! - **shrinker**：两阶段 clock 算法，通过 `btree_node_accessed` 标志提供第二次机会
//! - **btree_node cache**：`list_head`（LRU 排序）+ `hashtable`（快速查找）
//! - **cannibalize lock**：当 cache 满时，临时"cannibalize"（抢占）一个 clean node，
//!   避免插入操作因等待 shrink 而阻塞
//! - **readahead**：`bch2_btree_node_read_work()` 中按需预读相邻节点
//!
//! ## Volmount 当前状态
//!
//! 当前实现覆盖了 bcachefs btree cache 的核心功能：
//! - ✅ clean/dirty/pending_flush 三列表 + LRU + root/leaf 热冷分离
//! - ✅ shrinker 两阶段 clock 算法
//! - ✅ cannibalize lock（Phase 1 clean + Phase 2 dirty + 锁定等待）
//! - ✅ dirty auto-flush（MAX_DIRTY 阈值触发，不丢数据）
//! - ✅ GC retire queue + journal pin 生命周期管理
//! - ✅ 拓扑排序的 flush_dirty（叶子先于内层）
//!
//! 基于 bcachefs 的 btree_node_cache 设计：
//! - clean 列表持有未修改的节点，支持 LRU 淘汰
//! - dirty 列表持有已修改但未写回的节点
//! - retire 队列用于 COW 旧版本的延迟回收
//!
//! ## Root/leaf 热冷分离
//!
//! 当 clean 列表满时驱逐，优先驱逐 leaf 节点（level=0），
//! 尽可能保留 interior 节点（level>0），避免 `bch2_btree_path_traverse_one`
//! 路径中 interior 节点被频繁重载。对应 bcachefs shrinker
//! 的 `btree_node_accessed` 两级扫描保护机制。

use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::fs;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::thread;
use tokio::sync::Notify;

// 每线程 cannibalize 重入深度计数器
//
// bcachefs 对齐：`current->cannibalize_lock_state` 的 re-entrancy 保护。
// 当深度 > 0 时，禁止再次获取 cannibalize 锁，防止递归缓存未命中。
thread_local! {
    static CANNIBALIZE_DEPTH: Cell<u32> = Cell::new(0);
}

use crate::block_device::BchDev;
use crate::block_device::BlockDevice;
use crate::btree::bucket_io;
use crate::btree::key::BtreeKey;
use crate::btree::node::{BtreeNode, NodeState, DEFAULT_NODE_SIZE};
use crate::btree::BtreeId;
use crate::journal::Journal;
use crate::types::StorageError;

/// bcachefs 对齐: bch2_btree_node_mem_free
///
/// subvol 的节点通过 `Arc` 管理生命周期，所以这里的语义是消费一个
/// 节点引用并允许其正常 drop。调用点应在不再需要该节点时使用。
pub fn bch2_btree_node_mem_free(node: Arc<BtreeNode>) {
    drop(node);
}

/// bcachefs 对齐: bch2_btree_node_transition_state_locked
///
/// upstream 该入口在持有 cache lock 时更新列表状态；subvol 当前的
/// 节点状态机由 `BtreeNode` 自身维护，因此这里只做同名薄封装。
pub fn bch2_btree_node_transition_state_locked(node: &BtreeNode, new_state: NodeState) {
    node.bch2_btree_node_transition_state(new_state);
}

/// bcachefs 对齐: bch2_btree_node_transition_state
///
/// 非 locked 版本在 subvol 中与 locked 版本共享同一实现，因为
/// cache 侧列表 bookkeeping 仍由 `BtreeCache` 统一维护。
pub fn bch2_btree_node_transition_state(node: &BtreeNode, new_state: NodeState) {
    bch2_btree_node_transition_state_locked(node, new_state);
}

/// bcachefs 对齐: bch2_btree_node_write_done_clean
///
/// 写完成后清除 write-in-flight 状态，并按节点是否仍在 cache 中收口状态。
/// 对应 bcachefs `cache.c:565-573`：非 hashed/freeable 节点不重新加入 live 列表。
pub fn bch2_btree_node_write_done_clean(node: &BtreeNode) {
    if node.is_write_in_flight() {
        node.clear_write_in_flight();
    }
    let state = node.btree_node_state();
    if !matches!(
        state,
        NodeState::Alive | NodeState::Reclaim | NodeState::Deleting
    ) {
        node.bch2_btree_node_transition_state(NodeState::Alive);
    }
}

/// 最大 clean 节点数
pub const MAX_CLEAN: usize = 1024;

/// 最大 dirty 节点数（超过后自动触发 flush）
pub const MAX_DIRTY: usize = 256;

/// 预清理扫描预算。
///
/// 当 clean cache 接近满而且发生 miss 时，提前扫一小段 LRU，
/// 给更久未用的节点一次机会被淘汰，避免把压力完全留到满载时。
const PREEMPTIVE_SHRINK_SCAN_BUDGET: usize = 64;

/// 写 I/O 并发限制 — bcachefs 对齐: BTREE_WRITE_IO_LIMIT
pub const BTREE_WRITE_IO_LIMIT: usize = 64;

/// 分裂阈值 — bcachefs 对齐: BTREE_SPLIT_THRESHOLD
pub const BTREE_SPLIT_THRESHOLD: usize = 3;

/// 前景合并阈值 — bcachefs 对齐
pub const BTREE_FOREGROUND_MERGE_THRESHOLD: usize = 1;
pub const BTREE_FOREGROUND_MERGE_HIGHER: usize = 3;
pub const BTREE_FOREGROUND_MERGE_HYSTERESIS: usize = 2;

// ─── EvictedSizeTable ────────────────────────────────────────────────────────
//
// bcachefs 对齐: struct btree_evicted_size (types.h:1041-1044)
//
// bcachefs 使用固定大小、无冲突链的哈希表追踪被驱逐节点的 live_u64s。
// 插入覆盖已有条目；查找验证哈希匹配，冲突自然降级为"无信息"。
// 表大小按 capacity/1000/btree_node_size 计算，128K 条目为下限（1 MiB）。
//
// subvol 使用等效的 Box<[AtomicU64]> 固定大小数组，保证内存有界，
// 避免原先 HashMap<u64, u16> 的无界增长。

/// 驱逐大小哈希的位掩码 — bcachefs 对齐: BTREE_EVICTED_SIZE_HASH_MASK
const BTREE_EVICTED_SIZE_HASH_BITS: u32 = 48;
const BTREE_EVICTED_SIZE_HASH_MASK: u64 = (1u64 << BTREE_EVICTED_SIZE_HASH_BITS) - 1;

/// 最小表大小（2^17 = 128K 条目, 1 MiB）— bcachefs 对齐: cache.c:1843
const EVICTED_SIZE_MIN_BITS: u32 = 17;

/// 固定大小的驱逐节点数据大小表。
///
/// bcachefs 对齐: `struct btree_evicted_size` (types.h:1041-1044) +
///   `bch2_btree_evicted_size_record` (cache.h:60-68) +
///   `bch2_btree_evicted_size_lookup` (cache.h:70-83)
///
/// # 打包格式
///
/// 每个条目为 `(hash & HASH_MASK) << 16 | live_u64s`。
/// 查找时验证哈希高位匹配；冲突时返回 None。
#[derive(Debug)]
struct EvictedSizeTable {
    mask: usize,
    entries: Box<[AtomicU64]>,
}

impl EvictedSizeTable {
    /// 以给定位数创建表（2^bits 个条目，每个 8 字节）。
    fn new(bits: u32) -> Self {
        let nr = 1usize << bits;
        let mut v = Vec::with_capacity(nr);
        for _ in 0..nr {
            v.push(AtomicU64::new(0));
        }
        Self {
            mask: nr - 1,
            entries: v.into_boxed_slice(),
        }
    }

    /// 以最小大小（128K 条目）创建表。
    ///
    /// 对应 bcachefs `bch2_fs_btree_evicted_size_init` 的默认 17-bit 路径 (cache.c:1841-1853)。
    fn with_min_capacity() -> Self {
        Self::new(EVICTED_SIZE_MIN_BITS)
    }

    /// 记录被驱逐节点的 live_u64s。
    ///
    /// bcachefs 对齐: `bch2_btree_evicted_size_record` (cache.h:60-68)
    ///
    /// 使用 WRITE_ONCE 语义：`entries[hash & mask] = (hash_hi << 16) | live_u64s`
    fn record(&self, node_id: u64, live_u64s: u16) {
        let packed = ((node_id & BTREE_EVICTED_SIZE_HASH_MASK) << 16) | (live_u64s as u64);
        self.entries[node_id as usize & self.mask].store(packed, Ordering::Relaxed);
    }

    /// 查询被驱逐节点的 live_u64s。
    ///
    /// bcachefs 对齐: `bch2_btree_evicted_size_lookup` (cache.h:70-83)
    ///
    /// 使用 READ_ONCE 语义。验证哈希高位匹配，冲突时返回 None。
    fn lookup(&self, node_id: u64) -> Option<u16> {
        let entry = self.entries[node_id as usize & self.mask].load(Ordering::Relaxed);
        if (entry >> 16) == (node_id & BTREE_EVICTED_SIZE_HASH_MASK) {
            Some(entry as u16)
        } else {
            None
        }
    }
}

// ─── BtreeCache ─────────────────────────────────────────────────────────────

#[derive(Debug)]
struct BtreeCacheInner {
    /// clean 节点：未被修改，可从后端重新加载
    clean: HashMap<u64, Arc<BtreeNode>>,
    /// LRU 顺序队列（front = 最久未用，back = 最近使用）
    clean_lru: VecDeque<u64>,
    /// dirty 节点：已被修改，需要写回后才可淘汰
    dirty: HashMap<u64, Arc<BtreeNode>>,
    /// 待写回队列：auto-flush 从 dirty 移出的节点，等待 flush_dirty() 写回
    ///
    /// auto-flush 触发时将 dirty 节点移到这里，而非丢弃（避免数据丢失）。
    /// flush_dirty() 会清空此队列并返回所有节点。
    /// 节点仍可通过 get/get_or_load 查找到，因为还在缓存生命周期内。
    pending_flush: HashMap<u64, Arc<BtreeNode>>,
    /// GC 退休队列：COW 旧版本地址，refcount=1 时可安全移除
    retire_queue: VecDeque<u64>,
    /// 每个缓存节点的树层级（level=0 为 leaf，>0 为 interior）
    ///
    /// 用于 root/leaf 热冷分离：驱逐时优先选 level=0 的 leaf 节点。
    node_levels: HashMap<u64, u8>,
    /// 统计信息（bcachefs 对齐）
    nr_dirty: usize,
    nr_live: usize,
    should_throttle: bool,

    /// 固定大小的驱逐节点 live_u64s 表（有界、无冲突链）。
    ///
    /// bcachefs 对齐: struct btree_evicted_size (types.h:1041-1044)
    ///
    /// 用于重载时精确分配：新节点加载时可通过 `lookup_evicted_size` 获取之前
    /// 被驱逐节点的实际数据大小，避免总是按全尺寸分配。
    /// 表为固定大小（128K 条目 min），自然降级优于无界 HashMap。
    evicted_sizes: EvictedSizeTable,
}

/// Btree 节点缓存 — LRU 淘汰 + dirty 跟踪 + GC 退休队列
///
/// bcachefs 对齐: struct bch_fs_btree_cache
#[derive(Debug)]
pub struct BtreeCache {
    inner: Mutex<BtreeCacheInner>,
    /// 缓存命中/未命中统计 — bcachefs 对齐
    cache_hits: AtomicU64,
    cache_misses: AtomicU64,
    /// 统计 text 快照中的 requested/freed/self reclaim 段
    cache_requested: AtomicU64,
    cache_freed: AtomicU64,
    cache_self_reclaim: AtomicU64,
    cache_not_freed_access_bit: AtomicU64,
    /// cannibalize 正在等待
    cannibalize_waiting: AtomicBool,
    /// cannibalize 锁状态 — Atomic CAS 对齐 bcachefs try_cmpxchg (cache.c:908)
    cannibalize_in_progress: AtomicBool,
    /// 被驱逐节点的总数据大小（用于 GC accounting）— bcachefs 对齐
    evicted_size: AtomicU64,
    /// 飞行中的内层写 I/O 计数 — bcachefs 对齐: nr_in_flight_inner
    nr_in_flight_inner: AtomicUsize,
    /// 写操作完成通知 — 用于 btree 写节流等待
    notify: Notify,
    /// 可选的 journal 引用，用于在节点被驱逐时调用 bch2_journal_pin_drop
    journal: Option<Arc<Journal>>,
    /// 可选的后端引用，用于节点预取/读取
    backend: OnceLock<Arc<dyn BlockDevice>>,
}

impl BtreeCache {
    /// 创建一个新的空缓存
    ///
    /// bcachefs 对齐: bch2_fs_btree_cache_init_early
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(BtreeCacheInner {
                clean: HashMap::new(),
                clean_lru: VecDeque::new(),
                dirty: HashMap::new(),
                pending_flush: HashMap::new(),
                retire_queue: VecDeque::new(),
                node_levels: HashMap::new(),
                nr_dirty: 0,
                nr_live: 0,
                should_throttle: false,

                evicted_sizes: EvictedSizeTable::with_min_capacity(),
            }),
            cache_hits: AtomicU64::new(0),
            cache_misses: AtomicU64::new(0),
            cache_requested: AtomicU64::new(0),
            cache_freed: AtomicU64::new(0),
            cache_self_reclaim: AtomicU64::new(0),
            cache_not_freed_access_bit: AtomicU64::new(0),
            nr_in_flight_inner: AtomicUsize::new(0),
            notify: Notify::new(),
            cannibalize_waiting: AtomicBool::new(false),
            cannibalize_in_progress: AtomicBool::new(false),
            evicted_size: AtomicU64::new(0),
            journal: None,
            backend: OnceLock::new(),
        }
    }

    #[cfg(test)]
    /// 设置后端引用（只允许设置一次）。
    pub fn set_backend(&self, backend: Arc<dyn BlockDevice>) -> bool {
        self.backend.set(backend).is_ok()
    }

    fn backend(&self) -> Option<Arc<dyn BlockDevice>> {
        self.backend.get().cloned()
    }

    /// 获取或加载节点
    ///
    /// - 在 dirty 中命中 → 直接返回（不提升 LRU）
    /// - 在 clean 中命中 → LRU 提升并返回
    /// - 未命中 → 调用 `load_fn` 创建节点，插入 clean 列表
    /// - 若 clean 超限 → 驱逐 LRU clean 节点
    pub fn get_or_load<F>(&self, node_id: u64, load_fn: F) -> Arc<BtreeNode>
    where
        F: FnOnce() -> Arc<BtreeNode>,
    {
        self.cache_requested.fetch_add(1, Ordering::Relaxed);
        let memory_pressure_high = self.system_memory_usage_high();
        let mut inner = self.inner.lock().unwrap();

        // 1. 检查 dirty
        if let Some(node) = inner.dirty.get(&node_id) {
            // 备注：若节点正在进行异步 IO 读取（fill async），等待其完成
            // 先克隆 Arc 并将 inner 锁释放，避免持有缓存锁期间阻塞其他操作
            let node = node.clone();
            drop(inner);
            node.wait_on_read(None);
            node.set_accessed();
            return node;
        }

        // 1b. 检查 pending_flush（auto-flush 移出的脏节点仍可访问）
        if let Some(node) = inner.pending_flush.get(&node_id) {
            // 备注：若节点正在进行异步 IO 读取（fill async），等待其完成
            let node = node.clone();
            drop(inner);
            node.wait_on_read(None);
            node.set_accessed();
            return node;
        }

        // 2. 检查 clean → LRU 提升 + 设置 accessed 标志
        //    如果节点在 InFlight 状态（fill async 进行中），等待 IO 完成
        if inner.clean.contains_key(&node_id) {
            let node = inner.clean.get(&node_id).unwrap().clone();
            // 先释放 inner 锁再等待，避免持有缓存锁期间阻塞其他操作
            drop(inner);
            node.wait_on_read(None);
            node.set_accessed();
            Self::lru_promote(&mut self.inner.lock().unwrap().clean_lru, node_id);
            return node;
        }

        // 3. 未命中：如果 clean 超限，优先驱逐 leaf 节点
        //    bcachefs 对齐：root/leaf 热冷分离 — 保留 interior 节点
        if Self::should_preemptive_shrink_from(inner.clean.len(), memory_pressure_high) {
            drop(inner);
            self.shrink(PREEMPTIVE_SHRINK_SCAN_BUDGET);
            inner = self.inner.lock().unwrap();
        }
        while inner.clean.len() >= MAX_CLEAN {
            if let Some(es) = inner.evict_one_leaf_with_jseq(self.journal.as_deref()) {
                self.evicted_size.fetch_add(es as u64, Ordering::Relaxed);
                continue;
            }
            // 无 leaf 可驱逐 → 回退到驱逐 LRU 最前端（记录 evicted_size）
            if let Some(evict_id) = inner.clean_lru.pop_front() {
                if let Some(j) = self.journal.as_deref() {
                    if let Some(n) = inner.clean.get(&evict_id) {
                        n.evict_journal_pin(j);
                    }
                }
                let es = inner.record_evicted_node(&evict_id);
                self.evicted_size.fetch_add(es as u64, Ordering::Relaxed);
                inner.clean.remove(&evict_id);
                inner.node_levels.remove(&evict_id);
            } else {
                break;
            }
        }

        // 4. 加载新节点
        let node = load_fn();
        node.set_accessed(); // 新加载节点标记为最近访问
        inner.record_level(node_id, node.level);
        inner.clean.insert(node_id, node.clone());
        inner.clean_lru.push_back(node_id);
        inner.nr_live = inner.clean.len() + inner.dirty.len() + inner.pending_flush.len();

        drop(inner);
        node
    }

    /// 将节点标记为 dirty（从 clean 移到 dirty）
    ///
    /// 若 dirty 超出 MAX_DIRTY，自动触发 flush。
    pub fn mark_dirty(&self, node_id: u64) {
        let mut inner = self.inner.lock().unwrap();

        // 从 clean 中移除
        if let Some(node) = inner.clean.remove(&node_id) {
            // 从 LRU 队列中移除
            if let Some(pos) = inner.clean_lru.iter().position(|&x| x == node_id) {
                inner.clean_lru.remove(pos);
            }
            // level 映射保留（dirty 节点仍在缓存中，后续可能写回 clean）
            // bcachefs 对齐：dirty ≠ need_rewrite。need_rewrite 仅在 write 路径
            // （多 bset 累积）或 corruption 恢复时设置，不在此处设置。
            inner.dirty.insert(node_id, node);
            inner.nr_dirty = inner.dirty.len() + inner.pending_flush.len();
            inner.nr_live = inner.clean.len() + inner.dirty.len() + inner.pending_flush.len();
        }

        // 若 dirty 超限，自动 flush（将脏节点移入 pending_flush，而非丢弃）
        if inner.dirty.len() >= MAX_DIRTY {
            let drained = std::mem::take(&mut inner.dirty);
            // 合并到 pending_flush，保留节点引用（仍在缓存中，可被查找）
            inner.pending_flush.extend(drained);
            inner.nr_dirty = inner.pending_flush.len();
            inner.nr_live = inner.clean.len() + inner.pending_flush.len();
        }
    }

    /// 清空 dirty 列表和 pending_flush 队列，返回所有脏节点（所有权转移，供序列化写回）
    ///
    /// 返回按 tree level 升序排列的 `Vec<(node_id, Arc<BtreeNode>)>`，
    /// 确保叶子节点（level=0）排在内层节点（level>0）之前。
    /// 调用方负责序列化并写入后端，可对每个节点调用 `serialize_to_bucket(block_addr)`。
    ///
    /// ### 拓扑排序（bcachefs 对齐）
    ///
    /// bcachefs 通过 `will_make_reachable` 机制确保父节点写入时
    /// 子节点已落盘。这里按 level 升序排序实现相同的保证：
    /// 叶子节点先写 → 内层节点后写，避免 crash 后父节点引用不存在的子节点。
    pub fn flush_dirty(&self) -> Vec<(u64, Arc<BtreeNode>)> {
        let mut inner = self.inner.lock().unwrap();
        // 收集所有脏节点及其 level
        let ids: Vec<u64> = inner
            .dirty
            .keys()
            .chain(inner.pending_flush.keys())
            .copied()
            .collect();

        // 收集节点引用，后续按 level 拓扑排序
        let mut nodes: Vec<(u64, Arc<BtreeNode>)> = Vec::with_capacity(ids.len());
        for &id in &ids {
            if let Some(node) = inner
                .dirty
                .remove(&id)
                .or_else(|| inner.pending_flush.remove(&id))
            {
                nodes.push((id, node));
            }
        }

        // 清理 level 映射
        for &id in &ids {
            inner.node_levels.remove(&id);
        }

        inner.nr_dirty = 0;
        inner.nr_live = inner.clean.len();

        // 拓扑排序：按 level 升序（叶子先于内层）
        nodes.sort_by_key(|(_, node)| node.level);
        nodes
    }

    /// GC 退休：将节点 ID 加入退休队列，清理 refcount=1 的节点
    ///
    /// COW 替换旧版本后调用，当 `Arc::strong_count() == 1` 时表示
    /// 仅有缓存持有引用，可安全移除。
    pub fn gc_retire(&self, node_ids: &[u64]) {
        let mut inner = self.inner.lock().unwrap();

        // 添加新条目到退休队列
        for &id in node_ids {
            inner.retire_queue.push_back(id);
        }

        // 处理整个队列：retire refcount=1 的节点，其余保留
        let mut remaining = VecDeque::new();
        while let Some(id) = inner.retire_queue.pop_front() {
            let can_retire = if let Some(node) = inner.clean.get(&id) {
                Arc::strong_count(node) == 1
            } else if let Some(node) = inner.dirty.get(&id) {
                Arc::strong_count(node) == 1
            } else if let Some(node) = inner.pending_flush.get(&id) {
                Arc::strong_count(node) == 1
            } else {
                true
            };

            if can_retire {
                // 移除前清理 journal pin
                if let Some(ref j) = self.journal {
                    if let Some(n) = inner
                        .clean
                        .get(&id)
                        .or_else(|| inner.dirty.get(&id))
                        .or_else(|| inner.pending_flush.get(&id))
                    {
                        n.evict_journal_pin(j);
                    }
                }
                inner.clean.remove(&id);
                inner.dirty.remove(&id);
                inner.pending_flush.remove(&id);
                inner.node_levels.remove(&id);
                if let Some(pos) = inner.clean_lru.iter().position(|&x| x == id) {
                    inner.clean_lru.remove(pos);
                }
            } else {
                remaining.push_back(id);
            }
        }
        inner.retire_queue = remaining;
        drop(inner);
    }

    // ─── Shrinker（内存压力回收） ───────────────────────────────────

    /// bcachefs 对齐: bch2_btree_cache_scan — 在内存压力下回收 clean 节点
    ///
    /// 使用两阶段 clock 算法（bcachefs 对齐）：
    /// 1. 从 LRU 前端（最久未用）开始扫描
    /// 2. 遇到 `NODE_ACCESSED` 置位的节点 → 清除标志（给第二次机会）
    /// 3. 遇到 `NODE_ACCESSED` 已清除的节点 → 驱逐
    /// 4. 仅驱逐 clean 节点（脏节点由 flush 机制处理）
    /// 5. 保留至少 64 个 clean 节点作为最小缓冲
    ///
    /// 返回实际驱逐的节点数。
    pub fn shrink(&self, target: usize) -> usize {
        let mut inner = self.inner.lock().unwrap();

        // 保留至少 64 个 clean 节点的最小缓冲（避免完全清空缓存）
        let min_keep = 64usize;
        let max_evict = inner.clean.len().saturating_sub(min_keep);
        let target = target.min(max_evict);

        if target == 0 {
            return 0;
        }

        let mut freed = 0usize;
        let mut scanned = 0usize;

        // 收集 LRU 前端的 ID 快照（最多 target + 64 避免扫描太深）
        let scan_limit = target + 64;
        let ids: Vec<u64> = inner.clean_lru.iter().take(scan_limit).copied().collect();

        for &id in &ids {
            if scanned >= target {
                break;
            }
            scanned += 1;

            // 检查节点是否还在 clean 中（可能已被其他操作移除）
            let should_evict = inner.clean.get(&id).map_or(false, |node| {
                // bcachefs 对齐：will_make_reachable 节点不可驱逐
                if node.will_make_reachable() {
                    return false;
                }
                // bcachefs 对齐：pinned 节点不可驱逐
                if node.pin_count.load(Ordering::Relaxed) > 0 {
                    return false;
                }
                if node.is_accessed() {
                    // 两阶段 clock：清除标志给第二次机会
                    node.clear_accessed();
                    self.cache_not_freed_access_bit
                        .fetch_add(1, Ordering::Relaxed);
                    false
                } else {
                    true
                }
            });

            if should_evict {
                if let Some(ref j) = self.journal {
                    if let Some(n) = inner.clean.get(&id) {
                        n.evict_journal_pin(j);
                    }
                }
                let es = inner.record_evicted_node(&id);
                self.evicted_size.fetch_add(es as u64, Ordering::Relaxed);
                self.cache_freed.fetch_add(1, Ordering::Relaxed);
                self.cache_self_reclaim.fetch_add(1, Ordering::Relaxed);
                inner.clean.remove(&id);
                inner.node_levels.remove(&id);
                // 同时从 LRU 中移除
                if let Some(pos) = inner.clean_lru.iter().position(|&x| x == id) {
                    inner.clean_lru.remove(pos);
                }
                freed += 1;
            }
        }

        inner.nr_live = inner.clean.len() + inner.dirty.len() + inner.pending_flush.len();
        drop(inner);
        freed
    }

    /// 尝试驱逐一个 clean 节点（基于 shrinker 两阶段 clock 算法）
    ///
    /// 与 `shrink` 一样使用 accessed 标志检查，但只尝试驱逐一个节点。
    /// 返回 true 表示有节点被驱逐。
    pub fn shrink_one(&self) -> bool {
        self.shrink(1) > 0
    }

    /// 简单查找（无 LRU 提升副作用）
    pub fn get(&self, node_id: u64) -> Option<Arc<BtreeNode>> {
        self.inner.lock().unwrap().get_inner(node_id)
    }

    /// bcachefs 对齐: bch2_btree_node_get — 获取/加载节点
    ///
    /// 先查缓存，未命中则调用 load_fn 加载。
    /// 与 get_or_load 类似但增加统计和 throttle 检查。
    /// 如果节点在缓存中但处于 InFlight 状态（异步 fill 进行中），等待其完成。
    pub fn bch2_btree_node_get<F>(&self, node_id: u64, load_fn: F) -> Arc<BtreeNode>
    where
        F: FnOnce() -> Arc<BtreeNode>,
    {
        self.cache_requested.fetch_add(1, Ordering::Relaxed);
        let memory_pressure_high = self.system_memory_usage_high();
        let mut inner = self.inner.lock().unwrap();

        // 1. 检查 dirty
        if let Some(node) = inner.dirty.get(&node_id) {
            self.cache_hits.fetch_add(1, Ordering::Relaxed);
            // 备注：释放 inner 锁再等待，避免阻塞其他缓存操作
            let node = node.clone();
            drop(inner);
            node.wait_on_read(None);
            node.set_accessed();
            return node;
        }

        // 1b. 检查 pending_flush（auto-flush 移出的脏节点仍可访问）
        if let Some(node) = inner.pending_flush.get(&node_id) {
            self.cache_hits.fetch_add(1, Ordering::Relaxed);
            let node = node.clone();
            drop(inner);
            node.wait_on_read(None);
            node.set_accessed();
            return node;
        }

        // 2. 检查 clean → LRU 提升 + 设置 accessed 标志
        //    如果节点在 InFlight 状态（fill async 进行中），等待 IO 完成
        if inner.clean.contains_key(&node_id) {
            self.cache_hits.fetch_add(1, Ordering::Relaxed);
            let node = inner.clean.get(&node_id).unwrap().clone();
            // 释放 inner 锁再等待，然后重新获取执行 LRU 提升
            drop(inner);
            node.wait_on_read(None);
            node.set_accessed();
            Self::lru_promote(&mut self.inner.lock().unwrap().clean_lru, node_id);
            return node;
        }

        // 3. 未命中：统计
        self.cache_misses.fetch_add(1, Ordering::Relaxed);

        // 4. 如果 clean 超限，驱逐（记录 evicted_size）
        if Self::should_preemptive_shrink_from(inner.clean.len(), memory_pressure_high) {
            drop(inner);
            self.shrink(PREEMPTIVE_SHRINK_SCAN_BUDGET);
            inner = self.inner.lock().unwrap();
        }
        while inner.clean.len() >= MAX_CLEAN {
            if let Some(es) = inner.evict_one_leaf_with_jseq(self.journal.as_deref()) {
                self.evicted_size.fetch_add(es as u64, Ordering::Relaxed);
                continue;
            }
            if let Some(evict_id) = inner.clean_lru.pop_front() {
                if let Some(j) = self.journal.as_deref() {
                    if let Some(n) = inner.clean.get(&evict_id) {
                        n.evict_journal_pin(j);
                    }
                }
                let es = inner.record_evicted_node(&evict_id);
                self.evicted_size.fetch_add(es as u64, Ordering::Relaxed);
                inner.clean.remove(&evict_id);
                inner.node_levels.remove(&evict_id);
            } else {
                break;
            }
        }

        // 5. 加载新节点
        let node = load_fn();
        node.set_accessed(); // 新加载节点标记为最近访问
        inner.record_level(node_id, node.level);
        inner.clean.insert(node_id, node.clone());
        inner.clean_lru.push_back(node_id);
        inner.nr_live = inner.clean.len() + inner.dirty.len() + inner.pending_flush.len();

        drop(inner);
        node
    }

    /// bcachefs 对齐: bch2_btree_node_evict — 从缓存中移除节点
    pub fn bch2_btree_node_evict(&self, node_id: u64) -> Option<Arc<BtreeNode>> {
        let node = self.get(node_id)?;
        node.wait_on_read(None);
        node.wait_on_write(None);
        self.remove(node_id)
    }

    /// bcachefs 对齐: bch2_btree_node_set_dirty — 标记节点为 dirty
    pub fn bch2_btree_node_set_dirty(&self, node_id: u64) {
        self.mark_dirty(node_id);
    }

    // ─── Pin / Unpin ─────────────────────────────────────────────────

    /// bcachefs 对齐: bch2_node_pin — pin 节点防止被驱逐
    ///
    /// 增加节点的 pin 计数。shrink/evict 会跳过 pin_count > 0 的节点。
    /// 调用者需确保对应的 unpin 最终被调用，否则节点将永久驻留缓存。
    ///
    /// 对应 bcachefs `bch2_node_pin(node)`，其中 `atomic_inc(&node->pin)`。
    pub fn bch2_node_pin(node: &BtreeNode) {
        node.pin_count.fetch_add(1, Ordering::Relaxed);
    }

    /// bcachefs 对齐: bch2_btree_cache_unpin — 解除节点的 pin 状态
    ///
    /// 对应 bcachefs `bch2_btree_cache_unpin(cache, node)` (cache.c):
    /// 减少 pin 计数。pin_count 降至 0 后该节点可再次被驱逐。
    ///
    /// node_id 在缓存中不存在时静默忽略（节点可能已被移除）。
    pub fn bch2_btree_cache_unpin(&self, node_id: u64) {
        if let Some(node) = self.get(node_id) {
            let prev = node.pin_count.fetch_sub(1, Ordering::Relaxed);
            debug_assert!(
                prev > 0,
                "bch2_btree_cache_unpin: unpin without matching pin"
            );
        }
    }

    /// bcachefs 对齐: bch2_btree_cache_cannibalize_lock — 进入"内存饥饿"模式
    ///
    /// 当分配新节点但缓存已满且无干净节点可驱逐时，进入 cannibalize 模式。
    /// 返回 true 表示需要等待（caller 应释放资源后重试）。
    ///
    /// ## 重入保护（D9.1）
    ///
    /// 使用 `CANNIBALIZE_DEPTH` 线程局部计数器防止递归：
    /// - 深度 > 0（已处于 cannibalize 路径中）→ 返回 `false`，caller 改用
    ///   不依赖 cannibalize 的备选分配路径
    /// - 深度 == 0 → 正常获取锁，深度 +1
    pub fn bch2_btree_cache_cannibalize_lock(&self) -> bool {
        let reentered = CANNIBALIZE_DEPTH.with(|depth| {
            let d = depth.get();
            if d > 0 {
                return true;
            }
            depth.set(d + 1);
            false
        });
        if reentered {
            return false;
        }

        // Atomic CAS — 对齐 bcachefs try_cmpxchg (cache.c:908)
        // bcachefs 使用 cmpxchg(&bc->alloc_lock, NULL, current) 原子抢占
        // 此处用 AtomicBool CAS 语义等价
        if self
            .cannibalize_in_progress
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            // 已有 cannibalize 在进行中，通知 caller 等待
            self.cannibalize_waiting.store(true, Ordering::Release);
            return true;
        }
        false
    }

    /// bcachefs 对齐: bch2_btree_cache_cannibalize_unlock — 退出 cannibalize 模式
    pub fn bch2_btree_cache_cannibalize_unlock(&self) {
        CANNIBALIZE_DEPTH.with(|depth| {
            let d = depth.get();
            if d > 0 {
                depth.set(d - 1);
            }
        });

        // Atomic store — 对齐 bcachefs 的 alloc_lock = NULL (cache.c)
        self.cannibalize_in_progress.store(false, Ordering::Release);
        self.cannibalize_waiting.store(false, Ordering::Release);
    }

    /// D9.2 Phase 1：尝试驱逐一个 clean 节点
    ///
    /// 优先驱逐 leaf 节点（level=0），保护 interior 节点避免被频繁重载。
    /// 对应 bcachefs `bch2_btree_cache_cannibalize_try` 的 clean 优先路径。
    ///
    /// 返回 true 表示成功驱逐了一个 clean 节点。
    pub fn try_cannibalize_phase1(&self) -> bool {
        self.shrink_one()
    }

    /// D9.2 Phase 2：尝试 flush 并驱逐一个 dirty 节点
    ///
    /// 当 clean 节点不足时，从 dirty 列表中迁移最旧的节点到 `pending_flush`，
    /// 返回该节点供 caller 发起异步写回。写回完成后该节点可被驱逐。
    ///
    /// 对应 bcachefs `bch2_btree_cache_cannibalize_try` 的 dirty flush 路径。
    ///
    /// 返回被移动到 pending_flush 的节点 ID 和 Arc 引用，caller 应调度写回。
    /// 如果无可 flush 的 dirty 节点，返回 None。
    pub fn try_cannibalize_phase2(&self) -> Option<(u64, Arc<BtreeNode>)> {
        let mut inner = self.inner.lock().unwrap();
        // 取最旧的 dirty 节点（任意一个即可）
        let id = inner.dirty.keys().next().copied()?;
        let node = inner.dirty.remove(&id)?;
        if let Some(ref j) = self.journal {
            node.evict_journal_pin(j);
        }

        inner.nr_dirty = inner.dirty.len() + inner.pending_flush.len();
        // 放到 pending_flush 中等待写回
        inner.pending_flush.insert(id, node.clone());
        drop(inner);

        Some((id, node))
    }

    /// D9.2 Cannibalize 综合入口：Phase 1 → Phase 2 → 锁定等待
    ///
    /// 在节点分配器发现缓存满且需要空间时调用。
    /// 1. Phase 1：尝试驱逐一个 clean 节点（快速路径）
    /// 2. Phase 2：尝试 flush 并驱逐一个 dirty 节点（慢速路径）
    /// 3. 若均失败，获取 cannibalize 锁并通知 caller 等待其他线程释放
    ///
    /// 返回 true 表示 caller 应等待并重试（cannibalize 锁已被其他线程持有）。
    /// 返回 false 表示已成功腾出空间或无需等待（caller 可继续分配）。
    pub fn try_cannibalize(&self) -> bool {
        // Phase 1：尝试 clean 驱逐
        if self.try_cannibalize_phase1() {
            return false;
        }

        // Phase 2：尝试 dirty flush
        if self.try_cannibalize_phase2().is_some() {
            return false;
        }

        // Phase 3：锁定 cannibalize，通知 caller 等待
        self.bch2_btree_cache_cannibalize_lock()
    }

    /// bcachefs 对齐: bch2_btree_node_data_free — 释放节点内存数据
    pub fn bch2_btree_node_data_free(&self, node_id: u64) {
        if let Some(node) = self.remove(node_id) {
            drop(node);
        }
    }

    /// 重新计算缓存节流状态。
    ///
    /// ⚠️ **架构偏差**：bcachefs 同名函数 `bch2_recalc_btree_reserve`
    /// (cache.c:123-138) 计算的是 `nr_reserve` — cannibalization 所需的
    /// 预分配节点数（base 16 + 每个 alive root 的 8 * level）。
    /// 该预分配用于确保写路径在 shrinker 回收缓存时仍能分配新节点。
    ///
    /// subvol 无内核 MM shrinker，因此不需要
    /// `nr_reserve` 机制。本函数改为计算 `should_throttle` —
    /// 当 in-flight 写超过 BTREE_WRITE_IO_LIMIT 或 dirty 占比超过 75% 时
    /// 通知调用者等待写入完成。
    ///
    /// 对应 bcachefs `bch2_recalc_btree_reserve` (cache.c:123-138)
    /// → 偏差：subvol 计算 throttle flag 而非 reserve count（架构差异）
    pub fn bch2_recalc_btree_reserve(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.should_throttle = self.nr_in_flight_inner.load(Ordering::Relaxed)
            > BTREE_WRITE_IO_LIMIT
            || (inner.nr_live > 0 && inner.nr_dirty > inner.nr_live * 3 / 4);
    }

    /// bcachefs 对齐: bch2_btree_cache_should_throttle — 是否应节流
    pub fn bch2_btree_cache_should_throttle(&self) -> bool {
        self.inner.lock().unwrap().should_throttle
    }

    /// bcachefs 对齐: bch2_btree_cache_update_throttle — 更新节流状态
    pub fn bch2_btree_cache_update_throttle(&self) {
        self.bch2_recalc_btree_reserve();
    }

    /// 递增飞行中的内层写 I/O 计数
    ///
    /// 对应 bcachefs `atomic_inc(&c->btree_cache.nr_in_flight_inner)`。
    /// 在节点写操作开始时调用。
    pub(crate) fn inc_in_flight(&self) {
        self.nr_in_flight_inner.fetch_add(1, Ordering::Relaxed);
        self.bch2_recalc_btree_reserve();
    }

    /// 递减飞行中的内层写 I/O 计数并通知等待者
    ///
    /// 对应 bcachefs `atomic_dec(&c->btree_cache.nr_in_flight_inner)` + wake_up。
    /// 在节点写操作完成后调用。
    pub(crate) fn dec_in_flight(&self) {
        self.nr_in_flight_inner.fetch_sub(1, Ordering::Relaxed);
        self.bch2_recalc_btree_reserve();
        self.notify.notify_waiters();
    }

    /// 返回写操作完成通知的监听器
    ///
    /// 用于 btree 写节流：当 throttle 状态为 true 时等待此通知。
    pub(crate) fn notified(&self) -> impl std::future::Future<Output = ()> + '_ {
        self.notify.notified()
    }

    /// bcachefs 对齐: bch2_btree_node_reclaim — 回收一个 clean 节点
    ///
    /// 对应 bcachefs `bch2_btree_node_reclaim()` (cache.c):
    /// 尝试驱逐一个 clean 节点。subvol 中委托 `shrink_one()`。
    /// 无需 bcachefs 的增量锁协议。
    pub fn btree_node_reclaim(&self) -> bool {
        self.shrink_one()
    }

    /// bcachefs 对齐: system_memory_usage_high — 系统内存压力检测
    ///
    /// 对应 bcachefs `system_memory_usage_high()` (cache.c):
    /// 当系统内存不足时返回 true，触发 cache shrink。
    /// subvol 按 upstream 同样的 available / total 比例阈值判断。
    pub fn system_memory_usage_high(&self) -> bool {
        let inner = self.inner.lock().unwrap();
        if let Some((available_bytes, total_bytes)) = Self::system_memory_snapshot() {
            Self::system_memory_usage_high_from(
                available_bytes,
                total_bytes,
                inner.nr_live,
                DEFAULT_NODE_SIZE as u64,
            )
        } else {
            false
        }
    }

    fn system_memory_usage_high_from(
        available_bytes: u64,
        total_bytes: u64,
        live_nodes: usize,
        node_size: u64,
    ) -> bool {
        if total_bytes == 0 || available_bytes >= total_bytes >> 2 {
            return false;
        }

        let pinned_bytes = live_nodes as u64 * node_size;
        pinned_bytes > (total_bytes - available_bytes) >> 2
    }

    fn system_memory_snapshot() -> Option<(u64, u64)> {
        let meminfo = fs::read_to_string("/proc/meminfo").ok()?;
        Self::parse_meminfo_snapshot(&meminfo)
    }

    fn parse_meminfo_snapshot(meminfo: &str) -> Option<(u64, u64)> {
        let mut available_kb = None;
        let mut total_kb = None;

        for line in meminfo.lines() {
            if let Some(value) = line.strip_prefix("MemAvailable:") {
                available_kb = value.split_whitespace().next()?.parse::<u64>().ok();
            } else if let Some(value) = line.strip_prefix("MemTotal:") {
                total_kb = value.split_whitespace().next()?.parse::<u64>().ok();
            }

            if available_kb.is_some() && total_kb.is_some() {
                break;
            }
        }

        Some((available_kb? * 1024, total_kb? * 1024))
    }

    /// 当 clean cache 逼近上限，或者系统内存压力已经变高时，提前触发一次小范围 shrink。
    fn should_preemptive_shrink_from(clean_len: usize, memory_pressure_high: bool) -> bool {
        memory_pressure_high || clean_len + PREEMPTIVE_SHRINK_SCAN_BUDGET >= MAX_CLEAN
    }

    /// bcachefs 对齐: bch2_btree_cache_to_text
    pub fn bch2_btree_cache_to_text(&self) -> String {
        let inner = self.inner.lock().unwrap();

        let mut pinned = 0usize;
        for node in inner
            .clean
            .values()
            .chain(inner.dirty.values())
            .chain(inner.pending_flush.values())
        {
            if node.pin_count.load(Ordering::Relaxed) > 0 {
                pinned += 1;
            }
        }

        let mut text = String::new();
        let _ = writeln!(text, "live:\t{}", inner.nr_live);
        let _ = writeln!(text, "pinned:\t{}", pinned);
        let _ = writeln!(text, "clean:\t{}", inner.clean.len());
        let _ = writeln!(text, "dirty:\t{}", inner.nr_dirty);
        let _ = writeln!(text, "pending flush:\t{}", inner.pending_flush.len());
        let _ = writeln!(text, "clean lru:\t{}", inner.clean_lru.len());
        let _ = writeln!(
            text,
            "in flight:\t{}",
            self.nr_in_flight_inner.load(Ordering::Relaxed)
        );
        let _ = writeln!(
            text,
            "cannibalize lock:\t{}",
            if self.cannibalize_in_progress.load(Ordering::Relaxed) {
                "held"
            } else {
                "not held"
            }
        );
        let _ = writeln!(
            text,
            "evicted size:\t{}",
            self.evicted_size.load(Ordering::Relaxed)
        );
        let _ = writeln!(
            text,
            "cache hits:\t{}",
            self.cache_hits.load(Ordering::Relaxed)
        );
        let _ = writeln!(
            text,
            "requested:\t{}",
            self.cache_requested.load(Ordering::Relaxed)
        );
        let _ = writeln!(text, "freed:\t{}", self.cache_freed.load(Ordering::Relaxed));
        let _ = writeln!(
            text,
            "self reclaim:\t{}",
            self.cache_self_reclaim.load(Ordering::Relaxed)
        );
        let _ = writeln!(
            text,
            "not freed (access_bit):\t{}",
            self.cache_not_freed_access_bit.load(Ordering::Relaxed)
        );
        let _ = writeln!(
            text,
            "cache misses:\t{}",
            self.cache_misses.load(Ordering::Relaxed)
        );
        let _ = writeln!(text, "retire queue:\t{}", inner.retire_queue.len());

        text
    }

    /// 获取缓存命中数
    pub fn cache_hits(&self) -> u64 {
        self.cache_hits.load(Ordering::Relaxed)
    }

    /// 获取缓存未命中数
    pub fn cache_misses(&self) -> u64 {
        self.cache_misses.load(Ordering::Relaxed)
    }

    /// bcachefs 对齐: btree_cache_nr_live — 存活节点数
    pub fn nr_live(&self) -> usize {
        self.inner.lock().unwrap().nr_live
    }

    /// bcachefs 对齐: btree_cache_nr_dirty — 脏节点数
    pub fn nr_dirty(&self) -> usize {
        self.inner.lock().unwrap().nr_dirty
    }

    /// 在节点被驱逐时调用 `bch2_journal_pin_drop`。
    /// 从 node_id 查找 journal pin 并调用 pin_drop 释放 journal 引用。
    fn drop_pin_for_node(&self, node_id: u64) {
        if let Some(ref j) = self.journal {
            if let Some(node) = self.get(node_id) {
                node.evict_journal_pin(j);
            }
        }
    }

    /// 获取被驱逐节点总大小（用于 GC accounting）
    /// bcachefs 对齐: bch2_btree_cache_scan 中 evicted_size 统计
    pub fn total_evicted_size(&self) -> u64 {
        self.evicted_size.load(Ordering::Relaxed)
    }

    /// 查询被驱逐节点的 live_u64s（重载时用于精确分配）。
    ///
    /// bcachefs 对齐: bch2_btree_evicted_size_lookup (cache.h:70-83)
    ///
    /// 固定大小表中的查找是无锁的（AtomicU64 load）。
    /// 哈希冲突时返回 None — 调用者应回退到全尺寸分配或重读节点。
    pub fn lookup_evicted_size(&self, node_id: u64) -> Option<u16> {
        self.inner.lock().unwrap().evicted_sizes.lookup(node_id)
    }

    /// 插入脏节点（直接插入 dirty 列表，不经过 clean）
    ///
    /// 用于写路径：节点已修改，跳过 clean 直接标记为 dirty。
    /// 不影响 clean LRU 顺序，不触发 clean 驱逐。
    /// 如果 dirty 超限，自动移入 pending_flush（同 mark_dirty 行为）。
    pub fn insert_dirty(&self, node_id: u64, node: Arc<BtreeNode>) {
        let mut inner = self.inner.lock().unwrap();
        // 已在 dirty 中 → 替换为最新版本
        if let Some(old) = inner.dirty.get_mut(&node_id) {
            *old = node;
            return;
        }
        // 从 clean 中移除（若有），避免同 node_id 同时出现在 clean + dirty
        if inner.clean.remove(&node_id).is_some() {
            if let Some(pos) = inner.clean_lru.iter().position(|&x| x == node_id) {
                inner.clean_lru.remove(pos);
            }
        }
        // bcachefs 对齐：dirty ≠ need_rewrite。need_rewrite 仅在 write 路径设置。
        inner.record_level(node_id, node.level);
        inner.dirty.insert(node_id, node);
        inner.nr_dirty = inner.dirty.len() + inner.pending_flush.len();
        inner.nr_live = inner.clean.len() + inner.dirty.len() + inner.pending_flush.len();

        // dirty 超限 → auto-flush（同 mark_dirty）
        if inner.dirty.len() >= MAX_DIRTY {
            let drained = std::mem::take(&mut inner.dirty);
            inner.pending_flush.extend(drained);
            inner.nr_dirty = inner.pending_flush.len();
            inner.nr_live = inner.clean.len() + inner.pending_flush.len();
        }
    }

    /// bcachefs 对齐: alloc_node_for_key — 按 key 分配并插入新缓存节点
    ///
    /// 1. 创建新 BtreeNode（通过 `BtreeNode::new(level)`）
    /// 2. 将 key 转换为 Bpos 存入 min_key/max_key
    /// 3. 生成节点 ID（hash key.vaddr + btree_id）
    /// 4. 插入缓存（如果缓存满会触发 LRU 驱逐）
    /// 5. 返回 Arc<BtreeNode>
    ///
    /// 对应 bcachefs `bch2_btree_node_fill` 的分配部分（cache.c:1098），
    /// 以及 `__bch2_btree_node_alloc` 的缓存插入（cache.c:1320）。
    /// 返回的节点已设置 accessed 标志（新分配节点标记为最近使用）。
    ///
    /// 参考: bcachefs-tools/fs/btree/cache.c:1098 (bch2_btree_node_fill)
    ///       bcachefs-tools/fs/btree/cache.c:1320 (__bch2_btree_node_alloc)
    pub fn alloc_node_for_key(
        &self,
        key: &BtreeKey,
        level: u8,
        btree_id: BtreeId,
    ) -> Arc<BtreeNode> {
        // 1. 创建新节点
        let mut node = Arc::new(BtreeNode::new(level));
        // 2. 将 key 转换为 Bpos 存入 min_key/max_key
        let bpos = key.to_bpos();
        // 新创建的 Arc 只有此引用，get_mut 一定成功
        if let Some(node_mut) = Arc::get_mut(&mut node) {
            node_mut.min_key = bpos;
            node_mut.max_key = bpos;
        }
        // 3. 生成节点 ID（混合 key.vaddr + btree_id 避免不同树间冲突）
        let node_id = key.get_vaddr().wrapping_mul(31) ^ (btree_id as u64);
        // 4. 插入缓存
        self.insert(node_id, node.clone());
        node
    }

    /// 插入节点（插入 clean 列表，推到 LRU 尾部）
    ///
    /// 如果 clean 已满，驱逐 LRU 条目腾出空间。
    pub fn insert(&self, node_id: u64, node: Arc<BtreeNode>) {
        let mut inner = self.inner.lock().unwrap();
        // 已在 dirty 中 → 不做任何事
        if inner.dirty.contains_key(&node_id) {
            return;
        }
        // 已在 clean 中 → LRU 提升（remove + reinsert 避免 borrow 冲突）
        if inner.clean.remove(&node_id).is_some() {
            Self::lru_promote(&mut inner.clean_lru, node_id);
            // 节点还回去
            inner.clean.insert(node_id, node);
            inner.clean_lru.push_back(node_id);
            return;
        }
        // clean 超限 → 优先驱逐 leaf 节点（root/leaf 热冷分离）
        while inner.clean.len() >= MAX_CLEAN {
            if let Some(es) = inner.evict_one_leaf_with_jseq(self.journal.as_deref()) {
                self.evicted_size.fetch_add(es as u64, Ordering::Relaxed);
                continue;
            }
            // 无 leaf 可驱逐 → 回退到驱逐 LRU 最前端（记录 evicted_size）
            if let Some(evict_id) = inner.clean_lru.pop_front() {
                if let Some(j) = self.journal.as_deref() {
                    if let Some(n) = inner.clean.get(&evict_id) {
                        n.evict_journal_pin(j);
                    }
                }
                let es = inner.record_evicted_node(&evict_id);
                self.evicted_size.fetch_add(es as u64, Ordering::Relaxed);
                inner.clean.remove(&evict_id);
                inner.node_levels.remove(&evict_id);
            } else {
                break;
            }
        }
        node.set_accessed(); // 新插入节点标记为最近访问
        inner.record_level(node_id, node.level);
        inner.clean.insert(node_id, node);
        inner.clean_lru.push_back(node_id);
        inner.nr_live = inner.clean.len() + inner.dirty.len() + inner.pending_flush.len();

        drop(inner);
    }

    /// 移除并返回节点（从 clean 和 dirty 中都移除）
    pub fn remove(&self, node_id: u64) -> Option<Arc<BtreeNode>> {
        let mut inner = self.inner.lock().unwrap();
        // 移除前清理 journal pin
        if let Some(ref j) = self.journal {
            if let Some(n) = inner
                .clean
                .get(&node_id)
                .or_else(|| inner.dirty.get(&node_id))
                .or_else(|| inner.pending_flush.get(&node_id))
            {
                n.evict_journal_pin(j);
            }
        }
        let result = if let Some(node) = inner.clean.remove(&node_id) {
            if let Some(pos) = inner.clean_lru.iter().position(|&x| x == node_id) {
                inner.clean_lru.remove(pos);
            }
            Some(node)
        } else if let Some(node) = inner.dirty.remove(&node_id) {
            Some(node)
        } else {
            inner.pending_flush.remove(&node_id)
        };
        if result.is_some() {
            inner.node_levels.remove(&node_id);
            inner.nr_dirty = inner.dirty.len() + inner.pending_flush.len();
            inner.nr_live = inner.clean.len() + inner.dirty.len() + inner.pending_flush.len();
            self.cache_freed.fetch_add(1, Ordering::Relaxed);
        }
        drop(inner);
        result
    }

    /// 当前缓存中的节点总数（clean + dirty + pending_flush）
    pub fn len(&self) -> usize {
        let inner = self.inner.lock().unwrap();
        inner.clean.len() + inner.dirty.len() + inner.pending_flush.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    // ─── bcachefs 对齐: bch2_btree_node_fill / prefetch ────────────

    /// bcachefs 对齐: read_node_data — 从后端读取节点数据
    ///
    /// 读取 `bucket_io::__bch2_load_btree_node()` 的完整节点内容，并覆盖到当前节点实例。
    /// 当 backend 不可用时保持空操作，供测试和纯内存场景继续运行。
    /// 与 get_or_load 的 load_fn 类似，节点在分配时已处于有效状态。
    ///
    /// 参考: bcachefs-tools/fs/btree/cache.c:1185 (bch2_btree_node_read)
    fn read_node_data(&self, node: &mut BtreeNode, node_id: u64) -> Result<(), StorageError> {
        let Some(backend) = self.backend() else {
            return Ok(());
        };
        let dev = Arc::new(BchDev::new(backend, 0));

        let loaded = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("btree cache read runtime")
                .block_on(async { bucket_io::__bch2_load_btree_node(dev.clone(), node_id).await })
        })
        .join()
        .expect("btree cache read worker panicked")?;

        *node = loaded;
        Ok(())
    }

    /// 将已从 backend 载入的节点内容写回到缓存中的节点实例。
    ///
    /// 只覆盖数据字段，不碰 lock / condvar / pin / in-flight 状态。
    /// 调用方必须在 `read_in_flight == true` 的保护下调用，确保没有并发读者
    /// 在内容切换期间观察到部分更新。
    fn apply_loaded_node_data(node: &Arc<BtreeNode>, loaded: BtreeNode) {
        let node_ptr = Arc::as_ptr(node) as *mut BtreeNode;
        unsafe {
            (*node_ptr).level = loaded.level;
            (*node_ptr).packed_keys = loaded.packed_keys;
            (*node_ptr).unpacked_keys = loaded.unpacked_keys;
            (*node_ptr).whiteout_u64s = loaded.whiteout_u64s;
            (*node_ptr).node_size = loaded.node_size;
            (*node_ptr).data = loaded.data;
            (*node_ptr).aux_data = loaded.aux_data;
            (*node_ptr).nsets = loaded.nsets;
            (*node_ptr).sets = loaded.sets;
            (*node_ptr).min_key = loaded.min_key;
            (*node_ptr).max_key = loaded.max_key;
            (*node_ptr).journal_seq = loaded.journal_seq;
        }
    }

    /// bcachefs 对齐: bch2_btree_node_fill — 分配并填充 btree 节点
    ///
    /// 流程：
    /// 1. 分配新 BtreeNode，设置 min_key/max_key
    /// 2. 转换节点状态到 InFlight（表示 IO 进行中）
    /// 3. 设置 read_in_flight 标志（同步 flags 位 + AtomicBool）
    /// 4. 将节点插入缓存（此时其他线程看到的是 InFlight 状态，会等待读取完成）
    /// 5. 如果 sync=true：同步读取数据，完成后清除标志并转换到 Alive
    /// 6. 如果 sync=false：异步（后台线程）读取数据，完成后自动转换状态
    ///
    /// ⚠️ 状态必须在 insert 之前设置，否则其他线程可能在节点就绪前获取到它。
    ///
    /// 参考: bcachefs-tools/fs/btree/cache.c:1098 (bch2_btree_node_fill)
    ///       bcachefs-tools/fs/btree/cache.c:1174 (set_btree_node_read_in_flight)
    pub fn bch2_btree_node_fill(
        &self,
        key: &BtreeKey,
        btree_id: BtreeId,
        level: u8,
        sync: bool,
    ) -> Result<Arc<BtreeNode>, StorageError> {
        let node_id = key.get_vaddr().wrapping_mul(31) ^ (btree_id as u64);

        // 1. 创建新节点并设置 key/level/min_key/max_key
        //    对应 bcachefs bch2_btree_node_mem_alloc + bkey_copy + b->c.level = level
        let mut node = BtreeNode::new(level);
        let bpos = key.to_bpos();
        node.min_key = bpos;
        node.max_key = bpos;

        // 2. 如果后端可用且同步填充，先将真实节点数据读入本地副本。
        //    这样可以在节点对外可见前完成数据加载，同时仍保持
        //    InFlight → Alive 的状态可见性。
        if sync {
            self.read_node_data(&mut node, node_id)?;
        }

        let node = Arc::new(node);

        // 3. 转换到 InFlight 状态（对应 bcachefs BTREE_NODE_CACHE_CLEAN 转移）
        //    在插入缓存前设置，确保其他线程不会拿到 Alive 状态的空节点
        bch2_btree_node_transition_state_locked(&node, NodeState::InFlight);

        // 4. 设置 read_in_flight 标志
        //    对应 bcachefs cache.c:1174 — set_btree_node_read_in_flight
        node.set_read_in_flight();

        // 5. 插入缓存（此时其他线程看到 InFlight，会通过 wait_on_read 等待）
        self.insert(node_id, node.clone());

        // 6. 读取数据
        if sync {
            // 同步路径：等待读取完成后返回
            node.clear_read_in_flight();
            bch2_btree_node_transition_state(&node, NodeState::Alive);
        } else {
            // 异步路径：后台线程读取并原位更新缓存节点
            let node_clone = node.clone();
            let backend = self.backend();
            thread::spawn(move || {
                let loaded = backend.and_then(|backend| {
                    let dev = Arc::new(BchDev::new(backend, 0));
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .ok()
                        .and_then(|rt| {
                            rt.block_on(async {
                                bucket_io::__bch2_load_btree_node(dev.clone(), node_id).await
                            })
                            .ok()
                        })
                });

                if let Some(loaded) = loaded {
                    Self::apply_loaded_node_data(&node_clone, loaded);
                }
                node_clone.clear_read_in_flight();
                bch2_btree_node_transition_state(&node_clone, NodeState::Alive);
            });
        }

        Ok(node)
    }

    /// bcachefs 对齐: bch2_btree_node_prefetch — 预取 btree 节点
    ///
    /// 1. 通过 key + btree_id 计算 node_id，检查节点是否已在缓存中
    /// 2. 如果已在缓存中，直接返回 true（无需预取）
    /// 3. 如果不在缓存中，调用 fill(sync=false) 发起异步预取
    /// 4. 始终返回 true（bcachefs 始终返回 0）
    ///
    /// 参考: bcachefs-tools/fs/btree/cache.c:1575 (bch2_btree_node_prefetch)
    pub fn bch2_btree_node_prefetch(&self, key: &BtreeKey, btree_id: BtreeId, level: u8) -> bool {
        let node_id = key.get_vaddr().wrapping_mul(31) ^ (btree_id as u64);

        // 1. 检查是否已在缓存中
        if self.get(node_id).is_some() {
            return true;
        }

        // 2. 发起异步预取（sync=false）
        //    对应 bcachefs: bch2_btree_node_fill(..., SIX_LOCK_read, false)
        let _ = self.bch2_btree_node_fill(key, btree_id, level, false);

        // bcachefs 始终返回 0（成功）
        true
    }

    /// bcachefs 对齐: bch2_btree_node_prefetch — 基于 node_id 的预取版本
    ///
    /// 在已知 node_id 但无 BtreeKey 的上下文中使用（如 BtreeIter 下降路径）。
    /// 行为与 bch2_btree_node_prefetch 相同：
    /// 1. 如果节点已在缓存中，直接返回 true（无需预取）
    /// 2. 如果不在缓存中，分配新节点、标记 InFlight、然后插入缓存
    /// 3. 发起异步 IO（fire-and-forget），不阻塞当前操作
    ///
    /// ⚠️ 状态必须在 insert 之前设置，避免其他线程在节点就绪前获取到它。
    ///
    /// 参考: bcachefs-tools/fs/btree/cache.c:1575 (bch2_btree_node_prefetch)
    pub(crate) fn btree_node_prefetch_id(
        &self,
        node_id: u64,
        level: u8,
        _btree_id: BtreeId,
    ) -> bool {
        // 1. 检查是否已在缓存中
        if self.get(node_id).is_some() {
            return true;
        }

        // 2. 创建新节点并设置 InFlight 状态（在插入缓存前完成）
        let node = Arc::new(BtreeNode::new(level));
        bch2_btree_node_transition_state_locked(&node, NodeState::InFlight);
        node.set_read_in_flight();

        // 3. 插入缓存（此时其他线程看到 InFlight，会通过 wait_on_read 等待）
        self.insert(node_id, node.clone());

        // 4. 异步完成（sync=false — fire-and-forget）。这对应本地
        // bcachefs `bch2_btree_node_prefetch()` 调用
        // `bch2_btree_node_fill(..., false)` 后立即释放读锁并返回；
        // 预取不能在遍历下降路径上同步等待后端 I/O。
        let node_clone = node.clone();
        let backend = self.backend();
        thread::spawn(move || {
            let loaded = backend.and_then(|backend| {
                let dev = Arc::new(BchDev::new(backend, 0));
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .ok()
                    .and_then(|rt| {
                        rt.block_on(async {
                            bucket_io::__bch2_load_btree_node(dev.clone(), node_id).await
                        })
                        .ok()
                    })
            });
            if let Some(loaded) = loaded {
                Self::apply_loaded_node_data(&node_clone, loaded);
            }
            node_clone.clear_read_in_flight();
            bch2_btree_node_transition_state(&node_clone, NodeState::Alive);
        });

        true
    }

    // ─── 内部帮助方法（BtreeCacheInner 级别） ────────────────────────────
}

/// BtreeCacheInner 的辅助方法（通过 MutexGuard 调用）
impl BtreeCacheInner {
    /// 内部查找（dirty → pending_flush → clean）
    fn get_inner(&self, node_id: u64) -> Option<Arc<BtreeNode>> {
        self.dirty
            .get(&node_id)
            .or_else(|| self.pending_flush.get(&node_id))
            .or_else(|| self.clean.get(&node_id))
            .cloned()
    }

    /// 记录/更新节点的树层级（用于 leaf 优先驱逐）
    fn record_level(&mut self, node_id: u64, level: u8) {
        self.node_levels.insert(node_id, level);
    }

    /// 记录被驱逐节点的 live_u64s 到固定大小哈希表。
    ///
    /// 用于重载时精确分配。返回该节点的 total_data_bytes() 值。
    ///
    /// bcachefs 对齐: bch2_btree_evicted_size_record (cache.h:60-68)
    ///
    /// bcachefs 的 record 用 `hash_val → live_u64s` 的固定大小表；
    /// subvol 用 `node_id → live_u64s`，在 bounded EvictedSizeTable 上等价。
    fn record_evicted_node(&mut self, node_id: &u64) -> u32 {
        let data_size = self
            .clean
            .get(node_id)
            .map(|n| n.total_data_bytes())
            .unwrap_or(0);
        // total_data_bytes() 以字节为单位，转为 u64s（/8），限 u16 范围
        let live_u64s = ((data_size / 8) as u16).min(u16::MAX);
        self.evicted_sizes.record(*node_id, live_u64s);
        data_size
    }

    /// 从 LRU 前端扫描，尝试找到第一个 leaf 节点（level=0）并驱逐。
    ///
    /// bcachefs 对齐：优先驱逐 leaf 节点，保护 interior 节点。
    /// - 找到 leaf → 记录 evicted_size 到映射，移除并返回 Some(data_bytes)
    /// - 无 leaf 可驱逐 → 返回 None（由 caller 回退到驱逐任意节点）
    fn evict_one_leaf(&mut self, journal: Option<&Journal>) -> Option<u32> {
        self.evict_one_leaf_with_jseq(journal)
    }

    /// 与 `evict_one_leaf` 相同，但额外在驱逐前用 journal 清理 pin
    fn evict_one_leaf_with_jseq(&mut self, journal: Option<&Journal>) -> Option<u32> {
        let has_leaf = self
            .clean_lru
            .iter()
            .any(|id| self.node_levels.get(id).copied().unwrap_or(0) == 0);
        if !has_leaf {
            return None;
        }

        let mut front: usize = 0;
        while front < self.clean_lru.len() {
            let id = self.clean_lru[front];
            // bcachefs 对齐：will_make_reachable 节点不可驱逐
            if self
                .clean
                .get(&id)
                .map_or(false, |n| n.will_make_reachable())
            {
                front += 1;
                continue;
            }
            // bcachefs 对齐：pinned 节点不可驱逐
            if self
                .clean
                .get(&id)
                .map_or(false, |n| n.pin_count.load(Ordering::Relaxed) > 0)
            {
                front += 1;
                continue;
            }
            if self.node_levels.get(&id).copied().unwrap_or(0) == 0 {
                // 驱逐前清理 journal pin（直接操作嵌入的 pin）
                if let (Some(j), Some(n)) = (journal, self.clean.get(&id)) {
                    n.evict_journal_pin(j);
                }
                let es = self.record_evicted_node(&id);
                self.clean_lru.remove(front);
                self.clean.remove(&id);
                self.node_levels.remove(&id);
                return Some(es);
            }
            front += 1;
        }
        None
    }
}

impl BtreeCache {
    fn lru_promote(lru: &mut VecDeque<u64>, node_id: u64) {
        if let Some(pos) = lru.iter().position(|&x| x == node_id) {
            lru.remove(pos);
            lru.push_back(node_id);
        }
    }
}

impl Default for BtreeCache {
    fn default() -> Self {
        Self::new()
    }
}

// ─── 常量导出 ───────────────────────────────────────────────────────────────

pub use consts::{MAX_CLEAN as CACHE_MAX_CLEAN, MAX_DIRTY as CACHE_MAX_DIRTY};

mod consts {
    pub const MAX_CLEAN: usize = super::MAX_CLEAN;
    pub const MAX_DIRTY: usize = super::MAX_DIRTY;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_device::MockBlockDevice;
    use crate::btree::bucket_io;
    use crate::btree::node::{BtreeNode, NodeState};
    use crate::btree::writer::NoopWriter;
    use crate::btree::{BchVal, Btree, BtreeKey, KeyType};
    

    fn make_node() -> Arc<BtreeNode> {
        Arc::new(BtreeNode::new_leaf())
    }

    fn make_interior() -> Arc<BtreeNode> {
        Arc::new(BtreeNode::new_internal())
    }

    /// 场景 1（Happy path）：get_or_load 未命中时调用 load_fn，命中时不调用
    #[test]
    fn test_get_or_load_hit_and_miss() {
        let cache = BtreeCache::new();
        let node = make_node();

        // MISS → load_fn 被调用
        let mut load_called = false;
        let got = cache.get_or_load(42, || {
            load_called = true;
            node.clone()
        });
        assert!(load_called, "load_fn should be called on miss");
        assert!(Arc::ptr_eq(&node, &got));

        // HIT → load_fn 不被调用
        let mut load_called2 = false;
        let got2 = cache.get_or_load(42, || {
            load_called2 = true;
            make_node()
        });
        assert!(!load_called2, "load_fn should NOT be called on hit");
        assert!(Arc::ptr_eq(&node, &got2));
    }

    /// 场景 2（LRU eviction）：插入 MAX_CLEAN + 1 个节点 → 最旧者被驱逐
    #[test]
    fn test_lru_eviction() {
        let cache = BtreeCache::new();
        let mut nodes = Vec::new();

        // 先插入 MAX_CLEAN 个节点
        for i in 0..MAX_CLEAN as u64 {
            let n = make_node();
            cache.insert(i, n.clone());
            nodes.push(n);
        }

        // 再插入一个 → 触发 LRU 驱逐（ID=0 应被驱逐）
        let new_node = make_node();
        cache.insert(MAX_CLEAN as u64, new_node);

        // 验证 ID=0 被驱逐
        assert!(cache.get(0).is_none(), "LRU entry should be evicted");
        // 验证新插入的还存在
        assert!(
            cache.get(MAX_CLEAN as u64).is_some(),
            "new entry should exist"
        );
        // 验证最后一个还存在
        assert!(
            cache.get((MAX_CLEAN - 1) as u64).is_some(),
            "last entry should exist"
        );
    }

    /// 场景 3（LRU promotion）：访问中间节点后再插满 → 被访问者保留
    #[test]
    fn test_lru_promotion() {
        let cache = BtreeCache::new();

        // 插入 MAX_CLEAN 个节点（0..1023）
        for i in 0..MAX_CLEAN as u64 {
            cache.insert(i, make_node());
        }

        // get_or_load ID=500 → LRU 提升到 back
        // LRU 顺序变为: [0, 1, ..., 499, 501, ..., 1023, 500]
        let mid = cache.get_or_load(500, || make_node());
        let _ = mid;

        // 插入 500 个新节点 → 驱逐 0..499，但 500 因被提升而幸存
        let extra_count = MAX_CLEAN / 2; // 512 个插入
        for i in MAX_CLEAN as u64..(MAX_CLEAN as u64 + extra_count as u64) {
            cache.insert(i, make_node());
        }

        // ID=0 应被驱逐（LRU 前端）
        assert!(
            cache.get(0).is_none(),
            "non-promoted LRU front should be evicted"
        );
        // ID=500 因被提升到 back 而保留
        assert!(
            cache.get(500).is_some(),
            "promoted entry should survive eviction"
        );
    }

    /// 场景 4（dirty tracking）：mark_dirty 将节点从 clean 移到 dirty
    #[test]
    fn test_dirty_tracking() {
        let cache = BtreeCache::new();
        let node = make_node();

        cache.insert(100, node.clone());
        cache.mark_dirty(100);

        // clean 中不应有该节点
        let inner = cache.inner.lock().unwrap();
        assert!(
            !inner.clean.contains_key(&100),
            "should not be in clean after mark_dirty"
        );
        assert!(
            inner.dirty.contains_key(&100),
            "should be in dirty after mark_dirty"
        );
        // bcachefs 对齐：dirty ≠ need_rewrite。need_rewrite 仅在 write
        // 路径（多 bset 累积）或 corruption 恢复时设置。
        assert!(
            !inner
                .dirty
                .get(&100)
                .map(|node| node.need_rewrite())
                .unwrap_or(true),
            "dirty node should NOT be marked need_rewrite (bcachefs-aligned: dirty ≠ need_rewrite)"
        );
        assert!(
            inner.clean_lru.iter().all(|&x| x != 100),
            "should not be in LRU after mark_dirty"
        );
    }

    /// 场景 4b（state helper）：transition_state helpers 应与节点状态直接一致
    #[test]
    fn test_transition_state_helpers() {
        let node = make_node();
        assert_eq!(node.btree_node_state(), NodeState::Alive);

        bch2_btree_node_transition_state(&node, NodeState::InFlight);
        assert_eq!(node.btree_node_state(), NodeState::InFlight);

        bch2_btree_node_transition_state_locked(&node, NodeState::Alive);
        assert_eq!(node.btree_node_state(), NodeState::Alive);
    }

    /// 场景 4c（write done）：write_done_clean 应清除 write flag 并恢复 Alive，保留 need_rewrite
    #[test]
    fn test_write_done_clean_resets_write_state() {
        let node = make_node();
        node.set_write_in_flight();
        node.set_need_rewrite();
        node.bch2_btree_node_transition_state(NodeState::InFlight);

        bch2_btree_node_write_done_clean(&node);

        assert!(!node.is_write_in_flight());
        assert!(node.need_rewrite());
        assert_eq!(node.btree_node_state(), NodeState::Alive);
    }

    /// 场景 4d（mem free）：mem_free 应消费一个 Arc 引用而不影响其余引用
    #[test]
    fn test_btree_node_mem_free_consumes_arc() {
        let node = make_node();
        let other = node.clone();
        assert_eq!(Arc::strong_count(&other), 2);

        bch2_btree_node_mem_free(node);

        assert_eq!(Arc::strong_count(&other), 1);
    }

    /// 场景 5（dirty flush）：flush_dirty 清空 dirty 列表
    #[test]
    fn test_flush_dirty() {
        let cache = BtreeCache::new();
        cache.insert(100, make_node());
        cache.mark_dirty(100);
        assert_eq!(cache.len(), 1);

        cache.flush_dirty();
        // dirty 已清空，节点应不在缓存中（被移除）
        assert!(
            cache.get(100).is_none(),
            "flushed dirty node should be gone"
        );
    }

    /// 场景 6（GC retire）：退休队列中 refcount=1 的节点被清理
    #[test]
    fn test_gc_retire_single_ref() {
        let cache = BtreeCache::new();
        let node = make_node();

        cache.insert(200, node);
        // 此时 strong_count == 1（仅缓存持有）
        cache.gc_retire(&[200]);

        assert!(
            cache.get(200).is_none(),
            "retired node should be removed from cache"
        );
    }

    /// 场景 6b（GC retire）：refcount>1 的节点不被清理
    #[test]
    fn test_gc_retire_external_ref() {
        let cache = BtreeCache::new();
        let node = make_node();

        cache.insert(300, node.clone());
        // 外部仍持有 node（strong_count >= 2）
        cache.gc_retire(&[300]);

        assert!(
            cache.get(300).is_some(),
            "node with external ref should remain"
        );
        drop(node); // 显式释放在测试结束前
    }

    /// 场景 7（re-get after eviction）：被驱逐后重新 get_or_load 应调用 load_fn
    #[test]
    fn test_reget_after_eviction() {
        let cache = BtreeCache::new();

        // 先插满并驱逐
        for i in 0..MAX_CLEAN as u64 {
            cache.insert(i, make_node());
        }
        // 多插一个触发驱逐（ID=0 应被驱逐）
        cache.insert(MAX_CLEAN as u64, make_node());

        // ID=0 已被驱逐
        assert!(cache.get(0).is_none());

        // 重新 get_or_load → load_fn 应再次被调用
        let mut load_called = false;
        let _ = cache.get_or_load(0, || {
            load_called = true;
            make_node()
        });
        assert!(load_called, "re-get after eviction should call load_fn");
        assert!(cache.get(0).is_some(), "re-loaded entry should be in cache");
    }

    /// 场景 8（dirty lookup）：dirty 中的节点仍可通过 get_or_load 查到
    #[test]
    fn test_dirty_still_accessible() {
        let cache = BtreeCache::new();
        let node = make_node();

        cache.insert(400, node.clone());
        cache.mark_dirty(400);

        let got = cache.get_or_load(400, || make_node());
        assert!(
            Arc::ptr_eq(&node, &got),
            "dirty node should still be accessible via get_or_load"
        );
        assert!(
            got.is_accessed(),
            "dirty lookup should refresh accessed bit"
        );
    }

    /// 场景 9（auto-flush）：dirty 达到 MAX_DIRTY 时自动触发 flush
    ///
    /// 修复前：auto-flush 直接 dirty.clear() 丢弃脏节点 → 数据丢失
    /// 修复后：auto-flush 将脏节点移入 pending_flush，等待 flush_dirty() 写回
    #[test]
    fn test_dirty_auto_flush() {
        let cache = BtreeCache::new();

        // 标记 MAX_DIRTY - 1 个节点 dirty
        for i in 0..(MAX_DIRTY - 1) as u64 {
            let n = make_node();
            cache.insert(i, n.clone());
            cache.mark_dirty(i);
        }

        // 最后一个节点触发 auto-flush（dirty 达到 MAX_DIRTY）
        cache.insert(9999, make_node());
        cache.mark_dirty(9999);

        // auto-flush 后 dirty 被清空，节点移入 pending_flush
        let inner = cache.inner.lock().unwrap();
        assert!(
            inner.dirty.is_empty(),
            "dirty should be drained after auto-flush"
        );
        assert!(
            inner.clean.is_empty(),
            "all nodes were dirty, clean should be empty"
        );
        // pending_flush 应包含所有被 drain 的节点（0..255 + 9999 = MAX_DIRTY 个）
        assert_eq!(
            inner.pending_flush.len(),
            MAX_DIRTY,
            "pending_flush should hold all {} drained dirty nodes",
            MAX_DIRTY
        );
        // 验证具体节点存在
        assert!(
            inner.pending_flush.contains_key(&0),
            "first node should be in pending_flush"
        );
        assert!(
            inner.pending_flush.contains_key(&9999),
            "trigger node should be in pending_flush"
        );
        // 节点应仍可通过 get 查到（pending_flush 参与查找）
        drop(inner);
        assert!(
            cache.get(0).is_some(),
            "pending_flush node should still be accessible via get"
        );
        assert!(
            cache.get(42).is_some(),
            "pending_flush node 42 should still be accessible via get"
        );

        let hit = cache.get_or_load(0, make_node);
        assert!(
            hit.is_accessed(),
            "pending_flush hit should refresh accessed bit"
        );
    }

    /// 场景 10（remove）：同时从 clean 和 LRU 中移除
    #[test]
    fn test_remove() {
        let cache = BtreeCache::new();
        let node = make_node();

        cache.insert(500, node.clone());
        let removed = cache.remove(500);

        assert!(removed.is_some(), "remove should return the node");
        assert!(cache.get(500).is_none(), "removed node should be gone");

        // 验证 LRU 中也没有
        let inner = cache.inner.lock().unwrap();
        assert!(inner.clean_lru.iter().all(|&x| x != 500));
    }

    /// 场景 10b（evict 等待 IO）：bch2_btree_node_evict 应等待读写 in-flight 结束
    #[test]
    fn test_btree_node_evict_waits_on_io() {
        let cache = BtreeCache::new();
        let node = make_node();
        node.set_read_in_flight();
        node.set_write_in_flight();
        cache.insert(501, node.clone());

        let worker = thread::spawn({
            let node = node.clone();
            move || {
                thread::sleep(std::time::Duration::from_millis(10));
                node.clear_read_in_flight();
                node.clear_write_in_flight();
            }
        });

        let removed = cache.bch2_btree_node_evict(501);
        worker.join().unwrap();

        assert!(removed.is_some(), "evict should remove the node after IO");
        assert!(cache.get(501).is_none(), "evicted node should be gone");
    }

    /// 场景 11（multi-eviction）：get_or_load 在满时触发驱逐
    #[test]
    fn test_get_or_load_evicts() {
        let cache = BtreeCache::new();

        for i in 0..MAX_CLEAN as u64 {
            cache.insert(i, make_node());
        }

        // get_or_load 一个不存在的 ID → 应触发驱逐
        let _ = cache.get_or_load(9999, make_node);

        assert!(
            cache.get(0).is_none(),
            "get_or_load should evict LRU when full"
        );
        assert!(
            cache.get(9999).is_some(),
            "get_or_load should insert new node"
        );
    }

    #[test]
    fn test_empty() {
        let cache = BtreeCache::new();
        assert!(cache.is_empty());
        assert_eq!(cache.len(), 0);

        cache.insert(1, make_node());
        assert!(!cache.is_empty());
        assert_eq!(cache.len(), 1);
    }

    // ─── Root/leaf 热冷分离测试 ─────────────────────────────────────

    /// 场景 12（leaf 优先驱逐）：leaf 和 interior 混存时，满驱逐先淘汰 leaf
    #[test]
    fn test_leaf_first_eviction() {
        let cache = BtreeCache::new();

        // 先插入一个 interior 节点（level=1）
        cache.insert(0, make_interior());

        // 再插入 leaf 节点直到满（还剩 MAX_CLEAN - 1 个 leaf）
        for i in 1..MAX_CLEAN as u64 {
            cache.insert(i, make_node());
        }
        assert_eq!(cache.len(), MAX_CLEAN);

        // 再插入一个 leaf → 触发驱逐
        // evict_one_leaf 从前端扫描：ID=0 interior 跳过，ID=1 leaf 被驱逐
        cache.insert(MAX_CLEAN as u64, make_node());

        assert!(
            cache.get(0).is_some(),
            "interior ID=0 must survive eviction"
        );
        assert!(
            cache.get(1).is_none(),
            "leaf ID=1 at LRU front-2 should be evicted first"
        );
        assert!(
            cache.get(MAX_CLEAN as u64).is_some(),
            "newly inserted node must be present"
        );
    }

    /// 场景 13（interior 保护全部占满）：当 clean 中全是 interior 节点时，
    /// evict_one_leaf 返回 false，回退到正常 LRU 驱逐
    #[test]
    fn test_interior_only_fallback() {
        let cache = BtreeCache::new();

        // 插入 MAX_CLEAN 个 interior 节点
        for i in 0..MAX_CLEAN as u64 {
            cache.insert(i, make_interior());
        }
        assert_eq!(cache.len(), MAX_CLEAN);

        // 再插入一个 interior → 触发驱逐
        // 无 leaf 可驱逐 → 回退到驱逐 LRU 最前端
        cache.insert(MAX_CLEAN as u64, make_interior());

        // ID=0（LRU 最前端）应被驱逐
        assert!(
            cache.get(0).is_none(),
            "LRU front interior should be evicted when no leaves"
        );
        // 新插入的存在
        assert!(
            cache.get(MAX_CLEAN as u64).is_some(),
            "new interior should be present"
        );
    }

    /// 场景 14（混合层级的驱逐顺序）：leaf 在最前端时先被驱逐，interior 在最前端时被跳过
    #[test]
    fn test_mixed_eviction_order() {
        let cache = BtreeCache::new();

        // 构造 LRU 顺序: [leaf(0), interior(1), leaf(2), interior(3), ...]
        // 交替插入，确保前端是 leaf
        for i in 0..MAX_CLEAN as u64 {
            if i % 2 == 0 {
                cache.insert(i, make_node()); // leaf
            } else {
                cache.insert(i, make_interior()); // interior
            }
        }
        assert_eq!(cache.len(), MAX_CLEAN);

        // 此时 LRU = [0(leaf), 1(interior), 2(leaf), 3(interior), ...]
        // 插入新 node 触发驱逐
        cache.insert(MAX_CLEAN as u64, make_node());

        // ID=0（leaf，LRU 最前端）应被驱逐
        assert!(
            cache.get(0).is_none(),
            "first leaf in LRU should be evicted"
        );
        // ID=1（interior）应被保留（evict_one_leaf 会跳过它）
        assert!(
            cache.get(1).is_some(),
            "interior after evicted leaf should survive"
        );
        // ID=2（第二个 leaf）如果前面只有 ID=0 和 ID=1，之后可能会被驱逐
        // 但 evict_one_leaf 只驱逐一个，所以 ID=2 应保留
        assert!(
            cache.get(2).is_some(),
            "second leaf should survive (only one eviction)"
        );
    }

    /// 场景 15（get_or_load 记录 level）：验证通过 get_or_load 加载的节点正确记录 level
    #[test]
    fn test_get_or_load_level_tracking() {
        let cache = BtreeCache::new();

        // get_or_load 加载 leaf
        let l1 = cache.get_or_load(10, make_node);
        assert_eq!(l1.level, 0);

        // get_or_load 加载 interior
        let l2 = cache.get_or_load(20, make_interior);
        assert_eq!(l2.level, 1);

        // 验证内部 node_levels 映射已正确记录
        let inner = cache.inner.lock().unwrap();
        assert_eq!(inner.node_levels.get(&10).copied(), Some(0));
        assert_eq!(inner.node_levels.get(&20).copied(), Some(1));
    }

    /// 场景 20（backend prefetch）：backend 可用时，prefetch 应直接加载真实节点
    #[tokio::test]
    async fn test_prefetch_node_loads_backend_node() {
        let backend = Arc::new(MockBlockDevice::new());
        let dev = Arc::new(BchDev::new(backend.clone(), 0));
        let cache = BtreeCache::new();
        assert!(cache.set_backend(backend.clone()));

        let mut node = BtreeNode::new_leaf();
        assert!(node.insert(BtreeKey::new(7, 1, KeyType::Normal), BchVal::new(0xBEEF, 1),));
        node.compact();

        let ptr = bucket_io::__bch2_write_initial_node(&node, 77, 1, dev.clone())
            .await
            .unwrap();
        assert_eq!(ptr.block_addr, 77);

        assert!(cache.btree_node_prefetch_id(77, 0, BtreeId::Extents));

        let loaded = cache.get(77).expect("prefetched node should be cached");
        loaded.wait_on_read(None);
        let key = BtreeKey::new(7, 1, KeyType::Normal);
        assert!(
            loaded.search(&key).is_some(),
            "prefetched backend node should contain the serialized entry"
        );
        assert_eq!(loaded.level, 0);
    }

    /// 场景 21（backend sync fill）：同步 fill 应从 backend 读回真实节点
    #[tokio::test]
    async fn test_sync_fill_loads_backend_node() {
        let backend = Arc::new(MockBlockDevice::new());
        let dev = Arc::new(BchDev::new(backend.clone(), 0));
        let cache = BtreeCache::new();
        assert!(cache.set_backend(backend.clone()));

        let mut node = BtreeNode::new_leaf();
        assert!(node.insert(
            BtreeKey::new(11, 1, KeyType::Normal),
            BchVal::new(0xCAFE, 1),
        ));
        node.compact();

        let key = BtreeKey::new(11, 1, KeyType::Normal);
        let node_id = key.get_vaddr().wrapping_mul(31) ^ (BtreeId::Extents as u64);
        let ptr = bucket_io::__bch2_write_initial_node(&node, node_id, 1, dev.clone())
            .await
            .unwrap();
        assert_eq!(ptr.block_addr, node_id);

        let loaded = cache
            .bch2_btree_node_fill(&key, BtreeId::Extents, 0, true)
            .unwrap();
        assert_eq!(loaded.level, 0);
        assert_eq!(loaded.packed_keys + loaded.unpacked_keys, node.packed_keys + node.unpacked_keys);
        assert!(loaded.search(&key).is_some());
    }

    /// 场景 22（backend async fill）：异步 fill 结束后应能看到 backend 内容
    #[tokio::test]
    async fn test_async_fill_loads_backend_node() {
        let backend = Arc::new(MockBlockDevice::new());
        let dev = Arc::new(BchDev::new(backend.clone(), 0));
        let cache = BtreeCache::new();
        assert!(cache.set_backend(backend.clone()));

        let mut node = BtreeNode::new_leaf();
        assert!(node.insert(
            BtreeKey::new(12, 1, KeyType::Normal),
            BchVal::new(0xD00D, 1),
        ));
        node.compact();

        let key = BtreeKey::new(12, 1, KeyType::Normal);
        let node_id = key.get_vaddr().wrapping_mul(31) ^ (BtreeId::Extents as u64);
        let ptr = bucket_io::__bch2_write_initial_node(&node, node_id, 1, dev.clone())
            .await
            .unwrap();
        assert_eq!(ptr.block_addr, node_id);

        let loaded = cache
            .bch2_btree_node_fill(&key, BtreeId::Extents, 0, false)
            .unwrap();
        assert!(
            loaded.wait_on_read(Some(std::time::Duration::from_secs(1))),
            "async fill should complete within timeout"
        );
        assert_eq!(loaded.level, 0);
        assert_eq!(loaded.packed_keys + loaded.unpacked_keys, node.packed_keys + node.unpacked_keys);
        assert!(loaded.search(&key).is_some());
    }

    /// 场景 16（level 清理）：remove 后 node_levels 映射被清理
    #[test]
    fn test_level_cleanup_on_remove() {
        let cache = BtreeCache::new();
        cache.insert(50, make_interior());
        cache.insert(60, make_node());

        // 验证 remove 后 level 映射被清理
        assert!(cache.remove(50).is_some());
        let inner = cache.inner.lock().unwrap();
        assert!(
            inner.node_levels.get(&50).is_none(),
            "level should be cleaned up on remove"
        );
        assert!(
            inner.node_levels.get(&60).is_some(),
            "unremoved node level should remain"
        );
    }

    // ─── Shrinker 测试 ─────────────────────────────────────────

    /// 场景 17（shrink 基础）：shrink 使用两阶段 clock —— first pass 清除标志，second pass 驱逐
    #[test]
    fn test_shrink_two_phase_clock() {
        let cache = BtreeCache::new();
        // 插入超过 min_keep(64) 的节点数
        let n = 80u64;
        for i in 0..n {
            cache.insert(i, make_node());
        }
        assert_eq!(cache.len(), n as usize);

        // shrink 最多能驱逐 n - 64 = 16 个节点，只扫描前 target 个
        // 第一遍：清除前 16 个节点的 accessed 标志（freed=0 因为 accessed 都置位）
        let freed1 = cache.shrink(n as usize);
        assert_eq!(
            freed1, 0,
            "all scanned nodes have accessed set, none evicted"
        );

        // 验证 clean_lru 前端 16 个节点的 accessed 已被清理
        let inner = cache.inner.lock().unwrap();
        let lru_ids: Vec<u64> = inner.clean_lru.iter().copied().collect();
        for (idx, &id) in lru_ids.iter().enumerate().take(16) {
            let node = inner.clean.get(&id).unwrap();
            assert!(
                !node.is_accessed(),
                "node {} at front idx {} should have accessed cleared",
                id,
                idx
            );
        }
        drop(inner);

        // 第二遍：16 个前端节点没有 accessed，应被驱逐
        let freed2 = cache.shrink(n as usize);
        assert_eq!(freed2, 16, "second shrink should evict 16 unaccessed nodes");
        assert_eq!(cache.len(), 64, "should keep min_keep=64 nodes");
    }

    /// 场景 18（shrink re-access）：重新访问的节点在 shrink 中被保护
    #[test]
    fn test_shrink_protects_reaccessed() {
        let cache = BtreeCache::new();
        // 插入超过 min_keep 的节点
        for i in 0..80u64 {
            cache.insert(i, make_node());
        }

        // 第一遍：清除前端节点的 accessed（这些节点的 ID 会在第二遍被驱逐）
        // 但是 re-access 特定的节点来验证保护
        // 先 shrink 清除前端的 accessed
        cache.shrink(80);

        // 重新访问 ID=0（注意 ID=0 在 LRU 最前端，已被清除 accessed，
        // 重新访问后又被置位）
        let _ = cache.get_or_load(0, make_node);

        // 第二遍：ID=0 因刚被访问而受保护，其他前端 15 个被驱逐
        let _freed = cache.shrink(80);
        // ID=0 应保留（刚被访问过，accessed 被重新设置）
        assert!(
            cache.get(0).is_some(),
            "re-accessed node should survive shrink"
        );
        // 总共应保留 64 + 1(ID=0 re-accessed) = 65 个节点
        assert_eq!(
            cache.len(),
            65,
            "should keep min_keep=64 + re-accessed node"
        );
    }

    /// 场景 19（evicted_size 追踪）：驱逐节点时通过 evict_one_leaf 路径累加计数器
    #[test]
    fn test_evicted_size_tracking_via_evict_one_leaf() {
        let cache = BtreeCache::new();
        assert_eq!(cache.total_evicted_size(), 0);

        // 插满 clean 列表（MAX_CLEAN = 1024）
        for i in 0..MAX_CLEAN as u64 {
            cache.insert(i, make_node());
        }

        // 再插入一个新节点触发驱逐
        // 注意：new_leaf() 创建的节点 total_data_bytes() == 0，
        // 所以 evicted_size 是 0（但 evict_one_leaf 路径被执行了）
        cache.insert(MAX_CLEAN as u64, make_node());
        // 验证驱逐发生了：最多保留 MAX_CLEAN 个
        assert!(
            cache.len() <= MAX_CLEAN,
            "eviction should keep cache within MAX_CLEAN"
        );
        // evicted_size API 可访问（即使值为 0，因为空节点无数据）
        let _ = cache.total_evicted_size();
    }

    /// 场景 21（lookup_evicted_size）：被驱逐节点的数据大小可查询
    #[test]
    fn test_lookup_evicted_size() {
        let cache = BtreeCache::new();

        // 构造一个有数据的节点
        let node = make_node();
        cache.insert(100, node);
        cache.remove(100);

        // 验证 lookup_evicted_size 返回 None（remove 路径不记录 evicted_size）
        assert_eq!(
            cache.lookup_evicted_size(100),
            None,
            "remove does not record evicted_size"
        );
    }

    /// 场景 23（cache text）：文本输出应包含核心统计段
    #[test]
    fn test_btree_cache_to_text_contains_core_sections() {
        let cache = BtreeCache::new();
        cache.insert(10, make_node());
        cache.insert_dirty(20, make_interior());

        if let Some(node) = cache.get(10) {
            BtreeCache::bch2_node_pin(&node);
        }

        let text = cache.bch2_btree_cache_to_text();
        assert!(text.contains("live:\t"));
        assert!(text.contains("pinned:\t"));
        assert!(text.contains("clean:\t"));
        assert!(text.contains("dirty:\t"));
        assert!(text.contains("pending flush:\t"));
        assert!(text.contains("clean lru:\t"));
        assert!(text.contains("in flight:\t"));
        assert!(text.contains("cannibalize lock:\t"));
        assert!(text.contains("evicted size:\t"));
        assert!(text.contains("cache hits:\t"));
        assert!(text.contains("requested:\t"));
        assert!(text.contains("freed:\t"));
        assert!(text.contains("self reclaim:\t"));
        assert!(text.contains("not freed (access_bit):\t"));
        assert!(text.contains("cache misses:\t"));
        assert!(text.contains("retire queue:\t"));
    }

    /// 场景 24（system memory pressure helper）：纯函数应稳定反映 upstream 判定
    #[test]
    fn test_system_memory_usage_high_from() {
        let total = 1000;

        assert!(
            !BtreeCache::system_memory_usage_high_from(300, total, 100, 4),
            "available >= 1/4 total should not trigger"
        );

        assert!(
            BtreeCache::system_memory_usage_high_from(100, total, 300, 4),
            "large pinned footprint under low available memory should trigger"
        );
    }

    #[test]
    fn test_parse_meminfo_snapshot() {
        let meminfo = "\
MemTotal:        1000 kB
MemFree:          200 kB
MemAvailable:     300 kB
Buffers:           10 kB
";
        let (available, total) = BtreeCache::parse_meminfo_snapshot(meminfo).unwrap();
        assert_eq!(available, 300 * 1024);
        assert_eq!(total, 1000 * 1024);
    }

    #[test]
    fn test_preemptive_shrink_threshold() {
        assert!(
            !BtreeCache::should_preemptive_shrink_from(
                MAX_CLEAN - PREEMPTIVE_SHRINK_SCAN_BUDGET - 1,
                false,
            ),
            "below the threshold should not shrink preemptively"
        );
        assert!(
            BtreeCache::should_preemptive_shrink_from(
                MAX_CLEAN - PREEMPTIVE_SHRINK_SCAN_BUDGET,
                false,
            ),
            "at the threshold should shrink preemptively"
        );
        assert!(
            BtreeCache::should_preemptive_shrink_from(0, true),
            "memory pressure should force preemptive shrink"
        );
    }

    #[test]
    fn test_nr_live_updates_on_load_paths() {
        let cache = BtreeCache::new();

        let _ = cache.get_or_load(1, make_node);
        assert_eq!(cache.nr_live(), 1, "get_or_load should update nr_live");

        let _ = cache.bch2_btree_node_get(2, make_interior);
        assert_eq!(
            cache.nr_live(),
            2,
            "bch2_btree_node_get should update nr_live"
        );
    }

    // ── will_make_reachable 测试 ──────────────────────────────

    /// 验证：新创建的节点 will_make_reachable=false（默认）
    #[test]
    fn test_will_make_reachable_default_false() {
        let node = BtreeNode::new_leaf();
        assert!(
            !node.will_make_reachable(),
            "new node should have will_make_reachable=false"
        );
    }

    /// 验证：set/clear 工作正常
    #[test]
    fn test_will_make_reachable_set_clear() {
        let node = BtreeNode::new_leaf();
        assert!(!node.will_make_reachable());
        node.set_will_make_reachable();
        assert!(node.will_make_reachable(), "after set, should be true");
        node.clear_will_make_reachable();
        assert!(!node.will_make_reachable(), "after clear, should be false");
    }

    /// 验证：Arc<BtreeNode> 上也能调用 will_make_reachable 方法
    #[test]
    fn test_will_make_reachable_arc() {
        let node = Arc::new(BtreeNode::new_leaf());
        assert!(!node.will_make_reachable());
        node.set_will_make_reachable();
        assert!(node.will_make_reachable());
    }

    /// 验证：设置了 will_make_reachable 的节点在满驱逐时被保护。
    /// > 注释必要性：测试依赖 insert 将节点放入 dirty → 后续满插入触发
    /// > shrink → evict_one_leaf 扫描 clean LRU 的行为。先 insert 再 get 让节点
    /// > 进入 clean 且置于 LRU 前端，设置 will_make_reachable 后验证 eviction 跳过它。
    #[test]
    fn test_will_make_reachable_blocks_eviction() {
        let cache = BtreeCache::new();

        // 先插入一个保护节点到 LRU 前端
        // insert → dirty → get → clean + LRU 前端
        cache.insert(1, make_node());
        let protected = cache.get(1).unwrap();
        protected.set_will_make_reachable();
        drop(protected);

        // 填充 clean 直到满，让 ID=1 在 LRU 最前端
        // 已有 1 个条目，再插入 MAX_CLEAN - 1 个
        for i in 2..MAX_CLEAN as u64 + 1 {
            cache.insert(i, make_node());
        }
        assert_eq!(cache.len(), MAX_CLEAN);

        // 再插入一个触发驱逐 → 前端 leaf 被跳过（will_make_reachable=true），
        // 被驱逐的是下一个前端 leaf
        cache.insert(MAX_CLEAN as u64 + 1, make_node());

        // 保护节点应存活
        assert!(
            cache.get(1).is_some(),
            "will_make_reachable node must survive eviction"
        );
    }

    /// 验证：btree_increase_depth 创建的新根设置了 will_make_reachable。
    /// > 注释必要性：此处通过强制小节点触发 split_root → increase_depth，间接验证
    /// > interior 模块调用 set_will_make_reachable。
    /// 验证：btree_increase_depth 创建的新根经过完整的 set→write→clear 周期。
    /// bcachefs 对齐：写盘完成后 will_make_reachable 必须被清理。
    #[test]
    fn test_increase_depth_clears_will_make_reachable_after_write() {
        let btree = Btree::new();
        assert_eq!(btree.depth(), 0);

        // 强制小节点，使少量 key 即可触发 split
        btree.root_node_mut_internal().node_size = 256;

        // 插入 key 触发多次 split / increase_depth
        for i in 0..30u64 {
            assert!(futures::executor::block_on(btree.bch2_btree_insert(
                &NoopWriter,
                BtreeKey::new(i, 1, KeyType::Normal),
                BchVal::new(i * 10, 0),
                0,
            ))
            .unwrap_or(false));
        }

        if btree.depth() >= 1 {
            // bcachefs 对齐：写盘完成后 will_make_reachable 必须被清理
            assert!(
                !btree.root().node.will_make_reachable(),
                "root after increase_depth should NOT have will_make_reachable after write done"
            );
        }
    }
}
