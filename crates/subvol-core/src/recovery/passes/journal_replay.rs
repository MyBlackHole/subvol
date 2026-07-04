use crate::journal::JournalReplayer;
use crate::journal::Jset;
use crate::recovery::RecoveryState;
use crate::types::StorageError;

/// Pass: 重放 journal entries 到 BchVol / btree（两阶段回放）
///
/// 对应 bcachefs `journal_replay` pass：
/// - Phase 1: 重放所有 accounting keys（Alloc btree 条目），确保后续
///   data key 回放期间分配器看到正确的 accounting 状态
/// - Phase 2: accounting_replay_done 后重放所有 data keys
/// - 使用 insert_entry_raw 将 keys 应用到 btree
/// - 幂等性保证：同 seq 的 Jset 最多应用一次
///
/// R4 修复：重用 journal_read pass 已缓存的 state.jsets，避免双重读取。
pub async fn run(state: &mut RecoveryState) -> Result<(), StorageError> {
    // 使用 state.jsets（由 journal_read pass 填充）而非从磁盘重新读取（R4 修复）
    let preloaded: Vec<(u32, Jset)> = state.jsets.iter().map(|j| (0u32, j.clone())).collect();
    let mut replayer = JournalReplayer::from_jsets(&state.journal, preloaded);

    // Phase 1: 重放 accounting keys（Alloc btree 条目）
    // 对应 bcachefs `bch2_journal_replay()` 中 accounting keys 优先处理
    let accounting_applied = replayer.replay_accounting_to_vol(&state.vol).await?;

    // 标记 accounting replay 完成 — 在此点之后，分配器能看到正确的 btree state
    state.accounting_replay_done = true;

    // Phase 2: 重放 data keys（非 Alloc 条目）
    // 对应 bcachefs `bch2_journal_replay()` 中 data keys 第二阶段回放
    let data_applied = replayer.replay_data_to_vol(&state.vol).await?;
    state.applied_count = accounting_applied + data_applied;

    // Phase 3: 收集已重放的 seq（持久化到 superblock 以跳过后续 recovery）
    state.replayed_seqs = replayer.replayed_seqs();

    Ok(())
}
