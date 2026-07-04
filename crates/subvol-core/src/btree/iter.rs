//! BtreeIter — bcachefs 对齐的 btree 遍历器
//!
//! 核心设计（对应 bcachefs `btree_iter` + `btree_path`）：
//!
//! ## 临时 key buffer
//!
//! `peek_max` 等操作需要临时 unpack 空间来存储上限范围内的 key。
//! 使用 `BTREE_ITER_BUF_GRANULARITY = 2048` 作为 buffer 粒度，
//! 对齐 bcachefs `bkey_buf.h` 中 `kmalloc(2048)` 的 heap 分配尺寸。

/// bcachefs 对齐的 btree_iter 临时 key buffer 粒度
///
/// 对应 bcachefs `bkey_buf` 的 heap 分配尺寸（`bkey_buf.h:20`）：
/// `kmalloc_noprof(2048, GFP_KERNEL|__GFP_NOFAIL)`。
/// 用于 peek_max 等需要临时 unpack/重组 key 的操作。
/// 2048 字节可容纳 ~256 字段的极端 key，远超正常 entry 大小。
pub const BTREE_ITER_BUF_GRANULARITY: usize = 2048;
//
// 1. **路径缓存**: 从 root 到 leaf 的完整路径存储在 `path` 数组中，
//    每层级包含节点引用 + 锁状态 + entry 偏移。
// 2. **三级锁**: Read → Intent → Write，通过 SixLock 升级降级。
// 3. **Restart 机制**: 当锁竞争导致路径失效时，自动从 root 重新遍历。
// 4. **intent lock 语义**: 写路径先拿 intent（不阻塞读），再升级到 write。

use std::collections::HashMap;
use std::ptr::NonNull;
use std::sync::Arc;

use bitflags::bitflags;

use crate::btree::key::BtreeEntry;
use crate::btree::key::{BchVal, BkeyPacked, BtreeKey, ExtentValue, KeyType, KeyValue};
use crate::btree::node::{
    bch2_btree_node_iter_advance, bch2_btree_node_iter_init_from_start, bch2_btree_node_iter_peek,
    BtreeNode, BtreeNodeIter,
};
use crate::btree::types::{
    BtreeNodeLockedType, BtreePath, BtreePathError, BtreePathLevel, BtreePathNode, BtreeRoot,
    NodeCache, PathIdx, BTREE_MAX_DEPTH, PATH_IDX_INVALID, ROOT_CACHE_ADDR,
};
use crate::btree::Bpos;
use crate::btree::BtreeId;
use crate::btree::BtreeTrans;
use crate::BchVol;

/// 遍历标志
#[derive(Debug, Clone, Copy)]
pub struct IterFlags {
    /// 是否允许 intent 锁（写路径需要）
    pub intent: bool,
    /// 遍历方向（true = 正向）
    pub forward: bool,
    /// 是否包含 journal 数据
    pub with_journal: bool,
    /// 是否使用 key_cache 路径 — 对应 bcachefs `BTREE_ITER_cached`
    pub cached: bool,
    /// 是否跳过 preserve — 对应 bcachefs `BTREE_ITER_nopreserve`
    pub nopreserve: bool,
}

impl Default for IterFlags {
    fn default() -> Self {
        Self {
            intent: false,
            forward: true,
            with_journal: true,
            cached: false,
            nopreserve: false,
        }
    }
}

bitflags! {
    /// 对应本地 bcachefs `fs/btree/iter.rs:514-535` 的
    /// `UpdateTriggerFlags`；位值来自本地 `fs/btree/types.h:448-525` 中
    /// iterator、str-hash、update、trigger flags 的统一 enum 位序。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct UpdateTriggerFlags: u32 {
        const INTERNAL_SNAPSHOT_NODE   = 1 << 18;
        const NOJOURNAL                = 1 << 19;
        const KEY_CACHE_RECLAIM        = 1 << 20;
        const NORUN                    = 1 << 21;
        const TRANSACTIONAL            = 1 << 22;
        const ATOMIC                   = 1 << 23;
        const GC                       = 1 << 24;
        const INSERT                   = 1 << 25;
        const OVERWRITE                = 1 << 26;
        const IS_DISCARD               = 1 << 27;
        const SET_NEEDS_RECONCILE_DONE = 1 << 28;
    }
}

/// B-tree 遍历器 — 对应 bcachefs `struct btree_iter`
///
/// 维护从 root 到 leaf 的完整路径，支持：
/// - 锁升级/降级（read → intent → write）
/// - 路径缓存重用（advance 只在 leaf 内移动，不重新遍历）
/// - Restart（检测到锁失效时从 root 重新开始）
///
/// bcachefs 字段对齐：
/// - pos ↔ struct bpos pos
/// - snapshot ↔ unsigned snapshot
/// - flags ↔ u16 flags (BTREE_ITER_intent 等位标志)
/// - path ↔ btree_path_idx_t path
#[derive(Debug)]
pub struct BtreeIter {
    /// 对应本地 bcachefs `iter->path`。
    pub path: PathIdx,
    /// 对应本地 bcachefs `iter->update_path`。
    pub update_path: PathIdx,
    /// 对应本地 bcachefs `iter->key_cache_path`。
    pub key_cache_path: PathIdx,
    /// 对应本地 `iter->trans`；所有 path 访问都先经过
    /// transaction path pool，再用 `path` 索引解析。
    paths_ptr: NonNull<Vec<Option<Box<BtreePath>>>>,
    /// 当前位置（leaf 中的 key）
    /// 对应 bcachefs `iter->pos`
    pub pos: BtreeKey,
    /// 遍历标志
    /// 对应 bcachefs `iter->flags` (BTREE_ITER_intent 等)
    pub flags: IterFlags,
    /// 是否发生过 restart
    pub had_restart: bool,
    /// 节点缓存（用于多级树中子节点查找和重启）
    pub cache: Arc<NodeCache>,
    /// 当前快照 ID（快照过滤用，0 = 无过滤）
    /// 对应 bcachefs `iter->snapshot`
    pub snapshot: u32,
    /// B-tree 类型
    /// 对应 bcachefs `iter->btree_id`
    pub btree_type: BtreeId,
    /// 快照可见性缓存：存活期为整个 iter 生命周期
    /// key: (iter_snapshot, key_snapshot) → is_ancestor
    /// 对应 bcachefs `trans->snapshot_visible`
    /// 消除同一遍历中重复的 Snapshots btree 查询
    snapshot_visible_cache: HashMap<(u32, u32), bool>,
}

// SAFETY: iterator 只随其独占拥有者 `BtreeTrans` 整体跨线程移动。
// `paths_ptr` 指向 transaction 内 Box 固定地址持有的 path pool。
// 不实现 Sync，禁止通过共享引用并发访问该可变 path pool。
unsafe impl Send for BtreeIter {}

impl BtreeIter {
    /// 对应本地 `btree_iter_path(trans, iter)`；不可缓存返回引用跨越 path 操作。
    fn btree_iter_path(&self) -> &BtreePath {
        unsafe {
            self.paths_ptr
                .as_ref()
                .get(self.path as usize)
                .and_then(Option::as_deref)
                .expect("iterator path is not allocated")
        }
    }

    /// `btree_iter_path()` 的 Rust 可变借用形式。
    fn btree_iter_path_mut(&mut self) -> &mut BtreePath {
        unsafe {
            self.paths_ptr
                .as_mut()
                .get_mut(self.path as usize)
                .and_then(Option::as_deref_mut)
                .expect("iterator path is not allocated")
        }
    }
    /// 从已有的路径级别创建 iter（避免树遍历）
    ///
    /// 用于路径复用：当 `paths[]` 池中已有匹配的路径时，通过克隆其 levels
    /// 创建新的 iter 而无需重新下降。
    pub fn from_existing(
        target: &BtreeKey,
        flags: IterFlags,
        cache: Arc<NodeCache>,
        btree_type: BtreeId,
        path: PathIdx,
        paths: &mut Vec<Option<Box<BtreePath>>>,
    ) -> Self {
        Self {
            path,
            update_path: PATH_IDX_INVALID,
            key_cache_path: PATH_IDX_INVALID,
            paths_ptr: NonNull::from(paths),
            pos: *target,
            flags,
            had_restart: false,
            cache,
            btree_type,
            snapshot: 0,
            snapshot_visible_cache: HashMap::new(),
        }
    }

    /// 创建一个新的 iter（初始化为指定位置）
    ///
    /// 对应 bcachefs `bch2_btree_iter_peek()` 中从 root 下降的逻辑：
    /// 1. 从 root 开始，lock_read 根节点
    /// 2. 对每个 internal 节点，二分查找目标 key 属于哪个 child
    /// 3. lock_read 子节点，unlock 父节点（或升级到 intent）
    /// 4. 下降到 leaf 后，在 leaf 内定位目标 key
    pub fn init_with_path(
        root: &BtreeRoot,
        target: &BtreeKey,
        flags: IterFlags,
        cache: &Arc<NodeCache>,
        btree_type: BtreeId,
        path_idx: PathIdx,
        paths: &mut Vec<Option<Box<BtreePath>>>,
    ) -> Self {
        let path_ref = paths[path_idx as usize]
            .as_deref_mut()
            .expect("iterator path is not allocated");
        let mut path: Vec<BtreePathLevel> = Vec::with_capacity(BTREE_MAX_DEPTH);

        // Step 1: 按 `__btree_lock_want()` 锁定 root。单层树中 root 就是
        // update path 的 leaf，intent iter 必须真正获取 intent 锁。
        let root_node = root.node.clone();
        if flags.intent && root.depth == 0 {
            root_node.lock.six_lock_intent();
        } else {
            root_node.lock.six_lock_read();
        }
        let mut root_pl = BtreePathLevel::new(root_node);
        root_pl.block_addr = ROOT_CACHE_ADDR;
        root_pl.locked_seq = root_pl.node.lock.six_lock_seq();
        path.push(root_pl);

        // Step 2: 逐级下降到 leaf
        let depth = root.depth;
        for level in (1..=depth).rev() {
            let parent = &path[path.len() - 1];
            let (child_addr, child_idx) = Self::find_child_node(&parent.node, target);
            let child = cache.get_or_create(child_addr, level - 1);

            // 备注：bcachefs 对齐 — 预取下一个兄弟节点
            // 在下降路径中，如果下一个兄弟节点还未缓存，发起异步预取
            if let Some(v) = Self::read_entry_by_global_idx(&parent.node, child_idx + 1) {
                let next_addr = v.paddr();
                cache.prefetch_node(next_addr, level - 1, btree_type);
            }

            // 对应 `__btree_lock_want(path, child_level)`：update path 的
            // leaf 获取 intent，其余下降层级获取 read。
            if flags.intent && level == 1 {
                child.lock.six_lock_intent();
            } else {
                child.lock.six_lock_read();
            }
            // 本地 `bch2_btree_node_get()` 在取得 child 前释放 parent 的
            // read lock；当前入口的 intent 只令 locks_want=1，因此只有
            // level 0 获取 intent，所有 parent 都是临时 read lock。
            parent.node.lock.six_unlock_read();
            if let Some(p) = path.last_mut() {
                p.lock_state = BtreeNodeLockedType::None;
            }

            let locked_seq = child.lock.six_lock_seq();
            let mut pl = BtreePathLevel::new(child);
            pl.block_addr = child_addr;
            pl.child_idx = child_idx;
            pl.locked_seq = locked_seq;
            path.push(pl);
        }

        // 最后一级是 leaf — 赋值正确的锁状态
        if let Some(leaf) = path.last_mut() {
            leaf.lock_state = if flags.intent {
                BtreeNodeLockedType::Intent
            } else {
                BtreeNodeLockedType::Read
            };
        }

        // Step 3: 在 leaf 的跨 bset 有序视图中定位 key。
        let mut best_global_off = 0u16;
        if let Some(leaf) = path.last_mut() {
            bch2_btree_node_iter_init_from_start(&mut leaf.iter, &leaf.node);
            let mut index = 1u16;
            while let Some(bytes) = bch2_btree_node_iter_peek(&mut leaf.iter, &leaf.node) {
                let offset = bytes.as_ptr() as usize - leaf.node.data.as_ptr() as usize;
                let (key, _) = leaf.node.read_packed_entry(offset);
                let cmp = if btree_type == crate::btree::BtreeId::Extents {
                    key.vaddr_cmp(target)
                } else {
                    key.cmp(target)
                };
                if cmp != std::cmp::Ordering::Less {
                    best_global_off = index;
                    break;
                }
                index += 1;
                bch2_btree_node_iter_advance(&mut leaf.iter, &leaf.node);
            }
        }
        path.last_mut().map(|l| l.offset = best_global_off);

        path_ref.levels = std::array::from_fn(|_| BtreePathNode::Error(BtreePathError::Init));
        for (level, path_level) in path.into_iter().rev().take(BTREE_MAX_DEPTH).enumerate() {
            path_ref.levels[level] = BtreePathNode::Node(path_level);
        }
        path_ref.pos = target.to_bpos();
        path_ref.level = 0;
        path_ref.nodes_locked = 0;
        for (level, node) in path_ref.levels.iter().enumerate() {
            if let BtreePathNode::Node(path_level) = node {
                path_ref.nodes_locked |= ((path_level.lock_state as i8 + 1) as u8) << (level << 1);
            }
        }

        let mut iter = Self {
            path: path_idx,
            update_path: PATH_IDX_INVALID,
            key_cache_path: PATH_IDX_INVALID,
            paths_ptr: NonNull::from(paths),
            pos: *target,
            flags,
            had_restart: false,
            cache: cache.clone(),
            snapshot: 0,
            btree_type,
            snapshot_visible_cache: HashMap::new(),
        };

        if best_global_off == 0 {
            iter.back_up_and_advance();
        }

        iter
    }

    fn current_leaf_key(&self) -> Option<BtreeKey> {
        self.peek_entry().map(|entry| entry.to_key_value().0)
    }

    fn set_pos_from_current_leaf(&mut self) -> bool {
        if let Some(key) = self.current_leaf_key() {
            self.pos = key;
            true
        } else {
            false
        }
    }

    /// 在 `init_with_path()` 基础上设置 `snapshot`，
    /// 使 `peek_visible()` 能调用 `bch2_snapshot_is_ancestor()` 过滤。
    pub fn init_with_snapshot_with_path(
        root: &BtreeRoot,
        target: &BtreeKey,
        flags: IterFlags,
        cache: &Arc<NodeCache>,
        snapshot_id: u32,
        btree_type: BtreeId,
        path_idx: PathIdx,
        paths: &mut Vec<Option<Box<BtreePath>>>,
    ) -> Self {
        let mut iter = Self::init_with_path(
            root, target, flags, cache, btree_type, path_idx, paths,
        );
        iter.snapshot = snapshot_id;
        iter
    }

    /// 创建范围感知的 iterator：定位到可能覆盖 target 的 key
    ///
    /// 对于范围 extent 查找使用：position 在第一个满足 `vaddr + size > target` 的 key
    /// （即可能覆盖 target 的 key），而非精确匹配。
    pub fn init_with_snapshot_peek_prev_with_path(
        root: &BtreeRoot,
        target: &BtreeKey,
        flags: IterFlags,
        cache: &Arc<NodeCache>,
        snapshot_id: u32,
        btree_type: BtreeId,
        path_idx: PathIdx,
        paths: &mut Vec<Option<Box<BtreePath>>>,
    ) -> Self {
        let mut iter = Self::init_with_path(
            root, target, flags, cache, btree_type, path_idx, paths,
        );
        iter.snapshot = snapshot_id;
        // init 定位到第一个 ≥ target 的 key。
        // 如果当前位置的 key.vaddr > target.vaddr，说明所有 key 的起始 > target，
        // 回退到前一个 key（其起始 ≤ target，可能覆盖 target）。
        match iter.peek() {
            Some((k, _)) => {
                let k_vaddr = unsafe { std::ptr::addr_of!(k.vaddr).read_unaligned() };
                let t_vaddr = unsafe { std::ptr::addr_of!(target.vaddr).read_unaligned() };
                if k_vaddr > t_vaddr {
                    iter.prev_entry();
                }
            }
            None => {
                iter.prev_entry();
            }
        }
        iter
    }

    /// 回退一个 entry（leaf 内操作）
    ///
    /// 重新初始化 iter 从 start 前进到 offset-1 位置，
    /// 使得后续 peek() 读取到正确的 entry。
    fn prev_entry(&mut self) -> bool {
        if let BtreePathNode::Node(leaf) = &mut self.btree_iter_path_mut().levels[0] {
            let new_offset = if leaf.offset == 0 {
                leaf.node.packed_keys + leaf.node.unpacked_keys
            } else if leaf.offset > 1 {
                leaf.offset - 1
            } else {
                return false;
            };
            if new_offset > 0 {
                leaf.offset = new_offset;
                bch2_btree_node_iter_init_from_start(&mut leaf.iter, &leaf.node);
                for _ in 1..leaf.offset {
                    if bch2_btree_node_iter_peek(&mut leaf.iter, &leaf.node).is_none() {
                        return false;
                    }
                    bch2_btree_node_iter_advance(&mut leaf.iter, &leaf.node);
                }
                return true;
            }
        }
        false
    }

    /// 查找 child 节点的 block_addr（internal 节点专用）
    ///
    /// 在 internal 节点中，每个 entry 的 value 是 child 的 BtreePtrV2。
    /// 搜索所有 bset（set[0] 排序 + set[1..] 增量追加），
    /// 找到 target 应该属于哪个 child。
    /// 返回 (child_addr, global_entry_index) — 全局 entry 索引（1-indexed，跨所有 bset）
    ///
    /// 使用 bpos-only 比较（通过 aux key）避免额外的 paddr/ver 解包。
    pub(crate) fn find_child_node(node: &BtreeNode, target: &BtreeKey) -> (u64, u16) {
        if node.packed_keys + node.unpacked_keys == 0 {
            return (0, 0);
        }

        let target = Bpos::from_key(target);
        let mut node_iter = BtreeNodeIter::default();
        bch2_btree_node_iter_init_from_start(&mut node_iter, node);
        let mut best: Option<(u64, u16)> = None;
        let mut first: Option<(u64, u16)> = None;
        let mut index = 1u16;
        while bch2_btree_node_iter_peek(&mut node_iter, node).is_some() {
            let (key, value) = node.read_packed_entry(node_iter.data[0].k as usize * 8);
            let candidate = (value.paddr(), index);
            first.get_or_insert(candidate);
            if Bpos::from_key(&key) <= target {
                best = Some(candidate);
            }
            index += 1;
            bch2_btree_node_iter_advance(&mut node_iter, node);
        }
        best.or(first).unwrap_or((0, 0))
    }

    /// 获取当前位置的 (key, value)
    ///
    /// 对应 bcachefs `bch2_btree_iter_peek()`
    /// 将全局 offset 转换为各 set 内的局部 offset 来读取。
    /// 从当前 btree path 读取当前位置。
    pub fn peek(&self) -> Option<(BtreeKey, BchVal)> {
        let BtreePathNode::Node(leaf) = &self.btree_iter_path().levels[0] else {
            return None;
        };
        let global_off = leaf.offset;
        if global_off == 0 || global_off > leaf.node.packed_keys + leaf.node.unpacked_keys {
            return None;
        }
        // 跳过已删除 key
        let pk = unsafe {
            &*(leaf
                .node
                .data
                .as_ptr()
                .add(leaf.iter.data[0].k as usize * 8) as *const BkeyPacked)
        };
        if pk.type_ == KeyType::Deleted as u8 {
            return None;
        }
        let raw_entry = leaf
            .node
            .read_packed_entry_raw(leaf.iter.data[0].k as usize * 8);
        if matches!(raw_entry.value, KeyValue::Raw(ref bytes) if matches!(KeyValue::from_bytes(bytes), KeyValue::ExtentPtrs { .. }))
        {
            Some(raw_entry.to_key_value())
        } else {
            let (key, extent) = leaf
                .node
                .read_packed_entry(leaf.iter.data[0].k as usize * 8);
            Some((key, extent.to_bchval()))
        }
    }

    /// 获取当前位置的 BtreeEntry（支持 Extent 和 Raw value）
    pub fn peek_entry(&self) -> Option<BtreeEntry> {
        let BtreePathNode::Node(leaf) = &self.btree_iter_path().levels[0] else {
            return None;
        };
        let global_off = leaf.offset;
        if global_off == 0 || global_off > leaf.node.packed_keys + leaf.node.unpacked_keys {
            return None;
        }
        let pk = unsafe {
            &*(leaf
                .node
                .data
                .as_ptr()
                .add(leaf.iter.data[0].k as usize * 8) as *const BkeyPacked)
        };
        if pk.type_ == KeyType::Deleted as u8 {
            return None;
        }
        Some(
            leaf.node
                .read_packed_entry_raw(leaf.iter.data[0].k as usize * 8),
        )
    }

    /// 验证并重建路径 — 对应 bcachefs `bch2_btree_iter_traverse()`
    ///
    /// 检查从 root 到 leaf 的路径是否仍然有效。如果当前 leaf 的 key 范围
    /// 不再覆盖 `self.pos`（并发 split/merge 后），从 root 重新下降到
    /// `self.pos` 并重建 path。
    pub fn traverse(&mut self) -> bool {
        let BtreePathNode::Node(leaf) = &self.btree_iter_path().levels[0] else {
            return false;
        };

        // depth=0：root 就是 leaf，路径永不失效
        if !self.btree_iter_path().levels[1..]
            .iter()
            .any(|node| matches!(node, BtreePathNode::Node(_)))
        {
            return true;
        }

        // 空节点（min > max）：路径无效，跳过
        if leaf.node.min_key > leaf.node.max_key {
            return self.full_traverse();
        }

        // 如果 self.pos 仍在 leaf 的 key 范围内，路径有效
        let pos_bpos = Bpos::from_key(&self.pos);
        if pos_bpos >= leaf.node.min_key && pos_bpos <= leaf.node.max_key {
            return true;
        }

        // 路径失效，从 root 重建
        self.full_traverse()
    }

    /// 从 root 重新下降到 self.pos 并重建 path
    ///
    /// 保留 path[0]（root）的现有锁，丢弃之后的所有层级。
    fn full_traverse(&mut self) -> bool {
        let root_level = self
            .btree_iter_path()
            .levels
            .iter()
            .rposition(|node| matches!(node, BtreePathNode::Node(_)));
        let Some(root_level) = root_level else {
            return false;
        };

        let target = self.pos;
        for level in 0..root_level {
            let removed = std::mem::replace(
                &mut self.btree_iter_path_mut().levels[level],
                BtreePathNode::Error(BtreePathError::Init),
            );
            if let BtreePathNode::Node(removed) = removed {
                if removed.lock_state == BtreeNodeLockedType::Read {
                    removed.node.lock.six_unlock_read();
                }
            }
            self.btree_iter_path_mut()
                .mark_btree_node_locked_noreset(level, BtreeNodeLockedType::None);
        }

        for parent_level in (1..=root_level).rev() {
            let parent = match &self.btree_iter_path().levels[parent_level] {
                BtreePathNode::Node(parent) => parent,
                BtreePathNode::None | BtreePathNode::Error(_) => return false,
            };
            let (child_addr, child_idx) = Self::find_child_node(&parent.node, &target);
            let child = self
                .cache
                .get_or_create(child_addr, (parent_level - 1) as u8);
            child.lock.six_lock_read();
            let child_seq = child.lock.six_lock_seq();
            self.btree_iter_path_mut().levels[parent_level - 1] =
                BtreePathNode::Node(BtreePathLevel {
                    node: child,
                    block_addr: child_addr,
                    lock_state: BtreeNodeLockedType::Read,
                    offset: 1,
                    iter: BtreeNodeIter::default(),
                    child_idx,
                    locked_seq: child_seq,
                });
            self.btree_iter_path_mut()
                .mark_btree_node_locked_noreset(parent_level - 1, BtreeNodeLockedType::Read);
        }

        // 设置 leaf 中的定位（offset=1，从第一个 entry 开始）
        let has_entries =
            if let BtreePathNode::Node(leaf) = &mut self.btree_iter_path_mut().levels[0] {
                leaf.offset = 1;
                bch2_btree_node_iter_init_from_start(&mut leaf.iter, &leaf.node);
                leaf.node.packed_keys + leaf.node.unpacked_keys > 0
            } else {
                false
            };
        if has_entries {
            if let Some((k, _v)) = self.peek() {
                self.pos = k;
            }
        }

        true
    }

    /// 向前移动一个 entry（bcachefs 对齐的 advance）
    ///
    /// 对应 bcachefs `bch2_btree_iter_advance()`
    /// 优先在 leaf 内移动，超出范围则回溯 path
    /// 全局 offset 减去前面各 set 的 size 得到局部 offset。
    pub fn advance(&mut self) -> bool {
        if let BtreePathNode::Node(leaf) = &mut self.btree_iter_path_mut().levels[0] {
            let n = leaf.node.packed_keys + leaf.node.unpacked_keys;
            if leaf.offset < n {
                leaf.offset += 1;
                bch2_btree_node_iter_advance(&mut leaf.iter, &leaf.node);
                if bch2_btree_node_iter_peek(&mut leaf.iter, &leaf.node).is_some() {
                    let (key, _) = leaf
                        .node
                        .read_packed_entry(leaf.iter.data[0].k as usize * 8);
                    self.pos = key;
                    return true;
                }
            }
            // 超出 leaf 范围，尝试回溯
            self.back_up_and_advance()
        } else {
            false
        }
    }

    fn back_up_and_advance(&mut self) -> bool {
        let root_level = self
            .btree_iter_path()
            .levels
            .iter()
            .rposition(|node| matches!(node, BtreePathNode::Node(_)))
            .unwrap_or(0);
        for current_level in 0..root_level {
            let current = match std::mem::replace(
                &mut self.btree_iter_path_mut().levels[current_level],
                BtreePathNode::Error(BtreePathError::Init),
            ) {
                BtreePathNode::Node(current) => current,
                BtreePathNode::None | BtreePathNode::Error(_) => return false,
            };
            self.btree_iter_path_mut()
                .mark_btree_node_locked_noreset(current_level, BtreeNodeLockedType::None);
            let parent = match &self.btree_iter_path().levels[current_level + 1] {
                BtreePathNode::Node(parent) => parent,
                BtreePathNode::None | BtreePathNode::Error(_) => return false,
            };

            // T3：验证父节点锁 seq 未变（未发生并发修改）
            if parent.node.lock.six_lock_seq() != parent.locked_seq {
                // 父节点已被修改，路径可能失效 → 全路径重建
                return self.full_traverse();
            }

            let next_idx = current.child_idx + 1;
            if next_idx <= parent.node.packed_keys + parent.node.unpacked_keys {
                // 跨所有 bset 查找全局索引为 next_idx 的 entry
                if let Some(v) = Self::read_entry_by_global_idx(&parent.node, next_idx) {
                    let child_addr = v.paddr();
                    let child_level = parent.node.level.saturating_sub(1);

                    // 备注：bcachefs 对齐 — 预取再下一个兄弟节点
                    if let Some(v2) = Self::read_entry_by_global_idx(&parent.node, next_idx + 1) {
                        self.cache
                            .prefetch_node(v2.paddr(), child_level, self.btree_type);
                    }

                    let child = self.cache.get_or_create(child_addr, child_level);
                    child.lock.six_lock_read();
                    let child_seq = child.lock.six_lock_seq();

                    self.btree_iter_path_mut().levels[current_level] =
                        BtreePathNode::Node(BtreePathLevel {
                            node: child,
                            block_addr: child_addr,
                            lock_state: BtreeNodeLockedType::Read,
                            offset: 1,
                            iter: BtreeNodeIter::default(),
                            child_idx: next_idx,
                            locked_seq: child_seq,
                        });
                    self.btree_iter_path_mut()
                        .mark_btree_node_locked_noreset(current_level, BtreeNodeLockedType::Read);

                    if child_level > 0 {
                        self.descend_to_first_leaf();
                    }

                    // 确保 leaf 迭代器已初始化
                    if let BtreePathNode::Node(ref mut leaf) = self.btree_iter_path_mut().levels[0]
                    {
                        if leaf.iter.data[0].end == 0 {
                            bch2_btree_node_iter_init_from_start(&mut leaf.iter, &leaf.node);
                        }
                    }

                    if let Some((k, _v)) = self.peek() {
                        self.pos = k;

                        return true;
                    }
                }
            }
        }
        false
    }

    /// 跨所有 bset 按全局 1-indexed 索引读取 entry 的 value
    fn read_entry_by_global_idx(node: &BtreeNode, global_idx: u16) -> Option<ExtentValue> {
        if global_idx == 0 {
            return None;
        }
        let mut node_iter = BtreeNodeIter::default();
        bch2_btree_node_iter_init_from_start(&mut node_iter, node);
        for _ in 1..global_idx {
            bch2_btree_node_iter_peek(&mut node_iter, node)?;
            bch2_btree_node_iter_advance(&mut node_iter, node);
        }
        bch2_btree_node_iter_peek(&mut node_iter, node)?;
        Some(node.read_packed_entry(node_iter.data[0].k as usize * 8).1)
    }

    /// 从当前 path 的 last 节点（必须是 internal）下降到最左 leaf
    fn descend_to_first_leaf(&mut self) {
        let top_level = self
            .btree_iter_path()
            .levels
            .iter()
            .position(|node| matches!(node, BtreePathNode::Node(_)))
            .unwrap_or(0);
        for parent_level in (1..=top_level).rev() {
            let top = match &self.btree_iter_path().levels[parent_level] {
                BtreePathNode::Node(top) => top,
                BtreePathNode::None | BtreePathNode::Error(_) => return,
            };
            let target = &BtreeKey::MIN_KEY;
            let (child_addr, child_idx) = Self::find_child_node(&top.node, target);
            let child_lvl = parent_level.saturating_sub(1);
            let child = self.cache.get_or_create(child_addr, child_lvl as u8);
            child.lock.six_lock_read();
            let child_seq = child.lock.six_lock_seq();
            self.btree_iter_path_mut().levels[child_lvl] = BtreePathNode::Node(BtreePathLevel {
                node: child,
                block_addr: child_addr,
                lock_state: BtreeNodeLockedType::Read,
                offset: 1,
                iter: BtreeNodeIter::default(),
                child_idx,
                locked_seq: child_seq,
            });
            self.btree_iter_path_mut()
                .mark_btree_node_locked_noreset(child_lvl, BtreeNodeLockedType::Read);
        }
        // 初始化 leaf 迭代器
        if let BtreePathNode::Node(ref mut leaf) = self.btree_iter_path_mut().levels[0] {
            bch2_btree_node_iter_init_from_start(&mut leaf.iter, &leaf.node);
        }
    }

    /// 从当前 path 的 last 节点（必须是 internal）下降到最右 leaf。
    fn descend_to_last_leaf(&mut self) {
        let top_level = self
            .btree_iter_path()
            .levels
            .iter()
            .position(|node| matches!(node, BtreePathNode::Node(_)))
            .unwrap_or(0);
        for parent_level in (1..=top_level).rev() {
            let top = match &self.btree_iter_path().levels[parent_level] {
                BtreePathNode::Node(top) => top,
                BtreePathNode::None | BtreePathNode::Error(_) => return,
            };
            let target = &BtreeKey::MAX_KEY;
            let (child_addr, child_idx) = Self::find_child_node(&top.node, target);
            let child_lvl = parent_level.saturating_sub(1);
            let child = self.cache.get_or_create(child_addr, child_lvl as u8);
            child.lock.six_lock_read();
            let child_seq = child.lock.six_lock_seq();
            self.btree_iter_path_mut().levels[child_lvl] = BtreePathNode::Node(BtreePathLevel {
                node: child,
                block_addr: child_addr,
                lock_state: BtreeNodeLockedType::Read,
                offset: 1,
                iter: BtreeNodeIter::default(),
                child_idx,
                locked_seq: child_seq,
            });
            self.btree_iter_path_mut()
                .mark_btree_node_locked_noreset(child_lvl, BtreeNodeLockedType::Read);
        }
        // 初始化 leaf 迭代器
        if let BtreePathNode::Node(ref mut leaf) = self.btree_iter_path_mut().levels[0] {
            bch2_btree_node_iter_init_from_start(&mut leaf.iter, &leaf.node);
        }
    }

    fn rewind_impl(&mut self) -> bool {
        let can_step_back =
            if let BtreePathNode::Node(leaf) = &mut self.btree_iter_path_mut().levels[0] {
                if leaf.offset > 1 {
                    leaf.offset -= 1;
                    // 同步 iter 到新 offset
                    bch2_btree_node_iter_init_from_start(&mut leaf.iter, &leaf.node);
                    for _ in 1..leaf.offset {
                        if bch2_btree_node_iter_peek(&mut leaf.iter, &leaf.node).is_none() {
                            break;
                        }
                        bch2_btree_node_iter_advance(&mut leaf.iter, &leaf.node);
                    }
                    true
                } else {
                    false
                }
            } else {
                return false;
            };

        if can_step_back && self.set_pos_from_current_leaf() {
            return true;
        }

        let original_pos = self.pos;
        let original_bpos = Bpos::from_key(&original_pos);

        // 先尝试沿着当前 path 找前一个兄弟节点。
        let root_level = self
            .btree_iter_path()
            .levels
            .iter()
            .rposition(|node| matches!(node, BtreePathNode::Node(_)))
            .unwrap_or(0);
        for current_level in 0..root_level {
            let current = match std::mem::replace(
                &mut self.btree_iter_path_mut().levels[current_level],
                BtreePathNode::Error(BtreePathError::Init),
            ) {
                BtreePathNode::Node(current) => current,
                BtreePathNode::None | BtreePathNode::Error(_) => break,
            };
            self.btree_iter_path_mut()
                .mark_btree_node_locked_noreset(current_level, BtreeNodeLockedType::None);
            let parent = match &self.btree_iter_path().levels[current_level + 1] {
                BtreePathNode::Node(parent) => parent,
                BtreePathNode::None | BtreePathNode::Error(_) => break,
            };

            if parent.node.lock.six_lock_seq() != parent.locked_seq {
                break;
            }

            if current.child_idx > 1 {
                let prev_idx = current.child_idx - 1;
                if let Some(v) = Self::read_entry_by_global_idx(&parent.node, prev_idx) {
                    let child_addr = v.paddr();
                    let child_level = parent.node.level.saturating_sub(1);

                    if prev_idx > 1 {
                        if let Some(v2) = Self::read_entry_by_global_idx(&parent.node, prev_idx - 1)
                        {
                            self.cache
                                .prefetch_node(v2.paddr(), child_level, self.btree_type);
                        }
                    }

                    let child = self.cache.get_or_create(child_addr, child_level);
                    child.lock.six_lock_read();
                    let child_seq = child.lock.six_lock_seq();
                    self.btree_iter_path_mut().levels[current_level] =
                        BtreePathNode::Node(BtreePathLevel {
                            node: child,
                            block_addr: child_addr,
                            lock_state: BtreeNodeLockedType::Read,
                            offset: 1,
                            iter: BtreeNodeIter::default(),
                            child_idx: prev_idx,
                            locked_seq: child_seq,
                        });
                    self.btree_iter_path_mut()
                        .mark_btree_node_locked_noreset(current_level, BtreeNodeLockedType::Read);

                    if child_level > 0 {
                        self.descend_to_last_leaf();
                    }

                    if let BtreePathNode::Node(leaf) = &mut self.btree_iter_path_mut().levels[0] {
                        leaf.offset = leaf.node.packed_keys + leaf.node.unpacked_keys;
                        // 定位 iter 到末尾
                        bch2_btree_node_iter_init_from_start(&mut leaf.iter, &leaf.node);
                        for _ in 1..leaf.offset {
                            if bch2_btree_node_iter_peek(&mut leaf.iter, &leaf.node).is_none() {
                                break;
                            }
                            bch2_btree_node_iter_advance(&mut leaf.iter, &leaf.node);
                        }
                    }
                    if self.set_pos_from_current_leaf() {
                        return true;
                    }
                }
            }
        }

        // 当前 path 已经无法回退时，按 bcachefs 语义从 predecessor 重新定位。
        if !original_bpos.is_min() {
            let prev_bpos = original_bpos.predecessor();
            self.pos = BtreeKey::from_bpos(prev_bpos, original_pos.key_type);
            if self.full_traverse() {
                if let Some(key) = self.current_leaf_key() {
                    if Bpos::from_key(&key) < original_bpos {
                        return true;
                    }
                }
            }
        }

        false
    }

    /// 更新当前位置的 value
    ///
    /// 对应 bcachefs `bch2_btree_iter_update()`。
    /// 通过 SixLock write 锁保证独占后，在 packed buffer 中写入新 value 字节。
    pub fn update(&mut self, new_value: &BchVal) -> bool {
        let leaf_idx = 0;
        let offset = match &self.btree_iter_path().levels[leaf_idx] {
            BtreePathNode::Node(leaf) => leaf.offset,
            BtreePathNode::None | BtreePathNode::Error(_) => return false,
        };
        if offset == 0 {
            return false;
        }

        // 确保 write lock（try-lock only — Phase 1 语义）
        if !self.upgrade_to_write(leaf_idx) {
            return false;
        }

        // 找到当前 entry 在 data buffer 中的偏移
        let (entry_data_off, entry_sz) = self.find_entry_offset(leaf_idx, offset);
        if entry_sz == 0 {
            return false;
        }

        // value 在 packed entry 中的偏移 = key_bytes (format.key_u64s * 8)
        let fmt = &crate::btree::key::BKEY_FORMAT_CURRENT;
        let value_off = entry_data_off + fmt.key_bytes();

        // SixLock write lock 保证了独占写，通过 unsafe 写入 value 字节
        let leaf_node = match &self.btree_iter_path().levels[leaf_idx] {
            BtreePathNode::Node(leaf) => &leaf.node,
            BtreePathNode::None | BtreePathNode::Error(_) => return false,
        };
        unsafe {
            let data_ptr = leaf_node.data.as_ptr() as *mut u8;
            let paddr_bytes = new_value.paddr.get().to_le_bytes();
            std::ptr::copy_nonoverlapping(
                paddr_bytes.as_ptr(),
                data_ptr.add(value_off as usize),
                6,
            );
            let ver_bytes = new_value.ver.to_le_bytes();
            std::ptr::copy_nonoverlapping(
                ver_bytes.as_ptr(),
                data_ptr.add(value_off as usize + 6),
                2,
            );
        }
        true
    }

    /// 计算指定层级 offset 对应的 data buffer 偏移和 entry 字节数
    ///
    /// 支持 compacted set（set[0] 有 aux 数组）和 incremental set（线性扫描）。
    fn find_entry_offset(&self, level: usize, global_off: u16) -> (u32, u32) {
        let node = match self.btree_iter_path().levels.get(level) {
            Some(BtreePathNode::Node(level)) => &level.node,
            Some(BtreePathNode::None | BtreePathNode::Error(_)) | None => return (0, 0),
        };
        let mut remaining = global_off;
        for set in &node.sets[..node.nsets() as usize] {
            let mut cur = u32::from(set.first_key_offset()) * 8;
            let end = u32::from(set.end_offset) * 8;
            while cur < end {
                let u64s = node.read_entry_u64s(cur as usize);
                if remaining == 1 {
                    return (cur, u64s as u32 * 8);
                }
                remaining -= 1;
                cur += u64s as u32 * 8;
            }
        }
        (0, 0)
    }

    /// 将指定层级的锁升级到 write（try-lock only）
    ///
    /// Phase 1 使用 try-lock 语义（对应 SixLock 当前实现）。
    /// 更新 self.path[level].lock_state 以反映新状态。
    fn upgrade_to_write(&mut self, level: usize) -> bool {
        let pl = match self.btree_iter_path().levels.get(level) {
            Some(BtreePathNode::Node(level)) => level,
            Some(BtreePathNode::None | BtreePathNode::Error(_)) | None => return false,
        };
        // 对应 bcachefs bch2_btree_node_lock_write_contended locking.c:965-972
        // six_trylock_write 不自排除读者，调用方需临时减去自身读锁计数。
        let readers = pl.node.lock.six_lock_counts().n[0];
        if readers > 0 {
            pl.node.lock.six_lock_readers_add(-(readers as i32));
        }
        let ok = match pl.lock_state {
            BtreeNodeLockedType::None | BtreeNodeLockedType::Read => false,
            BtreeNodeLockedType::Intent => pl.node.lock.six_trylock_write(),
            BtreeNodeLockedType::Write => true,
        };
        if readers > 0 {
            pl.node.lock.six_lock_readers_add(readers as i32);
        }
        if ok {
            let path = self.btree_iter_path_mut();
            let BtreePathNode::Node(path_level) = &mut path.levels[level] else {
                unreachable!();
            };
            path_level.lock_state = BtreeNodeLockedType::Write;
            path.mark_btree_node_locked_noreset(level, BtreeNodeLockedType::Write);
        }
        ok
    }

    /// 重启遍历器（从 root 重新下降）
    ///
    /// 对应 bcachefs `bch2_btree_iter_restart()`
    /// 当检测到锁竞争导致路径失效时调用。
    pub fn restart(&mut self, root: &BtreeRoot) {
        // 释放所有当前持有的锁
        for node in &self.btree_iter_path().levels {
            let BtreePathNode::Node(level) = node else {
                continue;
            };
            match level.lock_state {
                BtreeNodeLockedType::Read => level.node.lock.six_unlock_read(),
                BtreeNodeLockedType::Intent => level.node.lock.six_unlock_intent(),
                BtreeNodeLockedType::Write => {
                    level.node.lock.six_unlock_write();
                    level.node.lock.six_unlock_intent();
                }
                BtreeNodeLockedType::None => {}
            }
        }

        // 重新初始化
        let path_idx = self.path;
        let mut paths_ptr = self.paths_ptr;
        let pos = self.pos;
        let flags = self.flags;
        let cache = Arc::clone(&self.cache);
        let btree_type = self.btree_type;
        let restarted = Self::init_with_path(
            root,
            &pos,
            flags,
            &cache,
            btree_type,
            path_idx,
            unsafe { paths_ptr.as_mut() },
        );
        *self = restarted;
        self.had_restart = true;
    }

    /// 优化版重启：当节点 seq 未变化时跳过从 root 重下降
    ///
    /// R2 优化：利用 locked_seq 检测自加锁以来节点是否被写操作修改。
    /// 如果所有 path level 的六锁序列号都与加锁时相同，说明节点未被修改，
    /// 无需重新下降遍历，只需释放锁并重置状态。
    ///
    /// # 返回值
    ///
    /// - `false` — 所有节点 seq 未变化，跳过了重下降（只需重置状态）
    /// - `true` — 回退到完整 `restart()`（有节点被修改过）
    pub fn restart_optimized(&mut self, root: &BtreeRoot) -> bool {
        // 1. 释放所有当前持有的锁
        for node in &self.btree_iter_path().levels {
            let BtreePathNode::Node(level) = node else {
                continue;
            };
            match level.lock_state {
                BtreeNodeLockedType::Read => level.node.lock.six_unlock_read(),
                BtreeNodeLockedType::Intent => level.node.lock.six_unlock_intent(),
                BtreeNodeLockedType::Write => {
                    level.node.lock.six_unlock_write();
                    level.node.lock.six_unlock_intent();
                }
                BtreeNodeLockedType::None => {}
            }
        }

        // 2. 从 leaf 开始检查 seq 是否变化
        // leaf 在 path.last()
        let leaf_unchanged = match &self.btree_iter_path().levels[0] {
            BtreePathNode::Node(leaf) => leaf.node.lock.six_lock_seq() == leaf.locked_seq,
            BtreePathNode::None | BtreePathNode::Error(_) => false,
        };

        if leaf_unchanged {
            // 3. 所有 level 都未变化 → 跳过重下降
            let all_unchanged = self.btree_iter_path().levels.iter().all(|node| {
                let BtreePathNode::Node(level) = node else {
                    return true;
                };
                level.lock_state == BtreeNodeLockedType::None
                    || level.node.lock.six_lock_seq() == level.locked_seq
            });

            if all_unchanged {
                // 不需 re-init，只需重置锁状态和重启标志
                let path = self.btree_iter_path_mut();
                for node in &mut path.levels {
                    if let BtreePathNode::Node(level) = node {
                        level.lock_state = BtreeNodeLockedType::None;
                    }
                }
                path.nodes_locked = 0;
                self.had_restart = false;

                return false; // false = 不需要重下降
            }
        }

        // 4. 回退到完整 restart（步骤 1 已释放锁，需重置 lock_state 避免重复释放）
        let path = self.btree_iter_path_mut();
        for node in &mut path.levels {
            if let BtreePathNode::Node(level) = node {
                level.lock_state = BtreeNodeLockedType::None;
            }
        }
        path.nodes_locked = 0;
        self.restart(root);

        true // true = 执行了重下降
    }

    /// 获取当前 leaf 中 entry 的数量
    pub fn leaf_key_count(&self) -> u32 {
        match &self.btree_iter_path().levels[0] {
            BtreePathNode::Node(leaf) => leaf.node.packed_keys as u32 + leaf.node.unpacked_keys as u32,
            BtreePathNode::None | BtreePathNode::Error(_) => 0,
        }
    }

    /// 是否已经到达 leaf
    pub fn at_leaf(&self) -> bool {
        matches!(
            &self.btree_iter_path().levels[0],
            BtreePathNode::Node(leaf) if leaf.node.level == 0
        )
    }

    // ─── 快照可见性过滤 ─────────────────────────────────

    /// 设置快照过滤：只返回在指定快照中可见的条目
    /// 对应 bcachefs `bch2_btree_iter_set_snapshot()`
    pub fn set_snapshot_filter(&mut self, sid: u32) {
        if self.snapshot != sid {
            self.snapshot = sid;
            self.snapshot_visible_cache.clear();
        }
    }

    fn shadowed_by_same_pos_delete(
        &mut self,
        current_key: &BtreeKey,
    ) -> Option<(BtreeKey, BchVal)> {
        let saved_path = match &self.btree_iter_path().levels[0] {
            BtreePathNode::Node(leaf) => Some((leaf.offset, leaf.iter.clone())),
            BtreePathNode::None | BtreePathNode::Error(_) => None,
        };
        if !self.advance() {
            return None;
        }

        let current_vaddr = unsafe { std::ptr::addr_of!(current_key.vaddr).read_unaligned() };
        loop {
            let Some((next_key, next_val)) = self.peek() else {
                break;
            };
            let next_vaddr = unsafe { std::ptr::addr_of!(next_key.vaddr).read_unaligned() };
            if next_vaddr != current_vaddr {
                break;
            }
            if next_key.key_type != KeyType::Normal {
                if let Some((off, iter)) = saved_path {
                    if let BtreePathNode::Node(leaf) = &mut self.btree_iter_path_mut().levels[0] {
                        leaf.offset = off;
                        leaf.iter = iter;
                    }
                }
                return Some((next_key, next_val));
            }
            if !self.advance() {
                break;
            }
        }

        if let Some((off, iter)) = saved_path {
            if let BtreePathNode::Node(leaf) = &mut self.btree_iter_path_mut().levels[0] {
                leaf.offset = off;
                leaf.iter = iter;
            }
        }
        None
    }

    /// 返回下一个对当前快照可见的 (key, value)
    ///
    /// 自动跳过：
    /// - Whiteout 类型的 key（始终跳过）
    /// - 在当前快照中不可见的 key（设置了过滤时）
    ///
    /// 无过滤时 (snapshot=0) 仅跳过 Whiteout（向后兼容）。
    pub fn peek_visible(&mut self, vol: &BchVol) -> Option<(BtreeKey, BchVal)> {
        let t = BtreeTrans::new_ro(vol);
        loop {
            let entry = self.peek()?;
            // 始终跳过 Whiteout
            if entry.0.key_type == KeyType::Whiteout {
                if !self.advance() {
                    return None;
                }
                continue;
            }

            if entry.0.key_type == KeyType::Normal {
                if let Some(shadowed) = self.shadowed_by_same_pos_delete(&entry.0) {
                    return Some(shadowed);
                }
            }

            // 检查快照可见性
            let key_sid = entry.0.get_snapshot_id();
            if self.snapshot != 0 && self.snapshot == key_sid {
                return Some(entry);
            }
            if self.snapshot != 0 {
                let visible = self
                    .snapshot_visible_cache
                    .entry((self.snapshot, key_sid))
                    .or_insert_with(|| {
                        crate::snap::snapshot::bch2_snapshot_is_ancestor(&t, self.snapshot, key_sid)
                    });
                if *visible {
                    // 祖先可见：检查下一 key 的范围是否与本 key 重叠（范围 override 检测）
                    let entry_vaddr = unsafe { std::ptr::addr_of!(entry.0.vaddr).read_unaligned() };
                    let entry_size = unsafe { std::ptr::addr_of!(entry.0.size).read_unaligned() };
                    let entry_effective_size = if entry_size == 0 { 1 } else { entry_size };
                    let entry_end = entry_vaddr + entry_effective_size as u64;
                    let saved_path = match &self.btree_iter_path().levels[0] {
                        BtreePathNode::Node(leaf) => Some((leaf.offset, leaf.iter.clone())),
                        BtreePathNode::None | BtreePathNode::Error(_) => None,
                    };
                    if self.advance() {
                        if let Some((next_k, next_v)) = self.peek() {
                            let next_vaddr =
                                unsafe { std::ptr::addr_of!(next_k.vaddr).read_unaligned() };
                            let next_size =
                                unsafe { std::ptr::addr_of!(next_k.size).read_unaligned() };
                            let next_effective_size = if next_size == 0 { 1 } else { next_size };
                            let next_end = next_vaddr + next_effective_size as u64;
                            // 范围重叠检测
                            if next_k.get_snapshot_id() == self.snapshot
                                && entry_vaddr < next_end
                                && next_vaddr < entry_end
                            {
                                return Some((next_k, next_v));
                            }
                        }
                        // 恢复位置（没有精确匹配）
                        if let Some((off, iter)) = saved_path {
                            if let BtreePathNode::Node(leaf) =
                                &mut self.btree_iter_path_mut().levels[0]
                            {
                                leaf.offset = off;
                                leaf.iter = iter;
                            }
                        }
                    }
                    return Some(entry);
                }
                if !self.advance() {
                    return None;
                }
                continue;
            }
            return Some(entry);
        }
    }

    /// 前进到下一个对当前快照可见的条目
    ///
    /// 跳过当前位置后的所有不可见和 Whiteout 条目。
    /// 返回 true 如果成功定位到下一个可见条目。
    pub fn advance_visible(&mut self, vol: &BchVol) -> bool {
        if !self.advance() {
            // advance 返回 false 时 cursor 仍指向最后一位，
            // 但当前条目已被消费，将 offset 设为 max 避免脏读
            if let BtreePathNode::Node(leaf) = &mut self.btree_iter_path_mut().levels[0] {
                if leaf.offset >= leaf.node.packed_keys + leaf.node.unpacked_keys {
                    leaf.offset = leaf.node.packed_keys + leaf.node.unpacked_keys + 1;
                }
            }
            return false;
        }
        // 跳过 Whiteout 或 Whiteout + 不可见
        self.peek_visible(vol).is_some()
    }

    /// 范围感知的 peek_visible：返回 (key, value, visible_start, visible_end)
    ///
    /// 与 `peek_visible()` 类似，但：
    /// 1. 返回可见范围（用于批读/split/trim）
    /// 2. 祖先 override 使用范围重叠检测而非精确 vaddr 匹配
    pub fn peek_visible_range(&mut self, vol: &BchVol) -> Option<(BtreeKey, BchVal, u64, u64)> {
        let t = BtreeTrans::new_ro(vol);
        loop {
            let entry = self.peek()?;
            if entry.0.key_type == KeyType::Whiteout {
                if !self.advance() {
                    return None;
                }
                continue;
            }
            let key_sid = entry.0.get_snapshot_id();
            let entry_vaddr = unsafe { std::ptr::addr_of!(entry.0.vaddr).read_unaligned() };
            let entry_size = unsafe { std::ptr::addr_of!(entry.0.size).read_unaligned() };
            let effective_size = if entry_size == 0 { 1 } else { entry_size };
            let entry_end = entry_vaddr + effective_size as u64;

            if entry.0.key_type == KeyType::Normal {
                if let Some((shadow_key, shadow_val)) = self.shadowed_by_same_pos_delete(&entry.0) {
                    let shadow_vaddr =
                        unsafe { std::ptr::addr_of!(shadow_key.vaddr).read_unaligned() };
                    let shadow_size =
                        unsafe { std::ptr::addr_of!(shadow_key.size).read_unaligned() };
                    let shadow_effective_size = if shadow_size == 0 { 1 } else { shadow_size };
                    return Some((
                        shadow_key,
                        shadow_val,
                        shadow_vaddr,
                        shadow_vaddr + shadow_effective_size as u64,
                    ));
                }
            }

            if self.snapshot != 0 && self.snapshot == key_sid {
                return Some((entry.0, entry.1, entry_vaddr, entry_end));
            }
            if self.snapshot != 0 {
                let visible = self
                    .snapshot_visible_cache
                    .entry((self.snapshot, key_sid))
                    .or_insert_with(|| {
                        crate::snap::snapshot::bch2_snapshot_is_ancestor(&t, self.snapshot, key_sid)
                    });
                if *visible {
                    // 范围 override 检测：检查下一 key 是否与本 key 范围重叠
                    let saved_path = match &self.btree_iter_path().levels[0] {
                        BtreePathNode::Node(leaf) => Some((leaf.offset, leaf.iter.clone())),
                        BtreePathNode::None | BtreePathNode::Error(_) => None,
                    };
                    if self.advance() {
                        if let Some((next_k, _next_v)) = self.peek() {
                            let next_vaddr =
                                unsafe { std::ptr::addr_of!(next_k.vaddr).read_unaligned() };
                            let next_size =
                                unsafe { std::ptr::addr_of!(next_k.size).read_unaligned() };
                            let next_effective_size = if next_size == 0 { 1 } else { next_size };
                            let next_end = next_vaddr + next_effective_size as u64;

                            // 范围重叠检测：[entry_start, entry_end) ∩ [next_start, next_end)
                            let overlap_start = entry_vaddr.max(next_vaddr);
                            let overlap_end = entry_end.min(next_end);
                            if next_k.get_snapshot_id() == self.snapshot
                                && overlap_start < overlap_end
                            {
                                // 子条目覆盖了部分范围
                                // 被覆盖部分：[overlap_start, overlap_end)
                                // 可见部分：[entry_vaddr, overlap_start)
                                if entry_vaddr < overlap_start {
                                    // 返回未被覆盖的左段
                                    if let Some((off, iter)) = saved_path {
                                        if let BtreePathNode::Node(leaf) =
                                            &mut self.btree_iter_path_mut().levels[0]
                                        {
                                            leaf.offset = off;
                                            leaf.iter = iter;
                                        }
                                    }
                                    return Some((entry.0, entry.1, entry_vaddr, overlap_start));
                                }
                                // 完全覆盖，返回子条目
                                return Some((next_k, _next_v, next_vaddr, next_end));
                            }
                        }
                        // 恢复位置
                        if let Some((off, iter)) = saved_path {
                            if let BtreePathNode::Node(leaf) =
                                &mut self.btree_iter_path_mut().levels[0]
                            {
                                leaf.offset = off;
                                leaf.iter = iter;
                            }
                        }
                    }
                    return Some((entry.0, entry.1, entry_vaddr, entry_end));
                }
                if !self.advance() {
                    return None;
                }
                continue;
            }
            return Some((entry.0, entry.1, entry_vaddr, entry_end));
        }
    }

    /// 范围感知的可见条目，同时保留原始 extent value 中的设备指针。
    ///
    /// `peek_visible_range()` 为历史调用者投影成 `BchVal`，而 bcachefs
    /// `bch2_read_extent()` 的重试必须使用完整的 `bch_extent_ptr` 列表。
    /// 这里复用完全相同的可见性/推进控制流，只从最终当前位置取回
    /// `BtreeEntry::value`，避免改变旧 API 或快照语义。
    pub fn peek_visible_range_with_entry(
        &mut self,
        vol: &BchVol,
    ) -> Option<(BtreeKey, BchVal, KeyValue, u64, u64)> {
        let (key, value, visible_start, visible_end) = self.peek_visible_range(vol)?;
        let entry = self.peek_entry()?;
        Some((key, value, entry.value, visible_start, visible_end))
    }

    // ─── bcachefs 对齐方法 ─────────────────────────────────

    /// 查看当前位置的 key（bcachefs 对齐：`bch2_btree_iter_peek()`）
    ///
    /// 返回当前迭代位置的 `(key, value)`。与 `peek()` 行为一致。
    pub fn bch2_btree_iter_peek(&self) -> Option<(BtreeKey, BchVal)> {
        self.peek()
    }

    /// 查看当前 slot 的 key（bcachefs 对齐：`bch2_btree_iter_peek_slot()`）
    ///
    /// slot 模式：返回当前位置的键值，不进行方向性移动。
    /// 与 `peek()` 行为一致。
    pub fn bch2_btree_iter_peek_slot(&self) -> Option<(BtreeKey, BchVal)> {
        self.peek()
    }

    /// 前进到下一个 key 并返回（bcachefs 对齐：`bch2_btree_iter_next()`）
    ///
    /// 组合了 `advance()` + `peek()` 的便捷方法。
    /// 对应 bcachefs `bch2_btree_iter_next()`，定位到下一项并返回。
    pub fn next(&mut self) -> Option<(BtreeKey, BchVal)> {
        if self.advance() {
            self.peek()
        } else {
            None
        }
    }

    /// 前进到下一个 slot 的 key（bcachefs 对齐：`bch2_btree_iter_next_slot()`）
    pub fn next_slot(&mut self) -> Option<(BtreeKey, BchVal)> {
        self.next()
    }

    /// 退回到上一个 key 并返回（bcachefs 对齐：`bch2_btree_iter_prev()`）
    pub fn prev_slot(&mut self) -> Option<(BtreeKey, BchVal)> {
        if !self.rewind_impl() {
            return None;
        }
        self.peek()
    }

    /// 在给定上限范围内 peek（bcachefs 对齐：`bch2_btree_iter_peek_max()`）
    pub fn peek_max(&mut self, end: &Bpos) -> Option<(BtreeKey, BchVal)> {
        let (k, v) = self.peek()?;
        if Bpos::from_key(&k) > *end {
            None
        } else {
            self.pos = k;

            Some((k, v))
        }
    }

    /// 带下限的向前 peek（bcachefs 对齐：`bch2_btree_iter_peek_prev_min()`）
    pub fn peek_prev_min(&mut self, min: Bpos) -> Option<(BtreeKey, BchVal)> {
        if self.btree_type == BtreeId::Extents {
            if let Some((k, v)) = self.peek() {
                let key_pos = Bpos::from_key(&k);
                let iter_pos = Bpos::from_key(&self.pos);
                if key_pos < iter_pos && key_pos >= min {
                    self.pos = k;

                    return Some((k, v));
                }
            }
        }

        let original_key_type = self.pos.key_type;
        if !self.rewind_impl() {
            return None;
        }

        let Some((k, v)) = self.peek() else {
            return None;
        };
        let key_pos = Bpos::from_key(&k);
        if key_pos < min {
            self.pos = BtreeKey::from_bpos(min, original_key_type);

            return None;
        }
        Some((k, v))
    }
}

impl std::fmt::Display for BtreeIter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "BtreeIter[pos={}]", self.pos)
    }
}

// ─── 测试 ───────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btree::key::KeyType;
    use crate::btree::node::BtreeNode;
    use crate::btree::types::NodeCache;
    use crate::btree::BtreeTrans;
    use crate::snap::snapshot::{bch2_snapshot_node_create, bch2_snapshot_read_value};

    #[test]
    fn test_update_trigger_flags_match_local_bcachefs_bit_layout() {
        let flags = [
            (UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE, 1u32 << 18),
            (UpdateTriggerFlags::NOJOURNAL, 1u32 << 19),
            (UpdateTriggerFlags::KEY_CACHE_RECLAIM, 1u32 << 20),
            (UpdateTriggerFlags::NORUN, 1u32 << 21),
            (UpdateTriggerFlags::TRANSACTIONAL, 1u32 << 22),
            (UpdateTriggerFlags::ATOMIC, 1u32 << 23),
            (UpdateTriggerFlags::GC, 1u32 << 24),
            (UpdateTriggerFlags::INSERT, 1u32 << 25),
            (UpdateTriggerFlags::OVERWRITE, 1u32 << 26),
            (UpdateTriggerFlags::IS_DISCARD, 1u32 << 27),
            (UpdateTriggerFlags::SET_NEEDS_RECONCILE_DONE, 1u32 << 28),
        ];

        assert_eq!(std::mem::size_of::<UpdateTriggerFlags>(), 4);
        for (flag, expected) in flags {
            assert_eq!(flag.bits(), expected);
        }
    }

    #[test]
    fn test_update_trigger_flags_combine() {
        assert_eq!(UpdateTriggerFlags::empty().bits(), 0);

        let transactional = UpdateTriggerFlags::TRANSACTIONAL | UpdateTriggerFlags::INSERT;
        assert!(transactional.contains(UpdateTriggerFlags::TRANSACTIONAL));
        assert!(transactional.contains(UpdateTriggerFlags::INSERT));
        assert!(!transactional.contains(UpdateTriggerFlags::GC));
        assert_eq!(transactional.bits(), (1u32 << 22) | (1u32 << 25));

        let gc = UpdateTriggerFlags::GC | UpdateTriggerFlags::INSERT;
        assert!(gc.contains(UpdateTriggerFlags::GC));
        assert!(gc.contains(UpdateTriggerFlags::INSERT));
        assert!(!gc.contains(UpdateTriggerFlags::TRANSACTIONAL));
        assert_eq!(gc.bits(), (1u32 << 24) | (1u32 << 25));
    }

    struct AutoApplyTrans<'a>(BtreeTrans<'a>);
    impl<'a> std::ops::Deref for AutoApplyTrans<'a> {
        type Target = BtreeTrans<'a>;
        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }
    impl<'a> std::ops::DerefMut for AutoApplyTrans<'a> {
        fn deref_mut(&mut self) -> &mut Self::Target {
            &mut self.0
        }
    }
    impl<'a> Drop for AutoApplyTrans<'a> {
        fn drop(&mut self) {
            self.0.bch2_trans_commit()
                .expect("AutoApplyTrans::apply failed");
        }
    }

    fn make_trans<'a>(vol: &'a BchVol) -> AutoApplyTrans<'a> {
        let trans = BtreeTrans::new(vol);
        AutoApplyTrans(trans)
    }

    fn make_root_with_cache() -> (BtreeRoot, Arc<NodeCache>) {
        let root = BtreeRoot {
            node: Arc::new(BtreeNode::new_leaf()),
            depth: 0,
        };
        let cache = Arc::new(NodeCache::new());
        (root, cache)
    }

    fn make_two_leaf_root() -> (BtreeRoot, Arc<NodeCache>) {
        use crate::btree::node::BsetTree;

        let cache = Arc::new(NodeCache::new());

        let mut left_node = BtreeNode::new_leaf();
        left_node.insert(BtreeKey::new(10, 1, KeyType::Normal), BchVal::new(100, 0));
        left_node.insert(BtreeKey::new(20, 1, KeyType::Normal), BchVal::new(200, 0));
        left_node.insert(BtreeKey::new(30, 1, KeyType::Normal), BchVal::new(300, 0));
        let left = Arc::new(left_node);

        let mut right_node = BtreeNode::new_leaf();
        right_node.insert(BtreeKey::new(40, 1, KeyType::Normal), BchVal::new(400, 0));
        right_node.insert(BtreeKey::new(50, 1, KeyType::Normal), BchVal::new(500, 0));
        let right = Arc::new(right_node);

        let left_addr = 1;
        let right_addr = 2;
        cache.insert(left_addr, left.clone());
        cache.insert(right_addr, right.clone());

        let mut internal = BtreeNode::new_internal();
        let mut cur = u32::from(crate::btree::node::BSET_HEADER_U64S) * 8;
        cur += internal.write_entry(cur, &BtreeKey::MIN_KEY, &BchVal::new(left_addr, 0), 0);
        cur += internal.write_entry(
            cur,
            &BtreeKey::new(40, 1, KeyType::Normal),
            &BchVal::new(right_addr, 0),
            0,
        );
        internal.sets[0] = BsetTree {
            size: 0,
            extra: crate::btree::node::BSET_NO_AUX_TREE_VAL,
            data_offset: 0,
            aux_data_offset: u16::MAX,
            end_offset: (cur / 8) as u16,
        };
        internal.packed_keys = 2;
        internal.unpacked_keys = 0;

        let root = BtreeRoot {
            node: Arc::new(internal),
            depth: 1,
        };

        (root, cache)
    }

    struct TestIter {
        trans: BtreeTrans<'static>,
        iter_idx: usize,
    }

    impl std::ops::Deref for TestIter {
        type Target = BtreeIter;

        fn deref(&self) -> &Self::Target {
            self.trans
                .iter(self.iter_idx)
                .expect("test iter is present")
        }
    }

    impl std::ops::DerefMut for TestIter {
        fn deref_mut(&mut self) -> &mut Self::Target {
            self.trans
                .iter_mut(self.iter_idx)
                .expect("test iter is present")
        }
    }

    fn test_iter_init(
        root: &BtreeRoot,
        target: &BtreeKey,
        flags: IterFlags,
        cache: &Arc<NodeCache>,
        btree_type: BtreeId,
    ) -> TestIter {
        let mut trans = BtreeTrans::new_with_cache(Arc::clone(cache));
        let iter_idx = trans.get_path(root, target, flags.intent, btree_type, Some(flags));
        let iter = trans.iter_mut(iter_idx).expect("test iter is present");
        iter.flags = flags;
        TestIter { trans, iter_idx }
    }

    fn iter_path(iter: &BtreeIter) -> &BtreePath {
        iter.btree_iter_path()
    }

    fn iter_path_mut(iter: &mut BtreeIter) -> &mut BtreePath {
        iter.btree_iter_path_mut()
    }

    #[test]
    fn test_iter_init_single_leaf() {
        let (root, cache) = make_root_with_cache();
        let target = BtreeKey::new(100, 1, KeyType::Normal);
        let iter = test_iter_init(
            &root,
            &target,
            IterFlags::default(),
            &cache,
            crate::btree::BtreeId::Extents,
        );
        assert_eq!(iter_path(&iter).level, 0);
        assert!(iter.at_leaf());
    }

    #[test]
    fn test_iter_init_keeps_lookup_position_separate_from_current_key() {
        let (mut root, cache) = make_root_with_cache();
        Arc::get_mut(&mut root.node)
            .unwrap()
            .insert(BtreeKey::new(20, 1, KeyType::Normal), BchVal::new(200, 0));
        let target = BtreeKey::new(15, 1, KeyType::Normal);
        let iter = test_iter_init(
            &root,
            &target,
            IterFlags::default(),
            &cache,
            crate::btree::BtreeId::Extents,
        );

        assert_eq!(iter.pos, target);
        assert_eq!(iter_path(&iter).pos, target.to_bpos());
        assert_eq!(
            iter.peek().unwrap().0,
            BtreeKey::new(20, 1, KeyType::Normal)
        );
    }

    #[test]
    fn test_iter_peek_empty() {
        let (root, cache) = make_root_with_cache();
        let target = BtreeKey::new(100, 1, KeyType::Normal);
        let iter = test_iter_init(
            &root,
            &target,
            IterFlags::default(),
            &cache,
            crate::btree::BtreeId::Extents,
        );
        assert!(iter.peek().is_none());
    }

    #[test]
    fn test_iter_init_intent() {
        let (root, cache) = make_root_with_cache();
        let target = BtreeKey::new(100, 1, KeyType::Normal);
        let flags = IterFlags {
            intent: true,
            forward: true,
            with_journal: false,
            cached: false,
            nopreserve: false,
        };
        let iter = test_iter_init(
            &root,
            &target,
            flags,
            &cache,
            crate::btree::BtreeId::Extents,
        );
        assert_eq!(iter.flags.intent, true);
    }

    #[test]
    fn test_iter_restart() {
        let (root, cache) = make_root_with_cache();
        let target = BtreeKey::new(100, 1, KeyType::Normal);
        let mut iter = test_iter_init(
            &root,
            &target,
            IterFlags::default(),
            &cache,
            crate::btree::BtreeId::Extents,
        );
        assert!(!iter.had_restart);
        iter.restart(&root);
        assert!(iter.had_restart);
    }

    #[test]
    fn test_iter_advance_empty() {
        let (root, cache) = make_root_with_cache();
        let target = BtreeKey::new(100, 1, KeyType::Normal);
        let mut iter = test_iter_init(
            &root,
            &target,
            IterFlags::default(),
            &cache,
            crate::btree::BtreeId::Extents,
        );
        assert!(!iter.advance());
    }

    #[test]
    fn test_iter_prev_slot_same_leaf() {
        let (mut root, cache) = make_root_with_cache();
        let leaf = Arc::get_mut(&mut root.node).unwrap();
        leaf.insert(BtreeKey::new(10, 1, KeyType::Normal), BchVal::new(100, 0));
        leaf.insert(BtreeKey::new(20, 1, KeyType::Normal), BchVal::new(200, 0));
        leaf.insert(BtreeKey::new(30, 1, KeyType::Normal), BchVal::new(300, 0));

        let mut iter = test_iter_init(
            &root,
            &BtreeKey::new(20, 1, KeyType::Normal),
            IterFlags::default(),
            &cache,
            crate::btree::BtreeId::Extents,
        );

        let prev = iter.prev_slot().expect("should rewind within leaf");
        assert_eq!(prev.0, BtreeKey::new(10, 1, KeyType::Normal));
        assert_eq!(
            iter.peek().unwrap().0,
            BtreeKey::new(10, 1, KeyType::Normal)
        );
    }

    #[test]
    fn test_iter_prev_slot_cross_leaf() {
        let (root, cache) = make_two_leaf_root();
        let mut iter = test_iter_init(
            &root,
            &BtreeKey::new(40, 1, KeyType::Normal),
            IterFlags::default(),
            &cache,
            crate::btree::BtreeId::Extents,
        );

        let prev = iter.prev_slot().expect("should rewind to previous leaf");
        assert_eq!(prev.0, BtreeKey::new(30, 1, KeyType::Normal));
        assert_eq!(
            iter.peek().unwrap().0,
            BtreeKey::new(30, 1, KeyType::Normal)
        );
    }

    #[test]
    fn test_iter_peek_prev_min_respects_lower_bound() {
        let (mut root, cache) = make_root_with_cache();
        let leaf = Arc::get_mut(&mut root.node).unwrap();
        leaf.insert(BtreeKey::new(10, 1, KeyType::Normal), BchVal::new(100, 0));
        leaf.insert(BtreeKey::new(20, 1, KeyType::Normal), BchVal::new(200, 0));
        leaf.insert(BtreeKey::new(30, 1, KeyType::Normal), BchVal::new(300, 0));

        let mut iter = test_iter_init(
            &root,
            &BtreeKey::new(30, 1, KeyType::Normal),
            IterFlags::default(),
            &cache,
            crate::btree::BtreeId::Extents,
        );

        let ok = iter
            .peek_prev_min(Bpos::new(0, 15, 1))
            .expect("should find entry above lower bound");
        assert_eq!(ok.0, BtreeKey::new(20, 1, KeyType::Normal));
        assert!(
            iter.peek_prev_min(Bpos::new(0, 25, 1)).is_none(),
            "lower bound should stop traversal"
        );
    }

    #[test]
    fn test_iter_peek_max_respects_upper_bound() {
        let (mut root, cache) = make_root_with_cache();
        let leaf = Arc::get_mut(&mut root.node).unwrap();
        leaf.insert(BtreeKey::new(10, 1, KeyType::Normal), BchVal::new(100, 0));
        leaf.insert(BtreeKey::new(20, 1, KeyType::Normal), BchVal::new(200, 0));

        let mut iter = test_iter_init(
            &root,
            &BtreeKey::new(10, 1, KeyType::Normal),
            IterFlags::default(),
            &cache,
            crate::btree::BtreeId::Extents,
        );

        assert_eq!(
            iter.peek_max(&Bpos::new(0, 10, 1)).unwrap().0,
            BtreeKey::new(10, 1, KeyType::Normal)
        );
        assert!(
            iter.peek_max(&Bpos::new(0, 5, 1)).is_none(),
            "upper bound should reject current key"
        );
    }

    #[test]
    fn test_iter_leaf_key_count() {
        let (root, cache) = make_root_with_cache();
        let target = BtreeKey::new(100, 1, KeyType::Normal);
        let iter = test_iter_init(
            &root,
            &target,
            IterFlags::default(),
            &cache,
            crate::btree::BtreeId::Extents,
        );
        assert_eq!(iter.leaf_key_count(), 0);
    }

    // ─── 快照可见性过滤测试 ─────────────────────────────

    /// 测试快照过滤：从 s2 看（可见：s2 自身及其后代 s3）
    #[test]
    fn test_iter_snapshot_filter_s2() {
        let (mut root, cache) = make_root_with_cache();
        let leaf = Arc::get_mut(&mut root.node).unwrap();

        // 使用 btree 创建快照树: root → s2 → s3, root → s4
        let mut vol = BchVol::test_trees();
        let mut trans = make_trans(&mut vol);
        let root_id = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut trans, 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        let s2 = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 2];
            bch2_snapshot_node_create(&mut trans, root_id, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };
        let s4 = bch2_snapshot_read_value(&trans, root_id).unwrap().children[1];
        let s3 = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut trans, s2, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };
        drop(trans);

        // 插入不同快照的 key
        leaf.insert(BtreeKey::new(10, s3, KeyType::Normal), BchVal::new(100, 0));
        leaf.insert(BtreeKey::new(20, s2, KeyType::Normal), BchVal::new(200, 0));
        leaf.insert(
            BtreeKey::new(30, s2, KeyType::Whiteout),
            BchVal::new(300, 0),
        );
        leaf.insert(
            BtreeKey::new(40, root_id, KeyType::Normal),
            BchVal::new(400, 0),
        );
        leaf.insert(BtreeKey::new(50, s4, KeyType::Normal), BchVal::new(500, 0));

        let target = BtreeKey::MIN_KEY;
        let mut iter = test_iter_init(
            &root,
            &target,
            IterFlags::default(),
            &cache,
            crate::btree::BtreeId::Extents,
        );
        iter.set_snapshot_filter(s2);

        // s2 可见: {s2, root_id（祖先）} → 20@s2, 40@root_id
        // s3 是 s2 的后代，不可见（bcachefs: 子继承父，不反向）
        let first = iter.peek_visible(&vol);
        assert!(first.is_some(), "should find first visible entry");
        assert_eq!(
            first.unwrap().0,
            BtreeKey::new(20, s2, KeyType::Normal),
            "first visible from s2 should be 20@s2"
        );

        // advance_visible → 下一个可见应该是 40@root_id（祖先）
        assert!(iter.advance_visible(&vol), "should advance to next visible");
        let second = iter.peek_visible(&vol);
        assert!(second.is_some(), "should find second visible");
        assert_eq!(
            second.unwrap().0,
            BtreeKey::new(40, root_id, KeyType::Normal),
            "second visible from s2 should be 40@root_id (ancestor)"
        );

        // 再 advance → 没有更多可见了
        assert!(
            !iter.advance_visible(&vol),
            "should have no more visible entries from s2"
        );
        assert!(
            iter.peek_visible(&vol).is_none(),
            "peek_visible should be None at end"
        );
    }

    /// 测试快照过滤：从 s4 看（可见：s4 自身）
    #[test]
    fn test_iter_snapshot_filter_s4() {
        let (mut root, cache) = make_root_with_cache();
        let leaf = Arc::get_mut(&mut root.node).unwrap();

        let mut vol = BchVol::test_trees();
        let mut trans = make_trans(&mut vol);
        let root_id = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut trans, 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        let s2 = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 2];
            bch2_snapshot_node_create(&mut trans, root_id, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };
        let s4 = bch2_snapshot_read_value(&trans, root_id).unwrap().children[1];
        let s3 = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut trans, s2, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };
        drop(trans);

        leaf.insert(BtreeKey::new(10, s3, KeyType::Normal), BchVal::new(100, 0));
        leaf.insert(BtreeKey::new(20, s2, KeyType::Normal), BchVal::new(200, 0));
        leaf.insert(
            BtreeKey::new(30, s2, KeyType::Whiteout),
            BchVal::new(300, 0),
        );
        leaf.insert(
            BtreeKey::new(40, root_id, KeyType::Normal),
            BchVal::new(400, 0),
        );
        leaf.insert(BtreeKey::new(50, s4, KeyType::Normal), BchVal::new(500, 0));

        let mut iter = test_iter_init(
            &root,
            &BtreeKey::MIN_KEY,
            IterFlags::default(),
            &cache,
            crate::btree::BtreeId::Extents,
        );
        iter.set_snapshot_filter(s4);

        // s4 可见: {s4, root_id（祖先）} → 40@root_id, 50@s4
        let first = iter.peek_visible(&vol);
        assert!(first.is_some(), "should find visible entry from s4");
        assert_eq!(
            first.unwrap().0,
            BtreeKey::new(40, root_id, KeyType::Normal),
            "first visible from s4 should be 40@root_id (ancestor)"
        );

        assert!(
            iter.advance_visible(&vol),
            "should advance to s4's own entry"
        );
        let second = iter.peek_visible(&vol);
        assert!(second.is_some(), "should find second visible entry");
        assert_eq!(
            second.unwrap().0,
            BtreeKey::new(50, s4, KeyType::Normal),
            "second visible from s4 should be 50@s4"
        );

        assert!(
            !iter.advance_visible(&vol),
            "should have no more visible from s4"
        );
    }

    /// 测试快照过滤：从根快照看（可见：所有后代快照）
    #[test]
    fn test_iter_snapshot_filter_root() {
        let (mut root, cache) = make_root_with_cache();
        let leaf = Arc::get_mut(&mut root.node).unwrap();

        let mut vol = BchVol::test_trees();
        let mut trans = make_trans(&mut vol);
        let root_id = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut trans, 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        let s2 = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 2];
            bch2_snapshot_node_create(&mut trans, root_id, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };
        let s4 = bch2_snapshot_read_value(&trans, root_id).unwrap().children[1];
        let s3 = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut trans, s2, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };
        drop(trans);

        leaf.insert(BtreeKey::new(10, s3, KeyType::Normal), BchVal::new(100, 0));
        leaf.insert(BtreeKey::new(20, s2, KeyType::Normal), BchVal::new(200, 0));
        leaf.insert(
            BtreeKey::new(30, s2, KeyType::Whiteout),
            BchVal::new(300, 0),
        );
        leaf.insert(
            BtreeKey::new(40, root_id, KeyType::Normal),
            BchVal::new(400, 0),
        );
        leaf.insert(BtreeKey::new(50, s4, KeyType::Normal), BchVal::new(500, 0));

        let mut iter = test_iter_init(
            &root,
            &BtreeKey::MIN_KEY,
            IterFlags::default(),
            &cache,
            crate::btree::BtreeId::Extents,
        );
        iter.set_snapshot_filter(root_id);

        // root 可见: 只有 root 自身的条目（40@root），后代不可见
        // bcachefs 语义：子继承父，父不反看子
        let entries: Vec<BtreeKey> = {
            let mut v = Vec::new();
            loop {
                let entry = iter.peek_visible(&vol);
                match entry {
                    Some((k, _)) => {
                        v.push(k);
                        if !iter.advance_visible(&vol) {
                            break;
                        }
                    }
                    None => break,
                }
            }
            v
        };

        assert_eq!(entries.len(), 1, "root should see only its own entry");
        assert_eq!(entries[0], BtreeKey::new(40, root_id, KeyType::Normal));
    }

    /// 测试无过滤时 peek_visible 向后兼容（仅跳过 Whiteout）
    #[test]
    fn test_iter_peek_visible_no_filter() {
        let (mut root, cache) = make_root_with_cache();
        let leaf = Arc::get_mut(&mut root.node).unwrap();

        leaf.insert(BtreeKey::new(10, 3, KeyType::Normal), BchVal::new(100, 0));
        leaf.insert(BtreeKey::new(20, 2, KeyType::Whiteout), BchVal::new(200, 0));
        leaf.insert(BtreeKey::new(30, 1, KeyType::Normal), BchVal::new(300, 0));

        let vol = BchVol::test_trees();
        let mut iter = test_iter_init(
            &root,
            &BtreeKey::MIN_KEY,
            IterFlags::default(),
            &cache,
            crate::btree::BtreeId::Extents,
        );
        // 无过滤时 peek_visible 应跳过 Whiteout
        // 第一个 entry 是 10@3 (Normal) → 直接返回
        let first = iter.peek_visible(&vol);
        assert!(
            first.is_some(),
            "peek_visible without filter should find first entry"
        );
        assert_eq!(first.unwrap().0, BtreeKey::new(10, 3, KeyType::Normal));

        // advance_visible → 跳过 Whiteout 到 30@1
        assert!(iter.advance_visible(&vol), "should advance past whiteout");
        let second = iter.peek_visible(&vol);
        assert!(second.is_some(), "second entry should exist");
        assert_eq!(second.unwrap().0, BtreeKey::new(30, 1, KeyType::Normal));

        // 再 advance → 结束
        assert!(!iter.advance_visible(&vol), "no more entries");
        assert!(iter.peek_visible(&vol).is_none(), "should be at end");
    }

    /// 多级树遍历测试：手动构造 2 层 B+tree，验证 iter 能正确下降到 leaf
    #[test]
    fn test_iter_multi_level_traversal() {
        use crate::btree::key::KeyType;
        use crate::btree::node::BsetTree;

        let cache = Arc::new(NodeCache::new());

        // 创建两个 leaf 节点（先裸节点插入，再包 Arc）
        let mut left_node = BtreeNode::new_leaf();
        left_node.insert(BtreeKey::new(10, 1, KeyType::Normal), BchVal::new(100, 0));
        left_node.insert(BtreeKey::new(20, 1, KeyType::Normal), BchVal::new(200, 0));
        left_node.insert(BtreeKey::new(30, 1, KeyType::Normal), BchVal::new(300, 0));
        let left = Arc::new(left_node);

        let mut right_node = BtreeNode::new_leaf();
        right_node.insert(BtreeKey::new(40, 1, KeyType::Normal), BchVal::new(400, 0));
        right_node.insert(BtreeKey::new(50, 1, KeyType::Normal), BchVal::new(500, 0));
        let right = Arc::new(right_node);

        let left_addr = 1;
        let right_addr = 2;
        cache.insert(left_addr, left.clone());
        cache.insert(right_addr, right.clone());

        // 创建 internal 根节点（depth=1）
        let mut internal = BtreeNode::new_internal();
        // entry 0: (MIN_KEY, ptr_to_left)
        let left_min = BtreeKey::MIN_KEY;
        let left_val = BchVal::new(left_addr, 0);
        let mut cur = u32::from(crate::btree::node::BSET_HEADER_U64S) * 8;
        cur += internal.write_entry(cur, &left_min, &left_val, 0);
        // entry 1: (key=40, ptr_to_right)
        let median = BtreeKey::new(40, 1, KeyType::Normal);
        let right_val = BchVal::new(right_addr, 0);
        cur += internal.write_entry(cur, &median, &right_val, 0);
        internal.sets[0] = BsetTree {
            size: 0,
            extra: crate::btree::node::BSET_NO_AUX_TREE_VAL,
            data_offset: 0,
            aux_data_offset: u16::MAX,
            end_offset: (cur / 8) as u16,
        };
        internal.packed_keys = 2;
        internal.unpacked_keys = 0;

        let root = BtreeRoot {
            node: Arc::new(internal),
            depth: 1,
        };

        // 测试：查找 key=20（应该在左叶子）
        let iter = test_iter_init(
            &root,
            &BtreeKey::new(20, 1, KeyType::Normal),
            IterFlags::default(),
            &cache,
            crate::btree::BtreeId::Extents,
        );
        let result = iter.peek();
        assert!(result.is_some(), "should find key=20");
        assert_eq!(result.unwrap().0, BtreeKey::new(20, 1, KeyType::Normal));
        assert_eq!(result.unwrap().1, BchVal::new(200, 0));

        // 测试：查找 key=50（应该在右叶子）
        let iter = test_iter_init(
            &root,
            &BtreeKey::new(50, 1, KeyType::Normal),
            IterFlags::default(),
            &cache,
            crate::btree::BtreeId::Extents,
        );
        let result = iter.peek();
        assert!(result.is_some(), "should find key=50");
        assert_eq!(result.unwrap().0, BtreeKey::new(50, 1, KeyType::Normal));
        assert_eq!(result.unwrap().1, BchVal::new(500, 0));

        // 测试：查找 key=35（左叶子中没有 ≥35 的 key，继续到右叶子的 key=40）
        let iter = test_iter_init(
            &root,
            &BtreeKey::new(35, 1, KeyType::Normal),
            IterFlags::default(),
            &cache,
            crate::btree::BtreeId::Extents,
        );
        let result = iter.peek();
        assert!(result.is_some(), "should advance to the next leaf");
        assert_eq!(result.unwrap().0, BtreeKey::new(40, 1, KeyType::Normal));
    }

    // ─── R2: restart_optimized 测试 ─────────────────────────

    /// 测试 restart_optimized: seq 未变时跳过重下降
    ///
    /// 新创建的 iter locked_seq 默认为 0，SixLock seq 也为 0，
    /// 因此 restart_optimized 应检测到 seq 未变 → 返回 false。
    #[test]
    fn test_restart_optimized_skips_when_seq_unchanged() {
        let (root, cache) = make_root_with_cache();
        let target = BtreeKey::new(100, 1, KeyType::Normal);
        let mut iter = test_iter_init(
            &root,
            &target,
            IterFlags::default(),
            &cache,
            crate::btree::BtreeId::Extents,
        );

        // locked_seq 默认为 0，与锁的当前 seq(0) 匹配
        let skipped = iter.restart_optimized(&root);
        assert!(!skipped, "should skip restart when seq unchanged");
        // 锁状态应被重置
        for level in &iter_path(&iter).levels {
            if let BtreePathNode::Node(level) = level {
                assert_eq!(
                    level.lock_state,
                    BtreeNodeLockedType::None,
                    "lock should be released"
                );
            }
        }
        assert!(!iter.had_restart, "had_restart should be false after skip");
    }

    /// 测试 restart_optimized: seq 变化时执行完整 restart
    ///
    /// 对节点执行 lock_write + unlock_write 会递增 seq，然后
    /// restart_optimized 应检测到 seq 变化 → 回退到完整 restart。
    #[test]
    fn test_restart_optimized_falls_back_when_seq_changed() {
        let (root, cache) = make_root_with_cache();
        let target = BtreeKey::new(100, 1, KeyType::Normal);
        let mut iter = test_iter_init(
            &root,
            &target,
            IterFlags::default(),
            &cache,
            crate::btree::BtreeId::Extents,
        );

        // 对 leaf 节点执行写操作，递增 seq
        let leaf = match &iter_path(&iter).levels[0] {
            BtreePathNode::Node(level) => &level.node,
            _ => panic!("expected leaf node in iter path"),
        };
        leaf.lock.six_lock_intent();
        // 对应 bcachefs bch2_btree_node_lock_write_contended — 先排除自身读者再升级写锁
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

        // locked_seq 仍是 0 → 不匹配 → 回退到完整 restart
        let restarted = iter.restart_optimized(&root);
        assert!(
            restarted,
            "should fall back to full restart when seq changed"
        );
        assert!(iter.had_restart, "had_restart should be true after restart");
    }

    /// 测试 restart_optimized: 空路径（无 path）返回 false
    #[test]
    fn test_restart_optimized_empty_path() {
        let (root, cache) = make_root_with_cache();
        // 创建一个空 iter（无 path）
        let flags = IterFlags {
            intent: false,
            forward: true,
            with_journal: false,
            cached: false,
            nopreserve: false,
        };
        let mut iter = test_iter_init(
            &root,
            &BtreeKey::new(100, 1, KeyType::Normal),
            flags,
            &cache,
            crate::btree::BtreeId::Extents,
        );
        // 有效 iter 至少有一个 path level，所以这里走正常路径
        // 对于一个不存在的场景（空 path）—— 通常是 init 永远不会产生空 path
        // 我们通过直接设置 path 为空来测试边界
        let path = iter_path_mut(&mut iter);
        path.levels = std::array::from_fn(|_| BtreePathNode::Error(BtreePathError::Init));
        path.nodes_locked = 0;
        let result = iter.restart_optimized(&root);
        // 空 path: leaf_unchanged = false, 回退到 restart
        assert!(result, "empty path should fall back to restart");
        // restart 后应有 path
        assert!(matches!(iter_path(&iter).levels[0], BtreePathNode::Node(_)));
    }

    /// 验证 snapshot_visible_cache 在多次 peek_visible 调用间共享
    ///
    /// 只要 snapshot 过滤器不变，同一 (snapshot, key_sid) 对
    /// 应被缓存，不会在第二次出现时重复查询 Snapshots btree。
    /// 注意：使用子快照的 key（key_sid != filter_snapshot），
    /// 这样才会触发祖先关系检查，进入缓存路径。
    #[test]
    fn test_snapshot_visible_cache_shared_across_calls() {
        let (mut root, cache) = make_root_with_cache();
        let leaf = Arc::get_mut(&mut root.node).unwrap();

        let mut vol = BchVol::test_trees();
        let mut trans = make_trans(&mut vol);
        let root_id = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut trans, 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        let s2 = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut trans, root_id, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };
        let s3 = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut trans, s2, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };
        drop(trans);

        leaf.insert(
            BtreeKey::new(10, root_id, KeyType::Normal),
            BchVal::new(100, 0),
        );
        leaf.insert(
            BtreeKey::new(20, root_id, KeyType::Normal),
            BchVal::new(200, 0),
        );

        let mut iter = test_iter_init(
            &root,
            &BtreeKey::MIN_KEY,
            IterFlags::default(),
            &cache,
            crate::btree::BtreeId::Extents,
        );
        iter.set_snapshot_filter(s3);

        // s3 过滤: root_id 是 s3 的祖先 → 可见（触发 cache 路径：key_sid != filter_snapshot）
        let first = iter.peek_visible(&vol);
        assert!(first.is_some(), "first peek should find entry");
        assert_eq!(
            first.as_ref().unwrap().0,
            BtreeKey::new(10, root_id, KeyType::Normal)
        );

        assert!(iter.advance_visible(&vol), "should advance to next");
        let second = iter.peek_visible(&vol);
        assert!(second.is_some(), "second peek should find entry");
        assert_eq!(
            second.as_ref().unwrap().0,
            BtreeKey::new(20, root_id, KeyType::Normal)
        );

        assert!(
            !iter.snapshot_visible_cache.is_empty(),
            "cache should have entries after peek_visible calls"
        );
    }

    /// 验证 set_snapshot_filter 切换 snapshot 时清空缓存
    #[test]
    fn test_snapshot_visible_cache_cleared_on_filter_change() {
        let (mut root, cache) = make_root_with_cache();
        let leaf = Arc::get_mut(&mut root.node).unwrap();

        let mut vol = BchVol::test_trees();
        let mut trans = make_trans(&mut vol);
        let root_id = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut trans, 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        let s2 = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 2];
            bch2_snapshot_node_create(&mut trans, root_id, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };
        let s4 = bch2_snapshot_read_value(&trans, root_id).unwrap().children[1];
        drop(trans);

        // s2 和 s4 下的 key 各一
        leaf.insert(BtreeKey::new(10, s2, KeyType::Normal), BchVal::new(100, 0));
        leaf.insert(BtreeKey::new(20, s4, KeyType::Normal), BchVal::new(200, 0));

        let mut iter = test_iter_init(
            &root,
            &BtreeKey::MIN_KEY,
            IterFlags::default(),
            &cache,
            crate::btree::BtreeId::Extents,
        );

        // s2 过滤 → 看到 10@s2
        iter.set_snapshot_filter(s2);
        let first = iter.peek_visible(&vol);
        assert!(first.is_some());
        assert_eq!(
            first.as_ref().unwrap().0,
            BtreeKey::new(10, s2, KeyType::Normal)
        );

        // 切换过滤 → 应清空缓存 → 看到 20@s4
        iter.set_snapshot_filter(s4);
        let first_s4 = iter.peek_visible(&vol);
        assert!(first_s4.is_some());
        assert_eq!(
            first_s4.as_ref().unwrap().0,
            BtreeKey::new(20, s4, KeyType::Normal)
        );
    }

    #[test]
    fn test_visible_range_with_entry_preserves_extent_device() {
        let (mut root, cache) = make_root_with_cache();
        let leaf = Arc::get_mut(&mut root.node).unwrap();
        let key = BtreeKey::new(10, 0, KeyType::Normal);
        assert!(leaf.insert_entry(&BtreeEntry::new(
            Bpos::from_key(&key),
            KeyType::Normal,
            KeyValue::Extent(ExtentValue {
                paddr: 100,
                size: 2,
                ver: 7,
                crc32c: 0,
                crc_offset_blocks: 0,
                dev_idx: 3,
            }),
        )));

        let vol = BchVol::test_trees();
        let mut iter = test_iter_init(
            &root,
            &BtreeKey::MIN_KEY,
            IterFlags::default(),
            &cache,
            BtreeId::Extents,
        );
        let (_, value, raw, start, end) = iter
            .peek_visible_range_with_entry(&vol)
            .expect("extent should be visible");
        assert_eq!(value, BchVal::new(100, 7));
        assert_eq!((start, end), (10, 12));
        let mut ptrs = Vec::new();
        raw.for_each_ptr(|ptr| ptrs.push(*ptr));
        assert_eq!(ptrs.len(), 1);
        assert_eq!(ptrs[0].dev, 3);
        assert_eq!(ptrs[0].offset, 100);
    }
}
