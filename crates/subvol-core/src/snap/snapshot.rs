//! Snapshot btree 操作 — bcachefs 对齐的 Snapshots btree 原生实现
//!
//! 所有快照操作直接读写 Snapshots btree，无需独立内存缓存。
//!
//! # 架构
//!
//! - `bch2_snapshot_is_ancestor()`: 使用 SnapshotT.skip[3] 持久化 skiplist
//!   直接从 Snapshots btree 查询祖先关系（O(log depth)）
//! - `bch2_snapshot_node_create()`: 在 Snapshots btree 中插入新快照节点
//! - `bch2_snapshot_node_set_deleted()`: 标记快照为已删除
//! - `bch2_snapshot_list()`: 遍历 Snapshots btree 列出快照
//!
//! 命名对齐 bcachefs：
//! - `bch_snapshot.skip[]` → SnapshotT.skip
//! - `bch2_snapshot_is_ancestor()` → bch2_snapshot_is_ancestor()
//! - `bch2_snapshot_node_create()` → bch2_snapshot_node_create()

use crate::btree::key::{Bpos, BtreeKey, KeyType, KeyValue};
use crate::btree::{BtreeId, BtreeTrans};
use crate::types::StorageError;
use crate::BchVol;
use rand::Rng;

#[cfg(test)]
use super::table::SnapshotTable;
#[cfg(test)]
use std::collections::HashSet;
use super::meta::{BchSnapshotFlags, SnapshotIdState, SnapshotT, SnapshotTreeT};

/// 从 Snapshots btree 读取 SnapshotT。
///
/// 对齐 bcachefs `bch2_snapshot_lookup()`。
/// WILL_DELETE 节点仍然保留在 Snapshots btree 中，供删除回收和祖先修复使用。
pub fn bch2_snapshot_lookup(trans: &BtreeTrans, id: u32) -> Option<SnapshotT> {
    let entry = trans.get_entry(BtreeId::Snapshots, Bpos::new(0, 0, id))?;
    let bytes = match &entry.value {
        KeyValue::Raw(b) => b,
        _ => return None,
    };
    bincode::deserialize(bytes).ok()
}

// 别名：为外部模块（volume、subvol）提供旧名称兼容
pub(crate) use bch2_snapshot_lookup as bch2_snapshot_read_value;

/// 直接从 BchVol 读取 SnapshotT。
///
/// 用于非事务上下文（如 DFS 工具函数）。
pub(crate) fn bch2_snapshot_read_value_direct(vol: &BchVol, id: u32) -> Option<SnapshotT> {
    let pos = Bpos::new(0, 0, id);
    let entry = vol.get_entry_raw(BtreeId::Snapshots, pos)?;
    let bytes = match &entry.value {
        KeyValue::Raw(b) => b,
        _ => return None,
    };
    bincode::deserialize(bytes).ok()
}

/// 从 Snapshots btree 批量读取多个快照。
///
/// 通过遍历 btree 收集所有未删除的快照，使用 HashMap 去重。
/// 注意：全表扫描仅读已提交数据。
pub(crate) fn bch2_snapshot_list(trans: &BtreeTrans) -> Vec<(u32, SnapshotT)> {
    use std::collections::HashMap;
    let mut map: HashMap<u32, SnapshotT> = HashMap::new();
    let btree = trans.btree(BtreeId::Snapshots);
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
    let mut result: Vec<(u32, SnapshotT)> = map.into_iter().collect();
    result.sort_by(|a, b| b.0.cmp(&a.0));
    result
}

/// IS_ANCESTOR_BITMAP：128 位祖先位图大小。
/// 对齐 bcachefs `IS_ANCESTOR_BITMAP`（types.h:40）。
const IS_ANCESTOR_BITMAP: u32 = 128;

/// 获取不超过 `ancestor` 的最远 skip list 祖先（btree 版本）。
///
/// 尝试顺序：`skip[2]` → `skip[1]` → `skip[0]` → `parent`。
/// 条件：跳表值不为 0 且 `<= ancestor`（保证不跳过目标）。
///
/// 对齐 bcachefs `get_ancestor_below()`（snapshot.c:221-234）。
fn get_ancestor_below_btree(snap: &SnapshotT, ancestor: u32) -> u32 {
    // 对齐 bcachefs：按 skip[2]→skip[1]→skip[0] 递减尝试
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

/// 检查 `ancestor` 是否为 `descendant` 的祖先（btree 版本）。
///
/// 对齐 bcachefs `__bch2_snapshot_is_ancestor()`（snapshot.c:328-353）：
///
/// # 算法（三阶段）
///
/// 阶段一（skip list）：如果 `ancestor >= IS_ANCESTOR_BITMAP`，
///   使用 `get_ancestor_below` 循环跳升，直到 `id >= ancestor - IS_ANCESTOR_BITMAP`
///   或 `id == 0`。每次从 btree 读取当前节点以获取 skip[] 数组。
///
/// 阶段二（bitmap）：在 128 位范围内，用 `test_ancestor_bitmap`
///   做 O(1) 位图判定。位 `(ancestor - id - 1)` 置位表示 `ancestor` 是 `id` 的祖先。
///   对齐 bcachefs `test_ancestor_bitmap()`（snapshot.c:236-243）。
///
/// 阶段三（fallback）：直接检查 `id == ancestor`。
///
/// 由于 parent_id > child_id（ID 从 u32::MAX 向下分配），
/// skip 中的 ID 也大于 current。
pub fn bch2_snapshot_is_ancestor(trans: &BtreeTrans, descendant: u32, ancestor: u32) -> bool {
    if ancestor == descendant {
        return true;
    }
    // 父 ID > 子 ID，所以 ancestor 必须大于 descendant
    if ancestor <= descendant || descendant == 0 {
        return false;
    }

    let mut current = descendant;

    // ── 阶段一：Skip list 跳跃（距离 > 128 时使用）──
    // 对齐 bcachefs snapshot.c:340-342：
    //   if (likely(ancestor >= IS_ANCESTOR_BITMAP))
    //       while (id && id < ancestor - IS_ANCESTOR_BITMAP)
    //           id = get_ancestor_below(t, id, ancestor);
    if ancestor >= IS_ANCESTOR_BITMAP {
        while current != 0 && current < ancestor - IS_ANCESTOR_BITMAP {
            let snap = match bch2_snapshot_read_value(trans, current) {
                Some(s) => s,
                None => return false,
            };

            if snap.parent == 0 {
                return false;
            }

            current = get_ancestor_below_btree(&snap, ancestor);
        }
    }

    // ── 阶段二：位图判定（128 范围内）──
    // 对齐 bcachefs snapshot.c:344-346：
    //   ret = id && id < ancestor
    //       ? test_ancestor_bitmap(t, id, ancestor)
    //       : id == ancestor;
    if current != 0 && current < ancestor {
        let dist = (ancestor - current - 1) as usize;
        if dist < IS_ANCESTOR_BITMAP as usize {
            // 读取 current 节点的位图
            let snap = match bch2_snapshot_read_value(trans, current) {
                Some(s) => s,
                None => return false,
            };
            return (snap.is_ancestor >> dist) & 1 == 1;
        }
    }

    current == ancestor
}

/// 从 Snapshots btree 获取下一个可用的快照 ID。
///
/// ID 从 u32::MAX 向下分配（父 > 子，对齐 bcachefs 但方向相反）。
/// 该实现扫描当前事务可见的 Snapshots btree 条目，并叠加 journal 中
/// 尚未提交的 snapshot 更新，返回最高的可用 ID。
///
/// 对齐 bcachefs `create_snapids()` 的 slot-walk 语义。
pub(crate) fn bch2_snapshot_next_id(trans: &BtreeTrans) -> u32 {
    bch2_snapshot_alloc_ids(trans, 1)
        .into_iter()
        .next()
        .unwrap_or(u32::MAX)
}

/// 分配当前事务可见的多个快照 ID。
///
/// 语义对齐 bcachefs `create_snapids()`：按从高到低的 slot 顺序分配，
/// 每次都跳过已经占用的 snapshot id。
pub(crate) fn bch2_snapshot_alloc_ids(trans: &BtreeTrans, count: usize) -> Vec<u32> {
    let mut occupied = trans.journal_snapshot_ids();

    let btree = trans.btree(BtreeId::Snapshots);
    btree.for_each_btree_key_entry(|entry| occupied.push(entry.pos.snapshot));

    occupied.sort_unstable_by(|a, b| b.cmp(a));
    occupied.dedup();

    let mut allocated = Vec::with_capacity(count);
    for _ in 0..count {
        let mut candidate = u32::MAX;
        for &id in &occupied {
            if id == candidate {
                if candidate == 0 {
                    break;
                }
                candidate = candidate.wrapping_sub(1);
                continue;
            }
            if id < candidate {
                break;
            }
        }

        allocated.push(candidate);
        occupied.push(candidate);
        occupied.sort_unstable_by(|a, b| b.cmp(a));
        occupied.dedup();
    }

    allocated
}

/// 批量重建所有快照的指数级 skip list。
///
/// 遍历 Snapshots btree，为每个快照节点使用 `bch2_snapshot_skiplist_get` 计算
/// bcachefs 对齐的指数级 skip（Batch B: skip[2] = skip[1].skip[1] 实现 4 步跳），然后写回。
fn build_skip_list_from_btree(trans: &mut BtreeTrans) -> Result<(), StorageError> {
    let snap_ids: Vec<u32> = {
        let btree = trans.btree(BtreeId::Snapshots);
        let mut ids = Vec::new();
        btree.for_each_btree_key_entry(|entry| {
            ids.push(entry.pos.snapshot);
        });
        ids
    };

    for &id in &snap_ids {
        let snap = match bch2_snapshot_read_value(trans, id) {
            Some(s) => s,
            None => continue,
        };
        let skiplist = if snap.parent == 0 {
            [0, 0, 0]
        } else {
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
            skiplist
        };
        {
            let mut updated_snap = snap;
            updated_snap.skip = skiplist;
            let bytes = bincode::serialize(&updated_snap).map_err(StorageError::Serialization)?;
            trans.bch2_trans_update_raw(
                BtreeId::Snapshots,
                0,
                false,
                BtreeKey::new(0, id, KeyType::Normal),
                bytes,
                0,
            );
        }
    }

    Ok(())
}

/// 检查 skip 条目对 ancestor 跳跃是否"良好"。
///
/// skip 合法的条件：
/// 1. skip != 0（有效值）
/// 2. skip <= ancestor（不跳过目标）
/// 3. skip > current（向前跳跃）
///
/// 当条件不满足时返回上一个有效的 skip 索引或 parent。
/// 用于 `bch2_snapshot_is_ancestor` 中的健壮跳跃。
pub(crate) fn bch2_snapshot_skiplist_good(skip: u32, current: u32, ancestor: u32) -> bool {
    skip != 0 && skip <= ancestor && skip > current
}



/// 对齐 bcachefs `bch2_snapshot_node_create()`。
///
/// 本地 bcachefs 的约束是：创建新树时只能创建一个 root leaf；
/// 在已有节点下创建 children 时必须同时创建两个 leaf。调用方通过
/// `new_snapids` 取得分配到的 ID，通过 `snapshot_subvols` 指定每个 leaf
/// 绑定的 subvolume。
pub(crate) fn bch2_snapshot_node_create(
    trans: &mut BtreeTrans,
    parent_id: u32,
    new_snapids: &mut [u32],
    snapshot_subvols: &[u32],
    nr_snapids: usize,
) -> Result<(), StorageError> {
    // This is the same contract as the local bcachefs BUG_ONs: root creation
    // produces one leaf, while a child split produces two leaves.
    if nr_snapids == 0
        || nr_snapids > new_snapids.len()
        || nr_snapids > snapshot_subvols.len()
        || (parent_id == 0) != (nr_snapids == 1)
    {
        return Err(StorageError::InvalidArgument(
            "invalid snapshot node create shape".into(),
        ));
    }

    if parent_id == 0 {
        let mut tree_id = 1u32;
        while trans
            .get_entry(BtreeId::SnapshotTrees, Bpos::new(0, 0, tree_id))
            .is_some()
        {
            if tree_id == u32::MAX {
                return Err(StorageError::AddressSpaceExhausted {
                    max_raw_addr: u32::MAX as u64,
                });
            }
            tree_id += 1;
        }

        let id = bch2_snapshot_alloc_ids(trans, 1)[0];
        new_snapids[0] = id;
        let tree_val = SnapshotTreeT::new(snapshot_subvols[0], id);
        let tree_bytes = bincode::serialize(&tree_val).map_err(StorageError::Serialization)?;
        trans.bch2_trans_update_raw(
            BtreeId::SnapshotTrees,
            0,
            false,
            BtreeKey::new(0, tree_id, KeyType::Normal),
            tree_bytes,
            0,
        );

        let snap_val = SnapshotT::new_leaf(
            0,
            snapshot_subvols[0],
            tree_id,
            1,
            current_timestamp(),
        );
        let bytes = bincode::serialize(&snap_val).map_err(StorageError::Serialization)?;
        trans.bch2_trans_update_raw(
            BtreeId::Snapshots,
            0,
            false,
            BtreeKey::new(0, id, KeyType::Normal),
            bytes,
            0,
        );
        return Ok(());
    }

    if nr_snapids != 2 {
        return Err(StorageError::InvalidArgument(
            "snapshot children require two snapids".into(),
        ));
    }

    let parent = bch2_snapshot_read_value(trans, parent_id).ok_or_else(|| {
        StorageError::NotFound(format!("parent snapshot {} not found", parent_id))
    })?;
    if parent.children[0] != 0 || parent.children[1] != 0 {
        return Err(StorageError::InvalidArgument(
            "snapshot parent already has children".into(),
        ));
    }

    let ids = bch2_snapshot_alloc_ids(trans, 2);
    new_snapids[..2].copy_from_slice(&ids[..2]);
    let depth = parent.depth + 1;
    let skip = if parent.parent == 0 {
        [0, 0, 0]
    } else {
        let mut rng = rand::thread_rng();
        let mut skiplist = [0u32; 3];
        for item in &mut skiplist {
            let n = rng.gen_range(0..parent.depth);
            let mut current = parent_id;
            for _ in 0..n {
                match bch2_snapshot_read_value(trans, current) {
                    Some(s) if s.parent != 0 => current = s.parent,
                    _ => break,
                }
            }
            *item = current;
        }
        skiplist.sort_unstable();
        skiplist
    };

    let mut new_parent = parent.clone();
    new_parent.children = [ids[0], ids[1]];
    new_parent.subvol = 0;
    new_parent.flags.remove(BchSnapshotFlags::SUBVOL);

    let make_leaf = |subvol: u32| SnapshotT {
        state: SnapshotIdState::Live,
        parent: parent_id,
        children: [0, 0],
        subvol,
        tree: parent.tree,
        skip,
        is_ancestor: 0,
        depth,
        btime: current_timestamp(),
        deleted: false,
        flags: BchSnapshotFlags::SUBVOL,
    };
    let mut snap = make_leaf(snapshot_subvols[0]);
    let mut extra = make_leaf(snapshot_subvols[1]);
    for (node, id) in [(&mut snap, ids[0]), (&mut extra, ids[1])] {
        let gap = parent_id.saturating_sub(id);
        node.is_ancestor = if gap < 128 {
            parent.is_ancestor.wrapping_shl(gap) | (1u128 << gap.saturating_sub(1))
        } else {
            0
        };
        let bytes = bincode::serialize(node).map_err(StorageError::Serialization)?;
        trans.bch2_trans_update_raw(
            BtreeId::Snapshots,
            0,
            false,
            BtreeKey::new(0, id, KeyType::Normal),
            bytes,
            0,
        );
    }
    let parent_bytes = bincode::serialize(&new_parent).map_err(StorageError::Serialization)?;
    trans.bch2_trans_update_raw(
        BtreeId::Snapshots,
        0,
        false,
        BtreeKey::new(0, parent_id, KeyType::Normal),
        parent_bytes,
        0,
    );
    Ok(())
}

/// 在 Snapshots btree 中标记快照为已删除。
///
/// 对齐 bcachefs `bch2_snapshot_node_set_deleted()`（delete.c:127-143）：
/// - 检测 ENOENT 为数据不一致（快照节点应存在）
/// - 若已标记 WILL_DELETE 则短路返回（幂等）
/// - 设置 WILL_DELETE、清除 SUBVOL、清零 subvol
/// 真正的垃圾回收（清理数据键）需要 Journal replay 支持。
pub fn bch2_snapshot_node_set_deleted(trans: &mut BtreeTrans, id: u32) -> Result<(), StorageError> {
    let mut snap = bch2_snapshot_read_value(trans, id)
        .ok_or_else(|| StorageError::NotFound(format!("snapshot {} not found", id)))?;

    // 对齐 bcachefs delete.c:137-138：already deleted? → 短路返回
    if snap.flags.contains(BchSnapshotFlags::WILL_DELETE) {
        return Ok(());
    }

    snap.mark_deleted();
    snap.flags.insert(BchSnapshotFlags::WILL_DELETE);
    snap.flags.remove(BchSnapshotFlags::SUBVOL);
    snap.subvol = 0;

    let bytes = bincode::serialize(&snap).map_err(StorageError::Serialization)?;
    trans.bch2_trans_update_raw(
        BtreeId::Snapshots,
        0,
        false,
        BtreeKey::new(0, id, KeyType::Normal),
        bytes,
        0,
    );

    Ok(())
}

/// 对齐 bcachefs `bch2_snapshot_node_set_no_keys()`（delete.c:146-164）。
///
/// interior 节点完成快照数据键处理后，先进入 NO_KEYS 状态；节点本身
/// 由 `bch2_delete_dead_interior_snapshots()` 的独立阶段重挂并删除。
fn bch2_snapshot_node_set_no_keys(
    trans: &mut BtreeTrans,
    id: u32,
) -> Result<(), StorageError> {
    let mut snap = bch2_snapshot_read_value(trans, id)
        .ok_or_else(|| StorageError::NotFound(format!("snapshot {} not found", id)))?;

    snap.flags.insert(BchSnapshotFlags::NO_KEYS);
    snap.flags.remove(BchSnapshotFlags::WILL_DELETE);
    snap.subvol = 0;
    snap.deleted = false;

    let bytes = bincode::serialize(&snap).map_err(StorageError::Serialization)?;
    trans.bch2_trans_update_raw(
        BtreeId::Snapshots,
        0,
        false,
        BtreeKey::new(0, id, KeyType::Normal),
        bytes,
        0,
    );

    Ok(())
}

/// 返回当前 Unix 时间戳（秒）
fn current_timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// 读取 SnapshotTreeT 从 SnapshotTrees btree。
///
/// 对齐 bcachefs `bch2_snapshot_tree_lookup()`：
/// - 使用事务视图做 typed lookup
/// - 缺失直接报错
/// - 值类型异常或反序列化失败都视为数据损坏
pub fn bch2_snapshot_tree_lookup(
    trans: &BtreeTrans,
    tree_id: u32,
) -> Result<SnapshotTreeT, StorageError> {
    let entry = trans
        .get_entry(BtreeId::SnapshotTrees, Bpos::new(0, 0, tree_id))
        .ok_or_else(|| StorageError::NotFound(format!("snapshot tree {tree_id}")))?;
    let bytes = match &entry.value {
        KeyValue::Raw(b) => b,
        _ => {
            return Err(StorageError::InvalidData(format!(
                "snapshot tree {tree_id} has non-raw value"
            )))
        }
    };
    bincode::deserialize(bytes).map_err(StorageError::Serialization)
}

// ─── 深度优先遍历 ───────────────────────────────

/// 返回 subtree 所有节点的后序列表（叶子→根）。
///
/// 物理删除快照节点。
///
/// 对齐 bcachefs `bch2_snapshot_node_delete()` (delete.c:167-290)。
///
/// 功能：
/// - 更新父节点的 children 指针（将当前节点替换为其子节点或清空）
/// - `delete_interior=true` 时：将子节点的 parent 指向当前节点的父节点（祖父）
/// - 子节点成为 root（parent=0）时：更新 SnapshotTrees btree 的 root_snapshot
/// - 删除自身 snapshot key
///
/// 两个孩子节点时返回 `StorageError::InvalidData`（对齐 bcachefs -EBUSY, delete.c:186-193）。
pub(crate) fn bch2_snapshot_node_delete(
    trans: &mut BtreeTrans,
    id: u32,
    delete_interior: bool,
) -> Result<(), StorageError> {
    // delete.c:176: 读取快照数据（允许已删除，因为可能先 set_deleted 再调本函数）
    let snap = bch2_snapshot_read_value(trans, id)
        .ok_or_else(|| StorageError::NotFound(format!("snapshot {} not found", id)))?;

    // delete.c:186-193: 两个孩子节点不能直接删除
    if snap.children[0] != 0 && snap.children[1] != 0 {
        return Err(StorageError::InvalidData(format!(
            "snapshot {} has two children, cannot delete",
            id
        )));
    }

    // 确定生存的子节点（如果有）
    let child = if snap.children[0] != 0 {
        snap.children[0]
    } else {
        snap.children[1]
    };

    // delete.c:211-219: 带一个 child 的 interior 只能在
    // delete_interior=true 时删除。
    if child != 0 && !delete_interior {
        return Err(StorageError::InvalidData(format!(
            "snapshot {} is an interior node",
            id
        )));
    }

    // delete.c:206-252: 更新父节点的 children 指针
    if snap.parent != 0 {
        if let Some(parent_snap) = bch2_snapshot_read_value(trans, snap.parent) {
            let mut new_parent = parent_snap;
            let mut found = false;
            for slot in new_parent.children.iter_mut() {
                if *slot == id {
                    // bcachefs delete.c:240: le32_add_cpu(&parent->v.children[i], child - id)
                    // 用 Rust 直接赋值语义等价的 child（child=0 时清空，child>0 时替换）
                    *slot = child;
                    found = true;
                    break;
                }
            }
            if !found {
                return Err(StorageError::InvalidData(format!(
                    "snapshot {} is not a child of {}",
                    id, snap.parent
                )));
            }
            if new_parent.children[0] < new_parent.children[1] {
                new_parent.children.swap(0, 1);
            }
            let bytes = bincode::serialize(&new_parent).map_err(StorageError::Serialization)?;
            trans.bch2_trans_update_raw(
                BtreeId::Snapshots,
                0,
                false,
                BtreeKey::new(0, snap.parent, KeyType::Normal),
                bytes,
                0,
            );
        }
    }

    // delete.c:272-280: delete_interior && child 存在 → 子节点 parent 指向祖父
    if delete_interior && child != 0 {
        if let Some(mut child_snap) = bch2_snapshot_read_value(trans, child) {
            child_snap.parent = snap.parent;

            let bytes = bincode::serialize(&child_snap).map_err(StorageError::Serialization)?;
            trans.bch2_trans_update_raw(
                BtreeId::Snapshots,
                0,
                false,
                BtreeKey::new(0, child, KeyType::Normal),
                bytes,
                0,
            );
        }
    }

    // delete.c:256-271: child 存在但无祖父 → 更新 SnapshotTrees root_snapshot
    if snap.parent == 0 {
        let tree_id = snap.tree;
        if tree_id != 0 {
            let mut tree_val = bch2_snapshot_tree_lookup(trans, tree_id)?;
            if child != 0 {
                tree_val.root_snapshot = child;
                let tree_bytes = bincode::serialize(&tree_val).map_err(StorageError::Serialization)?;
                trans.bch2_trans_update_raw(
                    BtreeId::SnapshotTrees,
                    0,
                    false,
                    BtreeKey::new(0, tree_id, KeyType::Normal),
                    tree_bytes,
                    0,
                );
            } else {
                trans.bch2_trans_delete(
                    BtreeId::SnapshotTrees,
                    0,
                    false,
                    BtreeKey::new(0, tree_id, KeyType::Normal),
                    0,
                );
            }
        }
    }

    // delete.c:284-290: 物理删除自身（KEY_TYPE_deleted / runtime 移除）
    trans.bch2_trans_delete(
        BtreeId::Snapshots,
        0,
        false,
        BtreeKey::new(0, id, KeyType::Normal),
        0,
    );

    Ok(())
}

/// 修复被删除 interior 节点的子节点的 depth/skip 字段。
///
/// 对齐 bcachefs `bch2_fix_child_of_deleted_snapshot()` (delete.c:611-662)。
/// 遍历所有快照节点，对拥有被删祖先的节点重新计算 depth 和 skip。
pub fn bch2_fix_child_of_deleted_snapshot(
    trans: &mut BtreeTrans,
    deleted_ids: &[u32],
) -> Result<(), StorageError> {
    let btree = trans.btree(BtreeId::Snapshots);
    // 收集所有节点 ID 用于遍历
    let mut all_ids: Vec<(u32, Vec<u8>)> = Vec::new();
    btree.for_each_btree_key_entry(|entry| {
        let sid = entry.pos.snapshot;
        let bytes = match &entry.value {
            KeyValue::Raw(b) => b.clone(),
            _ => return,
        };
        all_ids.push((sid, bytes));
    });
    // 释放 for_each_entry 的锁，后续需要写 BtreeTrans
    let _ = btree;

    for (id, bytes) in &all_ids {
        let snap: SnapshotT = match bincode::deserialize(bytes) {
            Ok(s) => s,
            Err(_) => continue,
        };
        // delete.c:621-622: 跳过自身在 deleted 列表中的节点
        if deleted_ids.contains(id) {
            continue;
        }
        // bch2_snapshot_is_ancestor(descendant, ancestor)
        // deleted_id 是祖先，*id 是后代
        let nr_deleted_ancestors: u32 = deleted_ids
            .iter()
            .filter(|deleted_id| bch2_snapshot_is_ancestor(trans, *id, **deleted_id))
            .count() as u32;

        if nr_deleted_ancestors == 0 {
            continue;
        }

        let mut updated = snap;
        updated.depth -= nr_deleted_ancestors;

        if updated.depth == 0 {
            updated.skip = [0; 3];
        } else {
            let mut rng = rand::thread_rng();
            for skip in &mut updated.skip {
                if deleted_ids.contains(skip) {
                    let mut replacement = updated.parent;
                    while deleted_ids.contains(&replacement) {
                        replacement = bch2_snapshot_read_value(trans, replacement)
                            .map(|snapshot| snapshot.parent)
                            .unwrap_or(0);
                    }

                    let mut n = if updated.depth > 1 {
                        rng.gen_range(0..updated.depth - 1)
                    } else {
                        0
                    };
                    while n > 0 {
                        replacement = bch2_snapshot_read_value(trans, replacement)
                            .map(|snapshot| snapshot.parent)
                            .unwrap_or(0);
                        while deleted_ids.contains(&replacement) {
                            replacement =
                                bch2_snapshot_read_value(trans, replacement)
                                    .map(|snapshot| snapshot.parent)
                                    .unwrap_or(0);
                        }
                        n -= 1;
                    }
                    *skip = replacement;
                }
            }
            updated.skip.sort_unstable();
        }

        let new_bytes = bincode::serialize(&updated).map_err(StorageError::Serialization)?;
        trans.bch2_trans_update_raw(
            BtreeId::Snapshots,
            0,
            false,
            BtreeKey::new(0, *id, KeyType::Normal),
            new_bytes,
            0,
        );
    }

    Ok(())
}

/// 对齐 bcachefs `bch2_check_snapshot_needs_deletion()` (delete.c:853-878)。
/// 检查 snapshot 是否需要 delete 处理。
pub fn bch2_check_snapshot_needs_deletion(snap: &SnapshotT) -> bool {
    if snap.flags.contains(BchSnapshotFlags::NO_KEYS) {
        return false;
    }
    if snap.flags.contains(BchSnapshotFlags::WILL_DELETE) {
        return true;
    }
    if snap.children[0] != 0 && snap.children[1] == 0
        || snap.children[0] == 0 && snap.children[1] != 0
    {
        return true;
    }
    false
}

/// 检测快照是否可删除及其类型。
///
/// 对齐 bcachefs `check_should_delete_snapshot()` (delete.c:532-610)。
///
/// 返回:
/// - `None` — 不可删除（有 subvol 引用）
/// - `Some(DeadSnapshotType::Leaf)` — 叶子节点可删除
/// - `Some(DeadSnapshotType::Interior)` — interior 节点可删除（NO_KEYS, subvol==0）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeadSnapshotType {
    Leaf,
    Interior,
}

pub fn check_should_delete_snapshot(snap: &SnapshotT) -> Option<DeadSnapshotType> {
    if snap.has_subvol() {
        return None;
    }

    let nr_children = (snap.children[0] != 0) as u8 + (snap.children[1] != 0) as u8;
    match nr_children {
        0 => Some(DeadSnapshotType::Leaf),
        1 => Some(DeadSnapshotType::Interior),
        _ => None,
    }
}

fn snapshot_has_subvol_ref(snap: &SnapshotT) -> bool {
    snap.has_subvol() || snap.subvol != 0
}

/// 批量删除所有标记为 deleted 的死快照。
///
/// 对齐 bcachefs `bch2_delete_dead_snapshots()`。
///
/// 流程：
/// 1. 全量扫描 Snapshots btree，收集 deleted==true 的节点
/// 2. 调用 fix_child_of_deleted_snapshot 修复受影响子节点的 depth/skip
/// 3. 对每个已删除节点，DFS 遍历其子树
/// 4. 如果子树中有被 volume 引用的快照（subvol != 0），跳过该子树
/// 5. 叶子→根后序删除，更新父节点 children
/// 6. 返回跳过的 snapshot_id 列表（被 volume 引用）
pub fn bch2_delete_dead_snapshots(trans: &mut BtreeTrans) -> Result<Vec<u32>, StorageError> {
    use std::collections::HashMap;

    // 1. 全量扫描，收集所有快照（含已删除）
    let mut all_snaps: HashMap<u32, SnapshotT> = HashMap::new();
    {
        let btree = trans.btree(BtreeId::Snapshots);
        btree.for_each_btree_key_entry(|entry| {
            let sid = entry.pos.snapshot;
            let bytes = match &entry.value {
                KeyValue::Raw(b) => b.clone(),
                _ => return,
            };
            if let Ok(snap) = bincode::deserialize::<SnapshotT>(&bytes) {
                all_snaps.insert(sid, snap);
            }
        });
    }

    // 2. 按 bcachefs check_should_delete_snapshot() 分类：
    //    leaf 直接删除；单 child interior 先进入 NO_KEYS 阶段。
    let mut leaf_ids = Vec::new();
    let mut interior_ids = Vec::new();
    let mut skipped = Vec::new();

    // 工具函数：在 HashMap 上 DFS 检查 volume 引用
    fn has_volume_ref(all_snaps: &HashMap<u32, SnapshotT>, id: u32) -> bool {
        let snap = match all_snaps.get(&id) {
            Some(s) => s,
            None => return false,
        };
        if snapshot_has_subvol_ref(snap) {
            return true;
        }
        if snap.children[0] != 0 && has_volume_ref(all_snaps, snap.children[0]) {
            return true;
        }
        if snap.children[1] != 0 && has_volume_ref(all_snaps, snap.children[1]) {
            return true;
        }
        false
    }

    for (&id, snap) in &all_snaps {
        if (snap.deleted || snap.flags.contains(BchSnapshotFlags::WILL_DELETE))
            && has_volume_ref(&all_snaps, id)
        {
            skipped.push(id);
            continue;
        }

        let kind = match check_should_delete_snapshot(snap) {
            Some(kind) => kind,
            None => continue,
        };

        if has_volume_ref(&all_snaps, id) {
            continue;
        }

        match kind {
            DeadSnapshotType::Leaf => leaf_ids.push(id),
            DeadSnapshotType::Interior => interior_ids.push(id),
        }
    }

    // 3. 与本地 delete_dead_snapshots_locked() 一致，先修复将进入
    //    NO_KEYS 阶段的 interior 子孙 skip/depth。
    if !interior_ids.is_empty() {
        bch2_fix_child_of_deleted_snapshot(trans, &interior_ids)?;
    }

    // 4. leaf 节点通过 snapshot_node_delete 统一更新父节点和 snapshot tree。
    for &id in &leaf_ids {
        if let Some(snap) = bch2_snapshot_read_value(trans, id) {
            if snap.children == [0, 0] && !snap.has_subvol() {
                bch2_snapshot_node_delete(trans, id, false)?;
            }
        }
    }

    // 5. interior 只切换到 NO_KEYS；物理重挂/删除由独立回收阶段完成。
    for &id in &interior_ids {
        if bch2_snapshot_read_value(trans, id).is_some() {
            bch2_snapshot_node_set_no_keys(trans, id)?;
        }
    }

    if !skipped.is_empty() || !leaf_ids.is_empty() || !interior_ids.is_empty() {
        trans.btree(BtreeId::Snapshots).compact();
    }

    Ok(skipped)
}

/// 删除所有 NO_KEYS 标记且仅有一个子节点的 interior 快照。
///
/// 对齐 bcachefs `bch2_delete_dead_interior_snapshots()` (delete.c:811-851)。
///
/// 流程:
/// 1. 遍历 Snapshots btree，收集 NO_KEYS + 单子节点 + 非 deleted 的 interior
/// 2. 调用 fix_child_of_deleted_snapshot 修复受影响子节点的 depth/skip
/// 3. 对每个 interior 调用 bch2_snapshot_node_delete(id, delete_interior=true)
///
/// 注意：调用者应先运行 bch2_check_snapshots 保证树结构一致（对齐 bcachefs delete.c:828）。
pub fn bch2_delete_dead_interior_snapshots(trans: &mut BtreeTrans) -> Result<(), StorageError> {
    // 1. 遍历收集 NO_KEYS + 单子节点 interior
    let mut interior_deletes: Vec<(u32, u32)> = Vec::new();
    {
        let btree = trans.btree(BtreeId::Snapshots);
        btree.for_each_btree_key_entry(|entry| {
            let sid = entry.pos.snapshot;
            if sid == 0 {
                return;
            }
            let bytes = match &entry.value {
                KeyValue::Raw(b) => b.clone(),
                _ => return,
            };
            if let Ok(snap) = bincode::deserialize::<SnapshotT>(&bytes) {
                if snap.state != super::meta::SnapshotIdState::Live {
                    return;
                }
                // NO_KEYS 节点由独立阶段回收：通常恰好一个 child；
                // child 已在同一轮回收完成时，也允许零 child 收尾。
                if snap.flags.contains(BchSnapshotFlags::NO_KEYS)
                    && (snap.children == [0, 0]
                        || ((snap.children[0] != 0) != (snap.children[1] != 0)))
                {
                    interior_deletes.push((sid, snap.parent));
                }
            }
        });
    }

    if interior_deletes.is_empty() {
        return Ok(());
    }

    // 2. fix_child_of_deleted_snapshot 修复受影响子节点
    let deleted_ids: Vec<u32> = interior_deletes.iter().map(|&(id, _)| id).collect();
    bch2_fix_child_of_deleted_snapshot(trans, &deleted_ids)?;

    // 3. 逐个删除 interior（从叶子端开始处理）
    for &(id, _parent) in &interior_deletes {
        // 跳过已被前序删除影响的节点
        if bch2_snapshot_read_value(trans, id).is_none()
            && bch2_snapshot_read_value(trans, id).is_none()
        {
            continue;
        }
        bch2_snapshot_node_delete(trans, id, true)?;
    }

    trans.btree(BtreeId::Snapshots).compact();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── DfsIter: 基于栈的 DFS 遍历器（仅测试用） ───

    /// 迭代器风格的 DFS 遍历器（基于栈，不递归）。
    pub struct DfsIter {
        /// 待遍历的栈 (id, visited_children)
        stack: Vec<(u32, bool)>,
        vol: *const BchVol,
    }

    impl DfsIter {
        /// 创建一个新的 DFS 遍历器，从 snapshot_id 开始。
        pub fn new(vol: &BchVol, snapshot_id: u32) -> Self {
            Self {
                stack: vec![(snapshot_id, false)],
                vol: vol as *const BchVol,
            }
        }

        /// 内部读取快照（通过裸指针转换，需要调用者保证 vol 存活期长于 iter）
        fn read_snap(&self, id: u32) -> Option<SnapshotT> {
            // SAFETY: DfsIter 不修改 vol，且要求调用者保证 vol 存活
            bch2_snapshot_read_value_direct(unsafe { &*self.vol }, id)
        }
    }

    impl Iterator for DfsIter {
        type Item = u32;

        fn next(&mut self) -> Option<Self::Item> {
            loop {
                let peek_id = match self.stack.last() {
                    Some(&(id, _)) => id,
                    None => return None,
                };
                let snap = self.read_snap(peek_id)?;
                // 检查栈顶是否已完成 children
                let (_id, visited) = self.stack.last_mut()?;
                if *visited {
                    let (id, _) = self.stack.pop().unwrap();
                    return Some(id);
                }
                *visited = true;
                // 右子节点先入栈（后入先出，保证左子先出）
                if snap.children[1] != 0 {
                    self.stack.push((snap.children[1], false));
                }
                if snap.children[0] != 0 {
                    self.stack.push((snap.children[0], false));
                }
            }
        }
    }
    use crate::btree::BtreeEntry;
    use crate::BchVol;

    fn make_vol() -> BchVol {
        crate::BchVol::test_trees()
    }

    /// 创建测试用 BtreeTrans，drop 时自动 apply
    fn make_trans<'a>(vol: &'a BchVol) -> AutoApplyTrans<'a> {
        let inner = BtreeTrans::new(vol);
        AutoApplyTrans { inner }
    }

    struct AutoApplyTrans<'a> {
        inner: BtreeTrans<'a>,
    }
    impl<'a> std::ops::Deref for AutoApplyTrans<'a> {
        type Target = BtreeTrans<'a>;
        fn deref(&self) -> &Self::Target {
            &self.inner
        }
    }
    impl<'a> std::ops::DerefMut for AutoApplyTrans<'a> {
        fn deref_mut(&mut self) -> &mut Self::Target {
            &mut self.inner
        }
    }
    impl<'a> Drop for AutoApplyTrans<'a> {
        fn drop(&mut self) {
            self.inner.bch2_trans_commit().ok();
        }
    }

    // ─── 测试专用的 Key Snapshot 验证 + 重建 ───

    /// Key 快照 ID 检查结果。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum CheckKeySnapshotResult {
        Valid,
        ShouldDelete,
        Missing,
    }

    /// 检查 key 的快照 ID 是否有效。
    fn bch2_check_key_has_snapshot(
        table: &crate::snap::table::SnapshotTable,
        snapshot_id: u32,
    ) -> CheckKeySnapshotResult {
        if snapshot_id == 0 {
            return CheckKeySnapshotResult::Valid;
        }
        match table.id_state(snapshot_id) {
            crate::snap::meta::SnapshotIdState::Live => CheckKeySnapshotResult::Valid,
            crate::snap::meta::SnapshotIdState::Deleted => CheckKeySnapshotResult::ShouldDelete,
            crate::snap::meta::SnapshotIdState::Empty => CheckKeySnapshotResult::Missing,
        }
    }

    /// 快照感知的 btree 列表。
    const SNAPSHOT_AWARE_BTREES: [BtreeId; 2] = [BtreeId::Extents, BtreeId::Subvolumes];

    /// 重建缺失的快照条目。
    fn bch2_reconstruct_snapshots(trans: &mut BtreeTrans) -> Result<(), StorageError> {
        // 1. 从 Snapshots btree 收集已存在的 snapshot ID
        let existing: HashSet<u32> = {
            let btree = trans.btree(BtreeId::Snapshots);
            let mut set = HashSet::new();
            btree.for_each_btree_key_entry(|entry| {
                let sid = entry.pos.snapshot;
                if sid != 0 {
                    set.insert(sid);
                }
            });
            set
        };

        // 2. 扫描所有快照感知 btree，收集被引用的 snapshot ID
        let mut referenced: HashSet<u32> = HashSet::new();
        {
            for btree_id in &SNAPSHOT_AWARE_BTREES {
                let btree = trans.btree(*btree_id);
                btree.for_each_btree_key_entry(|entry| {
                    let sid = entry.pos.snapshot;
                    if sid != 0 {
                        referenced.insert(sid);
                    }
                });
            }
        }

        // 3. 找出缺失的 ID
        let missing: Vec<u32> = {
            let mut v: Vec<u32> = referenced.difference(&existing).copied().collect();
            v.sort_unstable();
            v
        };

        if missing.is_empty() {
            return Ok(());
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        // 5. 收集已有的 tree_id → root_snapshot 映射
        let mut tree_roots: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
        {
            let btree = trans.btree(BtreeId::SnapshotTrees);
            btree.for_each_btree_key_entry(|entry| {
                let tree_id = entry.pos.snapshot;
                if tree_id != 0 {
                    let bytes = match &entry.value {
                        KeyValue::Raw(b) => b,
                        _ => return,
                    };
                    if let Ok(tree) = bincode::deserialize::<SnapshotTreeT>(bytes) {
                        tree_roots.insert(tree_id, tree.root_snapshot);
                    }
                }
            });
        }

        let missing_clone = missing.clone();
        let mut pending_trees: Vec<(u32, u32)> = Vec::new();

        for &snap_id in &missing_clone {
            let tree_id = match tree_roots.iter().find(|(_, &root)| root == snap_id) {
                Some((&tid, _)) => tid,
                None => {
                    let max_existing_id = tree_roots.keys().max().copied().unwrap_or(0);
                    let new_tree_id = max_existing_id + 1;
                    let tree_entry = SnapshotTreeT::new(0, snap_id);
                    let bytes = bincode::serialize(&tree_entry)
                        .map_err(StorageError::Serialization)?;
                    trans.bch2_trans_update_raw(
                        BtreeId::SnapshotTrees,
                        0,
                        false,
                        BtreeKey::new(0, new_tree_id, KeyType::Normal),
                        bytes,
                        0,
                    );
                    tree_roots.insert(new_tree_id, snap_id);
                    new_tree_id
                }
            };
            pending_trees.push((snap_id, tree_id));
        }

        let mut snap_to_subvol: std::collections::HashMap<u32, u32> =
            std::collections::HashMap::new();
        {
            let btree = trans.btree(BtreeId::Subvolumes);
            btree.for_each_btree_key_entry(|entry| {
                let sid = entry.pos.snapshot;
                let subvol_id = entry.pos.inode as u32;
                if sid != 0 && subvol_id != 0 {
                    snap_to_subvol.insert(sid, subvol_id);
                }
            });
        }

        for (snap_id, tree_id) in &pending_trees {
            let subvol = snap_to_subvol.get(snap_id).copied().unwrap_or(0);
            let snap = if subvol != 0 {
                SnapshotT::new_leaf(0, subvol, *tree_id, 1, now)
            } else {
                SnapshotT {
                    state: crate::snap::meta::SnapshotIdState::Live,
                    parent: 0,
                    children: [0, 0],
                    subvol: 0,
                    tree: *tree_id,
                    skip: [0, 0, 0],
                    is_ancestor: 0,
                    depth: 1,
                    btime: now,
                    deleted: false,
                    flags: BchSnapshotFlags::empty(),
                }
            };
            let bytes = bincode::serialize(&snap).map_err(StorageError::Serialization)?;
            trans.bch2_trans_update_raw(
                BtreeId::Snapshots,
                0,
                false,
                BtreeKey::new(0, *snap_id, KeyType::Normal),
                bytes,
                0,
            );
        }

        trans.btree(BtreeId::Snapshots).compact();
        trans.btree(BtreeId::SnapshotTrees).compact();

        Ok(())
    }

    // ─── 根快照创建测试 ───

    #[test]
    fn test_create_root_snapshot() {
        let mut vol = make_vol();
        let mut trans = make_trans(&mut vol);
        let id = { let mut new_snapids = [0u32; 2]; let snapshot_subvols = [1, 0]; bch2_snapshot_node_create(&mut trans, 0, &mut new_snapids, &snapshot_subvols, 1).unwrap(); new_snapids[0] };
        assert_eq!(id, u32::MAX, "first root snapshot should get u32::MAX");
        trans.bch2_trans_commit().unwrap();

        let snap = bch2_snapshot_read_value(&trans, id).unwrap();
        assert_eq!(snap.parent, 0);
        assert_eq!(snap.subvol, 1);
        assert_eq!(snap.depth, 1);
        assert!(!snap.deleted);
        assert!(snap.is_leaf());
        assert!(snap.has_subvol());
    }

    #[test]
    fn test_create_root_snapshot_twice() {
        let mut vol = make_vol();
        let mut trans = make_trans(&mut vol);
        let id1 = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut trans, 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        trans.bch2_trans_commit().unwrap();
        let id2 = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [2, 0];
            bch2_snapshot_node_create(&mut trans, 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        trans.bch2_trans_commit().unwrap();
        assert_eq!(id1, u32::MAX);
        assert_eq!(id2, u32::MAX - 1, "second root should decrement");

        let snap2 = bch2_snapshot_read_value(&trans, id2).unwrap();
        assert_eq!(snap2.subvol, 2);
    }

    #[test]
    fn test_snapshot_node_set_deleted_marks_pending_delete() {
        let mut vol = make_vol();
        let mut trans = make_trans(&mut vol);
        let root = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut trans, 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        trans.bch2_trans_commit().unwrap();

        bch2_snapshot_node_set_deleted(&mut trans, root).unwrap();
        trans.bch2_trans_commit().unwrap();

        let deleted = bch2_snapshot_read_value(&trans, root).unwrap();
        assert!(deleted.deleted);
        assert!(deleted.flags.contains(BchSnapshotFlags::WILL_DELETE));
        assert!(!deleted.has_subvol());
        assert_eq!(deleted.subvol, 0);
        assert!(
            bch2_snapshot_read_value(&trans, root).is_some(),
            "pending-delete snapshot should still be readable"
        );
    }

    // ─── 子快照创建测试 ───

    #[test]
    fn test_create_child_snapshot() {
        let mut vol = make_vol();
        let mut trans = make_trans(&mut vol);
        let root = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut trans, 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        trans.bch2_trans_commit().unwrap();
        let child = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut trans, root, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };

        assert_eq!(child, u32::MAX - 1);
        trans.bch2_trans_commit().unwrap();

        let child_snap = bch2_snapshot_read_value(&trans, child).unwrap();
        assert_eq!(child_snap.parent, root);
        assert_eq!(child_snap.depth, 2);
        assert!(!child_snap.deleted);

        // 父节点的 children 应已更新
        let root_snap = bch2_snapshot_read_value(&trans, root).unwrap();
        assert_eq!(root_snap.children[0], child);
    }

    #[test]
    fn test_create_deep_chain() {
        let mut vol = make_vol();
        let mut trans = make_trans(&mut vol);
        let root = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut trans, 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        trans.bch2_trans_commit().unwrap();
        let mut prev = root;
        let mut ids = vec![root];
        for _ in 0..10 {
            let id = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut trans, prev, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };
            trans.bch2_trans_commit().unwrap();
            ids.push(id);
            prev = id;
        }

        // 验证 depth 递增
        for (i, &id) in ids.iter().enumerate() {
            let snap = bch2_snapshot_read_value(&trans, id).unwrap();
            assert_eq!(snap.depth, i as u32 + 1, "depth mismatch for id={}", id);
        }
    }

    // ─── bch2_snapshot_is_ancestor 测试 ───

    #[test]
    fn test_is_ancestor_self() {
        let mut vol = make_vol();
        assert!(bch2_snapshot_is_ancestor(&make_trans(&mut vol), 42, 42));
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

        assert!(bch2_snapshot_is_ancestor(
            &make_trans(&mut vol),
            child,
            root
        ));
        assert!(!bch2_snapshot_is_ancestor(
            &make_trans(&mut vol),
            root,
            child
        ));
    }

    #[test]
    fn test_is_ancestor_no_relation() {
        let mut vol = make_vol();
        let t1 = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        let t2 = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [2, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };

        assert!(!bch2_snapshot_is_ancestor(&make_trans(&mut vol), t2, t1));
        assert!(!bch2_snapshot_is_ancestor(&make_trans(&mut vol), t1, t2));
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
            let id = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), prev, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };
            ids.push(id);
            prev = id;
        }

        // root 是所有后代祖先
        for &id in &ids {
            assert!(
                bch2_snapshot_is_ancestor(&make_trans(&mut vol), id, root),
                "root should be ancestor of {}",
                id
            );
        }

        // 每层的祖先关系
        for i in 0..ids.len() {
            for j in i..ids.len() {
                assert!(
                    bch2_snapshot_is_ancestor(&make_trans(&mut vol), ids[j], ids[i]),
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
                    !bch2_snapshot_is_ancestor(&make_trans(&mut vol), ids[j], ids[i]),
                    "{} should NOT be ancestor of {}",
                    ids[i],
                    ids[j]
                );
            }
        }
    }

    #[test]
    fn test_is_ancestor_deleted_node() {
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
        // 删除后，祖先链仍然有效（对齐 bcachefs：WILL_DELETE 节点仍在树中可遍历）
        assert!(
            bch2_snapshot_is_ancestor(&make_trans(&mut vol), child, root),
            "deleted node should still have valid ancestor chain"
        );
    }

    // ─── Skiplist 测试 ───

    #[test]
    fn test_skip_list_depth_under_4() {
        let mut vol = make_vol();
        let root = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        // depth=1（root）: skip 全为 0
        let root_snap = bch2_snapshot_read_value(&make_trans(&mut vol), root).unwrap();
        assert_eq!(root_snap.skip, [0, 0, 0]);

        // depth=2: skip 均为 root 或 d2（两者互为祖先），且已排序
        let d2 = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), root, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };
        let snap2 = bch2_snapshot_read_value(&make_trans(&mut vol), d2).unwrap();
        assert_eq!(snap2.depth, 2);
        for &s in &snap2.skip {
            assert!(
                s == 0 || s == root || s == d2,
                "depth-2 skip entry {s} must be 0, root({root}), or d2({d2})"
            );
        }
        assert!(
            snap2.skip[0] <= snap2.skip[1] && snap2.skip[1] <= snap2.skip[2],
            "depth-2 skip should be sorted"
        );

        // depth=3: skip 均为 root, d2, 或 d3，且已排序
        let d3 = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), d2, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };
        let snap3 = bch2_snapshot_read_value(&make_trans(&mut vol), d3).unwrap();
        assert_eq!(snap3.depth, 3);
        for &s in &snap3.skip {
            assert!(
                s == 0 || s == root || s == d2 || s == d3,
                "depth-3 skip entry {s} must be 0, root({root}), d2({d2}), or d3({d3})"
            );
        }
        assert!(
            snap3.skip[0] <= snap3.skip[1] && snap3.skip[1] <= snap3.skip[2],
            "depth-3 skip should be sorted"
        );
    }

    #[test]
    fn test_skip_list_populated_exponential() {
        let mut vol = make_vol();
        let root = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        let mut prev = root;
        for _ in 0..5 {
            prev = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), prev, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };
        }
        let snap = bch2_snapshot_read_value(&make_trans(&mut vol), prev).unwrap();
        assert_eq!(snap.depth, 6);
        assert!(snap.skip[0] != 0, "skip[0] should be populated at depth 6");
        assert!(snap.skip[1] != 0, "skip[1] should be populated at depth 6");
        assert!(snap.skip[2] != 0, "skip[2] should be populated at depth 6");
    }

    #[test]
    fn test_skip_list_ordered() {
        let mut vol = make_vol();
        let root = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        let mut prev = root;
        for _ in 0..20 {
            prev = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), prev, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };
        }
        let snap = bch2_snapshot_read_value(&make_trans(&mut vol), prev).unwrap();
        // skip[0] <= skip[1] <= skip[2]（非降序，bubble_sort 允许相等）
        // 快照 ID 从 u32::MAX 向下分配（父 > 子），祖先越老 ID 越大。
        if snap.skip[1] != 0 {
            assert!(
                snap.skip[0] <= snap.skip[1],
                "skip[0]={} <= skip[1]={}",
                snap.skip[0],
                snap.skip[1]
            );
        }
        if snap.skip[2] != 0 {
            assert!(
                snap.skip[1] <= snap.skip[2],
                "skip[1]={} <= skip[2]={}",
                snap.skip[1],
                snap.skip[2]
            );
        }
    }

    #[test]
    fn test_skip_list_ancestor_chain() {
        let mut vol = make_vol();
        let root = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        let mut prev = root;
        for _ in 0..20 {
            prev = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), prev, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };
        }
        let snap = bch2_snapshot_read_value(&make_trans(&mut vol), prev).unwrap();
        // 确认 skip 中的 ID 确实是 prev 的祖先
        for &s in &snap.skip {
            if s != 0 {
                assert!(
                    bch2_snapshot_is_ancestor(&make_trans(&mut vol), prev, s),
                    "skip {} should be ancestor of {}",
                    s,
                    prev
                );
            }
        }
    }

    // ─── 删除操作测试 ───

    #[test]
    fn test_delete_snapshot() {
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

        // 待删除节点仍属于 live table，可继续读取以完成后续清理
        assert!(
            bch2_snapshot_read_value(&make_trans(&mut vol), child).is_some(),
            "pending-delete snapshot should still be readable"
        );

        // list 仍应包含待删除快照
        let list = bch2_snapshot_list(&make_trans(&mut vol));
        let ids: Vec<u32> = list.iter().map(|(id, _)| *id).collect();
        assert!(ids.contains(&root), "root should still be in list");
        assert!(
            ids.contains(&child),
            "pending-delete child should still be in list"
        );
    }

    #[test]
    fn test_delete_nonexistent() {
        let mut vol = make_vol();
        let result = bch2_snapshot_node_set_deleted(&mut make_trans(&mut vol), 999);
        assert!(result.is_err());
    }

    // ─── 列表查询测试 ───

    #[test]
    fn test_list_snapshots_empty() {
        let mut vol = make_vol();
        let list = bch2_snapshot_list(&make_trans(&mut vol));
        assert!(list.is_empty());
    }

    #[test]
    fn test_list_snapshots() {
        let mut vol = make_vol();
        let root = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        let c1 = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 2];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), root, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };
        let parent = bch2_snapshot_read_value(&make_trans(&mut vol), root).unwrap();
        let c2 = parent.children[1];

        let list = bch2_snapshot_list(&make_trans(&mut vol));
        assert_eq!(list.len(), 3);

        // 按 ID 降序排列（父优先）
        assert_eq!(list[0].0, root); // u32::MAX
                                     // c1 和 c2 在后面
        let ids: Vec<u32> = list.iter().map(|(id, _)| *id).collect();
        assert!(ids.contains(&c1));
        assert!(ids.contains(&c2));
    }

    #[test]
    fn test_list_snapshots_after_delete() {
        let mut vol = make_vol();
        let root = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        let child = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 2];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), root, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };

        bch2_snapshot_node_set_deleted(&mut make_trans(&mut vol), child).unwrap();
        let list = bch2_snapshot_list(&make_trans(&mut vol));
        // 待删除节点仍保留在 live table；精确的 1->2 split 还包括 sibling。
        let ids: Vec<u32> = list.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids.len(), 3);
        assert!(ids.contains(&root));
        assert!(ids.contains(&child));
    }

    // ─── bch2_snapshot_next_id 测试 ───

    #[test]
    fn test_next_id_empty() {
        let mut vol = make_vol();
        assert_eq!(bch2_snapshot_next_id(&make_trans(&mut vol)), u32::MAX);
    }

    #[test]
    fn test_next_id_after_creation() {
        let mut vol = make_vol();
        let id = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        assert_eq!(id, u32::MAX);
        // 创建后，next_id 应返回 u32::MAX - 1
        assert_eq!(bch2_snapshot_next_id(&make_trans(&mut vol)), u32::MAX - 1);
    }

    #[test]
    fn test_next_id_sees_pending_journal_insert() {
        let mut vol = make_vol();
        let root = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        let mut trans = make_trans(&mut vol);

        let pending_id = root.wrapping_sub(1);
        let pending_snap = SnapshotT::new_leaf(root, 2, 0, 2, 0);
        let pending_bytes = bincode::serialize(&pending_snap).unwrap();
        trans.bch2_trans_update_raw(
            BtreeId::Snapshots,
            0,
            false,
            BtreeKey::new(0, pending_id, KeyType::Normal),
            pending_bytes,
            0,
        );

        assert_eq!(
            bch2_snapshot_next_id(&trans),
            root.wrapping_sub(2),
            "pending journal insert must reserve the slot"
        );
    }

    #[test]
    fn test_snapshot_node_create_skips_pending_snapshot_ids() {
        let mut vol = make_vol();
        let root = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        let mut trans = make_trans(&mut vol);

        let blocked_id = root.wrapping_sub(1);
        let blocked_snap = SnapshotT::new_leaf(root, 2, 0, 2, 0);
        let blocked_bytes = bincode::serialize(&blocked_snap).unwrap();
        trans.bch2_trans_update_raw(
            BtreeId::Snapshots,
            0,
            false,
            BtreeKey::new(0, blocked_id, KeyType::Normal),
            blocked_bytes,
            0,
        );

        let created = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [3, 4];
            bch2_snapshot_node_create(&mut trans, root, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };
        assert_eq!(
            created,
            root.wrapping_sub(2),
            "snapshot creation must skip pending journal ids"
        );

        let parent = bch2_snapshot_read_value(&trans, root).unwrap();
        assert_eq!(parent.children, [created, root.wrapping_sub(3)]);
    }

    #[test]
    fn test_snapshot_node_create_rejects_parent_with_children() {
        let mut vol = make_vol();
        let root = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        let _child =
            {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [2, 1];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), root, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };

        let result = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [3, 4];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), root, &mut new_snapids, &snapshot_subvols, 2)
        };
        assert!(matches!(
            result,
            Err(StorageError::InvalidArgument(message))
                if message == "snapshot parent already has children"
        ));
    }

    // ─── 序列化 roundtrip 测试 ───

    #[test]
    fn test_snapshot_value_serde_in_btree() {
        let mut vol = make_vol();
        let id = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        let snap = bch2_snapshot_read_value(&make_trans(&mut vol), id).unwrap();

        assert_eq!(snap.parent, 0);
        assert_eq!(snap.subvol, 1);
        assert_eq!(snap.depth, 1);
        assert!(!snap.deleted);

        // 验证 btree 中存在
        let entry = vol.get_entry_raw(BtreeId::Snapshots, Bpos::new(0, 0, id));
        assert!(entry.is_some(), "snapshot should exist in btree");
    }

    // ─── 大规模树祖先测试 ───

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
        for _ in 0..100 {
            prev = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), prev, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };
        }

        // root 是最后一个节点的祖先
        assert!(bch2_snapshot_is_ancestor(&make_trans(&mut vol), prev, root));
        // 中间节点也是祖先
        let mid = u32::MAX - 99;
        assert!(bch2_snapshot_is_ancestor(&make_trans(&mut vol), prev, mid));
        // 反向不是
        assert!(!bch2_snapshot_is_ancestor(&make_trans(&mut vol), mid, prev));
    }

    // ─── DFS 深度优先遍历测试 ───

    // ─── DfsIter 迭代器测试 ───
    #[test]
    fn test_dfs_iter_chain() {
        let mut vol = make_vol();
        let root = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        let c1 = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), root, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };
        let c2 = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), c1, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };

        let iter = DfsIter::new(&vol, root);
        let ids: Vec<u32> = iter.collect();
        assert_eq!(ids.len(), 5);
        assert_eq!(ids[0], c2, "DFS iter should start with leaf");
        assert_eq!(ids[4], root, "DFS iter should end with root");
    }

    // ─── 参数化 Skip List 有序性测试 ───

    /// 验证不同深度下 skip list 的有序性：skip[0] < skip[1] < skip[2]。
    /// 测试深度 1~20，确保在各级深度上都满足递增顺序。
    #[test]
    fn test_skip_list_ordered_depth_1() {
        test_skip_ordered_at_depth(1);
    }
    #[test]
    fn test_skip_list_ordered_depth_2() {
        test_skip_ordered_at_depth(2);
    }
    #[test]
    fn test_skip_list_ordered_depth_3() {
        test_skip_ordered_at_depth(3);
    }
    #[test]
    fn test_skip_list_ordered_depth_4() {
        test_skip_ordered_at_depth(4);
    }
    #[test]
    fn test_skip_list_ordered_depth_5() {
        test_skip_ordered_at_depth(5);
    }
    #[test]
    fn test_skip_list_ordered_depth_6() {
        test_skip_ordered_at_depth(6);
    }
    #[test]
    fn test_skip_list_ordered_depth_7() {
        test_skip_ordered_at_depth(7);
    }
    #[test]
    fn test_skip_list_ordered_depth_8() {
        test_skip_ordered_at_depth(8);
    }
    #[test]
    fn test_skip_list_ordered_depth_9() {
        test_skip_ordered_at_depth(9);
    }
    #[test]
    fn test_skip_list_ordered_depth_10() {
        test_skip_ordered_at_depth(10);
    }
    #[test]
    fn test_skip_list_ordered_depth_11() {
        test_skip_ordered_at_depth(11);
    }
    #[test]
    fn test_skip_list_ordered_depth_12() {
        test_skip_ordered_at_depth(12);
    }
    #[test]
    fn test_skip_list_ordered_depth_13() {
        test_skip_ordered_at_depth(13);
    }
    #[test]
    fn test_skip_list_ordered_depth_14() {
        test_skip_ordered_at_depth(14);
    }
    #[test]
    fn test_skip_list_ordered_depth_15() {
        test_skip_ordered_at_depth(15);
    }
    #[test]
    fn test_skip_list_ordered_depth_16() {
        test_skip_ordered_at_depth(16);
    }
    #[test]
    fn test_skip_list_ordered_depth_17() {
        test_skip_ordered_at_depth(17);
    }
    #[test]
    fn test_skip_list_ordered_depth_18() {
        test_skip_ordered_at_depth(18);
    }
    #[test]
    fn test_skip_list_ordered_depth_19() {
        test_skip_ordered_at_depth(19);
    }
    #[test]
    fn test_skip_list_ordered_depth_20() {
        test_skip_ordered_at_depth(20);
    }

    fn test_skip_ordered_at_depth(target_depth: u32) {
        let mut vol = make_vol();
        let root = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };

        // 创建链到 target_depth（root 是 depth=1）
        let mut prev = root;
        for _ in 1..target_depth {
            prev = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), prev, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };
        }

        let snap = bch2_snapshot_read_value(&make_trans(&mut vol), prev).unwrap();
        assert_eq!(snap.depth, target_depth, "depth mismatch");

        // depth = 1 时 skip 全为 0，不检查有序性
        if target_depth == 1 {
            assert_eq!(snap.skip, [0, 0, 0], "depth=1 should have empty skip");
            return;
        }
        // depth >= 4 时才可能有 skip[2] != 0；depth=2,3 只有跳过 skip[2] 的有序性检查

        // depth >= 4 时验证 skip[0] <= skip[1] <= skip[2]（非降序，bubble_sort 允许相等）
        // 快照 ID 从 u32::MAX 向下分配（父 > 子），祖先越老 ID 越大。
        if snap.skip[1] != 0 {
            assert!(
                snap.skip[0] <= snap.skip[1],
                "depth={}: skip[0]={} <= skip[1]={}",
                target_depth,
                snap.skip[0],
                snap.skip[1]
            );
        }
        if snap.skip[2] != 0 {
            assert!(
                snap.skip[1] <= snap.skip[2],
                "depth={}: skip[1]={} <= skip[2]={}",
                target_depth,
                snap.skip[1],
                snap.skip[2]
            );
        }
    }

    // ─── bch2_fix_child_of_deleted_snapshot 测试 ───

    #[test]
    fn test_fix_child_of_deleted_depth_adjust() {
        let mut vol = make_vol();
        let root = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        // root → a → b → leaf
        let a = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [0, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), root, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };
        let b = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [0, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), a, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };
        // 手动清除 SUBVOL 让 non-leaf 节点无 subvol
        for id in [a, b] {
            let mut snap = bch2_snapshot_read_value(&make_trans(&mut vol), id).unwrap();
            snap.flags = BchSnapshotFlags::empty();
            snap.subvol = 0;
            let bytes = bincode::serialize(&snap).unwrap();
            let entry = BtreeEntry::raw(Bpos::new(0, 0, id), KeyType::Normal, bytes);
            vol.insert_entry_raw(BtreeId::Snapshots, entry, 0);
        }
        let leaf = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [2, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), b, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };

        // 删除 b（interior），leaf 的 depth 应从 4 减到 3，skip 应更新
        bch2_snapshot_node_set_deleted(&mut make_trans(&mut vol), b).unwrap();
        bch2_fix_child_of_deleted_snapshot(&mut make_trans(&mut vol), &[b]).unwrap();

        let leaf_snap = bch2_snapshot_read_value(&make_trans(&mut vol), leaf).unwrap();
        assert_eq!(leaf_snap.depth, 3, "depth should reduce from 4 to 3");
        // skip 按值升序排列（0 < a < root），不应包含已删节点 b
        for &s in &leaf_snap.skip {
            if s != 0 {
                assert_ne!(s, b, "skip should not reference deleted node b");
                // 所有非 0 skip 必须是 leaf 的祖先
                assert!(
                    bch2_snapshot_is_ancestor(&make_trans(&mut vol), leaf, s),
                    "skip {s} should be ancestor of leaf"
                );
            }
        }
    }

    #[test]
    fn test_fix_child_of_deleted_skip_replacement() {
        let mut vol = make_vol();
        let root = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        // 建一条长链用于测试 skip 重定向
        let mut ids = vec![root];
        for _ in 0..8 {
            let id =
                {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [0, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), *ids.last().unwrap(), &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };
            ids.push(id);
        }
        // 清除 interior 的 SUBVOL
        for i in 1..8 {
            let mut snap = bch2_snapshot_read_value(&make_trans(&mut vol), ids[i]).unwrap();
            snap.flags = BchSnapshotFlags::empty();
            snap.subvol = 0;
            let bytes = bincode::serialize(&snap).unwrap();
            let entry = BtreeEntry::raw(Bpos::new(0, 0, ids[i]), KeyType::Normal, bytes);
            vol.insert_entry_raw(BtreeId::Snapshots, entry, 0);
        }
        let leaf_id =
            {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [2, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), *ids.last().unwrap(), &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };

        // 记录删除前的 leaf skip
        let before = bch2_snapshot_read_value(&make_trans(&mut vol), leaf_id).unwrap();
        let before_skip = before.skip;

        // 删除 leaf 当前 skip 表中实际引用到的一个祖先，保证 fixup 需要重定向 skip。
        let deleted = before_skip
            .into_iter()
            .find(|&s| s != 0)
            .expect("leaf should have a non-zero skip ancestor");
        bch2_snapshot_node_set_deleted(&mut make_trans(&mut vol), deleted).unwrap();
        bch2_fix_child_of_deleted_snapshot(&mut make_trans(&mut vol), &[deleted]).unwrap();

        let after = bch2_snapshot_read_value(&make_trans(&mut vol), leaf_id).unwrap();
        assert_eq!(after.depth, before.depth - 1, "depth reduced by 1");
        // skip 应该变化（移除了对被删节点的引用）
        assert_ne!(after.skip, before_skip, "skip should change after fixup");
        // 所有 skip 不应指向已删节点
        for s in after.skip {
            if s != 0 {
                assert_ne!(s, deleted, "skip should not point to deleted node");
            }
        }
    }

    #[test]
    fn test_fix_child_of_deleted_empty_deleted_list() {
        let mut vol = make_vol();
        let root = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        let _child = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [2, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), root, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };

        // 空 deleted_ids → 不应有变化
        bch2_fix_child_of_deleted_snapshot(&mut make_trans(&mut vol), &[]).unwrap();

        let snap = bch2_snapshot_read_value(&make_trans(&mut vol), root).unwrap();
        assert_eq!(snap.depth, 1, "should not change");
    }

    #[test]
    fn test_fix_child_of_deleted_self_in_list() {
        let mut vol = make_vol();
        let root = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        let child = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [2, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), root, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };

        // 自身在 deleted_ids 中应跳过
        bch2_fix_child_of_deleted_snapshot(&mut make_trans(&mut vol), &[child]).unwrap();
        let snap = bch2_snapshot_read_value(&make_trans(&mut vol), root).unwrap();
        assert_eq!(snap.depth, 1, "root should not change");
    }

    // ─── bch2_snapshot_node_delete 测试 ───

    #[test]
    fn test_snapshot_node_delete_leaf() {
        let mut vol = make_vol();
        let root = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        let child = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [2, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), root, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };

        bch2_snapshot_node_delete(&mut make_trans(&mut vol), child, false).unwrap();

        let root_snap = bch2_snapshot_read_value(&make_trans(&mut vol), root).unwrap();
        assert_ne!(root_snap.children, [0, 0], "the sibling child remains");
        assert!(
            bch2_snapshot_read_value(&make_trans(&mut vol), child).is_none(),
            "leaf should be deleted"
        );
    }

    #[test]
    fn test_snapshot_node_delete_interior() {
        let mut vol = make_vol();
        let root = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        let leaf = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [2, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), root, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };
        // interior 节点位于 root 和 leaf 之间，需要手动更新 leaf.parent
        let interior = create_interior_snapshot(&mut make_trans(&mut vol), root, [leaf, 0], 0, 0);
        {
            let mut snap = bch2_snapshot_read_value(&make_trans(&mut vol), root).unwrap();
            snap.children[0] = interior;
            let bytes = bincode::serialize(&snap).unwrap();
            vol.insert_entry_raw(
                BtreeId::Snapshots,
                BtreeEntry::raw(Bpos::new(0, 0, root), KeyType::Normal, bytes),
                0,
            );
        }
        {
            let mut snap = bch2_snapshot_read_value(&make_trans(&mut vol), leaf).unwrap();
            snap.parent = interior;
            let bytes = bincode::serialize(&snap).unwrap();
            let entry = BtreeEntry::raw(Bpos::new(0, 0, leaf), KeyType::Normal, bytes);
            vol.insert_entry_raw(BtreeId::Snapshots, entry, 0);
        }

        bch2_snapshot_node_delete(&mut make_trans(&mut vol), interior, true).unwrap();

        let leaf_snap = bch2_snapshot_read_value(&make_trans(&mut vol), leaf).unwrap();
        assert_eq!(
            leaf_snap.parent, root,
            "leaf parent re-parented to grandparent (root)"
        );
        assert!(
            bch2_snapshot_read_value(&make_trans(&mut vol), interior).is_none(),
            "interior should be deleted"
        );
    }

    #[test]
    fn test_snapshot_node_delete_two_children() {
        let mut vol = make_vol();
        let root = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        // extra_child_subvol 创建两个子节点（1变2 语义）
        let _pair = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [2, 3];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), root, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };

        let result = bch2_snapshot_node_delete(&mut make_trans(&mut vol), root, false);
        assert!(
            result.is_err(),
            "snapshot with two children cannot be deleted"
        );
    }

    #[test]
    fn test_snapshot_node_delete_root_leaf() {
        let mut vol = make_vol();
        let root = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        bch2_snapshot_node_delete(&mut make_trans(&mut vol), root, false).unwrap();
        assert!(
            bch2_snapshot_read_value(&make_trans(&mut vol), root).is_none(),
            "root leaf should be deletable"
        );
    }

    #[test]
    fn test_snapshot_node_delete_root_interior() {
        let mut vol = make_vol();
        let root = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        let _leaf = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [2, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), root, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };

        let result = bch2_snapshot_node_delete(&mut make_trans(&mut vol), root, true);
        assert!(result.is_err(), "a bcachefs interior with two children cannot be deleted");
    }

    #[test]
    fn test_snapshot_node_delete_nonexistent() {
        let mut vol = make_vol();
        let result = bch2_snapshot_node_delete(&mut make_trans(&mut vol), 999, false);
        assert!(result.is_err(), "nonexistent should error");
    }

    // ─── 死快照批量清理测试 ───

    #[test]
    fn test_delete_dead_no_snapshots() {
        let mut vol = make_vol();
        let skipped = bch2_delete_dead_snapshots(&mut make_trans(&mut vol)).unwrap();
        assert!(skipped.is_empty(), "no snapshots → no skipped");
    }

    #[test]
    fn test_delete_dead_no_dead_snapshots() {
        let mut vol = make_vol();
        let _root = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        let skipped = bch2_delete_dead_snapshots(&mut make_trans(&mut vol)).unwrap();
        assert!(skipped.is_empty(), "no deleted → nothing to clean");
    }

    // ─── 辅助：创建 interior 快照（无 SUBVOL 标志） ───

    fn create_interior_snapshot(
        trans: &mut BtreeTrans,
        parent: u32,
        children: [u32; 2],
        subvol: u32,
        tree: u32,
    ) -> u32 {
        let parent_snap = bch2_snapshot_read_value(trans, parent).unwrap();
        let depth = parent_snap.depth + 1;
        let id = bch2_snapshot_next_id(trans);
        let gap = parent.saturating_sub(id);
        let bitmap = if gap >= 128 {
            0
        } else if gap == 0 {
            parent_snap.is_ancestor
        } else {
            parent_snap.is_ancestor.wrapping_shl(gap) | (1u128 << (gap - 1))
        };
        let mut interior =
            SnapshotT::new_interior(parent, children, tree, depth, current_timestamp());
        interior.is_ancestor = bitmap;
        // interior 节点不持有子卷引用，但可以设置 subvol 为被引用的 leaf ID
        let mut interior_with_subvol = interior;
        interior_with_subvol.subvol = subvol;
        let bytes = bincode::serialize(&interior_with_subvol).unwrap();
        trans.bch2_trans_update_raw(
            BtreeId::Snapshots,
            0,
            false,
            BtreeKey::new(0, id, KeyType::Normal),
            bytes,
            0,
        );
        id
    }

    #[test]
    fn test_delete_dead_single_dead_leaf() {
        let mut vol = make_vol();
        let root = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        let leaf = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [2, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), root, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };
        let interior =
            create_interior_snapshot(&mut make_trans(&mut vol), root, [leaf, 0], leaf, 0);

        // 标记 interior 为已删除
        bch2_snapshot_node_set_deleted(&mut make_trans(&mut vol), interior).unwrap();

        let skipped = bch2_delete_dead_snapshots(&mut make_trans(&mut vol)).unwrap();
        assert_eq!(
            skipped.len(),
            1,
            "interior has volume ref leaf → should skip"
        );
        assert_eq!(skipped[0], interior, "interior should be in skip list");
    }

    #[test]
    fn test_delete_dead_skips_volume_ref() {
        let mut vol = make_vol();
        let root = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        let _child = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), root, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };

        // 标记 root 为已删除——但 root 有 SUBVOL（被 volume 引用），应跳过
        bch2_snapshot_node_set_deleted(&mut make_trans(&mut vol), root).unwrap();

        let skipped = bch2_delete_dead_snapshots(&mut make_trans(&mut vol)).unwrap();
        assert_eq!(skipped.len(), 1, "root should be skipped");
        assert_eq!(skipped[0], root, "skipped should be root");

        if let Some(entry) = vol.get_entry_raw(BtreeId::Snapshots, Bpos::new(0, 0, root)) {
            let bytes = match &entry.value {
                KeyValue::Raw(b) => b.clone(),
                _ => panic!("expected Raw value"),
            };
            let snap: SnapshotT = bincode::deserialize(&bytes).unwrap();
            assert!(snap.deleted, "root should still be marked deleted");
        }
    }

    #[test]
    fn test_delete_dead_removes_interior_without_subvol() {
        let mut vol = make_vol();
        let root = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        let leaf1 = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [2, 3];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), root, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };
        let parent = bch2_snapshot_read_value(&make_trans(&mut vol), root).unwrap();
        let leaf2 = parent.children[1];

        // 创建一个 interior 节点链接两个 leaf
        let interior =
            create_interior_snapshot(&mut make_trans(&mut vol), root, [leaf1, leaf2], 0, 0);
        // 更新 leaf 的 parent 为 interior
        let update_leaf = |trans: &mut BtreeTrans, id: u32, parent: u32| {
            let mut snap = bch2_snapshot_read_value(trans, id).unwrap();
            snap.parent = parent;
            let bytes = bincode::serialize(&snap).unwrap();
            trans.bch2_trans_update_raw(
                BtreeId::Snapshots,
                0,
                false,
                BtreeKey::new(0, id, KeyType::Normal),
                bytes,
                0,
            );
        };
        update_leaf(&mut make_trans(&mut vol), leaf1, interior);
        update_leaf(&mut make_trans(&mut vol), leaf2, interior);

        // 标记 interior 为已删除（无 SUBVOL）
        bch2_snapshot_node_set_deleted(&mut make_trans(&mut vol), interior).unwrap();

        // leaf1, leaf2 有 SUBVOL → 跳过
        let skipped = bch2_delete_dead_snapshots(&mut make_trans(&mut vol)).unwrap();
        assert_eq!(skipped.len(), 1, "subtree has volume ref leaves → skip");
    }

    #[test]
    fn test_delete_dead_idempotent() {
        let mut vol = make_vol();
        let root = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        // interior 无 SUBVOL
        let interior = create_interior_snapshot(&mut make_trans(&mut vol), root, [0, 0], 0, 0);
        {
            let mut snap = bch2_snapshot_read_value(&make_trans(&mut vol), root).unwrap();
            snap.children[0] = interior;
            let bytes = bincode::serialize(&snap).unwrap();
            vol.insert_entry_raw(
                BtreeId::Snapshots,
                BtreeEntry::raw(Bpos::new(0, 0, root), KeyType::Normal, bytes),
                0,
            );
        }

        bch2_snapshot_node_set_deleted(&mut make_trans(&mut vol), interior).unwrap();

        // 第一次清理：interior 无 SUBVOL 引用 → 应被删除
        let skipped1 = bch2_delete_dead_snapshots(&mut make_trans(&mut vol)).unwrap();
        assert!(skipped1.is_empty(), "first cleanup should delete interior");

        // 第二次清理，应空运行不 panic
        let skipped2 = bch2_delete_dead_snapshots(&mut make_trans(&mut vol)).unwrap();
        assert!(skipped2.is_empty(), "second cleanup should be no-op");
    }

    #[test]
    fn test_delete_dead_interior_preserves_children() {
        let mut vol = make_vol();
        let root = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        // root → a → b → leaf，a 和 b 无 subvol
        let a = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [0, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), root, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };
        {
            let mut snap = bch2_snapshot_read_value(&make_trans(&mut vol), a).unwrap();
            snap.flags = BchSnapshotFlags::empty();
            snap.subvol = 0;
            let bytes = bincode::serialize(&snap).unwrap();
            let entry = BtreeEntry::raw(Bpos::new(0, 0, a), KeyType::Normal, bytes);
            vol.insert_entry_raw(BtreeId::Snapshots, entry, 0);
        }
        let b = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [0, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), a, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };
        {
            let mut snap = bch2_snapshot_read_value(&make_trans(&mut vol), b).unwrap();
            snap.flags = BchSnapshotFlags::empty();
            snap.subvol = 0;
            let bytes = bincode::serialize(&snap).unwrap();
            let entry = BtreeEntry::raw(Bpos::new(0, 0, b), KeyType::Normal, bytes);
            vol.insert_entry_raw(BtreeId::Snapshots, entry, 0);
        }
        let leaf = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [0, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), b, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };
        // 清除 leaf 的 SUBVOL flag（subvol=0 时默认仍带 SUBVOL）
        {
            let mut snap = bch2_snapshot_read_value(&make_trans(&mut vol), leaf).unwrap();
            snap.flags = BchSnapshotFlags::empty();
            let bytes = bincode::serialize(&snap).unwrap();
            let entry = BtreeEntry::raw(Bpos::new(0, 0, leaf), KeyType::Normal, bytes);
            vol.insert_entry_raw(BtreeId::Snapshots, entry, 0);
        }

        // The exact bcachefs API creates a sibling leaf for every non-root
        // split.  This test models a single referenced path, so clear those
        // synthetic sibling leaves before reclaiming the interior chain.
        for parent_id in [root, a, b] {
            let mut parent = bch2_snapshot_read_value(&make_trans(&mut vol), parent_id).unwrap();
            let sibling = parent.children[1];
            parent.children[1] = 0;
            let parent_bytes = bincode::serialize(&parent).unwrap();
            vol.insert_entry_raw(
                BtreeId::Snapshots,
                BtreeEntry::raw(Bpos::new(0, 0, parent_id), KeyType::Normal, parent_bytes),
                0,
            );
            if sibling != 0 {
                let mut trans = make_trans(&mut vol);
                trans.bch2_trans_delete(
                    BtreeId::Snapshots,
                    0,
                    false,
                    BtreeKey::new(0, sibling, KeyType::Normal),
                    0,
                );
                trans.bch2_trans_commit().unwrap();
            }
        }

        // 标记 b 为已删除
        bch2_snapshot_node_set_deleted(&mut make_trans(&mut vol), b).unwrap();

        let skipped = bch2_delete_dead_snapshots(&mut make_trans(&mut vol)).unwrap();
        assert!(skipped.is_empty(), "skipped={skipped:?}");

        // b 先进入 NO_KEYS，物理删除由独立 interior 回收阶段完成。
        assert!(
            bch2_snapshot_read_value(&make_trans(&mut vol), b)
                .map(|snap| snap.flags.contains(BchSnapshotFlags::NO_KEYS))
                .unwrap_or(false),
            "b should enter NO_KEYS"
        );

        bch2_delete_dead_interior_snapshots(&mut make_trans(&mut vol)).unwrap();
        assert!(
            bch2_snapshot_read_value(&make_trans(&mut vol), b).is_none(),
            "b should be deleted by interior cleanup"
        );

        // 没有子卷引用的整条链均已完成 dead-snapshot 回收。
        assert!(bch2_snapshot_read_value(&make_trans(&mut vol), leaf).is_none());
        assert!(bch2_snapshot_read_value(&make_trans(&mut vol), a).is_none());
    }

    // ─── bch2_delete_dead_interior_snapshots 测试 ───

    #[test]
    fn test_delete_dead_interior_single_child() {
        let mut vol = make_vol();
        let root = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        let leaf = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [2, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), root, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };
        let interior = create_interior_snapshot(&mut make_trans(&mut vol), root, [leaf, 0], 0, 0);
        {
            let mut snap = bch2_snapshot_read_value(&make_trans(&mut vol), root).unwrap();
            snap.children[0] = interior;
            let bytes = bincode::serialize(&snap).unwrap();
            vol.insert_entry_raw(
                BtreeId::Snapshots,
                BtreeEntry::raw(Bpos::new(0, 0, root), KeyType::Normal, bytes),
                0,
            );
        }
        // 更新 leaf parent 指向 interior
        {
            let mut snap = bch2_snapshot_read_value(&make_trans(&mut vol), leaf).unwrap();
            snap.parent = interior;
            let bytes = bincode::serialize(&snap).unwrap();
            let entry = BtreeEntry::raw(Bpos::new(0, 0, leaf), KeyType::Normal, bytes);
            vol.insert_entry_raw(BtreeId::Snapshots, entry, 0);
        }
        // 设置 NO_KEYS flag
        {
            let mut snap = bch2_snapshot_read_value(&make_trans(&mut vol), interior).unwrap();
            snap.flags.insert(BchSnapshotFlags::NO_KEYS);
            let bytes = bincode::serialize(&snap).unwrap();
            let entry = BtreeEntry::raw(Bpos::new(0, 0, interior), KeyType::Normal, bytes);
            vol.insert_entry_raw(BtreeId::Snapshots, entry, 0);
        }

        bch2_delete_dead_interior_snapshots(&mut make_trans(&mut vol)).unwrap();

        // interior 应被删除
        assert!(bch2_snapshot_read_value(&make_trans(&mut vol), interior).is_none());
        // leaf 应存活
        assert!(bch2_snapshot_read_value(&make_trans(&mut vol), leaf).is_some());
    }

    #[test]
    fn test_delete_dead_interior_skips_two_children() {
        let mut vol = make_vol();
        let root = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        let leaf1 = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [2, 3];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), root, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };
        let parent = bch2_snapshot_read_value(&make_trans(&mut vol), root).unwrap();
        let leaf2 = parent.children[1];
        let interior =
            create_interior_snapshot(&mut make_trans(&mut vol), root, [leaf1, leaf2], 0, 0);
        {
            let mut snap = bch2_snapshot_read_value(&make_trans(&mut vol), interior).unwrap();
            snap.flags.insert(BchSnapshotFlags::NO_KEYS);
            let bytes = bincode::serialize(&snap).unwrap();
            let entry = BtreeEntry::raw(Bpos::new(0, 0, interior), KeyType::Normal, bytes);
            vol.insert_entry_raw(BtreeId::Snapshots, entry, 0);
        }

        bch2_delete_dead_interior_snapshots(&mut make_trans(&mut vol)).unwrap();

        // 两个孩子时不应删除
        assert!(bch2_snapshot_read_value(&make_trans(&mut vol), interior).is_some());
    }

    #[test]
    fn test_delete_dead_interior_skips_no_no_keys() {
        let mut vol = make_vol();
        let root = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        let leaf = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [2, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), root, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };
        let interior = create_interior_snapshot(&mut make_trans(&mut vol), root, [leaf, 0], 0, 0);
        bch2_delete_dead_interior_snapshots(&mut make_trans(&mut vol)).unwrap();
        assert!(bch2_snapshot_read_value(&make_trans(&mut vol), interior).is_some());
    }

    #[test]
    fn test_delete_dead_interior_chain() {
        let mut vol = make_vol();
        let root = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        let leaf = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [2, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), root, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };
        // root → i1 → i2 → leaf
        let i1 = create_interior_snapshot(&mut make_trans(&mut vol), root, [leaf, 0], 0, 0);
        let i2 = create_interior_snapshot(&mut make_trans(&mut vol), i1, [leaf, 0], 0, 0);
        // 构造与 bcachefs snapshot tree 一致的 root → i1 → i2 → leaf 链。
        for (parent, child) in [(root, i1), (i1, i2)] {
            let mut snap = bch2_snapshot_read_value(&make_trans(&mut vol), parent).unwrap();
            snap.children = [child, 0];
            let bytes = bincode::serialize(&snap).unwrap();
            vol.insert_entry_raw(
                BtreeId::Snapshots,
                BtreeEntry::raw(Bpos::new(0, 0, parent), KeyType::Normal, bytes),
                0,
            );
        }
        // 更新 leaf parent → i2
        {
            let mut snap = bch2_snapshot_read_value(&make_trans(&mut vol), leaf).unwrap();
            snap.parent = i2;
            let bytes = bincode::serialize(&snap).unwrap();
            let entry = BtreeEntry::raw(Bpos::new(0, 0, leaf), KeyType::Normal, bytes);
            vol.insert_entry_raw(BtreeId::Snapshots, entry, 0);
        }
        for &id in &[i2, i1] {
            let mut snap = bch2_snapshot_read_value(&make_trans(&mut vol), id).unwrap();
            snap.flags.insert(BchSnapshotFlags::NO_KEYS);
            let bytes = bincode::serialize(&snap).unwrap();
            let entry = BtreeEntry::raw(Bpos::new(0, 0, id), KeyType::Normal, bytes);
            vol.insert_entry_raw(BtreeId::Snapshots, entry, 0);
        }

        bch2_delete_dead_interior_snapshots(&mut make_trans(&mut vol)).unwrap();

        assert!(bch2_snapshot_read_value(&make_trans(&mut vol), i1).is_none());
        assert!(bch2_snapshot_read_value(&make_trans(&mut vol), i2).is_none());
        assert!(bch2_snapshot_read_value(&make_trans(&mut vol), leaf).is_some());
    }

    // ─── check_should_delete_snapshot 测试 ───

    #[test]
    fn test_check_should_delete_deleted_flag() {
        let mut snap = SnapshotT::new_leaf(1, 0, 0, 2, 0);
        snap.flags = BchSnapshotFlags::empty();
        snap.deleted = true;
        assert_eq!(
            check_should_delete_snapshot(&snap),
            Some(DeadSnapshotType::Leaf)
        );
    }

    #[test]
    fn test_check_should_delete_will_delete() {
        let mut snap = SnapshotT::new_leaf(1, 0, 0, 2, 0);
        snap.flags = BchSnapshotFlags::empty();
        snap.flags.insert(BchSnapshotFlags::WILL_DELETE);
        assert_eq!(
            check_should_delete_snapshot(&snap),
            Some(DeadSnapshotType::Leaf)
        );
    }

    #[test]
    fn test_check_should_delete_leaf_no_subvol() {
        let mut snap = SnapshotT::new_leaf(1, 0, 0, 2, 0);
        snap.flags = BchSnapshotFlags::empty();
        assert_eq!(
            check_should_delete_snapshot(&snap),
            Some(DeadSnapshotType::Leaf)
        );
    }

    #[test]
    fn test_check_should_delete_interior_no_keys() {
        let mut snap = SnapshotT::new_leaf(1, 0, 0, 2, 0);
        snap.children = [3, 0]; // has children, not a leaf
        snap.flags = BchSnapshotFlags::empty();
        snap.flags.insert(BchSnapshotFlags::NO_KEYS);
        assert_eq!(
            check_should_delete_snapshot(&snap),
            Some(DeadSnapshotType::Interior)
        );
    }

    #[test]
    fn test_check_should_delete_alive_has_subvol() {
        let mut snap = SnapshotT::new_leaf(1, 1, 0, 2, 0);
        snap.flags = BchSnapshotFlags::SUBVOL;
        assert_eq!(check_should_delete_snapshot(&snap), None);
    }

    // ─── bch2_check_snapshot_needs_deletion 测试 ───

    #[test]
    fn test_check_needs_deletion_will_delete() {
        let mut snap = SnapshotT::new_leaf(1, 0, 0, 2, 0);
        snap.flags.insert(BchSnapshotFlags::WILL_DELETE);
        assert!(bch2_check_snapshot_needs_deletion(&snap));
    }

    #[test]
    fn test_check_needs_deletion_single_child() {
        let mut snap = SnapshotT::new_leaf(1, 0, 0, 2, 0);
        snap.children = [2, 0];
        assert!(bch2_check_snapshot_needs_deletion(&snap));
    }

    #[test]
    fn test_check_needs_deletion_no_keys() {
        let mut snap = SnapshotT::new_leaf(1, 0, 0, 2, 0);
        snap.flags.insert(BchSnapshotFlags::NO_KEYS);
        assert!(
            !bch2_check_snapshot_needs_deletion(&snap),
            "NO_KEYS handled by interior delete"
        );
    }

    #[test]
    fn test_check_needs_deletion_normal() {
        let snap = SnapshotT::new_leaf(1, 1, 0, 2, 0);
        assert!(!bch2_check_snapshot_needs_deletion(&snap));
    }

    // ─── Skiplist 已在 bch2_snapshot_node_create 中内联测试 ───
    // 原来的 bch2_snapshot_skiplist_get 相关测试已被内联函数替代，
    // 相关行为由 test_skip_list_depth_under_4, test_skip_list_populated_exponential,
    // test_skip_list_ordered, test_skip_list_ancestor_chain 等验证。

    // ─── bch2_snapshot_is_ancestor 额外测试 ───

    #[test]
    fn test_bch2_is_ancestor_self() {
        let mut vol = make_vol();
        assert!(bch2_snapshot_is_ancestor(
            &make_trans(&mut vol),
            42,
            42
        ));
    }

    #[test]
    fn test_bch2_is_ancestor_chain() {
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
        assert!(bch2_snapshot_is_ancestor(
            &make_trans(&mut vol),
            child,
            root
        ));
        assert!(!bch2_snapshot_is_ancestor(
            &make_trans(&mut vol),
            root,
            child
        ));
    }

    // ─── Layer 3: bch2_check_key_has_snapshot ───

    #[test]
    fn test_check_key_has_snapshot_valid() {
        let mut vol = make_vol();
        let root = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        let table = SnapshotTable::build(&vol);
        assert_eq!(
            bch2_check_key_has_snapshot(&table, root),
            CheckKeySnapshotResult::Valid,
            "live snapshot should be valid"
        );
    }

    #[test]
    fn test_check_key_has_snapshot_deleted() {
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
        assert_eq!(
            bch2_check_key_has_snapshot(&table, child),
            CheckKeySnapshotResult::Valid,
            "pending-delete snapshot should still be valid"
        );
    }

    #[test]
    fn test_check_key_has_snapshot_missing() {
        let vol = make_vol();
        let table = SnapshotTable::build(&vol);
        assert_eq!(
            bch2_check_key_has_snapshot(&table, 42),
            CheckKeySnapshotResult::Missing,
            "nonexistent ID should be missing"
        );
    }

    #[test]
    fn test_check_key_has_snapshot_zero_id() {
        let vol = make_vol();
        let table = SnapshotTable::build(&vol);
        assert_eq!(
            bch2_check_key_has_snapshot(&table, 0),
            CheckKeySnapshotResult::Valid,
            "snapshot_id=0 should always be valid"
        );
    }

    #[test]
    fn test_check_key_has_snapshot_live_after_rebuild() {
        let mut vol = make_vol();
        let root = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        // 删除后重建表，确认状态正确
        bch2_snapshot_node_set_deleted(&mut make_trans(&mut vol), root).unwrap();
        let table = SnapshotTable::build(&vol);
        assert_eq!(
            bch2_check_key_has_snapshot(&table, root),
            CheckKeySnapshotResult::Valid,
            "pending-delete root should still be valid"
        );
    }

    // ─── Layer 3: bch2_reconstruct_snapshots ───

    #[test]
    fn test_reconstruct_empty_nothing_to_do() {
        let mut vol = make_vol();
        bch2_reconstruct_snapshots(&mut make_trans(&mut vol)).unwrap();
        let table = SnapshotTable::build(&vol);
        assert!(table.get(u32::MAX).is_none(), "no snapshots should exist");
    }

    #[test]
    fn test_reconstruct_missing_snapshot_from_extents() {
        let mut vol = make_vol();
        let snap_id = u32::MAX - 10;
        // 在 Extents btree 中创建一个引用缺失 snapshot 的条目
        let extent_entry = BtreeEntry::raw(
            Bpos::new(1, 100, snap_id),
            KeyType::Normal,
            vec![1, 2, 3, 4],
        );
        vol.insert_entry_raw(BtreeId::Extents, extent_entry, 0);

        // 运行重建
        bch2_reconstruct_snapshots(&mut make_trans(&mut vol)).unwrap();

        // 验证 snapshot 已被创建
        let snap = bch2_snapshot_read_value(&make_trans(&mut vol), snap_id)
            .expect("snapshot should be reconstructed");
        assert_eq!(snap.parent, 0, "reconstructed snapshot should be root");
        assert_eq!(snap.depth, 1, "reconstructed snapshot depth should be 1");
        // 应有一个对应的 SnapshotTree 条目
        let tree_val = bch2_snapshot_tree_lookup(&make_trans(&mut vol), 1)
            .expect("SnapshotTree entry 1 should exist");
        assert_eq!(
            tree_val.root_snapshot, snap_id,
            "SnapshotTree root should point to the reconstructed snapshot"
        );
    }

    #[test]
    fn test_reconstruct_multiple_missing_ids() {
        let mut vol = make_vol();
        // 在多个 btree 中引用不同的缺失 snapshot ID
        // 使用接近 u32::MAX 的 ID（bcachefs 分配惯例）
        let ids = [u32::MAX - 10, u32::MAX - 20, u32::MAX - 30];
        let entries = [
            (BtreeId::Extents, Bpos::new(1, 100, ids[0])),
            (BtreeId::Extents, Bpos::new(1, 200, ids[1])),
            (BtreeId::Subvolumes, Bpos::new(2, 0, ids[2])),
        ];
        for (btree_id, pos) in &entries {
            let entry = BtreeEntry::raw(*pos, KeyType::Normal, vec![1, 2, 3]);
            vol.insert_entry_raw(*btree_id, entry, 0);
        }

        bch2_reconstruct_snapshots(&mut make_trans(&mut vol)).unwrap();

        for &snap_id in &ids {
            assert!(
                bch2_snapshot_read_value(&make_trans(&mut vol), snap_id).is_some(),
                "snapshot {} should be reconstructed",
                snap_id
            );
        }
    }

    #[test]
    fn test_reconstruct_with_existing_snapshots() {
        let mut vol = make_vol();
        // 已有部分快照
        let root = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        let child = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [2, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), root, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };

        let missing_id = u32::MAX - 50;
        // 外加一个缺失的引用
        let extent_entry = BtreeEntry::raw(
            Bpos::new(1, 100, missing_id),
            KeyType::Normal,
            vec![1, 2, 3],
        );
        vol.insert_entry_raw(BtreeId::Extents, extent_entry, 0);

        bch2_reconstruct_snapshots(&mut make_trans(&mut vol)).unwrap();

        // 已有的仍然存在
        assert!(
            bch2_snapshot_read_value(&make_trans(&mut vol), root).is_some(),
            "existing root should remain"
        );
        assert!(
            bch2_snapshot_read_value(&make_trans(&mut vol), child).is_some(),
            "existing child should remain"
        );
        // 缺失的被重建
        assert!(
            bch2_snapshot_read_value(&make_trans(&mut vol), missing_id).is_some(),
            "missing snapshot should be reconstructed"
        );
    }

    #[test]
    fn test_reconstruct_all_existing_nothing_added() {
        let mut vol = make_vol();
        let root = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        let _child = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [2, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), root, &mut new_snapids, &snapshot_subvols, 2).unwrap();
            new_snapids[0]
        };

        // 所有被引用的 snapshot ID 都已存在
        let snap_count_before = {
            let btree = vol.btree(BtreeId::Snapshots);
            let mut count = 0;
            btree.for_each_btree_key_entry(|_| count += 1);
            count
        };

        bch2_reconstruct_snapshots(&mut make_trans(&mut vol)).unwrap();

        let snap_count_after = {
            let btree = vol.btree(BtreeId::Snapshots);
            let mut count = 0;
            btree.for_each_btree_key_entry(|_| count += 1);
            count
        };

        assert_eq!(
            snap_count_before, snap_count_after,
            "no new snapshots should be added"
        );
    }

    #[test]
    fn test_reconstruct_creates_tree_entry() {
        let mut vol = make_vol();
        let snap_id = u32::MAX - 50;
        // 只有 extents 引用，没有 SnapshotTrees
        let entry = BtreeEntry::raw(Bpos::new(1, 100, snap_id), KeyType::Normal, vec![1, 2, 3]);
        vol.insert_entry_raw(BtreeId::Extents, entry, 0);

        bch2_reconstruct_snapshots(&mut make_trans(&mut vol)).unwrap();

        // 验证 SnapshotTree 条目被创建
        let tree_val = bch2_snapshot_tree_lookup(&make_trans(&mut vol), 1)
            .expect("SnapshotTree entry should be created");
        assert_eq!(
            tree_val.root_snapshot, snap_id,
            "tree root should point to reconstructed snapshot"
        );
    }

    #[test]
    fn test_reconstruct_no_duplicate_trees() {
        let mut vol = make_vol();
        // 两个缺失的快照（使用接近 u32::MAX 的 ID）
        let ids = [u32::MAX - 50, u32::MAX - 100];
        let entries = [
            (BtreeId::Extents, Bpos::new(1, 100, ids[0])),
            (BtreeId::Extents, Bpos::new(2, 200, ids[1])),
        ];
        for (btree_id, pos) in &entries {
            let entry = BtreeEntry::raw(*pos, KeyType::Normal, vec![1, 2, 3]);
            vol.insert_entry_raw(*btree_id, entry, 0);
        }

        bch2_reconstruct_snapshots(&mut make_trans(&mut vol)).unwrap();

        // 应该有两个独立的 tree 条目
        let tree_count = {
            let btree = vol.btree(BtreeId::SnapshotTrees);
            let mut count = 0;
            btree.for_each_btree_key_entry(|_| count += 1);
            count
        };
        assert_eq!(
            tree_count, 2,
            "two missing snapshots should create two trees"
        );
    }

    // ─── Layer 3: 表集成 ───

    #[test]
    fn test_id_state_methods() {
        let mut vol = make_vol();
        let root = {
            let mut new_snapids = [0u32; 2];
            let snapshot_subvols = [1, 0];
            bch2_snapshot_node_create(&mut make_trans(&mut vol), 0, &mut new_snapids, &snapshot_subvols, 1).unwrap();
            new_snapids[0]
        };
        let table = SnapshotTable::build(&vol);

        assert_eq!(table.id_state(root), SnapshotIdState::Live);
        assert_eq!(table.id_state(0), SnapshotIdState::Empty);
        assert_eq!(table.id_state(9999), SnapshotIdState::Empty);

        // 删除后重建表
        bch2_snapshot_node_set_deleted(&mut make_trans(&mut vol), root).unwrap();
        let table2 = SnapshotTable::build(&vol);
        assert_eq!(table2.id_state(root), SnapshotIdState::Live);
    }

    #[test]
    fn test_snapshots_read_after_reconstruct() {
        let mut vol = make_vol();
        let snap_id = u32::MAX - 5;
        // 引用缺失的 snapshot
        let entry = BtreeEntry::raw(Bpos::new(1, 100, snap_id), KeyType::Normal, vec![1, 2, 3]);
        vol.insert_entry_raw(BtreeId::Extents, entry, 0);

        bch2_reconstruct_snapshots(&mut make_trans(&mut vol)).unwrap();

        // 验证 bch2_snapshots_read 能正确加载重建后的数据
        let (table, tree_table) = crate::snap::table::bch2_snapshots_read(&vol);
        assert!(
            table.exists(snap_id),
            "reconstructed snapshot should be in table"
        );
        assert!(
            tree_table.get(1).is_some(),
            "reconstructed tree should be in tree table"
        );
    }
}
