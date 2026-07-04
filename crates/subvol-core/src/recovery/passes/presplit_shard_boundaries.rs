use crate::btree::gc::bch2_presplit_shard_boundaries;
use crate::recovery::RecoveryState;
use crate::types::StorageError;

/// PresplitShardBoundaries pass — 分割跨越 shard 边界的 btree leaf
///
/// 对应 bcachefs `bch2_presplit_shard_boundaries()` (PASS_ALWAYS #48)。
/// 需要 journal_replay 已完成（journal 回放完成后再分割节点）。
///
/// 遍历所有 btree type 的 leaf 节点，将跨越 SHARD_FACTOR（1024）分片边界的
/// 节点预分割为两个，创建 depth=1 的多级树结构。
pub async fn run(state: &mut RecoveryState) -> Result<(), StorageError> {
    bch2_presplit_shard_boundaries(&state.vol)
}
