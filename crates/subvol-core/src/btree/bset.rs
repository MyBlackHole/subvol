//! Bset — B-tree 节点内的键集合
//!
//! 对应 bcachefs `struct bset` / `struct bset_tree` / `struct btree_node_iter`。
//! btree node 包含多个 bset 层，键分散在各层中。
//! 搜索时从最新层向下查找，插入只操作最新层。

use serde::{Deserialize, Serialize};

use crate::btree::key::{Bpos, BtreeEntry};
use crate::types::StorageError;

// ═══════════════════════════════════════════════════════════════
// Constants
// ═══════════════════════════════════════════════════════════════

pub const MAX_BSETS: usize = 3;
pub const BSET_CACHELINE: usize = 256;
/// 单 bset 最大条数
///
/// 按 NODE_SIZE(256KB) 计算：~512 条 × ~90 字节 ≈ 46KB，
/// 3 个 bset 合并后 ~138KB < 256KB，留余量给 header 和其他字段。
pub const BSET_ENTRY_LIMIT: usize = 512;

// ═══════════════════════════════════════════════════════════════
// Bset — 单层键集合
// ═══════════════════════════════════════════════════════════════

/// Bset — btree node 内的一层键集合
///
/// 对应 bcachefs `struct bset` (bcachefs_format.h:1905)
/// 包含一组按 Bpos 排序的 BtreeEntry。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bset {
    /// 序列号（对应 bset.seq）
    pub seq: u64,
    /// 最高 journal 序列号（对应 bset.journal_seq）
    pub journal_seq: u64,
    /// 标志位（对应 bset.flags）
    pub flags: u32,
    /// 版本号（对应 bset.version）
    pub version: u16,
    /// 排序的键列表
    pub entries: Vec<BtreeEntry>,
}

impl Bset {
    pub fn new(seq: u64) -> Self {
        Self {
            seq,
            journal_seq: 0,
            flags: 0,
            version: 0,
            entries: Vec::new(),
        }
    }

    pub fn with_capacity(seq: u64, cap: usize) -> Self {
        Self {
            seq,
            journal_seq: 0,
            flags: 0,
            version: 0,
            entries: Vec::with_capacity(cap),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 查找 pos 在 bset 中的插入位置/位置
    pub fn search_idx(&self, pos: &Bpos) -> Result<usize, usize> {
        self.entries.binary_search_by(|e| e.pos.cmp(pos))
    }

    /// 获取指定位置的键
    pub fn get(&self, pos: &Bpos) -> Option<&BtreeEntry> {
        let idx = self.entries.binary_search_by(|e| e.pos.cmp(pos)).ok()?;
        Some(&self.entries[idx])
    }

    pub fn get_mut(&mut self, pos: &Bpos) -> Option<&mut BtreeEntry> {
        let idx = self.entries.binary_search_by(|e| e.pos.cmp(pos)).ok()?;
        Some(&mut self.entries[idx])
    }

    /// 插入键（维持排序），返回 Ok(()) 或 Err(NoMem)
    pub fn insert(&mut self, entry: BtreeEntry) -> Result<(), StorageError> {
        let pos = entry.pos;
        match self.entries.binary_search_by(|e| e.pos.cmp(&pos)) {
            Ok(idx) => {
                self.entries[idx] = entry;
            }
            Err(idx) => {
                self.entries.insert(idx, entry);
            }
        }
        Ok(())
    }

    /// 删除键，返回是否找到并删除
    pub fn delete(&mut self, pos: &Bpos) -> bool {
        match self.entries.binary_search_by(|e| e.pos.cmp(pos)) {
            Ok(idx) => {
                self.entries.remove(idx);
                true
            }
            Err(_) => false,
        }
    }

    /// 清空
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// 查找严格大于 pos 的最小键
    ///
    /// 对应 bcachefs `bch2_btree_node_iter_peek` + `bkey_successor`
    pub fn search_successor(&self, pos: &Bpos) -> Option<&BtreeEntry> {
        match self.entries.binary_search_by(|e| e.pos.cmp(pos)) {
            Ok(i) => self.entries.get(i + 1),
            Err(i) => self.entries.get(i),
        }
    }

    /// 查找严格小于 pos 的最大键
    ///
    /// 对应 bcachefs `bkey_predecessor`
    pub fn search_predecessor(&self, pos: &Bpos) -> Option<&BtreeEntry> {
        match self.entries.binary_search_by(|e| e.pos.cmp(pos)) {
            Ok(i) if i > 0 => self.entries.get(i - 1),
            Err(i) if i > 0 => self.entries.get(i - 1),
            _ => None,
        }
    }

    /// 获取第一个键
    pub fn first_entry(&self) -> Option<&BtreeEntry> {
        self.entries.first()
    }

    /// 获取最后一个键
    pub fn last_entry(&self) -> Option<&BtreeEntry> {
        self.entries.last()
    }
}

// ═══════════════════════════════════════════════════════════════
// BsetAuxTreeType — 辅助搜索树类型
// ═══════════════════════════════════════════════════════════════

/// 辅助搜索树类型
///
/// 对应 bcachefs `enum bset_aux_tree_type`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BsetAuxTreeType {
    /// 无辅助树（bset 太小）
    NoAuxTree,
    /// 只读 Eytzinger 二叉搜索树
    RoAuxTree,
    /// 读写辅助索引（当前写入层）
    RwAuxTree,
}

// ═══════════════════════════════════════════════════════════════
// RwAuxEntry — 读写辅助索引条目
// ═══════════════════════════════════════════════════════════════

/// 读写辅助索引条目
///
/// 对应 bcachefs `struct rw_aux_tree`
/// 每 BSET_CACHELINE 字节一个索引点，记录该位置的第一个键
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RwAuxEntry {
    /// 在 entries 中的偏移
    pub entry_idx: u16,
    /// 该位置第一个键的 Bpos
    pub pos: Bpos,
}

// ═══════════════════════════════════════════════════════════════
// BsetTree — bset 在内存中的辅助搜索结构
// ═══════════════════════════════════════════════════════════════

/// BsetTree — bset 在内存中的辅助搜索结构
///
/// 对应 bcachefs `struct bset_tree` (types.h:94)
/// 包含 bset 数据本身 + 辅助索引，用于加速键搜索。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BsetTree {
    /// bset 数据
    pub bset: Bset,
    /// 辅助搜索树类型
    pub aux_type: BsetAuxTreeType,
    /// 读写辅助索引（RwAuxTree 模式时使用）
    pub rw_aux: Vec<RwAuxEntry>,
    /// 数据偏移（对应 data_offset，简化版未使用）
    pub data_offset: u16,
    /// 结束偏移（对应 end_offset）
    pub end_offset: u16,
}

impl BsetTree {
    pub fn new(seq: u64) -> Self {
        Self {
            bset: Bset::new(seq),
            aux_type: BsetAuxTreeType::NoAuxTree,
            rw_aux: Vec::new(),
            data_offset: 0,
            end_offset: 0,
        }
    }

    pub fn with_capacity(seq: u64, cap: usize) -> Self {
        Self {
            bset: Bset::with_capacity(seq, cap),
            aux_type: BsetAuxTreeType::NoAuxTree,
            rw_aux: Vec::new(),
            data_offset: 0,
            end_offset: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.bset.is_empty()
    }

    pub fn len(&self) -> usize {
        self.bset.len()
    }
}

// ═══════════════════════════════════════════════════════════════
// BtreeNodeIter — btree 节点内的多 bset 迭代器
// ═══════════════════════════════════════════════════════════════

/// BtreeNodeIter — 多 bset 迭代器
///
/// 对应 bcachefs `struct btree_node_iter` (types.h:441)
/// 同时在多个 bset 中推进，每次返回最小的 key。
#[derive(Debug, Clone)]
pub struct BtreeNodeIter {
    /// 每个 bset 的当前迭代索引
    pub indices: [u16; MAX_BSETS],
    /// 当前迭代器有效集数
    pub nr: u8,
}

impl BtreeNodeIter {
    pub fn new() -> Self {
        Self {
            indices: [0; MAX_BSETS],
            nr: 0,
        }
    }

    /// 初始化迭代器，定位到所有 bset 中 >= pos 的第一个键
    pub fn init(&mut self, sets: &[BsetTree; MAX_BSETS], nsets: u8, pos: &Bpos) {
        self.nr = nsets;
        for i in 0..nsets as usize {
            let set = &sets[i];
            match set.bset.search_idx(pos) {
                Ok(idx) => self.indices[i] = idx as u16,
                Err(idx) => self.indices[i] = idx as u16,
            }
        }
    }

    /// 获取当前最小的 key（所有 bset 中 indices 位置最小的）
    pub fn peek<'a>(&self, sets: &'a [BsetTree; MAX_BSETS]) -> Option<(u8, &'a BtreeEntry)> {
        let mut best: Option<(u8, &BtreeEntry)> = None;
        for i in 0..self.nr as usize {
            let idx = self.indices[i] as usize;
            if idx < sets[i].bset.entries.len() {
                let entry = &sets[i].bset.entries[idx];
                match best {
                    None => best = Some((i as u8, entry)),
                    Some((_, ref best_entry)) => {
                        if entry.pos < best_entry.pos {
                            best = Some((i as u8, entry));
                        }
                    }
                }
            }
        }
        best
    }

    /// 推进到下一个 key
    pub fn advance(&mut self, set_idx: u8) {
        let i = set_idx as usize;
        self.indices[i] += 1;
    }

    /// 返回当前最小的 key 并推进游标（跳过 BtreePtr 条目）
    ///
    /// 对应 bcachefs `bch2_btree_node_iter_next` (iter.c:506)
    /// `skip_ptrs=true` 时跳过 ENTRY_TYPE_BTREE_PTR 条目。
    pub fn next<'a>(
        &mut self,
        sets: &'a [BsetTree; MAX_BSETS],
        skip_ptrs: bool,
    ) -> Option<(u8, &'a BtreeEntry)> {
        loop {
            let (set_idx, entry) = self.peek(sets)?;
            self.advance(set_idx);
            if !skip_ptrs || entry.entry_type != crate::data::extents_format::ENTRY_TYPE_BTREE_PTR {
                return Some((set_idx, entry));
            }
        }
    }

    /// 重置迭代器：反向查找 < pos 的最大键（严格小于，用于 prev 迭代）
    ///
    /// 对应 bcachefs `bch2_btree_node_iter_init` + 反向扫
    pub fn init_reverse(&mut self, sets: &[BsetTree; MAX_BSETS], nsets: u8, pos: &Bpos) {
        self.nr = nsets;
        for i in 0..nsets as usize {
            let set = &sets[i];
            match set.bset.search_idx(pos) {
                // Ok(0) 表示最小键 == pos，无 < pos 的键 → 耗尽
                Ok(0) => self.indices[i] = set.bset.entries.len() as u16,
                // Ok(idx) 表示精确命中，指向 idx-1（严格小于 pos）
                Ok(idx) => self.indices[i] = (idx - 1) as u16,
                // Err(0) 表示所有键 > pos，无 < pos 的键 → 耗尽
                Err(0) => self.indices[i] = set.bset.entries.len() as u16,
                // Err(idx) 表示插入位置在 idx，< pos 的最大键在 idx-1
                Err(idx) => self.indices[i] = (idx - 1) as u16,
            }
        }
    }

    /// 反向 next（找当前游标中最大的键并回退）
    pub fn prev<'a>(
        &mut self,
        sets: &'a [BsetTree; MAX_BSETS],
        skip_ptrs: bool,
    ) -> Option<(u8, &'a BtreeEntry)> {
        loop {
            let (set_idx, entry) = self.peek_rev(sets)?;
            // 回退游标（指向更小的键）
            if self.indices[set_idx as usize] > 0 {
                self.indices[set_idx as usize] -= 1;
            } else {
                // 当前 bset 已到起点
                self.indices[set_idx as usize] = u16::MAX; // 标记耗尽
            }
            if !skip_ptrs || entry.entry_type != crate::data::extents_format::ENTRY_TYPE_BTREE_PTR {
                return Some((set_idx, entry));
            }
        }
    }

    /// 反向 peek：找当前游标中最大的键（所有 bset 中 indices 位置最大的）
    fn peek_rev<'a>(&self, sets: &'a [BsetTree; MAX_BSETS]) -> Option<(u8, &'a BtreeEntry)> {
        let mut best: Option<(u8, &BtreeEntry)> = None;
        for i in 0..self.nr as usize {
            let idx = self.indices[i] as usize;
            if idx < sets[i].bset.entries.len() {
                let entry = &sets[i].bset.entries[idx];
                match best {
                    None => best = Some((i as u8, entry)),
                    Some((_, ref best_entry)) => {
                        if entry.pos > best_entry.pos {
                            best = Some((i as u8, entry));
                        }
                    }
                }
            }
        }
        best
    }
}

impl Default for BtreeNodeIter {
    fn default() -> Self {
        Self::new()
    }
}

// ═══════════════════════════════════════════════════════════════
// Bset 搜索函数
// ═══════════════════════════════════════════════════════════════

/// 在单个 bset 中搜索 key
///
/// 对应 bcachefs `bch2_bset_search()`
/// 返回值表示找到的位置（Ok）或应插入的位置（Err）。
pub fn bch2_bset_search(bset: &Bset, pos: &Bpos) -> Result<usize, usize> {
    bset.search_idx(pos)
}

/// 在 btree node 的所有 bset 中搜索 key
///
/// 返回 (set_idx, entry)，从最新 bset 开始查找。
pub fn bch2_bset_search_all<'a>(
    sets: &'a [BsetTree; MAX_BSETS],
    nsets: u8,
    pos: &Bpos,
) -> Option<(u8, &'a BtreeEntry)> {
    // 从最新层（nsets-1）向下查找
    for i in (0..nsets as usize).rev() {
        if let Ok(idx) = sets[i].bset.search_idx(pos) {
            return Some((i as u8, &sets[i].bset.entries[idx]));
        }
    }
    None
}

/// 向最后一个 bset 插入 key
///
/// 对应 bcachefs `bch2_bset_insert()`
/// 在 bcachefs 中只能插入到最后一个 bset（bset_tree_last(b)）。
pub fn bch2_bset_insert(set: &mut BsetTree, entry: BtreeEntry) -> Result<(), StorageError> {
    // 更新辅助索引（简化版：清空并重建）
    let old_len = set.bset.entries.len();
    set.bset.insert(entry)?;
    // 如果条目数变了，重建 rw_aux
    if set.bset.entries.len() != old_len {
        rebuild_rw_aux(set);
    }
    Ok(())
}

/// 从最后一个 bset 删除 key
///
/// 对应 bcachefs `bch2_bset_delete()`
pub fn bch2_bset_delete(set: &mut BsetTree, pos: &Bpos) -> bool {
    let old_len = set.bset.entries.len();
    let found = set.bset.delete(pos);
    if found && set.bset.entries.len() != old_len {
        rebuild_rw_aux(set);
    }
    found
}

// ═══════════════════════════════════════════════════════════════
// 辅助索引
// ═══════════════════════════════════════════════════════════════

/// 重建 rw_aux 辅助索引
///
/// 对应 bcachefs `bch2_bset_build_aux_tree()`
pub fn rebuild_rw_aux(set: &mut BsetTree) {
    let entries = &set.bset.entries;
    if entries.is_empty() {
        set.rw_aux.clear();
        set.aux_type = BsetAuxTreeType::NoAuxTree;
        return;
    }

    // 每 BSET_CACHELINE 字节的条目数（近似：每个 entry 平均大小未知，按 entry 数量估算）
    let step = (BSET_CACHELINE / std::mem::size_of::<Bpos>()).max(1);
    let mut aux = Vec::new();

    let mut i = 0;
    while i < entries.len() {
        aux.push(RwAuxEntry {
            entry_idx: i as u16,
            pos: entries[i].pos,
        });
        i += step;
    }

    set.rw_aux = aux;
    if entries.len() > 256 {
        set.aux_type = BsetAuxTreeType::RwAuxTree;
    } else if entries.len() > 64 {
        set.aux_type = BsetAuxTreeType::RoAuxTree;
    } else {
        set.aux_type = BsetAuxTreeType::NoAuxTree;
    }
}

/// 通过 rw_aux 在 bset 中搜索
///
/// 对应 bcachefs `bset_search_write_set()`
pub fn bset_search_write_set(set: &BsetTree, pos: &Bpos) -> usize {
    let aux = &set.rw_aux;
    if aux.is_empty() {
        return 0;
    }
    // 在辅助索引上二分查找
    let idx = aux.partition_point(|a| a.pos < *pos);
    if idx == 0 {
        return 0;
    }
    // 返回辅助索引指向的 entry_idx
    aux[idx - 1].entry_idx as usize
}

// ═══════════════════════════════════════════════════════════════
// Bset 初始化
// ═══════════════════════════════════════════════════════════════

/// 初始化第一个 bset
///
/// 对应 bcachefs `bch2_bset_init_first()`
pub fn bch2_bset_init_first(set: &mut BsetTree, seq: u64) {
    set.bset = Bset::new(seq);
    set.aux_type = BsetAuxTreeType::NoAuxTree;
    set.rw_aux.clear();
}

/// 初始化下一个 bset（新增一层）
///
/// 对应 bcachefs `bch2_bset_init_next()`
/// 当当前层满时，创建新层，后续插入到新层。
pub fn bch2_bset_init_next(set: &mut BsetTree, seq: u64, capacity: usize) {
    set.bset = Bset::with_capacity(seq, capacity);
    set.aux_type = BsetAuxTreeType::RwAuxTree;
    set.rw_aux.clear();
}

pub fn bset_to_bytes(bset: &Bset) -> Vec<u8> {
    serde_json::to_vec(bset).unwrap_or_default()
}

pub fn bset_from_bytes(data: &[u8]) -> Option<Bset> {
    serde_json::from_slice(data).ok()
}
