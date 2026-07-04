use std::collections::HashMap;

use rand::Rng;

use crate::btree::key::{Bpos, BtreeKey, KeyType, KeyValue};
use crate::btree::{BtreeEntry, BtreeId, BtreeTrans};
use crate::recovery::RecoveryState;
use crate::snap::meta::{BchSnapshotFlags, SnapshotIdState, SnapshotT, SnapshotTreeT};
use crate::snap::snapshot::bch2_snapshot_read_value;
use crate::subvol::ops::bch2_subvolume_get;
use crate::types::StorageError;

/// 使用 in-memory HashMap 检查 `ancestor` 是否是 `descendant` 的祖先。
/// 用于 check_snapshots pass 中的 skiplist 验证（代替 btree 读取）。
fn is_ancestor_in_map(snapshots: &HashMap<u32, SnapshotT>, ancestor: u32, descendant: u32) -> bool {
    if ancestor == descendant {
        return true;
    }
    if ancestor <= descendant || descendant == 0 {
        return false;
    }
    let mut current = descendant;
    loop {
        let snap = match snapshots.get(&current) {
            Some(s) => s,
            None => return false,
        };
        if snap.parent == 0 {
            return false;
        }
        if snap.parent == ancestor {
            return true;
        }
        current = snap.parent;
    }
}

fn root_id(snapshots: &HashMap<u32, SnapshotT>, mut id: u32) -> Result<u32, StorageError> {
    let mut seen = 0u32;
    loop {
        let snap = snapshots.get(&id).ok_or_else(|| {
            StorageError::InvalidData(format!(
                "check_snapshots: snapshot {} missing while resolving root",
                id
            ))
        })?;
        if snap.parent == 0 {
            return Ok(id);
        }
        id = snap.parent;
        seen += 1;
        if seen > 1_000_000 {
            return Err(StorageError::InvalidData(
                "check_snapshots: parent chain too deep while resolving root".to_string(),
            ));
        }
    }
}

/// 本地 skiplist 计算函数（内联替代已删除的 `bch2_snapshot_skiplist_get`）。
fn skiplist_get_for_snapshot(trans: &BtreeTrans, id: u32) -> Option<[u32; 3]> {
    let snap = bch2_snapshot_read_value(trans, id)?;
    if snap.parent == 0 {
        return Some([0, 0, 0]);
    }
    let depth = snap.depth;
    let mut rng = rand::thread_rng();
    let mut skiplist = [0u32; 3];
    for j in 0..3 {
        let n = rng.gen_range(0..depth) as u32;
        let mut current = id;
        for _ in 0..n {
            match bch2_snapshot_read_value(trans, current) {
                Some(s) if s.parent != 0 => current = s.parent,
                _ => break,
            }
        }
        skiplist[j] = current;
    }
    skiplist.sort_unstable();
    Some(skiplist)
}

/// Pass: 快照一致性验证与修复（对齐 bcachefs `bch2_check_snapshots_trans()`）
///
/// 保留 upstream 会修复的字段：
/// - depth 错误 → 重算
/// - skip 错误 → 重建 skip list
/// - tree 错误 → 重新绑定或创建 SnapshotTrees 条目
/// - 不应持有 subvol 的节点 → 清空 subvol
///
/// 对 parent / children 关系的结构性错误直接返回错误。
pub async fn run(state: &mut RecoveryState) -> Result<(), StorageError> {
    let mut t = BtreeTrans::new(&state.vol);
    bch2_check_snapshots(&mut t)
}

/// 核心验证与修复逻辑
pub(crate) fn bch2_check_snapshots(trans: &mut BtreeTrans) -> Result<(), StorageError> {
    // 1. 从 Snapshots btree 收集所有非删除条目。
    let mut snapshots: HashMap<u32, SnapshotT> = HashMap::new();
    {
        let btree = trans.btree(BtreeId::Snapshots);
        btree.for_each_btree_key_entry(|entry: BtreeEntry| {
            let snapshot_id = entry.pos.snapshot;
            if snapshot_id == 0 {
                return;
            }
            let bytes = match &entry.value {
                KeyValue::Raw(b) => b.clone(),
                _ => return,
            };
            if let Ok(snap) = bincode::deserialize::<SnapshotT>(&bytes) {
                if snap.state != SnapshotIdState::Empty {
                    snapshots.insert(snapshot_id, snap);
                }
            }
        });
    }

    if snapshots.is_empty() {
        return Ok(());
    }

    // 2. 验证 parent 链没有环。
    for &sid in snapshots.keys() {
        let mut current = sid;
        let mut steps = 0u32;
        while current != 0 {
            if steps > 1_000_000 {
                return Err(StorageError::InvalidData(format!(
                    "check_snapshots: snapshot {} parent chain exceeds max depth (cycle?)",
                    sid
                )));
            }
            let snap = snapshots.get(&current).ok_or_else(|| {
                StorageError::InvalidData(format!(
                    "check_snapshots: snapshot {} parent chain broken at {}",
                    sid, current
                ))
            })?;
            if snap.parent == current {
                return Err(StorageError::InvalidData(format!(
                    "check_snapshots: snapshot {} parent is self",
                    current
                )));
            }
            current = snap.parent;
            steps += 1;
        }
    }

    // 3. 收集 SnapshotTrees 视图。
    let mut tree_by_id: HashMap<u32, SnapshotTreeT> = HashMap::new();
    let mut tree_by_root: HashMap<u32, u32> = HashMap::new();
    {
        let btree = trans.btree(BtreeId::SnapshotTrees);
        btree.for_each_btree_key_entry(|entry: BtreeEntry| {
            let tree_id = entry.pos.snapshot;
            if tree_id == 0 {
                return;
            }
            let bytes = match &entry.value {
                KeyValue::Raw(b) => b.clone(),
                _ => return,
            };
            if let Ok(tree) = bincode::deserialize::<SnapshotTreeT>(&bytes) {
                tree_by_root.insert(tree.root_snapshot, tree_id);
                tree_by_id.insert(tree_id, tree);
            }
        });
    }

    // 4. 逐项检查并收集修复。倒序遍历，保证 parent 先于 child。
    let mut ids: Vec<u32> = snapshots.keys().copied().collect();
    ids.sort_unstable_by(|a, b| b.cmp(a));

    let mut fixes: Vec<(u32, SnapshotT)> = Vec::new();
    let mut tree_fixes: Vec<(u32, SnapshotTreeT)> = Vec::new();

    for sid in ids {
        let snap = snapshots.get(&sid).cloned().ok_or_else(|| {
            StorageError::InvalidData(format!("check_snapshots: snapshot {} missing", sid))
        })?;
        let mut fixed = snap.clone();
        let mut changed = false;

        let parent_id = fixed.parent;
        if parent_id != 0 {
            let parent = snapshots.get(&parent_id).ok_or_else(|| {
                StorageError::InvalidData(format!(
                    "check_snapshots: snapshot {} parent {} not found",
                    sid, parent_id
                ))
            })?;

            if parent.children[0] != sid && parent.children[1] != sid {
                return Err(StorageError::InvalidData(format!(
                    "check_snapshots: snapshot parent {} missing pointer to child {}",
                    parent_id, sid
                )));
            }

            let expected_depth = parent.depth + 1;
            if fixed.depth != expected_depth {
                fixed.depth = expected_depth;
                changed = true;
            }
        } else if fixed.depth != 1 {
            fixed.depth = 1;
            changed = true;
        }

        let mut bad_skip = false;
        for i in 0..3 {
            let skip = snap.skip[i];
            if skip != 0 && !is_ancestor_in_map(&snapshots, skip, sid) {
                bad_skip = true;
                break;
            }
        }
        if bad_skip {
            if let Some(new_skip) = skiplist_get_for_snapshot(trans, parent_id) {
                fixed.skip = new_skip;
                changed = true;
            }
        }

        let root = root_id(&snapshots, sid)?;
        let tree_id = if let Some(tree_id) = tree_by_root.get(&root).copied() {
            tree_id
        } else {
            let new_tree_id = tree_by_id.keys().copied().max().unwrap_or(0) + 1;
            let tree_val = SnapshotTreeT::new(fixed.subvol, root);
            tree_by_root.insert(root, new_tree_id);
            tree_by_id.insert(new_tree_id, tree_val.clone());
            tree_fixes.push((new_tree_id, tree_val));
            new_tree_id
        };
        if fixed.tree != tree_id {
            fixed.tree = tree_id;
            changed = true;
        }

        let should_have_subvol = fixed.flags.contains(BchSnapshotFlags::SUBVOL) && !fixed.deleted;
        if should_have_subvol {
            if fixed.subvol == 0 {
                return Err(StorageError::InvalidData(format!(
                    "check_snapshots: snapshot {} has SUBVOL flag but subvol is 0",
                    sid
                )));
            }
            match bch2_subvolume_get(trans, fixed.subvol as u32, true) {
                Ok(subvol) if subvol.snapshot == sid => {}
                Ok(subvol) => {
                    return Err(StorageError::InvalidData(format!(
                        "check_snapshots: snapshot {} subvol {} points to {} not {}",
                        sid, fixed.subvol, subvol.snapshot, sid
                    )));
                }
                Err(_) => {
                    return Err(StorageError::InvalidData(format!(
                        "check_snapshots: snapshot {} points to missing subvol {}",
                        sid, fixed.subvol
                    )));
                }
            }
        } else if fixed.subvol != 0 {
            fixed.subvol = 0;
            changed = true;
        }

        // children 指向的子节点必须存在且反指回当前节点。
        for ci in 0..2 {
            let child_id = fixed.children[ci];
            if child_id == 0 {
                continue;
            }
            let child = snapshots.get(&child_id).ok_or_else(|| {
                StorageError::InvalidData(format!(
                    "check_snapshots: snapshot {} has nonexistent child {}",
                    sid, child_id
                ))
            })?;
            if child.parent != sid {
                return Err(StorageError::InvalidData(format!(
                    "check_snapshots: snapshot child {} has wrong parent {} (should be {})",
                    child_id, child.parent, sid
                )));
            }
        }

        if changed {
            fixes.push((sid, fixed));
        }
    }

    // 4. 应用修复到 btree
    if !fixes.is_empty() {
        for (sid, fixed_snap) in &fixes {
            let bytes = bincode::serialize(fixed_snap).map_err(StorageError::Serialization)?;
            let entry = BtreeEntry::raw(Bpos::new(0, 0, *sid), KeyType::Normal, bytes);
            trans
                .btree_mut(BtreeId::Snapshots)
                .bch2_btree_bset_insert_key_wrapper(entry, 0);
        }

        for (tree_id, tree_val) in &tree_fixes {
            let tree_bytes = bincode::serialize(tree_val).map_err(StorageError::Serialization)?;
            trans.bch2_trans_update_raw(
                BtreeId::SnapshotTrees,
                0,
                false,
                BtreeKey::new(0, *tree_id, KeyType::Normal),
                tree_bytes,
                0,
            );
        }

        trans.btree(BtreeId::Snapshots).compact();
        trans.btree(BtreeId::SnapshotTrees).compact();
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btree::key::Bpos;
    use crate::snap::meta::SnapshotT;
    use crate::snap::snapshot::{
        bch2_snapshot_node_create, bch2_snapshot_read_value, bch2_snapshot_read_value_direct,
    };

    fn make_vol() -> crate::BchVol {
        crate::BchVol::test_trees()
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

    fn make_trans<'a>(vol: &'a crate::BchVol) -> AutoApplyTrans<'a> {
        let trans = BtreeTrans::new(vol);
        AutoApplyTrans(trans)
    }

    #[test]
    fn test_check_snapshots_valid_tree() {
        let mut vol = make_vol();
        let mut trans = make_trans(&mut vol);
        let root = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut trans, 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        let _child = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [2, 0];
            bch2_snapshot_node_create(&mut trans, root, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };
        assert!(bch2_check_snapshots(&mut trans).is_ok());
        drop(trans);
    }

    #[test]
    fn test_check_snapshots_repairs_depth() {
        let mut vol = make_vol();
        let root = u32::MAX;
        let child = u32::MAX - 1;
        let root_snap = SnapshotT {
            state: SnapshotIdState::Live,
            parent: 0,
            children: [child, 0],
            subvol: 0,
            tree: 1,
            skip: [0, 0, 0],
            is_ancestor: 0,
            depth: 1,
            btime: 0,
            deleted: false,
            flags: BchSnapshotFlags::empty(),
        };
        let child_snap = SnapshotT {
            state: SnapshotIdState::Live,
            parent: root,
            children: [0, 0],
            subvol: 0,
            tree: 1,
            skip: [0, 0, 0],
            is_ancestor: 0,
            depth: 99,
            btime: 0,
            deleted: false,
            flags: BchSnapshotFlags::empty(),
        };
        let tree = SnapshotTreeT::new(0, root);
        for (id, snap) in [(root, root_snap), (child, child_snap)] {
            let bytes = bincode::serialize(&snap).unwrap();
            let entry = BtreeEntry::raw(Bpos::new(0, 0, id), KeyType::Normal, bytes);
            vol.insert_entry_raw(BtreeId::Snapshots, entry, 0);
        }
        let tree_bytes = bincode::serialize(&tree).unwrap();
        vol.insert_entry_raw(
            BtreeId::SnapshotTrees,
            BtreeEntry::raw(Bpos::new(0, 0, 1), KeyType::Normal, tree_bytes),
            0,
        );
        // 用新 trans 修复并验证
        let mut trans = make_trans(&mut vol);
        bch2_check_snapshots(&mut trans).unwrap();
        let fixed = bch2_snapshot_read_value(&trans, child).unwrap();
        assert_eq!(fixed.depth, 2, "depth should be repaired to parent.depth+1");
        drop(trans);
    }

    #[test]
    fn test_check_snapshots_repairs_skip() {
        let mut vol = make_vol();
        let root = u32::MAX;
        let child = u32::MAX - 1;
        let grandchild = u32::MAX - 2;
        let root_snap = SnapshotT {
            state: SnapshotIdState::Live,
            parent: 0,
            children: [child, 0],
            subvol: 0,
            tree: 1,
            skip: [0, 0, 0],
            is_ancestor: 0,
            depth: 1,
            btime: 0,
            deleted: false,
            flags: BchSnapshotFlags::empty(),
        };
        let child_snap = SnapshotT {
            state: SnapshotIdState::Live,
            parent: root,
            children: [grandchild, 0],
            subvol: 0,
            tree: 1,
            skip: [root, 0, 0],
            is_ancestor: 0,
            depth: 2,
            btime: 0,
            deleted: false,
            flags: BchSnapshotFlags::empty(),
        };
        let grandchild_snap = SnapshotT {
            state: SnapshotIdState::Live,
            parent: child,
            children: [0, 0],
            subvol: 0,
            tree: 1,
            skip: [u32::MAX, u32::MAX, u32::MAX],
            is_ancestor: 0,
            depth: 99,
            btime: 0,
            deleted: false,
            flags: BchSnapshotFlags::empty(),
        };
        let tree = SnapshotTreeT::new(0, root);
        for (id, snap) in [
            (root, root_snap),
            (child, child_snap),
            (grandchild, grandchild_snap),
        ] {
            let bytes = bincode::serialize(&snap).unwrap();
            let entry = BtreeEntry::raw(Bpos::new(0, 0, id), KeyType::Normal, bytes);
            vol.insert_entry_raw(BtreeId::Snapshots, entry, 0);
        }
        let tree_bytes = bincode::serialize(&tree).unwrap();
        vol.insert_entry_raw(
            BtreeId::SnapshotTrees,
            BtreeEntry::raw(Bpos::new(0, 0, 1), KeyType::Normal, tree_bytes),
            0,
        );
        let mut trans = make_trans(&mut vol);
        bch2_check_snapshots(&mut trans).unwrap();
        let fixed = bch2_snapshot_read_value(&trans, grandchild).unwrap();
        // 验证：修复后的 skip 条目都应是 grandchild 的有效祖先（0/root/child）
        for &entry in &fixed.skip {
            if entry != 0 {
                assert!(
                    entry == root || entry == child,
                    "skip entry {} should be an ancestor of {}: {:?}",
                    entry,
                    grandchild,
                    fixed.skip
                );
            }
        }
    }

    #[test]
    fn test_check_snapshots_clears_empty_subvol() {
        let mut vol = make_vol();
        let root = u32::MAX;
        let child = u32::MAX - 1;
        let root_snap = SnapshotT {
            state: SnapshotIdState::Live,
            parent: 0,
            children: [child, 0],
            subvol: 0,
            tree: 1,
            skip: [0, 0, 0],
            is_ancestor: 0,
            depth: 1,
            btime: 0,
            deleted: false,
            flags: BchSnapshotFlags::empty(),
        };
        let child_snap = SnapshotT {
            state: SnapshotIdState::Live,
            parent: root,
            children: [0, 0],
            subvol: 42,
            tree: 1,
            skip: [0, 0, 0],
            is_ancestor: 0,
            depth: 2,
            btime: 0,
            deleted: false,
            flags: BchSnapshotFlags::empty(),
        };
        let tree = SnapshotTreeT::new(0, root);
        for (id, snap) in [(root, root_snap), (child, child_snap)] {
            let bytes = bincode::serialize(&snap).unwrap();
            let entry = BtreeEntry::raw(Bpos::new(0, 0, id), KeyType::Normal, bytes);
            vol.insert_entry_raw(BtreeId::Snapshots, entry, 0);
        }
        let tree_bytes = bincode::serialize(&tree).unwrap();
        vol.insert_entry_raw(
            BtreeId::SnapshotTrees,
            BtreeEntry::raw(Bpos::new(0, 0, 1), KeyType::Normal, tree_bytes),
            0,
        );
        let mut trans = make_trans(&mut vol);
        bch2_check_snapshots(&mut trans).unwrap();
        let fixed = bch2_snapshot_read_value(&trans, child).unwrap();
        assert_eq!(fixed.subvol, 0, "subvol should stay cleared");
        assert!(!fixed.flags.contains(BchSnapshotFlags::SUBVOL));
    }

    #[test]
    fn test_check_snapshots_rejects_missing_parent_pointer() {
        let mut vol = make_vol();
        let root = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        // 损坏 root 的 children：清除 child 引用
        let mut snap = bch2_snapshot_read_value_direct(&vol, root).unwrap();
        snap.children = [0, 0];
        let bytes = bincode::serialize(&snap).unwrap();
        let entry = BtreeEntry::raw(Bpos::new(0, 0, root), KeyType::Normal, bytes);
        vol.insert_entry_raw(BtreeId::Snapshots, entry, 0);
        let mut trans = make_trans(&mut vol);
        assert!(bch2_check_snapshots(&mut trans).is_err());
    }

    #[test]
    fn test_check_snapshots_rejects_missing_subvol() {
        let mut vol = make_vol();
        let root = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        let child = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [999, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), root, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };
        // 强制设置 SUBVOL flag（node_create 不会自动设 flag，模拟损坏）
        let mut snap = bch2_snapshot_read_value_direct(&vol, child).unwrap();
        snap.flags.insert(BchSnapshotFlags::SUBVOL);
        snap.subvol = 999;
        let bytes = bincode::serialize(&snap).unwrap();
        let entry = BtreeEntry::raw(Bpos::new(0, 0, child), KeyType::Normal, bytes);
        vol.insert_entry_raw(BtreeId::Snapshots, entry, 0);
        let mut trans = make_trans(&mut vol);
        assert!(bch2_check_snapshots(&mut trans).is_err());
    }

    #[test]
    fn test_check_snapshots_detects_cycle() {
        let mut vol = make_vol();
        let mut trans = make_trans(&mut vol);
        let root = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut trans, 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        drop(trans);
        // 让 root 自引用 parent
        let mut snap = bch2_snapshot_read_value_direct(&vol, root).unwrap();
        snap.parent = root;
        let bytes = bincode::serialize(&snap).unwrap();
        let entry = BtreeEntry::raw(Bpos::new(0, 0, root), KeyType::Normal, bytes);
        vol.insert_entry_raw(BtreeId::Snapshots, entry, 0);
        let mut trans = make_trans(&mut vol);
        assert!(
            bch2_check_snapshots(&mut trans).is_err(),
            "self-parent cycle should be detected"
        );
        drop(trans);
    }
}
