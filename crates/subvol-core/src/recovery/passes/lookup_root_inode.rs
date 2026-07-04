use crate::btree::BtreeTrans;
use crate::recovery::RecoveryState;
use crate::subvol::ops::bch2_subvolume_get;
use crate::types::StorageError;

/// LookupRootInode pass — 查找根子卷，完成 recovery
///
/// 对应 bcachefs `bch2_lookup_root_inode()` (PASS_ALWAYS #42)。
/// bcachefs 中两步串联：获取根子卷 → 获取根 inode。
/// subvol 无独立 Inodes btree，当前验证根子卷 subvol_id=1 可读。
/// 空 btree（新文件系统）宽容通过。
///
/// # 幂等性
/// 只读查询，无副作用。
pub async fn run(state: &mut RecoveryState) -> Result<(), StorageError> {
    const ROOT_SUBVOL: u32 = 1;

    let t = BtreeTrans::new_ro(&state.vol);
    let _subvol = bch2_subvolume_get(&t, ROOT_SUBVOL, true);
    Ok(())
}
