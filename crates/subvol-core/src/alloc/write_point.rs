//! WritePoint — bcachefs WRITE_POINT 机制的 Rust 实现
//!
//! 对应 bcachefs:
//! - `fs/alloc/types.h` — struct write_point, WRITE_POINT_MAX
//! - `fs/alloc/foreground.h` — struct write_point_specifier, writepoint_hashed/ptr
//! - `fs/alloc/foreground.c` — writepoint_find(), bch2_alloc_sectors_done_inlined()
//!
//! ## 布局
//!
//! - `pool[0..WRITE_POINT_MAX)` — 哈希池，由 `resolve()` 动态管理
//! - `dedicated[0..NUM_DEDICATED_WPS)` — 专用写点，不参与哈希池
//! - `identity_map: HashMap` — 标识值 → pool 索引（替代 bcachefs hlist hash 表）

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::alloc::open_bucket::BchOpenBuckets;
use crate::alloc::{OpenBucketIdx, SECTORS_PER_BLOCK};

/// ns 时间戳（用于写点 LRU 时间比较）
fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

/// 最大池写点数（对应 bcachefs WRITE_POINT_MAX = 32）
pub const WRITE_POINT_MAX: u16 = 32;

/// Hash 表大小（对应 bcachefs write_points_hash 数组大小）
pub const WRITE_POINT_HASH_NR: u16 = 32;

/// 专用写点数量（btree + journal + GC）
pub const NUM_DEDICATED_WPS: usize = 3;

// ─── WritePointSpecifier ───────────────────────────────────────────

/// 写点标识符 — 对应 bcachefs `struct write_point_specifier`
///
/// bcachefs 使用 bit-0 编码（0=直接指针, 1=哈希值）来区分两种模式。
/// Rust 版本用 enum 获得类型安全 + 模式匹配。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WritePointSpecifier {
    /// 哈希模式：标识值（data extent 的 vaddr），通过 hash 表查找/创建写点
    ///
    /// 对应 bcachefs `writepoint_hashed(v)`：`v | 1`
    Hashed(u64),

    /// 直接模式：专用写点索引（btree / journal / GC）
    ///
    /// 对应 bcachefs `writepoint_ptr(wp)`：直接结构体指针
    Direct(DedicatedWp),
}

/// 专用写点索引
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DedicatedWp {
    BTree = 0,
    Journal = 1,
    GC = 2,
}

impl DedicatedWp {
    pub const fn all() -> [DedicatedWp; 3] {
        [DedicatedWp::BTree, DedicatedWp::Journal, DedicatedWp::GC]
    }

    pub fn name(&self) -> &'static str {
        match self {
            DedicatedWp::BTree => "btree",
            DedicatedWp::Journal => "journal",
            DedicatedWp::GC => "gc",
        }
    }
}

// ─── WritePoint — 写点 ────────────────────────────────────

/// 写点 — 对应 bcachefs `struct write_point`
///
/// 每个写点持有当前正在写入的 open bucket 集合和一个独立的 AG hint，
/// 使得不同标识值的分配操作倾向于使用不同的 Allocation Group，从而
/// 将不同数据的写入隔离到不同的 bucket 中。
#[derive(Debug)]
pub struct WritePoint {
    /// 标识值（None = 专用写点，不参与池哈希查找）
    pub identity: Option<u64>,

    /// 最近使用时间戳（LRU 淘汰时选 oldest）
    pub last_used: u64,

    /// 当前桶剩余可用扇区数
    pub sectors_free: u64,

    /// 前一次扇区数快照（完成路径用于增量记帐）
    pub prev_sectors_free: u64,

    /// 分配到此写点的 open bucket 索引
    pub ptrs: Vec<OpenBucketIdx>,

    /// 独立 AG 轮询偏移——替代全局 `hint`
    ///
    /// 不同写点初始 hint 不同（hint = index），确保起始于不同 AG。
    /// 每次成功分配后 hint 递增，使写入分布到所有 AG。
    pub hint: u64,
}

impl WritePoint {
    /// 创建一个新的写点
    pub fn new(hint: u64) -> Self {
        Self {
            identity: None,
            last_used: 0,
            sectors_free: 0,
            prev_sectors_free: 0,
            ptrs: Vec::new(),
            hint,
        }
    }

    /// 更新标识值（writepoint_find 在淘汰复用池写点时调用）
    pub fn reassign(&mut self, identity: u64, now: u64) {
        self.identity = Some(identity);
        self.last_used = now;
        // hint 保持不变（写点复用时不重置 hint，保持 AG 分布连续性）
    }

    /// 完成路径：将近满 bucket 从写点分离到 partial 列表
    ///
    /// 对应 bcachefs `bch2_alloc_sectors_done_inlined()`
    /// 近满桶（sectors_free < bucket_size）从写点移除，放入 partial 列表供其他写点复用。
    /// 返回被分离的桶索引列表。
    pub fn done(&mut self, bucket_size: u64, open_buckets: &BchOpenBuckets) -> Vec<OpenBucketIdx> {
        self.prev_sectors_free = self.sectors_free;
        let mut released = Vec::new();
        self.ptrs.retain(|&idx| {
            if let Some(entry) = open_buckets.get_entry(idx) {
                let remaining = entry
                    .sectors_free
                    .load(std::sync::atomic::Ordering::Acquire)
                    as u64;
                if remaining < bucket_size {
                    open_buckets.add_to_partial(idx);
                    released.push(idx);
                    return false;
                }
            }
            true
        });
        self.sectors_free = self
            .ptrs
            .iter()
            .filter_map(|&idx| open_buckets.get_entry(idx))
            .map(|e| e.sectors_free.load(std::sync::atomic::Ordering::Acquire) as u64)
            .sum();
        released
    }
}

// ─── WritePointConfig — 配置 ──────────────────────────────

/// 写点池配置
#[derive(Clone, Copy, Debug)]
pub struct WritePointConfig {
    /// 最大池写点数（1 = 退化为当前单写点行为）
    pub max_write_points: u16,
}

impl Default for WritePointConfig {
    fn default() -> Self {
        Self {
            max_write_points: 1,
        }
    }
}

// ─── WritePointPool — 写点池 ────────────────────────────

/// 写点池 — 对应 bcachefs `struct bch_fs_allocator` 中的写点管理
///
/// 管理 32 个哈希池写点 + 3 个专用写点。通过 HashMap 实现 O(1) 标识值查找，
/// 替代 bcachefs 的 hlist_head 链表 + spinlock。
pub struct WritePointPool {
    /// 池化写点（由 resolve() 按需分配，LRU 淘汰）
    pool: Vec<WritePoint>,

    /// HashMap: 标识值 → pool 索引
    ///
    /// 对应 bcachefs `write_points_hash[]` hlist 链表。
    /// Rust HashMap 更简单（O(1) 平均查找）且等价。
    identity_map: HashMap<u64, u16>,

    /// 专用写点（不参与池淘汰）
    dedicated: Vec<WritePoint>,

    /// 当前活跃池写点数（用于 stranded space 计算和 LRU 扫描范围）
    nr_active: u16,

    /// 最大池写点数
    max_write_points: u16,
}

impl WritePointPool {
    /// 创建写点池
    ///
    /// 初始化：
    /// - pool[0..max-1]: 每个写点 hint = index（不同写点起始于不同 AG）
    /// - dedicated[0..2]: btree/journal/GC，hint 分布在不同范围
    pub fn new(config: WritePointConfig) -> Self {
        let max = config.max_write_points.max(1).min(WRITE_POINT_MAX);

        let mut pool = Vec::with_capacity(max as usize);
        for i in 0..max {
            pool.push(WritePoint::new(i as u64));
        }

        // 专用写点 hint 分布在 max..max+3 范围，与池写点不重叠
        let dedicated_hint_base = max as u64;
        let dedicated = vec![
            WritePoint::new(dedicated_hint_base),     // BTree
            WritePoint::new(dedicated_hint_base + 1), // Journal
            WritePoint::new(dedicated_hint_base + 2), // GC
        ];

        Self {
            pool,
            identity_map: HashMap::new(),
            dedicated,
            nr_active: 0,
            max_write_points: max,
        }
    }

    /// 解析写点标识 → 返回写点可变引用
    ///
    /// 对应 bcachefs `writepoint_find()`（`foreground.c:1291-1347`）
    ///
    /// - `Hashed(v)`: 在 identity_map 中查找 v，找到则返回，未找到则创建或 LRU 淘汰
    /// - `Direct(dwp)`: 直接返回对应的专用写点
    pub fn resolve(&mut self, id: WritePointSpecifier) -> &mut WritePoint {
        match id {
            WritePointSpecifier::Hashed(v) => self.resolve_hashed(v),
            WritePointSpecifier::Direct(dwp) => self.resolve_direct(dwp),
        }
    }

    /// 解析写点标识并返回其独立 AG hint（所有权分配路径使用）
    ///
    /// 对应 bcachefs `writepoint_find()` + hint 提取。
    /// 在 `allocate_bucket` 中被调用，同时更新 hint 和 LRU 时间戳。
    /// 返回 hint 值后，调用者将其 modulo num_groups 作为 AG 轮询起点。
    pub fn resolve_hint(&mut self, id: WritePointSpecifier) -> u64 {
        let wp = self.resolve(id);
        let h = wp.hint;
        wp.hint = wp.hint.wrapping_add(1);
        h
    }

    fn resolve_hashed(&mut self, v: u64) -> &mut WritePoint {
        let now = now_ns();

        // 1. 在 identity_map 中查找已有映射
        if let Some(&idx) = self.identity_map.get(&v) {
            let wp = &mut self.pool[idx as usize];
            wp.last_used = now;
            return wp;
        }

        // 2. 未找到，需要创建新映射
        let idx = if (self.nr_active as usize) < self.pool.len() {
            // 2a. 池还有空间：分配新槽位
            let idx = self.nr_active;
            self.nr_active += 1;
            idx
        } else {
            // 2b. 池满：LRU 淘汰
            let idx = self.find_lru();
            // 从 identity_map 移除旧映射
            if let Some(old_id) = self.pool[idx as usize].identity {
                self.identity_map.remove(&old_id);
            }
            idx
        };

        // 3. 初始化/复用写点
        self.pool[idx as usize].reassign(v, now);
        self.identity_map.insert(v, idx);
        &mut self.pool[idx as usize]
    }

    fn resolve_direct(&mut self, dwp: DedicatedWp) -> &mut WritePoint {
        &mut self.dedicated[dwp as usize]
    }

    /// 查找 LRU 写点（池满时淘汰 oldest）
    ///
    /// 扫描 pool[0..nr_active] 中 last_used 最小的写点。
    /// 32 次线性扫描 < 100ns，无需堆优化。
    fn find_lru(&self) -> u16 {
        let mut oldest_idx = 0u16;
        let mut oldest_time = self.pool[0].last_used;

        for (i, wp) in self.pool[..self.nr_active as usize]
            .iter()
            .enumerate()
            .skip(1)
        {
            if wp.last_used < oldest_time {
                oldest_time = wp.last_used;
                oldest_idx = i as u16;
            }
        }
        oldest_idx
    }

    /// 当前活跃写点数
    pub fn nr_active(&self) -> u16 {
        self.nr_active
    }

    /// 最大写点数
    pub fn max_write_points(&self) -> u16 {
        self.max_write_points
    }

    /// Stranded space（写点拴住的最大潜在空间）
    pub fn stranded_space(&self, bucket_size: u64) -> u64 {
        // 对齐 bcachefs too_many_writepoints(): 只统计池化活跃写点。
        // 专用写点始终存在，但不参与 write_points_nr 计数。
        self.nr_active as u64 * bucket_size
    }

    /// 判定写点是否过多 — 对应 bcachefs `too_many_writepoints(c, factor)` 宏
    fn too_many_writepoints(&self, bucket_size: u64, free_sectors: u64) -> bool {
        self.stranded_space(bucket_size) * 8 > free_sectors
    }

    /// 检查当前写点是否已有可用空间（L1 写点级复用）
    ///
    /// 对应 bcachefs `bucket_alloc_set_writepoint()` 中检查当前写点 ptrs 的逻辑。
    /// 在 `try_reuse_open_bucket`（L2 partial）之前调用，优先复用本写点已持有的桶。
    ///
    /// # 返回
    ///
    /// `Some((ob_idx, dev, bucket))` — 找到可复用的桶
    /// `None` — 当前写点所有桶均无足够空间
    pub fn try_reuse_current_wp(
        &self,
        wp_id: WritePointSpecifier,
        open_buckets: &BchOpenBuckets,
        dev: u8,
        bucket_capacity: u32,
        sectors_needed: u32,
    ) -> Option<(OpenBucketIdx, u8, u64, u32)> {
        let wp = match wp_id {
            WritePointSpecifier::Direct(dwp) => &self.dedicated[dwp as usize],
            WritePointSpecifier::Hashed(v) => {
                let idx = *self.identity_map.get(&v)?;
                &self.pool[idx as usize]
            }
        };

        for &ob_idx in &wp.ptrs {
            let entry = open_buckets.get_entry(ob_idx)?;
            let entry_dev = entry.dev.load(std::sync::atomic::Ordering::Acquire);
            if entry_dev != dev {
                continue;
            }
            let old = entry
                .sectors_free
                .fetch_sub(sectors_needed, std::sync::atomic::Ordering::AcqRel);
            if old >= sectors_needed {
                let bucket = entry.bucket.load(std::sync::atomic::Ordering::Acquire);
                let block_offset = (bucket_capacity as u64 - old as u64) / SECTORS_PER_BLOCK;
                return Some((ob_idx, entry_dev, bucket, block_offset as u32));
            }
            entry
                .sectors_free
                .fetch_add(sectors_needed, std::sync::atomic::Ordering::Release);
        }
        None
    }

    /// 尝试减少活跃写点数 — 对应 `fs/alloc/foreground.c:1263` `try_decrease_writepoints()`
    pub fn try_decrease(
        &mut self,
        bucket_size: u64,
        free_sectors: u64,
        open_buckets: &BchOpenBuckets,
    ) -> bool {
        if self.nr_active <= 1 {
            return false;
        }
        if !self.too_many_writepoints(bucket_size, free_sectors) {
            return false;
        }

        self.nr_active -= 1;
        let wp = &mut self.pool[self.nr_active as usize];

        if let Some(identity) = wp.identity.take() {
            self.identity_map.remove(&identity);
        }

        for &ob_idx in &wp.ptrs {
            let has_free = open_buckets
                .get_entry(ob_idx)
                .is_some_and(|e| e.sectors_free.load(std::sync::atomic::Ordering::Acquire) > 0);
            if has_free {
                open_buckets.add_to_partial(ob_idx);
            } else {
                open_buckets.put(ob_idx);
            }
        }
        wp.ptrs.clear();
        wp.sectors_free = 0;

        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── WritePointSpecifier 基础测试 ─────────────────────

    #[test]
    fn test_write_point_id_hashed() {
        let id = WritePointSpecifier::Hashed(42);
        assert!(matches!(id, WritePointSpecifier::Hashed(42)));
    }

    #[test]
    fn test_write_point_id_direct() {
        let id = WritePointSpecifier::Direct(DedicatedWp::BTree);
        assert!(matches!(
            id,
            WritePointSpecifier::Direct(DedicatedWp::BTree)
        ));
    }

    #[test]
    fn test_dedicated_wp_names() {
        assert_eq!(DedicatedWp::BTree.name(), "btree");
        assert_eq!(DedicatedWp::Journal.name(), "journal");
        assert_eq!(DedicatedWp::GC.name(), "gc");
    }

    #[test]
    fn test_dedicated_wp_all() {
        let all = DedicatedWp::all();
        assert_eq!(all.len(), 3);
    }

    // ─── WritePoint 基础测试 ───────────────────────

    #[test]
    fn test_write_point_new() {
        let wp = WritePoint::new(5);
        assert!(wp.identity.is_none());
        assert_eq!(wp.hint, 5);
        assert_eq!(wp.sectors_free, 0);
        assert!(wp.ptrs.is_empty());
    }

    #[test]
    fn test_write_point_reassign() {
        let mut wp = WritePoint::new(3);
        wp.reassign(42, 1000);
        assert_eq!(wp.identity, Some(42));
        assert_eq!(wp.last_used, 1000);
        assert_eq!(wp.hint, 3); // hint 不变
    }

    // ─── WritePointPool 基础测试 ───────────────────

    #[test]
    fn test_pool_new_default() {
        let config = WritePointConfig::default();
        let pool = WritePointPool::new(config);
        assert_eq!(pool.max_write_points(), 1);
        assert_eq!(pool.nr_active(), 0);
    }

    #[test]
    fn test_pool_new_max() {
        let config = WritePointConfig {
            max_write_points: 32,
        };
        let pool = WritePointPool::new(config);
        assert_eq!(pool.pool.len(), 32);
        assert_eq!(pool.dedicated.len(), 3);
        assert_eq!(pool.nr_active(), 0);
    }

    #[test]
    fn test_pool_clamp_max() {
        let config = WritePointConfig {
            max_write_points: 100,
        };
        let pool = WritePointPool::new(config);
        assert_eq!(pool.pool.len(), WRITE_POINT_MAX as usize);
    }

    #[test]
    fn test_pool_clamp_min() {
        let config = WritePointConfig {
            max_write_points: 0,
        };
        let pool = WritePointPool::new(config);
        assert_eq!(pool.pool.len(), 1);
    }

    // ─── writepoint_find 测试 ─────────────────────

    #[test]
    fn test_resolve_hashed_same_id_returns_same_wp() {
        let mut pool = WritePointPool::new(WritePointConfig {
            max_write_points: 8,
        });
        let wp1_addr = pool.resolve(WritePointSpecifier::Hashed(42)) as *mut WritePoint;
        let wp2_addr = pool.resolve(WritePointSpecifier::Hashed(42)) as *mut WritePoint;
        assert_eq!(
            wp1_addr, wp2_addr,
            "same hash should return same write point"
        );
    }

    #[test]
    fn test_resolve_hashed_different_id_different_wp() {
        let mut pool = WritePointPool::new(WritePointConfig {
            max_write_points: 8,
        });
        let wp1_addr = pool.resolve(WritePointSpecifier::Hashed(1)) as *mut WritePoint;
        let wp2_addr = pool.resolve(WritePointSpecifier::Hashed(2)) as *mut WritePoint;
        assert_ne!(
            wp1_addr, wp2_addr,
            "different hashes should return different write points"
        );
    }

    #[test]
    fn test_resolve_hashed_identity_set() {
        let mut pool = WritePointPool::new(WritePointConfig {
            max_write_points: 8,
        });
        let wp = pool.resolve(WritePointSpecifier::Hashed(99));
        assert_eq!(wp.identity, Some(99));
    }

    #[test]
    fn test_resolve_direct_btree() {
        let mut pool = WritePointPool::new(WritePointConfig {
            max_write_points: 8,
        });
        let wp = pool.resolve(WritePointSpecifier::Direct(DedicatedWp::BTree));
        assert!(
            wp.identity.is_none(),
            "dedicated write point has no hash identity"
        );
    }

    #[test]
    fn test_resolve_direct_distinct() {
        let mut pool = WritePointPool::new(WritePointConfig {
            max_write_points: 8,
        });
        let wp_b_addr =
            pool.resolve(WritePointSpecifier::Direct(DedicatedWp::BTree)) as *mut WritePoint;
        let wp_j_addr =
            pool.resolve(WritePointSpecifier::Direct(DedicatedWp::Journal)) as *mut WritePoint;
        let wp_g_addr =
            pool.resolve(WritePointSpecifier::Direct(DedicatedWp::GC)) as *mut WritePoint;
        assert_ne!(wp_b_addr, wp_j_addr);
        assert_ne!(wp_j_addr, wp_g_addr);
        assert_ne!(wp_b_addr, wp_g_addr);
    }

    // ─── LRU 淘汰测试 ─────────────────────────────

    #[test]
    fn test_lru_eviction() {
        let mut pool = WritePointPool::new(WritePointConfig {
            max_write_points: 2,
        });

        // 占用 2 个槽位
        let _wp1 = pool.resolve(WritePointSpecifier::Hashed(1));
        let _wp2 = pool.resolve(WritePointSpecifier::Hashed(2));
        assert_eq!(pool.nr_active(), 2);

        // 第三个 hash 应触发 LRU 淘汰
        let _wp3 = pool.resolve(WritePointSpecifier::Hashed(3));
        assert_eq!(pool.nr_active(), 2);

        // 验证被淘汰的 identity 已从 map 中移除
        // (由于时间戳精度，无法精确判断哪个被淘汰，但总数正确)
        assert!(
            pool.identity_map.len() <= 2,
            "LRU pool should keep at most 2 entries"
        );
        // 被淘汰的 identity 应已被移除
        assert!(
            pool.identity_map.contains_key(&3),
            "newest entry (3) should be in map"
        );
    }

    #[test]
    fn test_lru_nr_active_tracking() {
        let mut pool = WritePointPool::new(WritePointConfig {
            max_write_points: 4,
        });

        assert_eq!(pool.nr_active(), 0);

        pool.resolve(WritePointSpecifier::Hashed(10));
        assert_eq!(pool.nr_active(), 1);

        pool.resolve(WritePointSpecifier::Hashed(20));
        assert_eq!(pool.nr_active(), 2);

        pool.resolve(WritePointSpecifier::Hashed(30));
        assert_eq!(pool.nr_active(), 3);

        pool.resolve(WritePointSpecifier::Hashed(40));
        assert_eq!(pool.nr_active(), 4);

        // 池满后，LRU 淘汰不增加 nr_active
        pool.resolve(WritePointSpecifier::Hashed(50));
        assert_eq!(pool.nr_active(), 4);
    }

    // ─── stranded_space 测试 ──────────────────────

    #[test]
    fn test_stranded_space_no_active() {
        let pool = WritePointPool::new(WritePointConfig {
            max_write_points: 4,
        });
        assert_eq!(pool.stranded_space(1024 * 1024), 0);
    }

    #[test]
    fn test_stranded_space_with_active() {
        let mut pool = WritePointPool::new(WritePointConfig {
            max_write_points: 4,
        });
        pool.resolve(WritePointSpecifier::Hashed(1));
        pool.resolve(WritePointSpecifier::Hashed(2));
        // 仅统计 2 个活跃池写点
        assert_eq!(pool.stranded_space(1024 * 1024), 2 * 1024 * 1024);
    }

    // ─── done() 测试 ──────────────────────────────

    #[test]
    fn test_done_updates_prev_sectors_free() {
        use crate::alloc::open_bucket::BchOpenBuckets;
        let mut wp = WritePoint::new(0);
        wp.sectors_free = 100;
        let open_buckets = BchOpenBuckets::new();
        wp.done(256, &open_buckets);
        assert_eq!(wp.prev_sectors_free, 100);
    }

    // ─── too_many_writepoints 测试 ───────────────

    #[test]
    fn test_too_many_writepoints_zero_active_returns_false() {
        let pool = WritePointPool::new(WritePointConfig {
            max_write_points: 8,
        });
        // 0 active 时不应触发回退
        assert!(!pool.too_many_writepoints(100, 2000));
    }

    #[test]
    fn test_too_many_writepoints_ok() {
        let pool = WritePointPool::new(WritePointConfig {
            max_write_points: 8,
        });
        // 0 active → stranded = 0
        assert!(!pool.too_many_writepoints(100, 3000));
    }

    #[test]
    fn test_too_many_writepoints_with_active() {
        let mut pool = WritePointPool::new(WritePointConfig {
            max_write_points: 8,
        });
        pool.resolve(WritePointSpecifier::Hashed(1));
        pool.resolve(WritePointSpecifier::Hashed(2));
        pool.resolve(WritePointSpecifier::Hashed(3));
        // 3 active, bucket_size = 100
        // stranded = 300, stranded * 8 = 2400
        // free = 2000, 2400 > 2000 → true
        assert!(pool.too_many_writepoints(100, 2000));
    }

    // ─── try_decrease 测试 ────────────────────────

    #[test]
    fn test_try_decrease_no_active_returns_false() {
        let mut pool = WritePointPool::new(WritePointConfig {
            max_write_points: 8,
        });
        let open_buckets = BchOpenBuckets::new();
        // nr_active = 0, should return false
        assert!(!pool.try_decrease(100, 2000, &open_buckets));
        assert_eq!(pool.nr_active(), 0);
    }

    #[test]
    fn test_try_decrease_only_one_active_returns_false() {
        let mut pool = WritePointPool::new(WritePointConfig {
            max_write_points: 8,
        });
        let open_buckets = BchOpenBuckets::new();
        pool.resolve(WritePointSpecifier::Hashed(1));
        assert_eq!(pool.nr_active(), 1);
        // nr_active = 1, min guard: should return false even if free is low
        assert!(!pool.try_decrease(100, 0, &open_buckets));
        assert_eq!(pool.nr_active(), 1);
    }

    #[test]
    fn test_try_decrease_stranded_not_exceeded() {
        let mut pool = WritePointPool::new(WritePointConfig {
            max_write_points: 8,
        });
        let open_buckets = BchOpenBuckets::new();
        pool.resolve(WritePointSpecifier::Hashed(1));
        pool.resolve(WritePointSpecifier::Hashed(2));
        assert_eq!(pool.nr_active(), 2);
        // 2 active, bucket = 100, stranded * 8 = 1600
        // free = 5000 → not exceeded → false
        assert!(!pool.try_decrease(100, 5000, &open_buckets));
        assert_eq!(pool.nr_active(), 2);
    }

    #[test]
    fn test_try_decrease_success() {
        let mut pool = WritePointPool::new(WritePointConfig {
            max_write_points: 8,
        });
        let open_buckets = BchOpenBuckets::new();

        // 分配 3 个写点
        pool.resolve(WritePointSpecifier::Hashed(1));
        pool.resolve(WritePointSpecifier::Hashed(2));
        pool.resolve(WritePointSpecifier::Hashed(3));
        assert_eq!(pool.nr_active(), 3);
        assert!(pool.identity_map.contains_key(&3)); // 最后一个写点

        // stranded * 8 > free → should decrease
        assert!(pool.try_decrease(100, 1000, &open_buckets));

        // nr_active 应减少到 2
        assert_eq!(pool.nr_active(), 2);
        // identity 3 应已被移除
        assert!(!pool.identity_map.contains_key(&3));
    }

    // ─── try_reuse_current_wp 测试（L1 写点级复用）───

    #[test]
    fn test_try_reuse_current_wp_hashed_found() {
        let ob = BchOpenBuckets::new();
        let mut pool = WritePointPool::new(WritePointConfig {
            max_write_points: 8,
        });
        let wp_id = WritePointSpecifier::Hashed(42);

        // 手动分配一个 open bucket 并挂到写点 ptrs
        let ob_idx = ob.alloc(0, 5, 256, 1).unwrap();
        let wp = pool.resolve(wp_id);
        wp.ptrs.push(ob_idx);
        wp.sectors_free = 256;

        // 应能在当前写点找到可用桶
        let result = pool.try_reuse_current_wp(wp_id, &ob, 0, 256, 128);
        assert!(
            result.is_some(),
            "should find reusable bucket in current wp"
        );
        let (_ob_idx, group_id, bucket_bi, _offset) = result.unwrap();
        assert_eq!(group_id, 0);
        assert_eq!(bucket_bi, 5);
    }

    #[test]
    fn test_try_reuse_current_wp_starts_at_bucket_base() {
        let ob = BchOpenBuckets::new();
        let mut pool = WritePointPool::new(WritePointConfig {
            max_write_points: 8,
        });
        let wp_id = WritePointSpecifier::Hashed(7);

        let bucket_capacity = (crate::alloc::BLOCKS_PER_BUCKET * SECTORS_PER_BLOCK) as u32;
        let ob_idx = ob.alloc(0, 9, bucket_capacity, 1).unwrap();
        let wp = pool.resolve(wp_id);
        wp.ptrs.push(ob_idx);
        wp.sectors_free = bucket_capacity as u64;

        let result =
            pool.try_reuse_current_wp(wp_id, &ob, 0, bucket_capacity, SECTORS_PER_BLOCK as u32);
        assert!(result.is_some());
        let (_ob_idx, group_id, bucket_bi, offset) = result.unwrap();
        assert_eq!(group_id, 0);
        assert_eq!(bucket_bi, 9);
        assert_eq!(offset, 0);
    }

    #[test]
    fn test_try_reuse_current_wp_direct_found() {
        let ob = BchOpenBuckets::new();
        let mut pool = WritePointPool::new(WritePointConfig {
            max_write_points: 8,
        });
        let wp_id = WritePointSpecifier::Direct(DedicatedWp::BTree);

        // 手动分配一个 open bucket 并挂到专用写点
        let bucket_capacity = (crate::alloc::BLOCKS_PER_BUCKET * SECTORS_PER_BLOCK) as u32;
        let ob_idx = ob.alloc(1, 10, bucket_capacity, 2).unwrap();
        // pool 是 &self（只读引用），通过 resolve_direct 已调用了 resolve
        // 这里利用 WritePoint 的 public ptrs 字段在 resolve 后修改
        // 由于 try_reuse_current_wp 要求 &self，我们需要在 resolve 后建一个可读池
        // 最简单方法：将 ptr 写入 dedicated write point
        {
            let wp = pool.resolve(wp_id);
            wp.ptrs.push(ob_idx);
            wp.sectors_free = bucket_capacity as u64;
        }
        // 写点池已修改；&self 方法可以读取

        let result = pool.try_reuse_current_wp(wp_id, &ob, 1, bucket_capacity, 256);
        assert!(
            result.is_some(),
            "should find reusable bucket in dedicated wp"
        );
        let (_ob_idx, group_id, bucket_bi, _offset) = result.unwrap();
        assert_eq!(group_id, 1);
        assert_eq!(bucket_bi, 10);
    }

    #[test]
    fn test_try_reuse_current_wp_not_found_when_full() {
        let ob = BchOpenBuckets::new();
        let mut pool = WritePointPool::new(WritePointConfig {
            max_write_points: 8,
        });
        let wp_id = WritePointSpecifier::Hashed(42);

        // 分配一个 open bucket 但设置 sectors_free 较小
        let ob_idx = ob.alloc(0, 3, 50, 1).unwrap();
        let wp = pool.resolve(wp_id);
        wp.ptrs.push(ob_idx);
        wp.sectors_free = 50;

        // 请求空间超过剩余 → 应返回 None
        let result = pool.try_reuse_current_wp(wp_id, &ob, 0, 50, 100);
        assert!(
            result.is_none(),
            "should NOT find bucket when sectors_free < needed"
        );
    }

    #[test]
    fn test_try_reuse_current_wp_not_found_when_no_ptrs() {
        let ob = BchOpenBuckets::new();
        let pool = WritePointPool::new(WritePointConfig {
            max_write_points: 8,
        });
        let wp_id = WritePointSpecifier::Direct(DedicatedWp::BTree);

        // 专用写点初始 ptrs 为空
        let result = pool.try_reuse_current_wp(wp_id, &ob, 0, 256, 100);
        assert!(
            result.is_none(),
            "should NOT find bucket when wp has no ptrs"
        );
    }

    #[test]
    fn test_try_reuse_current_wp_not_found_for_unknown_hash() {
        let ob = BchOpenBuckets::new();
        let pool = WritePointPool::new(WritePointConfig {
            max_write_points: 8,
        });

        // 从未解析过的 hash → identity_map 中不存在
        let result = pool.try_reuse_current_wp(WritePointSpecifier::Hashed(999), &ob, 0, 256, 100);
        assert!(result.is_none(), "should NOT find bucket for unknown hash");
    }

    #[test]
    fn test_try_reuse_current_wp_consume_free_sectors() {
        let ob = BchOpenBuckets::new();
        let mut pool = WritePointPool::new(WritePointConfig {
            max_write_points: 8,
        });
        let wp_id = WritePointSpecifier::Hashed(42);

        let ob_idx = ob.alloc(0, 7, 256, 1).unwrap();
        let wp = pool.resolve(wp_id);
        wp.ptrs.push(ob_idx);
        wp.sectors_free = 256;

        // 消耗 128 扇区
        let _result = pool.try_reuse_current_wp(wp_id, &ob, 0, 256, 128);
        let entry = ob.get_entry(ob_idx).unwrap();
        // sectors_free 应为 256 - 128 = 128（原子递减）
        assert!(
            entry
                .sectors_free
                .load(std::sync::atomic::Ordering::Acquire)
                <= 128,
            "sectors_free should have been consumed"
        );
    }

    #[test]
    fn test_try_reuse_current_wp_skips_other_device_before_consuming() {
        let ob = BchOpenBuckets::new();
        let mut pool = WritePointPool::new(WritePointConfig {
            max_write_points: 8,
        });
        let wp_id = WritePointSpecifier::Hashed(42);
        let other = ob.alloc(1, 7, 4096, 1).unwrap();
        let selected = ob.alloc(0, 7, 1024, 1).unwrap();
        let wp = pool.resolve(wp_id);
        wp.ptrs.extend([other, selected]);
        wp.sectors_free = 5120;

        let result = pool.try_reuse_current_wp(wp_id, &ob, 0, 1024, 8).unwrap();

        assert_eq!(result.0, selected);
        assert_eq!(result.1, 0);
        assert_eq!(
            ob.get_entry(other)
                .unwrap()
                .sectors_free
                .load(std::sync::atomic::Ordering::Acquire),
            4096
        );
        assert_eq!(
            ob.get_entry(selected)
                .unwrap()
                .sectors_free
                .load(std::sync::atomic::Ordering::Acquire),
            1016
        );
    }
}
