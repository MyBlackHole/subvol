//! 内存快照表 —— 从 Snapshots btree 加载，提供 O(1) 查询和三阶祖先判定
//!
//! 对齐 bcachefs `struct snapshot_table`：
//! - 条目按 `U32_MAX - id` 索引存放在 Vec 中（O(1) 查找）
//! - 祖先查询三阶策略：skip list → is_ancestor bitmap → 父节点线性遍历
//! - 位图在表构建时从父链计算生成，写入时作废
//!
//! # 快照树表
//!
//! `SnapshotTreeTable` 从 SnapshotTrees btree 加载，按 tree_id 索引。
//! 对应 bcachefs 的 `struct snapshot_tree` 数组。

use super::meta::SnapshotIdState;
use super::meta::SnapshotT;
use super::meta::SnapshotTreeT;
use crate::btree::key::KeyValue;
use crate::btree::BtreeId;
use crate::BchVol;

pub trait SnapshotBtreeView {
    fn btree(&self, id: BtreeId) -> &crate::btree::Btree;
}

impl SnapshotBtreeView for BchVol {
    fn btree(&self, id: BtreeId) -> &crate::btree::Btree {
        self.btree(id)
    }
}

/// 128 位祖先位图大小（对齐 bcachefs IS_ANCESTOR_BITMAP）
const IS_ANCESTOR_BITMAP: u32 = 128;

/// 内存快照表。
///
/// 对齐 bcachefs `struct snapshot_table`。
///
/// 条目按 `U32_MAX - id` 索引存放在连续 Vec 中。ID 从 `u32::MAX` 向下分配，
/// 因此索引从 0 开始连续递增，实现 O(1) 查找。
///
/// 祖先查询三阶策略（复杂度从低到高）：
/// 1. **Skip list**：`skip[2]` → `skip[1]` → `skip[0]`，O(log depth) 跳升
/// 2. **位图**：128 位 `is_ancestor` 位图，O(1) 判定 128 范围内的祖先关系
/// 3. **父链**：线性回退，最坏情况 O(depth)
///
/// 构建时全量扫描 Snapshots btree，写入时需重建（当前不实现增量更新）。
#[derive(Debug, Clone, Default)]
pub struct SnapshotTable {
    /// 快照条目，按 `(U32_MAX - id)` 索引。
    /// 有效条目仅当索引在范围内且 `state == Live`。
    entries: Vec<SnapshotT>,
}

impl SnapshotTable {
    /// 从 Snapshots btree 构建快照表。
    ///
    /// 扫描整个 Snapshots btree，反序列化每个条目，
    /// 按 ID 存放到对应索引位置，然后为每个条目计算祖先位图。
    ///
    /// 使用 HashMap 处理重复 Bpos 条目（Normal + Whiteout），
    /// 后写入的条目覆盖先写入的（最后写入获胜），与 `bch2_snapshot_list` 一致。
    pub fn build<V: SnapshotBtreeView>(vol: &V) -> Self {
        // 第一遍：从 btree 收集所有条目，用 HashMap 处理重复（去重）
        use std::collections::HashMap;
        let mut map: HashMap<u32, SnapshotT> = HashMap::new();
        let btree = vol.btree(BtreeId::Snapshots);
        btree.for_each_btree_key_entry(|entry| {
            let sid = entry.pos.snapshot;
            let bytes = match &entry.value {
                KeyValue::Raw(b) => b.clone(),
                _ => return,
            };
            if let Ok(snap) = bincode::deserialize::<SnapshotT>(&bytes) {
                map.insert(sid, snap);
            }
        });

        // 注意：不过滤待删除条目，因为 bcachefs 的 WILL_DELETE 节点仍然留在 live table 中。
        // get() 只排除非 live 状态，id_state() 通过 state 字段区分。

        if map.is_empty() {
            return Self {
                entries: Vec::new(),
            };
        }

        // 收集整理为 Vec
        let snapshots: Vec<(u32, SnapshotT)> = map.into_iter().collect();

        // 找出最小 ID 以确定表大小
        let min_id = snapshots.iter().map(|(id, _)| *id).min().unwrap();
        let table_size = (u32::MAX - min_id + 1) as usize;

        // 用占位条目填充 Vec
        let mut entries = vec![SnapshotT::deleted_placeholder(); table_size];

        // 将实际条目放入对应位置
        for (id, snap) in &snapshots {
            let idx = (u32::MAX - id) as usize;
            entries[idx] = snap.clone();
        }

        // 为每个条目计算祖先位图
        Self::build_bitmaps(&mut entries);

        Self { entries }
    }

    /// 根据快照 ID 查找条目。
    ///
    /// 返回 `None` 如果 ID 超出范围或条目已被删除。
    pub fn get(&self, id: u32) -> Option<&SnapshotT> {
        let idx = (u32::MAX - id) as usize;
        if idx < self.entries.len() && self.entries[idx].state == SnapshotIdState::Live {
            Some(&self.entries[idx])
        } else {
            None
        }
    }

    /// 按快照 ID 获取原始条目，保留已删除节点。
    ///
    /// 与 `get()` 不同，此方法会返回 `Deleted` 状态的快照，
    /// 但不会返回未分配的空槽。
    pub fn get_any(&self, id: u32) -> Option<&SnapshotT> {
        let idx = (u32::MAX - id) as usize;
        if idx < self.entries.len() && self.entries[idx].state != SnapshotIdState::Empty {
            Some(&self.entries[idx])
        } else {
            None
        }
    }

    /// 确保表中包含指定 ID 的槽位。
    fn ensure_slot(&mut self, id: u32) -> usize {
        let idx = (u32::MAX - id) as usize;
        if idx >= self.entries.len() {
            self.entries
                .resize(idx + 1, SnapshotT::deleted_placeholder());
        }
        idx
    }

    /// 写入或覆盖指定快照 ID。
    pub fn set(&mut self, id: u32, snap: SnapshotT) {
        let idx = self.ensure_slot(id);
        self.entries[idx] = snap;
    }

    /// 从表中移除指定快照 ID。
    pub fn remove(&mut self, id: u32) {
        let idx = (u32::MAX - id) as usize;
        if idx < self.entries.len() {
            self.entries[idx] = SnapshotT::deleted_placeholder();
        }
    }

    /// 找到下一个可分配的快照 ID。
    ///
    /// 优先返回当前表中从高位向下遇到的第一个空槽；如果没有空槽，
    /// 则返回当前最小已使用 ID 的前一个值。
    pub fn next_empty_id(&self) -> u32 {
        for (idx, snap) in self.entries.iter().enumerate() {
            if snap.state == SnapshotIdState::Empty {
                return u32::MAX - idx as u32;
            }
        }

        self.entries
            .iter()
            .rposition(|snap| snap.state != SnapshotIdState::Empty)
            .map(|idx| (u32::MAX - idx as u32).wrapping_sub(1))
            .unwrap_or(u32::MAX)
    }

    /// 返回当前快照树根节点 ID。
    pub fn root_id(&self) -> Option<u32> {
        self.entries.iter().enumerate().find_map(|(idx, snap)| {
            (snap.state == SnapshotIdState::Live && snap.parent == 0).then(|| u32::MAX - idx as u32)
        })
    }

    /// 返回快照的父节点 ID（`None` 表示根节点）。
    ///
    /// 对齐 bcachefs `bch2_snapshot_parent()`。
    pub fn parent(&self, id: u32) -> Option<u32> {
        let snap = self.get(id)?;
        if snap.parent == 0 {
            None
        } else {
            Some(snap.parent)
        }
    }

    /// 从任意快照节点找到树的根节点 ID。
    ///
    /// 对齐 bcachefs `bch2_snapshot_root()`。
    /// 沿父链上溯到 `parent == 0` 的节点。
    pub fn root(&self, id: u32) -> Option<u32> {
        let mut current = id;
        if self.get(current).is_none() {
            return None;
        }
        loop {
            let snap = self.get(current)?;
            if snap.parent == 0 {
                return Some(current);
            }
            current = snap.parent;
        }
    }

    /// 返回快照的子节点数组 `[left, right]`。
    pub fn children(&self, id: u32) -> [u32; 2] {
        self.get(id).map_or([0, 0], |s| s.children)
    }

    /// 返回快照的深度。
    ///
    /// 对齐 bcachefs `bch2_snapshot_depth()`。
    pub fn depth(&self, id: u32) -> Option<u32> {
        self.get(id).map(|s| s.depth)
    }

    /// 检查快照是否存在且活跃。
    ///
    /// 对齐 bcachefs `bch2_snapshot_exists()`。
    pub fn exists(&self, id: u32) -> bool {
        self.get(id).is_some()
    }

    /// 返回快照 ID 的生命周期状态。
    ///
    /// 对齐 bcachefs `bch2_snapshot_id_state()`：
    /// - `Live`：快照存在且活跃
    /// - `Deleted`：快照已标记删除
    /// - `Empty`：快照 ID 未使用/不存在
    pub fn id_state(&self, id: u32) -> SnapshotIdState {
        let idx = (u32::MAX - id) as usize;
        if idx < self.entries.len() {
            self.entries[idx].state
        } else {
            SnapshotIdState::Empty
        }
    }

    /// 检查 `ancestor` 是否为 `id` 的祖先。
    ///
    /// 参数顺序对齐 bcachefs `bch2_snapshot_is_ancestor(trans, id, ancestor)`：
    /// `id` 是后代，`ancestor` 是潜在的祖先。
    ///
    /// 三阶策略：
    /// 1. `ancestor >= 128` → 用 skip list（`skip[2→1→0]`）跳到 `ancestor - 128` 范围
    /// 2. 在位图范围内 → 检查 128 位 `is_ancestor` 位图（O(1)）
    ///
    /// 对齐 bcachefs `__bch2_snapshot_is_ancestor()`：
    /// 位图命中直接返回，未命中则认为不存在祖先关系。
    pub fn is_ancestor(&self, id: u32, ancestor: u32) -> bool {
        if id == ancestor {
            return true;
        }
        // bcachefs: 父 ID > 子 ID，所以 ancestor > id
        if ancestor <= id || id == 0 {
            return false;
        }

        let mut current = id;

        // 阶段一：Skip list 跳跃 —— 对距离超过 128 的祖先用跳表
        if ancestor >= IS_ANCESTOR_BITMAP {
            while current != 0 && current < ancestor - IS_ANCESTOR_BITMAP {
                current = self.get_ancestor_below(current, ancestor);
                if current == 0 {
                    return false;
                }
            }
        }

        // 阶段二：位图判定 —— 在 128 范围内用位图。
        if current != 0 && current < ancestor {
            return self.test_ancestor_bitmap(current, ancestor);
        }

        current == ancestor
    }

    /// 获取不超过 `ancestor` 的最远 skip list 祖先。
    ///
    /// 尝试顺序：`skip[2]` → `skip[1]` → `skip[0]` → `parent`
    /// 条件：跳表值不为 0 且 `<= ancestor`（保证不跳过目标）。
    ///
    /// 对齐 bcachefs `get_ancestor_below()`。
    fn get_ancestor_below(&self, id: u32, ancestor: u32) -> u32 {
        let snap = match self.get(id) {
            Some(s) => s,
            None => return 0,
        };

        if snap.skip[2] != 0 && snap.skip[2] <= ancestor {
            return snap.skip[2];
        }
        if snap.skip[1] != 0 && snap.skip[1] <= ancestor {
            return snap.skip[1];
        }
        if snap.skip[0] != 0 && snap.skip[0] <= ancestor {
            return snap.skip[0];
        }
        snap.parent
    }

    /// 检查位图中 `ancestor` 是否为 `id` 的祖先。
    ///
    /// 位 `(ancestor - id - 1)` 置位表示 `ancestor` 是 `id` 的祖先。
    ///
    /// 对齐 bcachefs `test_ancestor_bitmap()`。
    fn test_ancestor_bitmap(&self, id: u32, ancestor: u32) -> bool {
        let bit = (ancestor - id - 1) as usize;
        if bit >= IS_ANCESTOR_BITMAP as usize {
            return false;
        }
        self.get(id)
            .map_or(false, |s| (s.is_ancestor >> bit) & 1 == 1)
    }

    /// 为表中所有条目构建祖先位图。
    ///
    /// 对每个条目，沿父链上溯最多 `IS_ANCESTOR_BITMAP` 步，
    /// 为遇到的每个祖先设置对应位 `(ancestor - id - 1)`。
    ///
    /// 对齐 bcachefs 中 `bch2_mark_snapshot()` 的 bitmap 构建逻辑。
    fn build_bitmaps(entries: &mut [SnapshotT]) {
        let len = entries.len();
        for idx in 0..len {
            let id = u32::MAX - idx as u32;
            if entries[idx].state != SnapshotIdState::Live {
                continue;
            }
            let mut is_ancestor = 0u128;
            let mut current = entries[idx].parent;
            let mut count = 0;
            while current != 0 && (count as u32) < IS_ANCESTOR_BITMAP {
                let bit = (current - id - 1) as usize;
                if bit < IS_ANCESTOR_BITMAP as usize {
                    is_ancestor |= 1u128 << bit;
                }
                let cur_idx = (u32::MAX - current) as usize;
                if cur_idx >= len {
                    break;
                }
                current = entries[cur_idx].parent;
                count += 1;
            }
            entries[idx].is_ancestor = is_ancestor;
        }
    }
}

// ═══════════════════════════════════════════════════════════════
// SnapshotTreeTable — 从 SnapshotTrees btree 加载
// ═══════════════════════════════════════════════════════════════

/// 内存快照树表，从 SnapshotTrees btree 加载，按 tree_id 索引。
///
/// 对齐 bcachefs 的 `struct snapshot_tree` 数组。
/// tree_id 从 1 开始连续分配，Vec 索引 = tree_id - 1。
#[derive(Debug, Clone)]
pub struct SnapshotTreeTable {
    entries: Vec<SnapshotTreeT>,
}

impl Default for SnapshotTreeTable {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
        }
    }
}

impl SnapshotTreeTable {
    /// 从 SnapshotTrees btree 构建快照树表。
    ///
    /// 遍历 SnapshotTrees btree，反序列化每个 SnapshotTreeT 条目，
    /// 按 tree_id 存放到 Vec 中（索引 = tree_id - 1）。
    pub fn build<V: SnapshotBtreeView>(vol: &V) -> Self {
        use std::collections::HashMap;
        let mut map: HashMap<u32, SnapshotTreeT> = HashMap::new();
        let btree = vol.btree(BtreeId::SnapshotTrees);
        btree.for_each_btree_key_entry(|entry| {
            let tree_id = entry.pos.snapshot;
            if tree_id == 0 {
                return;
            }
            let bytes = match &entry.value {
                KeyValue::Raw(b) => b.clone(),
                _ => return,
            };
            if let Ok(tree) = bincode::deserialize::<SnapshotTreeT>(&bytes) {
                map.insert(tree_id, tree);
            }
        });

        if map.is_empty() {
            return Self {
                entries: Vec::new(),
            };
        }

        let max_id = map.keys().max().copied().unwrap_or(0);
        let size = max_id as usize;
        let mut entries = vec![SnapshotTreeT::default(); size];

        for (id, tree) in map {
            let idx = (id - 1) as usize;
            if idx < entries.len() {
                entries[idx] = tree;
            }
        }

        Self { entries }
    }

    /// 根据 tree_id 查找快照树条目。
    pub fn get(&self, tree_id: u32) -> Option<&SnapshotTreeT> {
        let idx = (tree_id - 1) as usize;
        self.entries.get(idx).filter(|_| tree_id != 0)
    }

    fn ensure_slot(&mut self, tree_id: u32) -> usize {
        let idx = tree_id.saturating_sub(1) as usize;
        if idx >= self.entries.len() {
            self.entries.resize(idx + 1, SnapshotTreeT::default());
        }
        idx
    }

    pub fn set(&mut self, tree_id: u32, tree: SnapshotTreeT) {
        if tree_id == 0 {
            return;
        }
        let idx = self.ensure_slot(tree_id);
        self.entries[idx] = tree;
    }
}

// ═══════════════════════════════════════════════════════════════
// bch2_snapshots_read — 读取快照 + 快照树
// ═══════════════════════════════════════════════════════════════

/// 从 btrees 读取快照表和快照树表。
///
/// 对齐 bcachefs `bch2_snapshots_read()`。
/// 返回 `(SnapshotTable, SnapshotTreeTable)`，调用方可按需使用。
pub fn bch2_snapshots_read<V: SnapshotBtreeView>(vol: &V) -> (SnapshotTable, SnapshotTreeTable) {
    let table = SnapshotTable::build(vol);
    let tree_table = SnapshotTreeTable::build(vol);
    (table, tree_table)
}

// ═══════════════════════════════════════════════════════════════
// bch2_fs_snapshots_init / exit — 初始化与清理
// ═══════════════════════════════════════════════════════════════

/// 初始化快照子系统。
///
/// 对齐 bcachefs `bch2_fs_snapshots_init()`（snapshot.c:832-835）。
///
/// # 偏差说明
///
/// bcachefs 版本仅初始化 `create_lock`（percpu rwsem），不接触 btree。
/// subvol 版本除初始化外还确保 SnapshotTrees btree 中有默认条目（tree_id=1），
/// 该行为在 bcachefs 中由 `bch2_initialize_subvolumes()` 承担。
///
/// 若 btree 已由 initialize_subvolumes 或 recovery 填充了 SnapshotTree 条目，
/// 则直接返回成功（幂等）。
pub fn bch2_fs_snapshots_init(vol: &BchVol) -> Result<(), crate::types::StorageError> {
    let btree = vol.btree(BtreeId::SnapshotTrees);
    let has_entries = btree.root().node.packed_keys + btree.root().node.unpacked_keys > 0;

    if !has_entries {
        // 创建一个默认的 SnapshotTree 条目（tree_id=1）
        let tree_val = SnapshotTreeT::new(0, 0);
        let bytes =
            bincode::serialize(&tree_val).map_err(crate::types::StorageError::Serialization)?;
        let entry = crate::btree::key::BtreeEntry::raw(
            crate::btree::key::Bpos::new(0, 0, 1),
            crate::btree::key::KeyType::Normal,
            bytes,
        );
        vol.insert_entry_raw(BtreeId::SnapshotTrees, entry, 0);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btree::BtreeTrans;
    use crate::snap::snapshot::{
        bch2_snapshot_node_create, bch2_snapshot_node_set_deleted,
    };

    fn make_vol() -> BchVol {
        BchVol::test_trees()
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

    // ─── 构建与查找 ───

    #[test]
    fn test_build_empty() {
        let vol = make_vol();
        let table = SnapshotTable::build(&vol);
        assert!(table.get(42).is_none());
    }

    #[test]
    fn test_build_and_get() {
        let mut vol = make_vol();
        let root = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        let child = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), root, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };

        let table = SnapshotTable::build(&vol);
        let root_snap = table.get(root).expect("root should exist");
        assert_eq!(root_snap.parent, 0);
        assert_eq!(root_snap.depth, 1);

        let child_snap = table.get(child).expect("child should exist");
        assert_eq!(child_snap.parent, root);
        assert_eq!(child_snap.depth, 2);
    }

    #[test]
    fn test_get_pending_delete_returns_live_entry() {
        let mut vol = make_vol();
        let root = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        let child = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), root, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };
        bch2_snapshot_node_set_deleted(&mut make_trans(&mut vol), child).unwrap();

        let table = SnapshotTable::build(&vol);
        assert!(
            table.get(child).is_some(),
            "pending-delete should remain visible"
        );
        assert!(table.get(root).is_some(), "root should still exist");
    }

    #[test]
    fn test_get_nonexistent() {
        let vol = make_vol();
        let table = SnapshotTable::build(&vol);
        assert!(table.get(999).is_none());
    }

    // ─── parent / root / children ───

    #[test]
    fn test_parent_method() {
        let mut vol = make_vol();
        let root = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        let child = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), root, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };

        let table = SnapshotTable::build(&vol);
        assert_eq!(table.parent(root), None, "root has no parent");
        assert_eq!(table.parent(child), Some(root));
    }

    #[test]
    fn test_root_method() {
        let mut vol = make_vol();
        let root = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        let mut prev = root;
        for _ in 0..10 {
            prev = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), prev, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };
        }

        let table = SnapshotTable::build(&vol);
        assert_eq!(table.root(prev), Some(root));
        assert_eq!(table.root(root), Some(root));
    }

    #[test]
    fn test_children_method() {
        let mut vol = make_vol();
        let root = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        let child = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), root, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };

        let table = SnapshotTable::build(&vol);
        let kids = table.children(root);
        assert_eq!(kids[0], child);
        assert_ne!(kids[1], 0);
    }

    #[test]
    fn test_depth_method() {
        let mut vol = make_vol();
        let root = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        let child = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), root, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };

        let table = SnapshotTable::build(&vol);
        assert_eq!(table.depth(root), Some(1));
        assert_eq!(table.depth(child), Some(2));
        assert_eq!(table.depth(999), None);
    }

    #[test]
    fn test_exists_method() {
        let mut vol = make_vol();
        let root = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };

        let table = SnapshotTable::build(&vol);
        assert!(table.exists(root));
        assert!(!table.exists(999));
    }

    // ─── is_ancestor ───

    #[test]
    fn test_is_ancestor_self() {
        let vol = make_vol();
        let table = SnapshotTable::build(&vol);
        assert!(table.is_ancestor(42, 42));
    }

    #[test]
    fn test_is_ancestor_root_and_child() {
        let mut vol = make_vol();
        let root = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        let child = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), root, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };

        let table = SnapshotTable::build(&vol);
        assert!(
            table.is_ancestor(child, root),
            "root should be ancestor of child"
        );
        assert!(
            !table.is_ancestor(root, child),
            "child should not be ancestor of root"
        );
    }

    #[test]
    fn test_is_ancestor_chain() {
        let mut vol = make_vol();
        let root = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        let mut prev = root;
        let mut ids = vec![root];
        for _ in 0..20 {
            prev = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), prev, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };
            ids.push(prev);
        }

        let table = SnapshotTable::build(&vol);
        // 每对祖先关系
        for i in 0..ids.len() {
            for j in i..ids.len() {
                assert!(
                    table.is_ancestor(ids[j], ids[i]),
                    "{} should be ancestor of {}",
                    ids[i],
                    ids[j]
                );
            }
        }
        // 反向不是祖先
        for i in 0..ids.len() {
            for j in 0..i {
                assert!(
                    !table.is_ancestor(ids[j], ids[i]),
                    "{} should NOT be ancestor of {}",
                    ids[i],
                    ids[j]
                );
            }
        }
    }

    #[test]
    fn test_is_ancestor_bitmap_populated() {
        let mut vol = make_vol();
        let root = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        let mut prev = root;
        let mut ids = vec![root];
        for _ in 0..10 {
            prev = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), prev, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };
            ids.push(prev);
        }

        let table = SnapshotTable::build(&vol);
        // 验证最后一个快照的位图已填充
        let last = ids.last().unwrap();
        let snap = table.get(*last).unwrap();
        assert!(
            snap.is_ancestor != 0,
            "is_ancestor should be populated for depth > 1"
        );
        // 确认位图查询匹配父链查询
        for ancestor in &ids[..ids.len() - 1] {
            assert!(
                table.is_ancestor(*last, *ancestor),
                "bitmap should confirm ancestor {}",
                ancestor
            );
        }
    }

    #[test]
    fn test_is_ancestor_no_relation() {
        let mut vol = make_vol();
        let _t1 = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        let _t2 = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [2, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };

        let table = SnapshotTable::build(&vol);
        assert!(table.get(u32::MAX).is_some());
        assert!(table.get(u32::MAX - 1).is_some());
    }

    #[test]
    fn test_is_ancestor_large_tree() {
        let mut vol = make_vol();
        let root = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        let mut prev = root;
        for _ in 0..150 {
            prev = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), prev, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };
        }

        let table = SnapshotTable::build(&vol);
        // root 是最后一个节点的祖先（远距离，需要跳表 + 位图）
        assert!(
            table.is_ancestor(prev, root),
            "root should be ancestor of last"
        );
        // 中间节点
        let mid = u32::MAX - 99;
        assert!(
            table.is_ancestor(prev, mid),
            "mid should be ancestor of last"
        );
        // 反向
        assert!(
            !table.is_ancestor(mid, prev),
            "last should not be ancestor of mid"
        );
    }

    // ─── 删除后表重建 ───

    #[test]
    fn test_after_delete_rebuild() {
        let mut vol = make_vol();
        let root = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        let child = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), root, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };
        bch2_snapshot_node_set_deleted(&mut make_trans(&mut vol), child).unwrap();

        let table = SnapshotTable::build(&vol);
        assert!(table.get(child).is_some());
        assert!(table.get(root).is_some());
        // 验证重建后祖先查询仍然正确
        assert!(table.is_ancestor(root, root));
    }

    // ─── 位图正确性验证 ───

    #[test]
    fn test_bitmap_bit_positions() {
        let mut entries = vec![SnapshotT::deleted_placeholder(); 10];
        for i in 0..10 {
            let id = u32::MAX - i as u32;
            let parent = if i == 0 { 0 } else { u32::MAX - (i - 1) as u32 };
            let idx = (u32::MAX - id) as usize;
            entries[idx] = SnapshotT {
                state: crate::snap::meta::SnapshotIdState::Live,
                parent,
                children: [0, 0],
                subvol: 0,
                tree: 0,
                skip: [0, 0, 0],
                is_ancestor: 0,
                depth: (i + 1) as u32,
                btime: 0,
                deleted: false,
                flags: crate::snap::meta::BchSnapshotFlags::SUBVOL,
            };
        }

        SnapshotTable::build_bitmaps(&mut entries);

        // 验证：对 id=u32::MAX-5 (depth=6), ancestor=u32::MAX (root)
        // ancestor - id - 1 = (u32::MAX) - (u32::MAX-5) - 1 = 4
        let leaf_idx = 5; // u32::MAX - 5
        let root_idx = 0; // u32::MAX
        let is_ancestor = entries[leaf_idx].is_ancestor;
        // 位 4 应该置位（u32::MAX 是 u32::MAX-5 的祖先，间隔 5）
        assert!(
            (is_ancestor >> 4) & 1 == 1,
            "bit 4 should be set (root is 5 above)"
        );
        // 位 0 应该置位（u32::MAX-4 是 u32::MAX-5 的父节点，间隔 1）
        assert!(
            (is_ancestor >> 0) & 1 == 1,
            "bit 0 should be set (parent is 1 above)"
        );
        let root_is_ancestor = entries[root_idx].is_ancestor;
        assert_eq!(root_is_ancestor, 0, "root should have empty is_ancestor");
    }

    // ─── SnapshotTreeTable ───

    fn make_tree_vol() -> BchVol {
        use crate::btree::key::{Bpos, BtreeEntry, KeyType};
        use crate::btree::BtreeId;
        let vol = BchVol::test_trees();
        // tree_id=1: root=u32::MAX, subvol=1
        let t1 = SnapshotTreeT::new(1, u32::MAX);
        let bytes = bincode::serialize(&t1).unwrap();
        vol.insert_entry_raw(
            BtreeId::SnapshotTrees,
            BtreeEntry::raw(Bpos::new(0, 0, 1), KeyType::Normal, bytes),
            0,
        );
        // tree_id=2: root=u32::MAX-1, subvol=2
        let t2 = SnapshotTreeT::new(2, u32::MAX - 1);
        let bytes = bincode::serialize(&t2).unwrap();
        vol.insert_entry_raw(
            BtreeId::SnapshotTrees,
            BtreeEntry::raw(Bpos::new(0, 0, 2), KeyType::Normal, bytes),
            0,
        );
        vol
    }

    #[test]
    fn test_snapshot_tree_table_build() {
        let vol = make_tree_vol();
        let table = SnapshotTreeTable::build(&vol);
        assert_eq!(table.entries.len(), 2);
        let t1 = table.get(1).expect("tree_id=1 should exist");
        assert_eq!(t1.root_snapshot, u32::MAX);
        assert_eq!(t1.master_subvol, 1);
        let t2 = table.get(2).expect("tree_id=2 should exist");
        assert_eq!(t2.root_snapshot, u32::MAX - 1);
        assert_eq!(t2.master_subvol, 2);
    }

    #[test]
    fn test_snapshot_tree_table_get_nonexistent() {
        let vol = make_tree_vol();
        let table = SnapshotTreeTable::build(&vol);
        assert!(table.get(99).is_none());
    }

    #[test]
    fn test_snapshot_tree_table_empty() {
        let vol = BchVol::test_trees();
        let table = SnapshotTreeTable::build(&vol);
        assert_eq!(table.entries.len(), 0);
    }

    // ─── bch2_snapshots_read ───

    #[test]
    fn test_bch2_snapshots_read_returns_both_tables() {
        let vol = make_tree_vol();
        let root = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&vol), 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        let (table, tree_table) = bch2_snapshots_read(&vol);
        assert!(
            table.get(root).is_some(),
            "root snapshot should be in table"
        );
        assert!(
            tree_table.get(1).is_some(),
            "tree 1 should be in tree table"
        );
    }

    // ─── bch2_fs_snapshots_init ───

    #[test]
    fn test_fs_snapshots_init_empty_btree_creates_entry() {
        let vol = BchVol::test_trees();
        bch2_fs_snapshots_init(&vol).unwrap();
        let table = SnapshotTreeTable::build(&vol);
        assert!(
            !table.entries.is_empty(),
            "init should create at least one entry"
        );
        assert!(table.get(1).is_some(), "tree_id=1 should exist after init");
    }

    #[test]
    fn test_fs_snapshots_init_nonempty_btree_noop() {
        let vol = make_tree_vol();
        // 预先计数
        let before = vol.btree(crate::btree::BtreeId::SnapshotTrees).root().node.packed_keys + vol.btree(crate::btree::BtreeId::SnapshotTrees).root().node.unpacked_keys;
        bch2_fs_snapshots_init(&vol).unwrap();
        let after = vol.btree(crate::btree::BtreeId::SnapshotTrees).root().node.packed_keys + vol.btree(crate::btree::BtreeId::SnapshotTrees).root().node.unpacked_keys;
        assert_eq!(
            before, after,
            "init should not modify non-empty SnapshotTrees btree"
        );
    }
}
