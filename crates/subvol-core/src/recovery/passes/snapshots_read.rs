use crate::recovery::RecoveryState;
use crate::snap::table::bch2_snapshots_read;
use crate::types::StorageError;

/// SnapshotsRead pass — 从 Snapshots + SnapshotTrees btrees 构建快照表
///
/// 对应 bcachefs `bch2_snapshots_read()` (PASS_ALWAYS #3)。
/// 同时读取：
/// - Snapshots btree → SnapshotTable（快照元数据 + 祖先位图）
/// - SnapshotTrees btree → SnapshotTreeTable（快照树信息）
///
/// 当前 RecoveryState 不持久化这些表（轻量重建）；
/// Volume 在 recovery 完成后按需重建快照表。
///
/// # 幂等性
/// 多次执行结果相同（每次重新从 btree 构建表）。
pub async fn run(state: &mut RecoveryState) -> Result<(), StorageError> {
    let (_table, _tree_table) = bch2_snapshots_read(&*state.vol);
    Ok(())
}
