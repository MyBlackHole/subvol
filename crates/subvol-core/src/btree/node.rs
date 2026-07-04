//! BtreeNode — B-tree 节点
//!
//! 对应 bcachefs `struct btree` (btree/types.h:174)
//! 使用 bset 布局存储键集合，支持多 bset 层。

use serde::{Deserialize, Serialize};

use crate::btree::bset::{
    bch2_bset_delete, bch2_bset_init_first, bch2_bset_init_next, bch2_bset_insert, bset_from_bytes,
    bset_to_bytes, rebuild_rw_aux, BsetAuxTreeType, BsetTree, BSET_ENTRY_LIMIT, MAX_BSETS,
};
use crate::btree::key::{Bpos, BtreeEntry};
use crate::btree::types::{BtreeId, NODE_SIZE};
use crate::data::extents_format::{BtreePtr, ENTRY_TYPE_BTREE_PTR};
use crate::lock::six::{SixLock, SixLockType, SixLockWaiter};
use crate::types::StorageError;

/// 写块阈值 — 当一个 bset 超过此大小时触发新 bset 创建
const BTREE_WRITE_SET_BUFFER: u64 = 4096;
/// 节点在磁盘上的 header 大小：nsets(4 bytes)
const NODE_DISK_HEADER: usize = 4;
/// 每个 bset record 的 header 大小：data_len(4 bytes)
const BSET_RECORD_HEADER: usize = 4;

// ═══════════════════════════════════════════════════════════════
// BtreeNode
// ═══════════════════════════════════════════════════════════════

/// BtreeNode — B-tree 节点
///
/// 对应 bcachefs `struct btree` (btree/types.h:174)
///
/// 字段对应:
/// - `lock` → `struct btree_bkey_cached_common.lock` (types.h:128)
/// - `level` / `btree_id` → `.level` / `.btree_id` (types.h:129-130)
/// - `set[]` / `nsets` → `.set[MAX_BSETS]` / `.nsets` (types.h:196-197)
/// - `key_count` → `.nr.live_u64s` 的简化
#[derive(Serialize, Deserialize)]
pub struct BtreeNode {
    /// SixLock — 对应 btree_bkey_cached_common.lock (types.h:128)
    #[serde(skip, default = "SixLock::new")]
    pub lock: SixLock,

    /// B-tree level — 0 为叶子节点 (types.h:129)
    pub level: u8,

    /// Btree type id (types.h:130)
    pub btree_id: BtreeId,

    /// 是否 cached 节点 (types.h:131)
    pub cached: bool,

    /// 键数量 — 简化版 `struct btree_nr_keys` (types.h:80)
    pub key_count: u32,

    /// bset 层数（对应 nsets, types.h:197）
    pub nsets: u8,

    /// bset 集合（对应 set[MAX_BSETS], types.h:196）
    pub set: [BsetTree; MAX_BSETS],

    /// 已持久化的 bset 数量（对应 bcachefs b->written）
    ///
    /// bset 索引 < written 的已被持久化（从磁盘读取或已写入磁盘）。
    /// bset 索引 >= written 的是内存中未持久化的修改。
    pub written: u8,

    /// 节点缓冲区总大小（字节），默认 NODE_SIZE
    pub size: u64,

    /// 节点在设备上的偏移（0 = 尚未分配设备空间）
    #[serde(skip, default)]
    pub disk_offset: u64,

    /// 节点序列化数据在设备上的大小
    #[serde(skip, default)]
    pub disk_size: u32,
}

// ═══════════════════════════════════════════════════════════════
// 构造函数
// ═══════════════════════════════════════════════════════════════

impl BtreeNode {
    /// 创建新的 BtreeNode
    ///
    /// 对应 bcachefs `struct btree` 分配 + `bch2_btree_node_init` (btree/init.c)
    pub fn new(level: u8, btree_id: BtreeId) -> Self {
        Self {
            lock: SixLock::new(),
            level,
            btree_id,
            cached: false,
            key_count: 0,
            nsets: 1,
            set: [BsetTree::new(0), BsetTree::new(0), BsetTree::new(0)],
            written: 0,
            size: NODE_SIZE,
            disk_offset: 0,
            disk_size: 0,
        }
    }

    /// 创建带初始 seq 的节点
    pub fn with_seq(level: u8, btree_id: BtreeId, seq: u64) -> Self {
        let mut node = Self::new(level, btree_id);
        bch2_bset_init_first(&mut node.set[0], seq);
        node
    }

    /// 创建默认容量的叶子节点
    pub fn new_leaf(btree_id: BtreeId) -> Self {
        Self::new(0, btree_id)
    }

    /// 获取最后一个 bset（当前写入层）
    ///
    /// 对应 bcachefs `bset_tree_last()`
    pub fn bset_tree_last(&self) -> &BsetTree {
        &self.set[(self.nsets - 1) as usize]
    }

    /// 获取最后一个 bset 的可变引用
    pub fn bset_tree_last_mut(&mut self) -> &mut BsetTree {
        let last = self.nsets - 1;
        &mut self.set[last as usize]
    }

    /// 新增一层 bset（当前层满或最后一个 bset 已写入时调用）
    ///
    /// 对应 bcachefs `bch2_bset_init_next()`
    ///
    /// 当 nsets 已达 MAX_BSETS 时先合并再创建。
    /// 若合并后的序列化大小超过 NODE_SIZE，返回 CowNeeded。
    pub fn init_next_bset(&mut self, seq: u64, capacity: usize) -> Result<(), StorageError> {
        if self.nsets as usize >= MAX_BSETS {
            self.compact()?;
            let serialized = self.to_bytes();
            if serialized.len() > self.size as usize {
                return Err(StorageError::CowNeeded(format!(
                    "init_next_bset: {} bytes > size {}",
                    serialized.len(),
                    self.size
                )));
            }
            // compact 后，所有键合并到一个 bset，nsets=1
            // 这个 bset 的地址在 written 之下（已写入）
        }
        let idx = self.nsets as usize;
        self.nsets += 1;
        bch2_bset_init_next(&mut self.set[idx], seq, capacity);
        Ok(())
    }

    /// 合并所有 bset 到第一层
    ///
    /// 对应 bcachefs `__bch2_btree_node_compact()` 的简化
    fn merge_bsets(&mut self) -> Result<(), StorageError> {
        if self.nsets <= 1 {
            return Ok(());
        }
        // 找到第一个未写入 bset
        let _unwritten_idx = self.first_unwritten_bset();

        // 从所有 bset 收集键，最新层优先
        let mut merged: Vec<BtreeEntry> = Vec::new();
        for i in (0..self.nsets as usize).rev() {
            for entry in &self.set[i].bset.entries {
                let pos = entry.pos;
                if merged.binary_search_by(|e| e.pos.cmp(&pos)).is_ok() {
                    continue;
                }
                merged.push(entry.clone());
            }
        }
        merged.sort_by(|a, b| a.pos.cmp(&b.pos));

        // 重置第一层（地址在 written 之下，视为已写入）
        let seq = self.set[0].bset.seq.max(1) + 1;
        bch2_bset_init_first(&mut self.set[0], seq);
        for entry in merged {
            self.set[0].bset.entries.push(entry);
        }
        rebuild_rw_aux(&mut self.set[0]);
        self.set[0].bset.seq = seq;

        // 清空后续层
        for i in 1..self.nsets as usize {
            self.set[i] = BsetTree::new(0);
        }
        self.nsets = 1;
        self.key_count = self.set[0].bset.entries.len() as u32;

        // 检查已写入数据是否超过节点大小
        let serialized = self.serialized_data_size();
        if serialized > self.size {
            return Err(StorageError::BtreeNodeFull);
        }
        Ok(())
    }
}

// ═══════════════════════════════════════════════════════════════
// bset 状态机 — written / write_block / capacity
// ═══════════════════════════════════════════════════════════════

impl BtreeNode {
    /// write_block — 未写入区域的索引起始（对应 bcachefs write_block 的简化）
    /// 返回第一个未写入 bset 的索引
    pub fn write_block(&self) -> u8 {
        self.written
    }

    /// 检查指定索引的 bset 是否已被持久化
    ///
    /// 对应 bcachefs `bset_written(b, bset(b, &b->set[idx]))`
    /// 简化：bset 索引 < written 视为已写入
    pub fn bset_written(&self, idx: usize) -> bool {
        idx < self.written as usize
    }

    /// 找到第一个未写入 bset 的索引
    fn first_unwritten_bset(&self) -> usize {
        self.written as usize
    }

    /// 计算剩余空间（u64 为单位）
    ///
    /// 对应 bcachefs `bch2_btree_keys_u64s_remaining(b)`
    ///
    /// 计算所有 bset 的键占用空间，从节点总容量中减去得到剩余。
    /// 如果最后一个 bset 已写入，但仍有剩余容量，可以创建新 bset 继续写入。
    pub fn keys_u64s_remaining(&self) -> isize {
        let total = (self.size / 8) as isize;

        let used_u64s: isize = self.set[..self.nsets as usize]
            .iter()
            .flat_map(|s| &s.bset.entries)
            .map(|e| {
                let header_u64s = 4;
                let payload_u64s = (e.payload.len() + 7) / 8;
                (header_u64s + payload_u64s) as isize
            })
            .sum();

        total - used_u64s
    }

    /// 判断是否需要创建新的 bset
    ///
    /// 对应 bcachefs `want_new_bset(c, b)`
    ///
    /// 两种场景：
    /// 1. 最后一个 bset 已写入 → 需要新 bset
    /// 2. 最后一个 bset 未写入但太大（> 4KB）且剩余空间足够 → 创建新 bset
    pub fn want_new_bset(&self) -> bool {
        let last_idx = self.nsets as usize - 1;

        if self.bset_written(last_idx) {
            return true;
        }

        // 场景 2：最后一个 bset 未写入但太大
        let bset_bytes = bset_to_bytes(&self.set[last_idx].bset).len() as u64;
        if bset_bytes > BTREE_WRITE_SET_BUFFER {
            let remaining = self.keys_u64s_remaining() as u64;
            return remaining > BTREE_WRITE_SET_BUFFER / 8;
        }

        false
    }

    /// 插入前的准备：确保有可写入的 bset
    ///
    /// 对应 bcachefs `bch2_btree_node_prep_for_write()` + `bch2_btree_init_next()`
    ///
    /// 如果最后一个 bset 已写入或空间不足，创建新 bset。
    /// 当节点没有剩余空间时返回 `BtreeNodeFull`。
    pub fn prep_for_insert(&mut self, entry_size: usize) -> Result<(), StorageError> {
        let entry_u64s = ((entry_size + 7) / 8) as isize + 4;
        let remaining = self.keys_u64s_remaining();
        if entry_u64s > remaining {
            return Err(StorageError::BtreeNodeFull);
        }

        // 如果需要新 bset，创建它
        if self.want_new_bset() {
            if self.nsets as usize >= MAX_BSETS {
                self.compact()?;
            }
            let max_seq = self
                .set
                .iter()
                .take(self.nsets as usize)
                .map(|s| s.bset.seq)
                .max()
                .unwrap_or(0)
                + 1;
            let idx = self.nsets as usize;
            self.nsets += 1;
            bch2_bset_init_next(&mut self.set[idx], max_seq, BSET_ENTRY_LIMIT);
        }

        Ok(())
    }

    /// 计算当前序列化数据的总大小（近似）
    fn serialized_data_size(&self) -> u64 {
        NODE_DISK_HEADER as u64
            + self
                .set
                .iter()
                .take(self.nsets as usize)
                .map(|s| BSET_RECORD_HEADER as u64 + bset_to_bytes(&s.bset).len() as u64)
                .sum::<u64>()
    }

    /// 在持久化后推进 written 计数
    ///
    /// 对应 bcachefs 节点写入后推进 `b->written`
    pub fn advance_written(&mut self) {
        self.written = self.nsets;
    }
}

// ═══════════════════════════════════════════════════════════════
// Btree 操作 — Split / Merge / Compact
// ═══════════════════════════════════════════════════════════════

impl BtreeNode {
    /// 分裂节点为两个
    ///
    /// 对应 bcachefs `btree_split()` (interior.c:1962)
    /// 按 median pivot 将 keys 分配到左右两个新节点，每个新节点仅有 1 个 bset。
    /// 返回 (left, right)。
    pub fn split(&self) -> Result<(BtreeNode, BtreeNode), StorageError> {
        // 收集所有 keys，最新层优先（去重）
        let all = self.collect_all_keys();
        if all.is_empty() {
            return Err(StorageError::NotFound);
        }

        let max_seq = self
            .set
            .iter()
            .take(self.nsets as usize)
            .map(|s| s.bset.seq)
            .max()
            .unwrap_or(0);

        // Split by serialized capacity, not just key count: btree keys have
        // variable payload sizes and a count-balanced split can still leave
        // one half larger than NODE_SIZE.
        for mid in (1..=all.len() / 2).rev() {
            let mut left = BtreeNode::with_seq(self.level, self.btree_id, max_seq + 1);
            let mut right = BtreeNode::with_seq(self.level, self.btree_id, max_seq + 2);
            let left_ok = all[..mid]
                .iter()
                .try_for_each(|e| left.insert_key(e.clone()))
                .is_ok();
            let right_ok = left_ok
                && all[mid..]
                    .iter()
                    .try_for_each(|e| right.insert_key(e.clone()))
                    .is_ok();
            if left_ok
                && right_ok
                && left.to_bytes().len() <= left.size as usize
                && right.to_bytes().len() <= right.size as usize
            {
                return Ok((left, right));
            }
        }
        Err(StorageError::BtreeNodeFull)
    }

    pub(crate) fn split_for_root(&self) -> Result<Vec<BtreeNode>, StorageError> {
        let all = self.collect_all_keys();
        if all.is_empty() {
            return Err(StorageError::NotFound);
        }
        let max_seq = self
            .set
            .iter()
            .take(self.nsets as usize)
            .map(|s| s.bset.seq)
            .max()
            .unwrap_or(0);
        let mut chunks = Vec::new();
        let mut current = BtreeNode::with_seq(self.level, self.btree_id, max_seq + 1);
        for entry in all {
            if current.insert_key(entry.clone()).is_err()
                || current.to_bytes().len() > current.size as usize
            {
                let _ = current.remove_key(&entry.pos);
                if current.total_key_count() == 0 {
                    return Err(StorageError::BtreeNodeFull);
                }
                chunks.push(current);
                current = BtreeNode::with_seq(self.level, self.btree_id, max_seq + 1);
                current.insert_key(entry)?;
            }
        }
        if current.total_key_count() != 0 {
            chunks.push(current);
        }
        Ok(chunks)
    }

    pub(crate) fn would_fit_entries(&self, entries: &[BtreeEntry]) -> bool {
        let mut shadow = BtreeNode::with_seq(self.level, self.btree_id, 1);
        for entry in self.collect_all_keys().into_iter().chain(entries.iter().cloned()) {
            if entry.entry_type == 1 && entry.payload.is_empty() {
                let _ = shadow.remove_key(&entry.pos);
            } else if shadow.insert_key(entry).is_err() {
                return false;
            }
        }
        shadow.to_bytes().len() <= shadow.size as usize
    }

    /// 如果空间允许，将 `other` 的 keys 合并到当前节点
    ///
    /// 对应 bcachefs `compute_merge()` + `__bch2_foreground_maybe_merge()`
    /// 返回合并后的新节点（当前节点和 other 不变）。
    pub fn try_merge(&self, other: &BtreeNode) -> Option<BtreeNode> {
        let mut all = self.collect_all_keys();
        let other_keys = other.collect_all_keys();

        all.extend(other_keys);
        all.sort_by(|a, b| a.pos.cmp(&b.pos));
        all.dedup_by(|a, b| a.pos == b.pos);

        let max_seq = self
            .set
            .iter()
            .take(self.nsets as usize)
            .chain(other.set.iter().take(other.nsets as usize))
            .map(|s| s.bset.seq)
            .max()
            .unwrap_or(0);

        let mut merged = BtreeNode::with_seq(self.level, self.btree_id, max_seq + 1);
        merged.size = self.size.min(other.size);
        for e in &all {
            if merged.insert_key(e.clone()).is_err() {
                return None;
            }
        }
        if merged.serialized_data_size() > merged.size {
            return None;
        }
        Some(merged)
    }

    /// 合并本节点内所有 bset 层为一层，清理重复键
    ///
    /// 对应 bcachefs `bch2_btree_node_compact()` (sort.c:582)
    ///
    /// - 合并所有 bset（以 written 为边界）
    /// - 合并后 nsets = 1，written 不变
    /// - 若已写入数据超过节点大小返回 BtreeNodeFull
    ///
    /// 返回 true 表示发生了合并。
    pub fn compact(&mut self) -> Result<bool, StorageError> {
        if self.nsets <= 1 {
            return Ok(false);
        }

        let _unwritten_idx = self.first_unwritten_bset();

        let all = self.collect_all_keys();
        let max_seq = self
            .set
            .iter()
            .take(self.nsets as usize)
            .map(|s| s.bset.seq)
            .max()
            .unwrap_or(0);

        // 重置为单 bset
        bch2_bset_init_first(&mut self.set[0], max_seq + 1);
        for e in &all {
            let _ = self.set[0].bset.insert(e.clone());
        }
        rebuild_rw_aux(&mut self.set[0]);

        // compact 后所有数据合并到 bset[0]，清空后续层
        for i in 1..self.nsets as usize {
            self.set[i] = BsetTree::new(0);
        }
        self.nsets = 1;
        // compact 重新组织了数据，written 设为 0（需要完整写入）
        self.written = 0;
        self.key_count = self.set[0].bset.entries.len() as u32;

        // 检查已写入数据是否超过节点大小
        let serialized = self.serialized_data_size();
        if serialized > self.size {
            return Err(StorageError::BtreeNodeFull);
        }

        Ok(true)
    }

    /// 从最后一个 bset 中移除指向指定 child_offset 的 BtreePtr 条目
    ///
    /// 用于分裂叶子节点时，清除 root 中旧的子节点指针。
    pub fn remove_child_ptr_by_offset(&mut self, child_offset: u64) {
        for idx in 0..self.nsets as usize {
            let set = &mut self.set[idx];
            set.bset.entries.retain(|e| {
                if e.entry_type != ENTRY_TYPE_BTREE_PTR {
                    return true;
                }
                if let Some(ptr) = BtreePtr::from_bytes(&e.payload) {
                    ptr.offset != child_offset
                } else {
                    true
                }
            });
            rebuild_rw_aux(set);
        }
        self.key_count = self
            .set
            .iter()
            .take(self.nsets as usize)
            .map(|s| s.bset.entries.len() as u32)
            .sum();
    }

    /// 从所有 bset 层收集去重的 sorted keys（最新层优先）
    fn collect_all_keys(&self) -> Vec<BtreeEntry> {
        let mut all: Vec<BtreeEntry> = Vec::new();
        for i in (0..self.nsets as usize).rev() {
            for e in &self.set[i].bset.entries {
                let pos = e.pos;
                if !all.iter().any(|x| x.pos == pos) {
                    all.push(e.clone());
                }
            }
        }
        all.sort_by(|a, b| a.pos.cmp(&b.pos));
        all
    }
}

// ═══════════════════════════════════════════════════════════════
// 键操作
// ═══════════════════════════════════════════════════════════════

impl BtreeNode {
    /// 检查最后一个 bset 是否达到条数上限
    pub fn last_bset_is_full(&self) -> bool {
        self.bset_tree_last().bset.entries.len() >= BSET_ENTRY_LIMIT
    }

    /// 当最后一个 bset 满时，创建新层（可能触发合并）
    pub fn try_rotate_bset(&mut self) -> Result<(), StorageError> {
        if !self.last_bset_is_full() {
            return Ok(());
        }
        let max_seq = self
            .set
            .iter()
            .take(self.nsets as usize)
            .map(|s| s.bset.seq)
            .max()
            .unwrap_or(0)
            + 1;
        self.init_next_bset(max_seq, BSET_ENTRY_LIMIT)
    }

    /// 插入键（维持 Bpos 排序）
    ///
    /// 对应 bcachefs `bch2_btree_bset_insert_key` (update.c)
    /// 只操作最后一个 bset（bset_tree_last）。
    ///
    /// 流程：
    /// 1. `prep_for_insert` — 确保有可写入的 bset
    /// 2. 插入到最后一个 bset
    ///
    /// 当节点没有剩余空间时返回 `BtreeNodeFull`，调用者应 rewrite/split 并重试。
    pub fn insert_key(&mut self, entry: BtreeEntry) -> Result<(), StorageError> {
        // A transaction update replaces the live key, even when the previous
        // version resides in an older on-disk bset. Remove older versions
        // first so replaying an already persisted key does not create a
        // duplicate live entry.
        let pos = entry.pos;
        for idx in 0..self.nsets as usize {
            bch2_bset_delete(&mut self.set[idx], &pos);
        }
        let entry_size = entry.payload.len() + std::mem::size_of::<Bpos>();
        self.prep_for_insert(entry_size)?;
        let last = self.bset_tree_last_mut();
        bch2_bset_insert(last, entry)?;
        self.key_count = self
            .set
            .iter()
            .take(self.nsets as usize)
            .map(|s| s.bset.entries.len() as u32)
            .sum();
        Ok(())
    }

    /// 按位置删除键
    ///
    /// 对应 bcachefs `bch2_btree_delete_at` (update.h:166)
    /// 在最后一个 bset 中删除。
    pub fn remove_key(&mut self, pos: &Bpos) -> Result<(), StorageError> {
        let mut removed = false;
        for idx in 0..self.nsets as usize {
            removed |= bch2_bset_delete(&mut self.set[idx], pos);
        }
        if removed {
            self.key_count = self
                .set
                .iter()
                .take(self.nsets as usize)
                .map(|s| s.bset.entries.len() as u32)
                .sum();
            Ok(())
        } else {
            Err(StorageError::NotFound)
        }
    }

    /// 在所有 bset 中查找键（从最新层开始）
    ///
    /// 对应 bcachefs `bch2_btree_path_peek_slot` (iter.c)
    pub fn lookup(&self, pos: &Bpos) -> Option<&BtreeEntry> {
        // 从最新层到最旧层查找
        for i in (0..self.nsets as usize).rev() {
            if let Ok(idx) = self.set[i].bset.search_idx(pos) {
                return Some(&self.set[i].bset.entries[idx]);
            }
        }
        None
    }

    /// 在所有 bset 中查找严格大于 pos 的最小键
    ///
    /// 对应 bcachefs `bch2_btree_iter_peek` + btree_node_iter 推进
    pub fn next_entry(&self, pos: &Bpos) -> Option<&BtreeEntry> {
        let mut best: Option<&BtreeEntry> = None;
        for i in (0..self.nsets as usize).rev() {
            if let Some(entry) = self.set[i].bset.search_successor(pos) {
                best = match best {
                    None => Some(entry),
                    Some(cur) => Some(if entry.pos < cur.pos { entry } else { cur }),
                };
            }
        }
        best
    }

    /// 在所有 bset 中查找严格小于 pos 的最大键
    ///
    /// 对应 bcachefs `bch2_btree_iter_peek_prev`
    pub fn prev_entry(&self, pos: &Bpos) -> Option<&BtreeEntry> {
        let mut best: Option<&BtreeEntry> = None;
        for i in (0..self.nsets as usize).rev() {
            if let Some(entry) = self.set[i].bset.search_predecessor(pos) {
                best = match best {
                    None => Some(entry),
                    Some(cur) => Some(if entry.pos > cur.pos { entry } else { cur }),
                };
            }
        }
        best
    }

    /// 获取所有 bset 中的第一个键
    pub fn first_entry(&self) -> Option<&BtreeEntry> {
        let mut best: Option<&BtreeEntry> = None;
        for i in (0..self.nsets as usize).rev() {
            if let Some(entry) = self.set[i].bset.first_entry() {
                best = match best {
                    None => Some(entry),
                    Some(cur) => Some(if entry.pos < cur.pos { entry } else { cur }),
                };
            }
        }
        best
    }

    /// 获取所有 bset 中的最后一个键
    pub fn last_entry(&self) -> Option<&BtreeEntry> {
        let mut best: Option<&BtreeEntry> = None;
        for i in (0..self.nsets as usize).rev() {
            if let Some(entry) = self.set[i].bset.last_entry() {
                best = match best {
                    None => Some(entry),
                    Some(cur) => Some(if entry.pos > cur.pos { entry } else { cur }),
                };
            }
        }
        best
    }

    /// 按位置查找可变引用
    pub fn lookup_mut(&mut self, pos: &Bpos) -> Option<&mut BtreeEntry> {
        let nsets = self.nsets;
        for i in (0..nsets as usize).rev() {
            let (set_idx, entry_idx) = {
                let entries = &self.set[i].bset.entries;
                match entries.binary_search_by(|e| e.pos.cmp(pos)) {
                    Ok(idx) => (i, idx),
                    Err(_) => continue,
                }
            };
            return Some(&mut self.set[set_idx].bset.entries[entry_idx]);
        }
        None
    }

    /// 节点是否已满（检查剩余空间）
    ///
    /// 对应 bcachefs `bch2_btree_keys_u64s_remaining <= 0`
    /// 剩余空间不足时返回 true，需要 rewrite/split。
    pub fn is_full(&self) -> bool {
        self.keys_u64s_remaining() <= 0
    }

    /// 清空节点
    pub fn clear(&mut self) {
        for i in 0..self.nsets as usize {
            self.set[i].bset.clear();
            self.set[i].rw_aux.clear();
            self.set[i].aux_type = BsetAuxTreeType::NoAuxTree;
        }
        self.key_count = 0;
    }

    /// 获取所有 bset 中的总键数
    pub fn total_key_count(&self) -> u32 {
        self.set
            .iter()
            .take(self.nsets as usize)
            .map(|s| s.bset.entries.len() as u32)
            .sum()
    }
}

// ═══════════════════════════════════════════════════════════════
// SixLock 辅助方法
// ═══════════════════════════════════════════════════════════════

impl BtreeNode {
    /// 获取 SixLock 引用，供事务使用
    /// 对应 bcachefs btree_bkey_cached_common.lock 访问
    pub fn six_lock(&self) -> &SixLock {
        &self.lock
    }

    /// 阻塞获取读锁（同步代码使用）
    ///
    /// 对应 bcachefs `six_lock_read` 或 `bch2_btree_node_lock(READ)` (locking.h:408)
    pub fn lock_read_blocking(&self) {
        let mut waiter = SixLockWaiter {
            trans_start_time: 0,
            thread: Some(std::thread::current()),
            lock_want: SixLockType::Read,
            lock_acquired: false,
            slot_idx: 0,
        };
        let ret = self.lock.six_lock_ip_waiter(SixLockType::Read, &mut waiter);
        debug_assert_eq!(ret, 0, "six_lock_ip_waiter(Read) failed");
    }

    /// 阻塞获取意向锁（同步代码使用）
    ///
    /// 对应 bcachefs `six_lock_intent` (six.h:237)
    pub fn lock_intent_blocking(&self) {
        if self.lock.six_lock_intent_recurse_if_owner() {
            return;
        }
        let mut waiter = SixLockWaiter {
            trans_start_time: 0,
            thread: Some(std::thread::current()),
            lock_want: SixLockType::Intent,
            lock_acquired: false,
            slot_idx: 0,
        };
        let ret = self
            .lock
            .six_lock_ip_waiter(SixLockType::Intent, &mut waiter);
        debug_assert_eq!(ret, 0, "six_lock_ip_waiter(Intent) failed");
    }

    /// 阻塞获取写锁（同步代码使用）
    ///
    /// 对应 bcachefs `bch2_btree_node_lock_write` (locking.h:538)
    pub fn lock_write_blocking(&self) {
        let mut waiter = SixLockWaiter {
            trans_start_time: 0,
            thread: Some(std::thread::current()),
            lock_want: SixLockType::Write,
            lock_acquired: false,
            slot_idx: 0,
        };
        let ret = self
            .lock
            .six_lock_ip_waiter(SixLockType::Write, &mut waiter);
        debug_assert_eq!(ret, 0, "six_lock_ip_waiter(Write) failed");
    }

    /// 异步获取读锁（async 代码使用，使用 try_lock + yield 避免阻塞 executor）
    ///
    /// 对应 bcachefs `six_lock_read` 非阻塞版本
    pub async fn lock_read(&self) {
        loop {
            if self.lock.six_trylock_read() {
                return;
            }
            tokio::task::yield_now().await;
        }
    }

    /// 异步获取写锁（async 代码使用，使用 try_lock + yield 避免阻塞 executor）
    ///
    /// 对应 bcachefs `bch2_btree_node_lock_write` 非阻塞版本
    pub async fn lock_write(&self) {
        loop {
            if self.lock.six_trylock_write() {
                return;
            }
            tokio::task::yield_now().await;
        }
    }

    /// 释放读锁
    ///
    /// 对应 bcachefs `six_unlock_read` (locking.h:six_unlock_type)
    pub fn unlock_read(&self) {
        self.lock.six_unlock_read();
    }

    /// 释放写锁
    ///
    /// 对应 bcachefs `bch2_btree_node_unlock_write` (locking.h:360)
    pub fn unlock_write(&self) {
        self.lock.six_unlock_write();
    }

    /// 递增锁引用计数（用于 read→intent 升级）
    ///
    /// 对应 bcachefs `six_lock_increment` (six.h:147)
    pub fn lock_increment(&self, typ: crate::lock::six::SixLockType) {
        self.lock.six_lock_increment(typ);
    }

    /// 释放意向锁
    ///
    /// 对应 bcachefs `six_unlock_intent` (six.h:267)
    pub fn unlock_intent(&self) {
        self.lock.six_unlock_intent();
    }
}

// ═══════════════════════════════════════════════════════════════
// 序列化
// ═══════════════════════════════════════════════════════════════

impl BtreeNode {
    /// 序列化 BtreeNode 为长度前缀的多 bset 字节序列
    ///
    /// 格式：
    ///   [nsets: 4 bytes LE]
    ///   [bset_0_len: 4 bytes LE][bset_0 JSON data...]
    ///   [bset_1_len: 4 bytes LE][bset_1 JSON data...]
    ///   ...
    ///
    /// 支持任意数量 bset（磁盘不限量）。
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bufs: Vec<Vec<u8>> = Vec::with_capacity(self.nsets as usize);
        let mut total = NODE_DISK_HEADER;
        for i in 0..self.nsets as usize {
            let bset_data = bset_to_bytes(&self.set[i].bset);
            total += BSET_RECORD_HEADER + bset_data.len();
            bufs.push(bset_data);
        }
        let mut buf = Vec::with_capacity(total);
        buf.extend_from_slice(&(self.nsets as u32).to_le_bytes());
        for bset_data in &bufs {
            buf.extend_from_slice(&(bset_data.len() as u32).to_le_bytes());
            buf.extend_from_slice(bset_data);
        }
        buf
    }

    /// 从多 bset 字节序列反序列化 BtreeNode（磁盘读取路径）
    ///
    /// 对应 bcachefs `bch2_btree_node_read_done`：
    /// 1. 读取全部历史 bset
    /// 2. merge/sort 去重合并到单个 bset
    /// 3. 内存 nsets = 1
    /// 4. written 设置为读取的数据大小（作为磁盘追加边界）
    ///
    /// 调用者需要用 `prep_for_insert`/`want_new_bset` 创建新 bset 才能写入。
    pub fn from_bytes(data: &[u8]) -> Option<Self> {
        if data.len() < NODE_DISK_HEADER {
            return None;
        }
        let nsets = u32::from_le_bytes(data[..NODE_DISK_HEADER].try_into().ok()?) as u8;
        if nsets == 0 || nsets as usize > MAX_BSETS {
            return None;
        }
        // 读取所有 bset
        let mut all_bsets: Vec<Vec<u8>> = Vec::with_capacity(nsets as usize);
        let mut off = NODE_DISK_HEADER;
        for _ in 0..nsets as usize {
            if off + BSET_RECORD_HEADER > data.len() {
                return None;
            }
            let bset_len =
                u32::from_le_bytes(data[off..off + BSET_RECORD_HEADER].try_into().ok()?) as usize;
            off += BSET_RECORD_HEADER;
            if off + bset_len > data.len() {
                return None;
            }
            let bset_data = data[off..off + bset_len].to_vec();
            all_bsets.push(bset_data);
            off += bset_len;
        }

        // 解析并合并所有 bset 到单个 bset
        // 对应 bcachefs: bch2_key_sort_fix_overlapping → 单个 bset
        let mut merged_entries: Vec<BtreeEntry> = Vec::new();
        let mut max_seq = 0u64;
        for bset_data in &all_bsets {
            if let Some(bset) = bset_from_bytes(bset_data) {
                max_seq = max_seq.max(bset.seq);
                for entry in bset.entries {
                    let pos = entry.pos;
                    if !merged_entries.iter().any(|e| e.pos == pos) {
                        merged_entries.push(entry);
                    }
                }
            }
        }
        merged_entries.sort_by(|a, b| a.pos.cmp(&b.pos));

        // 创建节点：nsets = 1，所有键在 bset[0]
        let mut node = Self::new(0, crate::btree::types::BtreeId::from_u8(0));
        bch2_bset_init_first(&mut node.set[0], max_seq + 1);
        for entry in merged_entries {
            node.set[0].bset.entries.push(entry);
        }
        rebuild_rw_aux(&mut node.set[0]);
        node.key_count = node.set[0].bset.entries.len() as u32;

        // from_bytes 后只有一个 bset(set[0])，它来自磁盘数据，视为已写入
        node.written = 1;

        Some(node)
    }

    /// 将整个节点序列化为磁盘格式（等价于 to_bytes）
    pub fn to_disk_bytes(&self) -> Vec<u8> {
        self.to_bytes()
    }

    /// 从磁盘格式反序列化（等价于 from_bytes）
    pub fn from_disk_bytes(data: &[u8]) -> Option<Self> {
        Self::from_bytes(data)
    }

    /// 合并所有 bset 后序列化节点数据
    ///
    /// 调用 merge_bsets 将多 bset 合并为一层，然后序列化。
    /// 返回的字节可直接写入设备 btree node 块。
    /// 当序列化数据超过 size 时返回 CowNeeded。
    pub fn compact_and_serialize(&mut self) -> Result<Vec<u8>, StorageError> {
        self.merge_bsets()?;
        let bytes = self.to_bytes();
        if bytes.len() > self.size as usize {
            return Err(StorageError::CowNeeded(format!(
                "compact node: {} bytes > size {}",
                bytes.len(),
                self.size
            )));
        }
        Ok(bytes)
    }

    /// 检查序列化数据是否适合 btree node 块
    pub fn fits_in_node(&self, serialized_len: usize) -> bool {
        serialized_len <= self.size as usize
    }

    /// 将节点内所有 BtreePtr 的 offset 从 child_idx 重写为 disk_offset
    ///
    /// 写盘前调用：用实际的磁盘偏移替换内存中的子节点索引。
    /// 返回旧值列表，写盘后可调用 `restore_ptr_offsets` 恢复。
    pub fn rewrite_ptrs_for_write(&mut self, child_offsets: &[u64]) -> Vec<(Bpos, u64)> {
        let mut saved = Vec::new();
        for i in 0..self.nsets as usize {
            for entry in &mut self.set[i].bset.entries {
                if entry.entry_type != ENTRY_TYPE_BTREE_PTR {
                    continue;
                }
                if let Some(ptr) = BtreePtr::from_bytes(&entry.payload) {
                    let idx = ptr.offset as usize;
                    if idx < child_offsets.len() && child_offsets[idx] != 0 {
                        saved.push((entry.pos, ptr.offset));
                        let new_ptr = BtreePtr {
                            offset: child_offsets[idx],
                            child_level: ptr.child_level,
                        };
                        entry.payload = new_ptr.to_bytes();
                    }
                }
            }
        }
        saved
    }

    /// 恢复 write_ptrs 时保存的旧 offset 值
    pub fn restore_ptr_offsets(&mut self, saved: &[(Bpos, u64)]) {
        if saved.is_empty() {
            return;
        }
        for i in 0..self.nsets as usize {
            for entry in &mut self.set[i].bset.entries {
                if entry.entry_type != ENTRY_TYPE_BTREE_PTR {
                    continue;
                }
                if let Some((_, old_off)) = saved.iter().find(|(p, _)| *p == entry.pos) {
                    if let Some(ptr) = BtreePtr::from_bytes(&entry.payload) {
                        let new_ptr = BtreePtr {
                            offset: *old_off,
                            child_level: ptr.child_level,
                        };
                        entry.payload = new_ptr.to_bytes();
                    }
                }
            }
        }
    }

    /// 检查当前节点的完整序列化大小是否超过 NODE_SIZE
    pub fn exceeds_node_size(&self) -> bool {
        self.to_bytes().len() > self.size as usize
    }
}

// ═══════════════════════════════════════════════════════════════
// Debug + Default
// ═══════════════════════════════════════════════════════════════

impl std::fmt::Debug for BtreeNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BtreeNode")
            .field("level", &self.level)
            .field("btree_id", &self.btree_id)
            .field("key_count", &self.key_count)
            .field("nsets", &self.nsets)
            .field("cached", &self.cached)
            .finish()
    }
}

impl Default for BtreeNode {
    fn default() -> Self {
        Self::new_leaf(BtreeId::from_u8(0))
    }
}

// ═══════════════════════════════════════════════════════════════
// 测试
// ═══════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use crate::btree::bset::BSET_ENTRY_LIMIT;
    use crate::btree::key::{Bpos, BtreeEntry};
    use crate::btree::node::BtreeNode;
    use crate::btree::types::{BtreeId, NODE_SIZE};
    use crate::types::StorageError;

    fn make_leaf() -> BtreeNode {
        BtreeNode::new_leaf(BtreeId::from_u8(1))
    }

    #[test]
    fn test_new_node_has_written_zero() {
        let node = make_leaf();
        assert_eq!(node.written, 0, "新节点 written 应为 0");
        assert_eq!(node.size, NODE_SIZE, "新节点 size 应为 NODE_SIZE");
        assert!(!node.bset_written(0), "新节点 bset[0] 不应被视为已写入");
    }

    #[test]
    fn would_fit_entries_rejects_serialized_node_overflow() {
        let mut node = make_leaf();
        for offset in 0..2_600 {
            node.set[0]
                .bset
                .insert(BtreeEntry {
                    btree_type: 1,
                    level: 0,
                    entry_type: 0,
                    pos: Bpos { inode: 0, offset, snapshot: 0 },
                    payload: vec![offset as u8; 16],
                })
                .unwrap();
        }
        assert!(node.to_bytes().len() > NODE_SIZE as usize);
        let pending = [BtreeEntry {
            btree_type: 1,
            level: 0,
            entry_type: 0,
            pos: Bpos { inode: 0, offset: 2_600, snapshot: 0 },
            payload: vec![0; 16],
        }];
        assert!(!node.would_fit_entries(&pending));
    }

    #[test]
    fn test_from_bytes_sets_written() {
        let mut node = make_leaf();
        let entry = BtreeEntry {
            btree_type: 1,
            level: 0,
            entry_type: 0,
            pos: Bpos {
                inode: 0,
                offset: 100,
                snapshot: 0,
            },
            payload: vec![42],
        };
        node.insert_key(entry.clone()).unwrap();

        let bytes = node.to_bytes();
        let restored = BtreeNode::from_bytes(&bytes).unwrap();

        assert_eq!(restored.nsets, 1, "从磁盘读取后 nsets=1");
        assert!(restored.written > 0, "从磁盘读取后 written > 0");
        assert!(restored.bset_written(0), "bset[0] 应标记为已写入");
        assert_eq!(restored.total_key_count(), 1);
        assert_eq!(
            restored
                .lookup(&Bpos {
                    inode: 0,
                    offset: 100,
                    snapshot: 0
                })
                .unwrap()
                .payload,
            vec![42]
        );
    }

    #[test]
    fn test_from_bytes_merges_multi_bset() {
        let mut node = make_leaf();
        let total_keys = 2000usize;
        // 插入足够多的键 → 自动触发多 bset 创建
        for i in 0..total_keys {
            let entry = BtreeEntry {
                btree_type: 1,
                level: 0,
                entry_type: 0,
                pos: Bpos {
                    inode: 0,
                    offset: i as u64,
                    snapshot: 0,
                },
                payload: vec![i as u8],
            };
            node.insert_key(entry).unwrap();
        }
        // 应有多个 bset（自动旋转）
        assert!(node.nsets >= 2, "应有至少 2 个 bset");

        // 序列化后反序列化（模拟磁盘读取）
        let bytes = node.to_bytes();
        let restored = BtreeNode::from_bytes(&bytes).unwrap();

        // 从磁盘读取会 merge 到 nsets=1
        assert_eq!(restored.nsets, 1, "磁盘读取后应合并为 1 个 bset");
        assert_eq!(restored.written, 1, "written 应 = 1");
        assert!(restored.bset_written(0), "bset[0] 应已写入");
        assert_eq!(restored.total_key_count() as usize, total_keys);

        // 验证所有键可查
        for i in 0..total_keys {
            let entry = restored.lookup(&Bpos {
                inode: 0,
                offset: i as u64,
                snapshot: 0,
            });
            assert!(entry.is_some(), "key {} should exist", i);
            assert_eq!(entry.unwrap().payload, vec![i as u8]);
        }
    }

    #[test]
    fn test_want_new_bset_after_disk_read() {
        let mut node = make_leaf();
        let entry = BtreeEntry {
            btree_type: 1,
            level: 0,
            entry_type: 0,
            pos: Bpos {
                inode: 0,
                offset: 200,
                snapshot: 0,
            },
            payload: vec![99],
        };
        node.insert_key(entry).unwrap();

        let bytes = node.to_bytes();
        let mut restored = BtreeNode::from_bytes(&bytes).unwrap();

        // 读取后 bset[0] 已写入 → want_new_bset 应为 true
        assert!(restored.bset_written(0), "从磁盘读取后 bset[0] 已写入");
        assert!(restored.want_new_bset(), "应需要新 bset");

        // prep_for_insert 应创建新 bset
        restored.prep_for_insert(64).unwrap();
        assert_eq!(restored.nsets, 2, "应创建新 bset");
        assert!(!restored.bset_written(1), "新 bset 不应已写入");
    }

    #[test]
    fn test_compact_and_serialize() {
        let mut node = make_leaf();
        let total_keys = 500usize;
        // 插入足够多的键 → 自动创建多 bset（JSON 序列化较膨胀，用 500 条保证不超过 NODE_SIZE）
        for i in 0..total_keys {
            let entry = BtreeEntry {
                btree_type: 1,
                level: 0,
                entry_type: 0,
                pos: Bpos {
                    inode: 0,
                    offset: i as u64,
                    snapshot: 0,
                },
                payload: vec![(i % 256) as u8],
            };
            node.insert_key(entry).unwrap();
        }
        assert!(node.nsets >= 2, "应有多个 bset");

        // compact_and_serialize: 合并到单 bset
        let serialized = node.compact_and_serialize().unwrap();
        assert!(
            node.fits_in_node(serialized.len()),
            "合并后应适合 NODE_SIZE"
        );

        let restored = BtreeNode::from_bytes(&serialized).unwrap();
        assert_eq!(restored.nsets, 1, "compact 后应为单 bset");
        assert_eq!(restored.total_key_count() as usize, total_keys);
    }

    #[test]
    fn test_btree_node_full_error() {
        let mut node = make_leaf();
        // 设置一个极小 size 模拟空间不足
        node.size = 64;

        let entry = BtreeEntry {
            btree_type: 1,
            level: 0,
            entry_type: 0,
            pos: Bpos {
                inode: 0,
                offset: 1,
                snapshot: 0,
            },
            payload: vec![0u8; 128],
        };

        match node.insert_key(entry) {
            Err(StorageError::BtreeNodeFull) => { /* 正确 */ }
            other => panic!("期望 BtreeNodeFull, 得到 {:?}", other),
        }
    }

    #[test]
    fn test_to_bytes_from_bytes_edge_cases() {
        // 空数据
        assert!(BtreeNode::from_bytes(&[]).is_none());
        assert!(BtreeNode::from_bytes(&[0u8; 3]).is_none());

        // nsets=0 的无效数据
        assert!(BtreeNode::from_bytes(&[0, 0, 0, 0]).is_none());

        // nsets=1 但无 bset 数据的截断
        assert!(BtreeNode::from_bytes(&[1, 0, 0, 0]).is_none());
    }

    #[test]
    fn test_disk_fields_preserved() {
        let mut node = make_leaf();
        node.disk_offset = 4096;
        node.disk_size = 1024;

        // disk fields 不应被序列化（skip serde）
        let bytes = node.to_bytes();
        let restored = BtreeNode::from_bytes(&bytes).unwrap();

        // disk 字段恢复为默认值 0
        assert_eq!(restored.disk_offset, 0, "disk_offset 不应被序列化");
        assert_eq!(restored.disk_size, 0, "disk_size 不应被序列化");
    }

    #[test]
    fn test_compact_and_written_boundary() {
        let mut node = make_leaf();
        // 插入一些键，填满第一个 bset
        for i in 0..BSET_ENTRY_LIMIT {
            let entry = BtreeEntry {
                btree_type: 1,
                level: 0,
                entry_type: 0,
                pos: Bpos {
                    inode: 0,
                    offset: i as u64,
                    snapshot: 0,
                },
                payload: vec![i as u8],
            };
            node.insert_key(entry).unwrap();
        }

        // 旋转到第二个 bset
        node.try_rotate_bset().unwrap();

        // 写入第二个 bset
        for i in 0..10 {
            let entry = BtreeEntry {
                btree_type: 1,
                level: 0,
                entry_type: 0,
                pos: Bpos {
                    inode: 0,
                    offset: (BSET_ENTRY_LIMIT + i) as u64,
                    snapshot: 0,
                },
                payload: vec![(BSET_ENTRY_LIMIT + i) as u8],
            };
            node.insert_key(entry).unwrap();
        }

        // compact 合并所有 bset
        node.compact().unwrap();

        assert_eq!(node.nsets, 1, "compact 后 nsets=1");
        assert_eq!(
            node.total_key_count() as usize,
            BSET_ENTRY_LIMIT + 10,
            "compact 后应保留所有键"
        );
    }

    #[test]
    fn try_merge_uses_serialized_capacity_instead_of_key_count() {
        let mut left = make_leaf();
        let mut right = make_leaf();
        left.size = 64 * 1024;
        right.size = 64 * 1024;
        for i in 0..32u64 {
            left.insert_key(BtreeEntry {
                btree_type: 1,
                level: 0,
                entry_type: 0,
                pos: Bpos {
                    inode: 0,
                    offset: i,
                    snapshot: 0,
                },
                payload: vec![i as u8; 32],
            })
            .unwrap();
            right.insert_key(BtreeEntry {
                btree_type: 1,
                level: 0,
                entry_type: 0,
                pos: Bpos {
                    inode: 1,
                    offset: i,
                    snapshot: 0,
                },
                payload: vec![i as u8; 32],
            })
            .unwrap();
        }
        let merged = left.try_merge(&right).expect("small nodes should merge");
        assert_eq!(merged.total_key_count(), 64);
        assert!(!merged.exceeds_node_size());

        let mut oversized = make_leaf();
        for i in 0..32u64 {
            oversized
                .insert_key(BtreeEntry {
                    btree_type: 1,
                    level: 0,
                    entry_type: 0,
                    pos: Bpos {
                        inode: 2,
                        offset: i,
                        snapshot: 0,
                    },
                    payload: vec![i as u8; 2048],
                })
                .unwrap();
        }
        assert!(left.try_merge(&oversized).is_none());
    }
}
