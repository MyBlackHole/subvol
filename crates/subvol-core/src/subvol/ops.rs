//! Subvolume — 子卷管理器（BtreeTrans 集成）
//!
//! 使用 BtreeTrans 通过 Subvolumes btree 持久化存储 BchSubvolume。
//! 函数名与 bcachefs API 对齐：`bch2_subvolume_*` / `bch2_subvol_*`。

use crate::btree::key::{BtreeKey, KeyType};
use crate::btree::{Bpos, BtreeId, BtreeTrans};
use crate::snap::meta::{SnapshotT, SnapshotTreeT};
use crate::snap::snapshot as snap_snapshot;
use crate::snap::snapshot::{
    bch2_snapshot_node_create, bch2_snapshot_node_set_deleted, bch2_snapshot_read_value,
};
use crate::types::StorageError;

use super::types::{BchSubvolume, BCACHEFS_ROOT_INO, BCACHEFS_ROOT_SUBVOL};

/// 分配一个新的子卷 ID（扫描 Subvolumes Btree 空槽）。
///
/// 对齐 bcachefs `bch2_bkey_get_empty_slot()`：ID 来自 Btree key 的
/// `offset`，而不是来自运行时计数器。0 号保留给未分配状态。
fn allocate_subvol_id(trans: &BtreeTrans) -> Result<u32, StorageError> {
    // 对应本地 bcachefs `bch2_bkey_get_empty_slot()`（btree/update.c:654-670）：
    // 空槽搜索必须使用事务 iterator 的可见视图，不能只读取已落盘的
    // Subvolumes btree；当前事务中已经写入 journal 的 subvolume key 也必须
    // 占用该槽位。
    let mut subvol_id = 1u32;
    while trans
        .get_entry(BtreeId::Subvolumes, Bpos::new(0, subvol_id as u64, 0))
        .is_some()
    {
        if subvol_id == u32::MAX {
            return Err(StorageError::AddressSpaceExhausted {
                max_raw_addr: u32::MAX as u64,
            });
        }
        subvol_id += 1;
    }
    Ok(subvol_id)
}

/// 对齐 bcachefs `subvolume_children_pos()`：
/// `POS(fs_path_parent, subvol_id)`。
fn subvolume_children_pos(parent_subvolid: u32, subvolid: u32) -> Bpos {
    Bpos::new(parent_subvolid as u64, subvolid as u64, 0)
}

/// 对齐 bcachefs `subvolume_children_mod()`，在独立
/// `BTREE_ID_subvolume_children` 中维护 KEY_TYPE_set 条目。
fn subvolume_children_mod(
    trans: &mut BtreeTrans,
    parent_subvolid: u32,
    subvolid: u32,
    set: bool,
) {
    if parent_subvolid == 0 {
        return;
    }

    let pos = subvolume_children_pos(parent_subvolid, subvolid);
    let key = BtreeKey::from_bpos(pos, KeyType::Set);
    if set {
        trans.bch2_trans_update_raw(
            BtreeId::SubvolumeChildren,
            0,
            false,
            key,
            Vec::new(),
            0,
        );
    } else {
        trans.bch2_trans_delete(BtreeId::SubvolumeChildren, 0, false, key, 0);
    }
}

// ─── 子卷创建 / 快照 ───

/// 创建新子卷或 snapshot 子卷，统一执行 bcachefs 的 subvolume 事务流程。
///
/// 对齐 bcachefs `bch2_subvolume_create()`（subvolume.c:576-651）。
///
/// # 流程（对齐 bcachefs）
///
/// 1. 分配 subvol_id（`allocate_subvol_id`，对齐 `bch2_bkey_get_empty_slot`）
/// 2. 在 SnapshotTrees/Snapshots Btree 中创建树条目和根快照节点
///    （由 `bch2_snapshot_node_create` 对齐 `bch2_snapshot_node_create_tree`）
/// 3. 在 Subvolumes btree 中分配并初始化子卷条目
///    （对齐 bcachefs `bch2_bkey_alloc` + 字段初始化）
///
/// # 偏差说明
///
/// - `size` / `created_at` 是 subvol 扩展字段，不在 bcachefs 原始结构中
pub fn bch2_subvolume_create(
    trans: &mut BtreeTrans,
    inode: u64,
    parent_subvolid: u32,
    src_subvolid: u32,
    new_subvolid: &mut u32,
    new_snapshotid: &mut u32,
    new_subvol_out: &mut BchSubvolume,
    ro: bool,
) -> Result<(), StorageError> {
    // `size`/`otime_lo` are subvol-only fields.  The bcachefs ABI does not
    // carry them; callers seed the output value when they need this extension.
    let extension_size = new_subvol_out.size;
    let extension_otime = new_subvol_out.otime_lo;
    // 1. 分配 subvol_id（对齐 bcachefs `bch2_bkey_get_empty_slot`）
    let subvol_id = allocate_subvol_id(trans)?;

    // 2. 创建新的 snapshot tree，或在源 snapshot 下创建两个 children
    let (root_snap_id, source_new_snap_id) = if src_subvolid != 0 {
        let source = bch2_subvolume_get(trans, src_subvolid, true)?;
        if source.is_unlinked() {
            return Err(StorageError::NotFound(format!(
                "source subvolume {src_subvolid} is deleted"
            )));
        }
        let parent_snapshot = source.snapshot;
        let mut new_snapids = [0u32; 2];
        let snapshot_subvols = [subvol_id, src_subvolid];
        bch2_snapshot_node_create(
            trans,
            parent_snapshot,
            &mut new_snapids,
            &snapshot_subvols,
            2,
        )?;
        let new_snap_id = new_snapids[0];
        let source_new_snap_id = bch2_snapshot_read_value(trans, parent_snapshot)
            .ok_or_else(|| {
                StorageError::NotFound(format!("parent snapshot {parent_snapshot} disappeared"))
            })?
            .children[1];
        (new_snap_id, Some(source_new_snap_id))
    } else {
        let mut new_snapids = [0u32; 2];
        let snapshot_subvols = [subvol_id];
        bch2_snapshot_node_create(trans, 0, &mut new_snapids, &snapshot_subvols, 1)?;
        (new_snapids[0], None)
    };

    // 3. 更新源子卷的 snapshot（开始 COW）
    if let Some(source_new_snap_id) = source_new_snap_id {
        let mut source = bch2_subvolume_get(trans, src_subvolid, true)?;
        source.snapshot = source_new_snap_id;
        trans.bch2_trans_delete(
            BtreeId::Subvolumes,
            0,
            false,
            BtreeKey::new(src_subvolid as u64, 0, KeyType::Normal),
            0,
        );
        trans.bch2_trans_update_raw(
            BtreeId::Subvolumes,
            0,
            false,
            BtreeKey::new(src_subvolid as u64, 0, KeyType::Normal),
            source.to_bytes(),
            0,
        );
    }

    // 4. 在 Subvolumes btree 中分配并初始化子卷条目
    //    对齐 bcachefs `bch2_bkey_alloc` + 字段初始化
    let mut sv = BchSubvolume::new(root_snap_id, inode, extension_size, extension_otime);
    sv.creation_parent = src_subvolid;
    sv.fs_path_parent = parent_subvolid;
    // bcachefs subvolume.c:621-622 sets RO only from the caller's `ro`;
    // SNAP is a separate flag and does not implicitly set RO here.
    sv.set_read_only(ro);
    sv.set_snapshot(src_subvolid != 0);
    let key = BtreeKey::new(subvol_id as u64, 0, KeyType::Normal);
    let bytes = bincode::serialize(&sv).map_err(StorageError::Serialization)?;
    trans.bch2_trans_update_raw(BtreeId::Subvolumes, 0, false, key, bytes, 0);

    // bcachefs subvolume trigger: 新 subvolume 有 fs_path_parent 时，
    // 同一事务写入 subvolume_children 的 set key。
    subvolume_children_mod(trans, parent_subvolid, subvol_id, true);

    // 触发器校验（对齐 bcachefs `bch2_subvolume_trigger`）
    bch2_subvolume_validate(trans, subvol_id, &sv)?;

    // 5. 注册 inode 映射
    trans.vol().register_ino_map(inode, subvol_id);

    *new_subvolid = subvol_id;
    *new_snapshotid = root_snap_id;
    *new_subvol_out = sv;
    Ok(())
}

/// 创建快照子卷，自动创建快照节点
///
/// 对齐 bcachefs `bch2_subvolume_create()` 的 snapshot 模式（src_subvolid != 0）。
/// 在 Snapshots btree 中创建子快照节点，再创建对应的子卷条目。
///
/// `parent_snapshot` 从父子卷加载（`parent_subvol` 的 snapshot 字段）。
pub(crate) fn bch2_subvolume_snapshot(
    trans: &mut BtreeTrans,
    parent_subvol: u32,
    inode: u64,
    size: u64,
    created_at: i64,
) -> Result<u32, StorageError> {
    let mut new_subvolid = 0;
    let mut new_snapshotid = 0;
    let mut new_subvol = BchSubvolume::new(0, 0, size, created_at as u64);
    bch2_subvolume_create(
        trans,
        inode,
        0,
        parent_subvol,
        &mut new_subvolid,
        &mut new_snapshotid,
        &mut new_subvol,
        true,
    )?;
    Ok(new_subvolid)
}

// ─── 子卷查询 ───

/// 获取子卷（反序列化返回 owned 值）。
///
/// 对齐 bcachefs `bch2_subvolume_get()`：调用者显式传入
/// `inconsistent_if_not_found`，缺失或损坏通过 `Result` 返回错误。
pub fn bch2_subvolume_get(
    trans: &BtreeTrans,
    subvol: u32,
    _inconsistent_if_not_found: bool,
) -> Result<BchSubvolume, StorageError> {
    let pos = Bpos::new(0, subvol as u64, 0);
    let entry = trans
        .get_entry(BtreeId::Subvolumes, pos)
        .ok_or_else(|| StorageError::NotFound(format!("subvolume {subvol}")))?;
    BchSubvolume::from_bytes(&entry.value.to_bytes())
        .map_err(|_| StorageError::InvalidData(format!("subvolume {subvol} corrupt")))
}

/// 获取子卷的快照 ID
///
/// 对齐 bcachefs `bch2_subvolume_get_snapshot()`。
pub fn bch2_subvolume_get_snapshot(
    trans: &BtreeTrans,
    subvolid: u32,
) -> Result<u32, StorageError> {
    let sv = bch2_subvolume_get(trans, subvolid, true)?;
    Ok(sv.snapshot)
}

/// 子卷值校验（对齐 bcachefs `bch2_subvolume_validate`）。
///
/// 在子卷条目变更时校验以下约束：
/// - 子卷引用的快照 ID 必须存在于 Snapshots btree 中
/// - 快照节点回引的 subvol 必须与当前子卷 ID 一致（双向引用一致性）
/// - 根子卷 (ID 1) 不可执行删除标记
/// - 子卷的 creation_parent 必须指向一个存在的子卷（除非为 0）
///
/// 这是 read-only 校验，不修改任何数据。
/// 返回 `Ok(())` 表示校验通过。
pub fn bch2_subvolume_validate(
    trans: &BtreeTrans,
    subvolid: u32,
    sv: &BchSubvolume,
) -> Result<(), StorageError> {
    // 1. 根子卷保护
    if sv.is_unlinked() && subvolid == 1 {
        return Err(StorageError::InvalidArgument(
            "cannot mark root subvolume as unlinked".into(),
        ));
    }

    // 2. 快照引用校验
    let snap = bch2_snapshot_read_value(trans, sv.snapshot);
    if snap.is_none() {
        return Err(StorageError::NotFound(format!(
            "subvolume {} references non-existent snapshot {}",
            subvolid, sv.snapshot
        )));
    }

    // 3. 双向引用一致性校验
    if let Some(snap) = snap {
        if snap.subvol != 0 && snap.subvol != subvolid {
            return Err(StorageError::InvalidArgument(format!(
                "snapshot {} subvol pointer mismatch: expected {}, got {}",
                sv.snapshot, subvolid, snap.subvol
            )));
        }
    }

    // 4. creation_parent 引用校验
    if sv.creation_parent != 0 && sv.creation_parent != subvolid {
        let parent_exists = bch2_subvolume_get(trans, sv.creation_parent, false).is_ok();
        if !parent_exists {
            return Err(StorageError::NotFound(format!(
                "subvolume {} references non-existent parent {}",
                subvolid, sv.creation_parent
            )));
        }
    }

    Ok(())
}

/// 子卷 btree transactional trigger（对齐 bcachefs `bch2_subvolume_trigger`）。
///
/// bcachefs 在 subvolume key 的旧值/新值路径父级发生变化时，
/// 同一事务删除旧 `subvolume_children` set key 并写入新 key。
pub fn bch2_subvolume_trigger(
    trans: &mut BtreeTrans<'_>,
    btree_type: BtreeId,
    key_bytes: &[u8],
    old_val: Option<&[u8]>,
    new_val: Option<&[u8]>,
) -> Result<(), StorageError> {
    if btree_type != BtreeId::Subvolumes {
        return Ok(());
    }

    let key: BtreeKey = bincode::deserialize(key_bytes)?;
    let parse_subvolume = |bytes: &[u8]| -> Result<BchSubvolume, StorageError> {
        bincode::deserialize(bytes).map_err(StorageError::Serialization)
    };
    let old_parent = old_val
        .map(parse_subvolume)
        .transpose()?
        .map(|sv| sv.fs_path_parent)
        .unwrap_or(0);
    let new_parent = new_val
        .map(parse_subvolume)
        .transpose()?
        .map(|sv| sv.fs_path_parent)
        .unwrap_or(0);
    if old_parent == new_parent {
        return Ok(());
    }

    let subvolid = key.get_vaddr() as u32;
    subvolume_children_mod(trans, old_parent, subvolid, false);
    subvolume_children_mod(trans, new_parent, subvolid, true);
    Ok(())
}

/// 检查子卷是否允许写入。
///
/// 对齐本地 bcachefs `bch2_subvol_is_ro()` (`subvolume.c:323-329`)：
/// 可写时返回 0（Rust 中为 `Ok(())`），只读或 UNLINKED 时返回
/// `-EROFS`（Rust 中为 `PermissionDenied`），而不是把只读状态作为
/// 成功返回值传给调用者。
pub fn bch2_subvol_is_ro(trans: &BtreeTrans, subvol: u32) -> Result<(), StorageError> {
    let sv = bch2_subvolume_get(trans, subvol, true)?;
    if sv.is_read_only() || sv.is_unlinked() {
        return Err(StorageError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "subvolume is read-only",
        )));
    }
    Ok(())
}

/// 从 snapshot ID 获取对应的子卷。
///
/// 对齐 bcachefs `bch2_snapshot_get_subvol()`：调用方提供输出对象，函数
/// 返回事务错误，而不是把 snapshot/subvolume 查找失败折叠成 `Option`。
pub fn bch2_snapshot_get_subvol(
    trans: &BtreeTrans,
    snapshot: u32,
    subvol_out: &mut BchSubvolume,
) -> Result<(), StorageError> {
    let snap: SnapshotT = bch2_snapshot_read_value(trans, snapshot).ok_or_else(|| {
        StorageError::NotFound(format!("snapshot {snapshot} not found"))
    })?;
    *subvol_out = bch2_subvolume_get(trans, snap.subvol, true)?;
    Ok(())
}

// ─── 子卷删除 ───

/// 标记子卷为 UNLINKED，保留 Btree 条目等待后续删除。
///
/// 对齐 bcachefs `bch2_subvolume_unlink()`（subvolume.c:523-545）。
pub fn bch2_subvolume_unlink(trans: &mut BtreeTrans, subvolid: u32) -> Result<(), StorageError> {
    if subvolid == 1 || subvolid == u32::try_from(BCACHEFS_ROOT_INO).unwrap_or(1) {
        return Err(StorageError::InvalidArgument(
            "cannot unlink root subvolume".into(),
        ));
    }

    let pos = Bpos::new(0, subvolid as u64, 0);
    let entry = trans
        .get_entry(BtreeId::Subvolumes, pos)
        .ok_or_else(|| StorageError::NotFound(format!("subvolume {subvolid}")))?;
    let mut sv = BchSubvolume::from_bytes(&entry.value.to_bytes())
        .map_err(|_| StorageError::NotFound(format!("subvolume {subvolid} corrupt")))?;
    sv.mark_unlinked();
    let old_fs_path_parent = sv.fs_path_parent;
    sv.fs_path_parent = 0;

    trans.bch2_trans_delete(
        BtreeId::Subvolumes,
        0,
        false,
        BtreeKey::new(subvolid as u64, 0, KeyType::Normal),
        0,
    );
    trans.bch2_trans_update_raw(
        BtreeId::Subvolumes,
        0,
        false,
        BtreeKey::new(subvolid as u64, 0, KeyType::Normal),
        sv.to_bytes(),
        0,
    );
    subvolume_children_mod(trans, old_fs_path_parent, subvolid, false);
    Ok(())
}

/// 删除子卷并标记关联 snapshot 待回收。
///
/// 对齐 bcachefs `bch2_subvolume_delete()`（subvolume.c:411-461）：
/// 1. 重挂子卷；2. 清空 snapshot tree master；3. 删除 subvolume key；
/// 4. 标记关联 snapshot。
pub fn bch2_subvolume_delete(trans: &mut BtreeTrans, subvolid: u32) -> Result<(), StorageError> {
    // 根子卷 (ID 1) 不可删除 — 对齐 bcachefs 语义
    if subvolid == 1 || subvolid == u32::try_from(BCACHEFS_ROOT_INO).unwrap_or(1) {
        return Err(StorageError::InvalidArgument(
            "cannot delete root subvolume".into(),
        ));
    }

    let pos = Bpos::new(0, subvolid as u64, 0);
    let entry = trans
        .get_entry(BtreeId::Subvolumes, pos)
        .ok_or_else(|| StorageError::NotFound(format!("subvolume {subvolid}")))?;
    let sv = BchSubvolume::from_bytes(&entry.value.to_bytes())
        .map_err(|_| StorageError::NotFound(format!("subvolume {subvolid} corrupt")))?;
    let old_fs_path_parent = sv.fs_path_parent;

    // 触发器校验：删除前验证一致性
    bch2_subvolume_validate(trans, subvolid, &sv)?;

    // 先重挂所有子卷，避免删除后留下悬挂 creation_parent 引用。
    bch2_subvolumes_reparent(trans, subvolid, sv.creation_parent)?;

    // 若该子卷是当前快照树 master，则清空 master_subvol，和 bcachefs 删除路径一致。
    if let Some(snap) = bch2_snapshot_read_value(trans, sv.snapshot) {
        if snap.tree != 0 {
            let tree_val = snap_snapshot::bch2_snapshot_tree_lookup(trans, snap.tree)?;
            if tree_val.master_subvol == subvolid {
                let mut tree_val = snap_snapshot::bch2_snapshot_tree_lookup(trans, snap.tree)?;
                tree_val.master_subvol = 0;
                let tree_bytes = bincode::serialize(&tree_val).map_err(StorageError::Serialization)?;
                trans.bch2_trans_update_raw(
                    BtreeId::SnapshotTrees,
                    0,
                    false,
                    BtreeKey::new(0, snap.tree, KeyType::Normal),
                    tree_bytes,
                    0,
                );
            }
        }
    }

    // 清理 inode 映射（在删除 key 之前执行，确保映射数据一致性）
    trans.vol().cleanup_ino_map(sv.inode, subvolid);

    // 删除 subvolume key（对齐 bcachefs `bch2_btree_delete_at`）
    trans.bch2_trans_delete(
        BtreeId::Subvolumes,
        0,
        false,
        BtreeKey::new(subvolid as u64, 0, KeyType::Normal),
        0,
    );
    subvolume_children_mod(trans, old_fs_path_parent, subvolid, false);

    // 4. 标记关联快照待回收
    let snap_id = sv.snapshot;
    if snap_id != 0 {
        bch2_snapshot_node_set_deleted(trans, snap_id)?;
    }

    Ok(())
}

// ─── 子卷列表 / 计数 ───

/// 列出所有活跃（未删除）子卷，按 ID 排序
///
/// 对齐 bcachefs `bch2_subvolume_list()`。
pub(crate) fn bch2_subvolume_list(trans: &BtreeTrans) -> Vec<(u32, BchSubvolume)> {
    let mut result = Vec::new();
    let vol = trans.btree(BtreeId::Subvolumes);
    vol.for_each_btree_key_entry(|entry| {
        let bytes = entry.value.to_bytes();
        if let Ok(sv) = BchSubvolume::from_bytes(&bytes) {
            if !sv.is_unlinked() {
                let id = entry.pos.offset as u32;
                result.push((id, sv));
            }
        }
    });
    result.sort_by_key(|(id, _)| *id);
    result
}

/// 活跃子卷数量
pub(crate) fn bch2_subvolume_count(trans: &BtreeTrans) -> usize {
    let mut count = 0usize;
    let vol = trans.btree(BtreeId::Subvolumes);
    vol.for_each_btree_key_entry(|entry| {
        let bytes = entry.value.to_bytes();
        if let Ok(sv) = BchSubvolume::from_bytes(&bytes) {
            if !sv.is_unlinked() {
                count += 1;
            }
        }
    });
    count
}

// ─── 子卷关系操作 ───

/// 重挂子卷：将 `subvolid` 的所有非删除子卷的 parent 改为 `new_parent`
///
/// 对齐 bcachefs `bch2_subvolumes_reparent()`。
/// 用于删除子卷前，避免 orphan 子卷。
pub fn bch2_subvolumes_reparent(
    trans: &mut BtreeTrans,
    subvolid: u32,
    new_parent: u32,
) -> Result<(), StorageError> {
    // 收集所有 creation_parent == subvolid 的子卷
    let mut children: Vec<(u32, BchSubvolume)> = Vec::new();
    {
        let vol = trans.btree(BtreeId::Subvolumes);
        vol.for_each_btree_key_entry(|entry| {
            let bytes = entry.value.to_bytes();
            if let Ok(sv) = BchSubvolume::from_bytes(&bytes) {
                if sv.creation_parent == subvolid {
                    let id = entry.pos.offset as u32;
                    children.push((id, sv));
                }
            }
        });
    }

    for (child_id, mut sv) in children {
        sv.creation_parent = new_parent;
        // delete + insert via journal
        trans.bch2_trans_delete(
            BtreeId::Subvolumes,
            0,
            false,
            BtreeKey::new(child_id as u64, 0, KeyType::Normal),
            0,
        );
        let bytes = bincode::serialize(&sv).map_err(StorageError::Serialization)?;
        trans.bch2_trans_update_raw(
            BtreeId::Subvolumes,
            0,
            false,
            BtreeKey::new(child_id as u64, 0, KeyType::Normal),
            bytes,
            0,
        );
    }
    Ok(())
}

// ═══════════════════════════════════════════════════════════════
// 测试
// ═══════════════════════════════════════════════════════════════

/// 创建根快照/子卷结构 — bcachefs 精确对齐
///
/// 对应 bcachefs `bch2_initialize_subvolumes()` (subvolume.c:653-681)。
/// 在全新文件系统上创建三条记录：
///   1. SnapshotTrees btree: 树 ID=1 → SnapshotTreeT
///   2. Snapshots btree:     snapshot_id=U32_MAX → 根快照节点 (SUBVOL leaf)
///   3. Subvolumes btree:    subvol_id=BCACHEFS_ROOT_SUBVOL(1) → BchSubvolume
///
/// 本函数仅在 bch2_fs_initialize() 中调用，不参与 recovery pass 调度。
/// 每次调用从 vollen 新建，不存在幂等问题。
pub fn bch2_initialize_subvolumes(trans: &mut BtreeTrans) -> Result<(), StorageError> {
    // bcachefs 只做三个 btree insert，无 runtime 操作
    // 参见 subvolume.c:653-681

    // 1. SnapshotTrees btree: tree_id=1, master_subvol=1, root_snapshot=U32_MAX
    let tree_val = SnapshotTreeT::new(BCACHEFS_ROOT_SUBVOL as u32, u32::MAX);
    let raw = bincode::serialize(&tree_val).map_err(|e| StorageError::Serialization(e))?;
    trans.bch2_trans_update_raw(
        BtreeId::SnapshotTrees,
        0,
        false,
        BtreeKey::new(0, 1, KeyType::Normal),
        raw,
        0,
    );

    // 2. Snapshots btree: snapshot_id=U32_MAX, subvol=1, tree=1
    let snap_val = SnapshotT::new_leaf(0, BCACHEFS_ROOT_SUBVOL as u32, 1, 1, 0);
    let raw = bincode::serialize(&snap_val).map_err(|e| StorageError::Serialization(e))?;
    trans.bch2_trans_update_raw(
        BtreeId::Snapshots,
        0,
        false,
        BtreeKey::new(0, u32::MAX, KeyType::Normal),
        raw,
        0,
    );

    // 3. Subvolumes btree: subvol_id=BCACHEFS_ROOT_SUBVOL (1)
    let subvol_val = BchSubvolume::new(u32::MAX, BCACHEFS_ROOT_INO, 0, 0);
    let raw = bincode::serialize(&subvol_val).map_err(|e| StorageError::Serialization(e))?;
    trans.bch2_trans_update_raw(
        BtreeId::Subvolumes,
        0,
        false,
        BtreeKey::new(BCACHEFS_ROOT_SUBVOL, 0, KeyType::Normal),
        raw,
        0,
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BchVol;

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

    // ─── bch2_subvolume_create / list ───

    #[test]
    fn test_create_and_list() {
        let mut vol = BchVol::test_trees();
        let id = {
            let mut new_subvolid = 0;
            let mut new_snapshotid = 0;
            let mut new_subvol_out = BchSubvolume::new(0, 0, 4096, 1000u64);
            bch2_subvolume_create(&mut make_trans(&mut vol), 0, 0, 0, &mut new_subvolid, &mut new_snapshotid, &mut new_subvol_out, false).unwrap();
            new_subvolid
        };
        assert!(id > 0);
        assert_eq!(bch2_subvolume_list(&make_trans(&mut vol)).len(), 1);
    }

    #[test]
    fn test_create_multiple() {
        let mut vol = BchVol::test_trees();
        let id1 = {
            let mut new_subvolid = 0;
            let mut new_snapshotid = 0;
            let mut new_subvol_out = BchSubvolume::new(0, 0, 4096, 1000u64);
            bch2_subvolume_create(&mut make_trans(&mut vol), 0, 0, 0, &mut new_subvolid, &mut new_snapshotid, &mut new_subvol_out, false).unwrap();
            new_subvolid
        };
        let id2 = {
            let mut new_subvolid = 0;
            let mut new_snapshotid = 0;
            let mut new_subvol_out = BchSubvolume::new(0, 0, 8192, 2000u64);
            bch2_subvolume_create(&mut make_trans(&mut vol), 0, 0, 0, &mut new_subvolid, &mut new_snapshotid, &mut new_subvol_out, false).unwrap();
            new_subvolid
        };
        assert!(id2 > id1);
        assert_eq!(bch2_subvolume_list(&make_trans(&mut vol)).len(), 2);
        assert_eq!(bch2_subvolume_count(&make_trans(&mut vol)), 2);
    }

    #[test]
    fn test_subvolume_children_index_tracks_create_unlink_delete() {
        let mut vol = BchVol::test_trees();
        let parent = {
            let mut new_subvolid = 0;
            let mut new_snapshotid = 0;
            let mut new_subvol_out = BchSubvolume::new(0, 0, 4096, 1000u64);
            bch2_subvolume_create(&mut make_trans(&mut vol), 0, 0, 0, &mut new_subvolid, &mut new_snapshotid, &mut new_subvol_out, false).unwrap();
            new_subvolid
        };
        let child = {
            let mut new_subvolid = 0;
            let mut new_snapshotid = 0;
            let mut new_subvol_out = BchSubvolume::new(0, 0, 4096, 2000u64);
            bch2_subvolume_create(
                &mut make_trans(&mut vol),
                0,
                parent,
                0,
                &mut new_subvolid,
                &mut new_snapshotid,
                &mut new_subvol_out,
                false,
            )
            .unwrap();
            new_subvolid
        };

        let child_pos = subvolume_children_pos(parent, child);
        assert_eq!(vol.btree(BtreeId::SubvolumeChildren).root().node.packed_keys + vol.btree(BtreeId::SubvolumeChildren).root().node.unpacked_keys, 1);
        let child_entry = vol
            .get_entry_raw(BtreeId::SubvolumeChildren, child_pos)
            .expect("created child must have a subvolume_children set key");
        assert_eq!(child_entry.key_type, KeyType::Set);

        bch2_subvolume_unlink(&mut make_trans(&mut vol), child).unwrap();
        assert!(vol
            .get_entry_raw(BtreeId::SubvolumeChildren, child_pos)
            .is_none());

        bch2_subvolume_delete(&mut make_trans(&mut vol), child).unwrap();
        assert!(vol
            .get_entry_raw(BtreeId::SubvolumeChildren, child_pos)
            .is_none());
    }

    // ─── bch2_subvolume_get ───

    #[test]
    fn test_load() {
        let mut vol = BchVol::test_trees();
        let id = {
            let mut new_subvolid = 0;
            let mut new_snapshotid = 0;
            let mut new_subvol_out = BchSubvolume::new(0, 0, 65536, 500u64);
            bch2_subvolume_create(&mut make_trans(&mut vol), 0, 0, 0, &mut new_subvolid, &mut new_snapshotid, &mut new_subvol_out, false).unwrap();
            new_subvolid
        };
        let loaded = bch2_subvolume_get(&make_trans(&mut vol), id, true).unwrap();
        assert!(loaded.snapshot > 0);
        assert_eq!(loaded.size, 65536);
    }

    #[test]
    fn test_load_nonexistent() {
        let mut vol = BchVol::test_trees();
        assert!(bch2_subvolume_get(&make_trans(&mut vol), 999, true).is_err());
    }

    #[test]
    fn test_subvolume_id_allocation_sees_pending_transaction_keys() {
        let mut vol = BchVol::test_trees();
        let mut trans = make_trans(&mut vol);

        let first = {
            let mut new_subvolid = 0;
            let mut new_snapshotid = 0;
            let mut new_subvol_out = BchSubvolume::new(0, 0, 4096, 100u64);
            bch2_subvolume_create(&mut trans, 0, 0, 0, &mut new_subvolid, &mut new_snapshotid, &mut new_subvol_out, false).unwrap();
            new_subvolid
        };
        let second = {
            let mut new_subvolid = 0;
            let mut new_snapshotid = 0;
            let mut new_subvol_out = BchSubvolume::new(0, 0, 4096, 200u64);
            bch2_subvolume_create(&mut trans, 0, 0, 0, &mut new_subvolid, &mut new_snapshotid, &mut new_subvol_out, false).unwrap();
            new_subvolid
        };

        assert_ne!(first, second);
        assert!(bch2_subvolume_get(&trans, first, true).is_ok());
        assert!(bch2_subvolume_get(&trans, second, true).is_ok());
    }

    // ─── bch2_subvolume_delete ───

    #[test]
    fn test_delete() {
        let mut vol = BchVol::test_trees();
        let _root = {
            let mut new_subvolid = 0;
            let mut new_snapshotid = 0;
            let mut new_subvol_out = BchSubvolume::new(0, 0, 4096, 100u64);
            bch2_subvolume_create(&mut make_trans(&mut vol), 0, 0, 0, &mut new_subvolid, &mut new_snapshotid, &mut new_subvol_out, false).unwrap();
            new_subvolid
        };
        let target = {
            let mut new_subvolid = 0;
            let mut new_snapshotid = 0;
            let mut new_subvol_out = BchSubvolume::new(0, 0, 4096, 100u64);
            bch2_subvolume_create(&mut make_trans(&mut vol), 0, 0, 0, &mut new_subvolid, &mut new_snapshotid, &mut new_subvol_out, false).unwrap();
            new_subvolid
        };
        assert!(bch2_subvolume_get(&make_trans(&mut vol), target, true).is_ok());
        bch2_subvolume_delete(&mut make_trans(&mut vol), target).unwrap();
        assert!(bch2_subvolume_get(&make_trans(&mut vol), target, true).is_err());
        let list = bch2_subvolume_list(&make_trans(&mut vol));
        assert!(list.len() == 1);
    }

    #[test]
    fn test_unlink_retains_entry_until_delete() {
        let mut vol = BchVol::test_trees();
        let _root = {
            let mut new_subvolid = 0;
            let mut new_snapshotid = 0;
            let mut new_subvol_out = BchSubvolume::new(0, 0, 4096, 100u64);
            bch2_subvolume_create(&mut make_trans(&mut vol), 0, 0, 0, &mut new_subvolid, &mut new_snapshotid, &mut new_subvol_out, false).unwrap();
            new_subvolid
        };
        let target = {
            let mut new_subvolid = 0;
            let mut new_snapshotid = 0;
            let mut new_subvol_out = BchSubvolume::new(0, 0, 4096, 100u64);
            bch2_subvolume_create(&mut make_trans(&mut vol), 0, 0, 0, &mut new_subvolid, &mut new_snapshotid, &mut new_subvol_out, false).unwrap();
            new_subvolid
        };

        bch2_subvolume_unlink(&mut make_trans(&mut vol), target).unwrap();
        let unlinked = bch2_subvolume_get(&make_trans(&mut vol), target, true).unwrap();
        assert!(unlinked.is_unlinked());
        assert!(bch2_subvolume_list(&make_trans(&mut vol)).iter().all(|(id, _)| *id != target));

        bch2_subvolume_delete(&mut make_trans(&mut vol), target).unwrap();
        assert!(bch2_subvolume_get(&make_trans(&mut vol), target, true).is_err());
    }

    #[test]
    fn test_delete_reparents_children_and_clears_master_subvol() {
        let mut vol = BchVol::test_trees();
        let _root = {
            let mut new_subvolid = 0;
            let mut new_snapshotid = 0;
            let mut new_subvol_out = BchSubvolume::new(0, 0, 4096, 50u64);
            bch2_subvolume_create(&mut make_trans(&mut vol), 0, 0, 0, &mut new_subvolid, &mut new_snapshotid, &mut new_subvol_out, false).unwrap();
            new_subvolid
        };
        let parent = {
            let mut new_subvolid = 0;
            let mut new_snapshotid = 0;
            let mut new_subvol_out = BchSubvolume::new(0, 0, 4096, 100u64);
            bch2_subvolume_create(&mut make_trans(&mut vol), 0, 0, 0, &mut new_subvolid, &mut new_snapshotid, &mut new_subvol_out, false).unwrap();
            new_subvolid
        };
        let child =
            bch2_subvolume_snapshot(&mut make_trans(&mut vol), parent, 0, 4096, 200).unwrap();

        let parent_snap = bch2_subvolume_get_snapshot(&make_trans(&mut vol), parent).unwrap();
        let tree_id = bch2_snapshot_read_value(&make_trans(&mut vol), parent_snap)
            .unwrap()
            .tree;
        assert_eq!(
            snap_snapshot::bch2_snapshot_tree_lookup(&make_trans(&mut vol), tree_id)
                .unwrap()
                .master_subvol,
            parent
        );

        bch2_subvolume_delete(&mut make_trans(&mut vol), parent).unwrap();

        let child_sv = bch2_subvolume_get(&make_trans(&mut vol), child, true).unwrap();
        assert_eq!(child_sv.creation_parent, 0);

        let tree =
            snap_snapshot::bch2_snapshot_tree_lookup(&make_trans(&mut vol), tree_id).unwrap();
        assert_eq!(tree.master_subvol, 0);
    }

    #[test]
    fn test_delete_nonexistent() {
        let mut vol = BchVol::test_trees();
        assert!(bch2_subvolume_delete(&mut make_trans(&mut vol), 999).is_err());
    }

    // ─── bch2_subvolume_snapshot ───

    #[test]
    fn test_create_snapshot_subvolume() {
        let mut vol = BchVol::test_trees();
        let parent = {
            let mut new_subvolid = 0;
            let mut new_snapshotid = 0;
            let mut new_subvol_out = BchSubvolume::new(0, 0, 4096, 1000u64);
            bch2_subvolume_create(&mut make_trans(&mut vol), 0, 0, 0, &mut new_subvolid, &mut new_snapshotid, &mut new_subvol_out, false).unwrap();
            new_subvolid
        };
        let snap =
            bch2_subvolume_snapshot(&mut make_trans(&mut vol), parent, 0, 4096, 2000).unwrap();
        assert!(snap > parent);
        let loaded = bch2_subvolume_get(&make_trans(&mut vol), snap, true).unwrap();
        assert!(loaded.is_snapshot());
        assert!(loaded.is_read_only());
        assert_eq!(loaded.creation_parent, parent);
    }

    #[test]
    fn test_create_with_source_only_sets_snapshot_flag() {
        let mut vol = BchVol::test_trees();
        let parent = {
            let mut new_subvolid = 0;
            let mut new_snapshotid = 0;
            let mut new_subvol_out = BchSubvolume::new(0, 0, 4096, 1000u64);
            bch2_subvolume_create(
                &mut make_trans(&mut vol),
                0,
                0,
                0,
                &mut new_subvolid,
                &mut new_snapshotid,
                &mut new_subvol_out,
                false,
            )
            .unwrap();
            new_subvolid
        };
        let mut new_subvolid = 0;
        let mut new_snapshotid = 0;
        let mut new_subvol_out = BchSubvolume::new(0, 0, 4096, 2000u64);
        bch2_subvolume_create(
            &mut make_trans(&mut vol),
            0,
            0,
            parent,
            &mut new_subvolid,
            &mut new_snapshotid,
            &mut new_subvol_out,
            false,
        )
        .unwrap();

        let created = bch2_subvolume_get(&make_trans(&mut vol), new_subvolid, true).unwrap();
        assert!(created.is_snapshot());
        assert!(!created.is_read_only());
    }

    #[test]
    fn test_create_snapshot_invalid_parent() {
        let mut vol = BchVol::test_trees();
        assert!(bch2_subvolume_snapshot(&mut make_trans(&mut vol), 999, 0, 4096, 1000).is_err());
    }

    #[test]
    fn test_create_snapshot_unlinked_parent() {
        let mut vol = BchVol::test_trees();
        let _root = {
            let mut new_subvolid = 0;
            let mut new_snapshotid = 0;
            let mut new_subvol_out = BchSubvolume::new(0, 0, 4096, 100u64);
            bch2_subvolume_create(&mut make_trans(&mut vol), 0, 0, 0, &mut new_subvolid, &mut new_snapshotid, &mut new_subvol_out, false).unwrap();
            new_subvolid
        };
        let parent = {
            let mut new_subvolid = 0;
            let mut new_snapshotid = 0;
            let mut new_subvol_out = BchSubvolume::new(0, 0, 4096, 100u64);
            bch2_subvolume_create(&mut make_trans(&mut vol), 0, 0, 0, &mut new_subvolid, &mut new_snapshotid, &mut new_subvol_out, false).unwrap();
            new_subvolid
        };
        bch2_subvolume_delete(&mut make_trans(&mut vol), parent).unwrap();
        assert!(bch2_subvolume_snapshot(&mut make_trans(&mut vol), parent, 0, 4096, 200).is_err());
    }

    // ─── bch2_subvolume_get_snapshot ───

    #[test]
    fn test_get_snapshot() {
        let mut vol = BchVol::test_trees();
        let id = {
            let mut new_subvolid = 0;
            let mut new_snapshotid = 0;
            let mut new_subvol_out = BchSubvolume::new(0, 0, 4096, 1000u64);
            bch2_subvolume_create(&mut make_trans(&mut vol), 0, 0, 0, &mut new_subvolid, &mut new_snapshotid, &mut new_subvol_out, false).unwrap();
            new_subvolid
        };
        let snap = bch2_subvolume_get_snapshot(&make_trans(&mut vol), id);
        assert!(snap.is_ok());
        assert!(snap.unwrap() > 0);
    }

    #[test]
    fn test_get_snapshot_nonexistent() {
        let mut vol = BchVol::test_trees();
        assert!(bch2_subvolume_get_snapshot(&make_trans(&mut vol), 999).is_err());
    }

    // ─── bch2_subvol_is_ro ───

    #[test]
    fn test_is_ro_normal() {
        let mut vol = BchVol::test_trees();
        let id = {
            let mut new_subvolid = 0;
            let mut new_snapshotid = 0;
            let mut new_subvol_out = BchSubvolume::new(0, 0, 4096, 1000u64);
            bch2_subvolume_create(&mut make_trans(&mut vol), 0, 0, 0, &mut new_subvolid, &mut new_snapshotid, &mut new_subvol_out, false).unwrap();
            new_subvolid
        };
        assert!(bch2_subvol_is_ro(&make_trans(&mut vol), id).is_ok());
    }

    #[test]
    fn test_is_ro_snapshot() {
        let mut vol = BchVol::test_trees();
        let parent = {
            let mut new_subvolid = 0;
            let mut new_snapshotid = 0;
            let mut new_subvol_out = BchSubvolume::new(0, 0, 4096, 1000u64);
            bch2_subvolume_create(&mut make_trans(&mut vol), 0, 0, 0, &mut new_subvolid, &mut new_snapshotid, &mut new_subvol_out, false).unwrap();
            new_subvolid
        };
        let snap =
            bch2_subvolume_snapshot(&mut make_trans(&mut vol), parent, 0, 4096, 2000).unwrap();
        let err = bch2_subvol_is_ro(&make_trans(&mut vol), snap).unwrap_err();
        assert!(matches!(
            err,
            StorageError::Io(ref io) if io.kind() == std::io::ErrorKind::PermissionDenied
        ));
    }

    #[test]
    fn test_is_ro_deleted() {
        let mut vol = BchVol::test_trees();
        let _root = {
            let mut new_subvolid = 0;
            let mut new_snapshotid = 0;
            let mut new_subvol_out = BchSubvolume::new(0, 0, 4096, 1000u64);
            bch2_subvolume_create(&mut make_trans(&mut vol), 0, 0, 0, &mut new_subvolid, &mut new_snapshotid, &mut new_subvol_out, false).unwrap();
            new_subvolid
        };
        let target = {
            let mut new_subvolid = 0;
            let mut new_snapshotid = 0;
            let mut new_subvol_out = BchSubvolume::new(0, 0, 4096, 1000u64);
            bch2_subvolume_create(&mut make_trans(&mut vol), 0, 0, 0, &mut new_subvolid, &mut new_snapshotid, &mut new_subvol_out, false).unwrap();
            new_subvolid
        };
        bch2_subvolume_delete(&mut make_trans(&mut vol), target).unwrap();
        assert!(bch2_subvol_is_ro(&make_trans(&mut vol), target).is_err());
    }

    // ─── 持久化和一致性 ───

    #[test]
    fn test_btree_persistence_across_operations() {
        let mut vol = BchVol::test_trees();

        let _root = {
            let mut new_subvolid = 0;
            let mut new_snapshotid = 0;
            let mut new_subvol_out = BchSubvolume::new(0, 0, 4096, 1000u64);
            bch2_subvolume_create(&mut make_trans(&mut vol), 0, 0, 0, &mut new_subvolid, &mut new_snapshotid, &mut new_subvol_out, false).unwrap();
            new_subvolid
        };
        let id2 = {
            let mut new_subvolid = 0;
            let mut new_snapshotid = 0;
            let mut new_subvol_out = BchSubvolume::new(0, 0, 4096, 1000u64);
            bch2_subvolume_create(&mut make_trans(&mut vol), 0, 0, 0, &mut new_subvolid, &mut new_snapshotid, &mut new_subvol_out, false).unwrap();
            new_subvolid
        };
        let id3 = {
            let mut new_subvolid = 0;
            let mut new_snapshotid = 0;
            let mut new_subvol_out = BchSubvolume::new(0, 0, 8192, 2000u64);
            bch2_subvolume_create(&mut make_trans(&mut vol), 0, 0, 0, &mut new_subvolid, &mut new_snapshotid, &mut new_subvol_out, false).unwrap();
            new_subvolid
        };
        assert_eq!(bch2_subvolume_count(&make_trans(&mut vol)), 3);

        let sv2 = bch2_subvolume_get(&make_trans(&mut vol), id2, true).unwrap();
        assert_eq!(sv2.size, 4096);
        let sv3 = bch2_subvolume_get(&make_trans(&mut vol), id3, true).unwrap();
        assert!(sv3.snapshot > 0);

        bch2_subvolume_delete(&mut make_trans(&mut vol), id2).unwrap();
        assert_eq!(bch2_subvolume_count(&make_trans(&mut vol)), 2);
    }
}
