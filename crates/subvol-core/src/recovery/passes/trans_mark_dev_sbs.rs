use crate::alloc::bch2_trans_mark_dev_sbs;
use crate::recovery::RecoveryState;
use crate::types::StorageError;

/// TransMarkDevSbs pass — 标记 superblock 和 journal 区域到 Alloc btree
///
/// 对应 bcachefs `bch2_trans_mark_dev_sbs()` (PASS_ALWAYS #6)。
/// 将 superblock bucket (0) 标记为 `BchDataType::Sb`，
/// journal bucket 标记为 `BchDataType::Journal`，防止普通分配使用。
///
/// 通过对齐的 metadata bucket 标记链，以 `BTREE_TRIGGER_transactional`
/// 更新 alloc v4 key，并按每个设备自身的 superblock layout/journal 状态标记。
///
/// # 幂等性
/// 多次写入相同的 BchAllocEntry 结果不变。
pub async fn run(state: &mut RecoveryState) -> Result<(), StorageError> {
    bch2_trans_mark_dev_sbs(&state.vol)
}
