use crate::recovery::RecoveryState;
use crate::types::StorageError;

/// Pass: 从 Alloc btree 恢复 allocator 状态
///
/// 对应 bcachefs `bch2_alloc_read()` (PASS_ALWAYS #2)。
/// 遍历 Alloc btree 所有条目，按 bucket_index 合并最新状态，
/// 更新 BchAllocator 各 group 的 bucket 状态。
///
/// # 幂等性
/// 多次执行结果相同（每次从头遍历 btree 重建状态）。
pub async fn run(state: &mut RecoveryState) -> Result<(), StorageError> {
    let allocator = unsafe { &*state.vol.allocator.get() };
    allocator.bch2_alloc_read(&state.vol)
}
