use crate::recovery::RecoveryState;
use crate::types::StorageError;

/// AccountingRead pass — 验证 allocator 使用计数一致性
///
/// 对应 bcachefs `bch2_accounting_read()` (PASS_ALWAYS #39)。
/// subvol 中 accounting 简化（Alloc btree 条目直接携带 bucket state），
/// bch2_alloc_read() 已完成状态恢复。此 pass 做完整性验证。
///
/// # 幂等性
/// 只读验证，无副作用。
pub async fn run(state: &mut RecoveryState) -> Result<(), StorageError> {
    let allocator = unsafe { &*state.vol.allocator.get() };
    let (used, free, total) = state
        .vol
        .device_registry
        .dev_indices()
        .into_iter()
        .filter_map(|dev| state.vol.device_registry.resolve_bch_dev(dev))
        .fold((0, 0, 0), |(used, free, total), ca| {
            (
                used + allocator.allocated_blocks(&ca),
                free + allocator.free_blocks(&ca),
                total + allocator.total_blocks(&ca),
            )
        });
    if used + free > total {
        return Err(StorageError::InvalidData(format!(
            "accounting mismatch: used {} + free {} > total {}",
            used, free, total
        )));
    }
    Ok(())
}
