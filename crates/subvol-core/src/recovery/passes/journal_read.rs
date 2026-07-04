use crate::btree::BTREE_ID_NR;
use crate::journal::{JournalReplayer, JournalStartInfo, Jset};
use crate::recovery::RecoveryState;
use crate::types::StorageError;

/// Pass: 读取所有 journal buckets + 加载 btree roots（合并 journal & superblock）
///
/// 对应 bcachefs 的 journal read + btree root loading 阶段。
/// 两个步骤合并为一个 pass，与 bcachefs 语义一致（journal 读取和 root 提取一起完成）。
///
/// 操作：
/// 1. 从 superblock 初始化 blacklist table，再读取所有 Jset
/// 2. 从 journal 的 BtreeRoot 条目中获取 root 指针
/// 3. 与 superblock 的 root_addrs/root_levels 合并（journal 覆盖 superblock）
/// 4. 调用 load_root() 加载根节点
pub async fn run(state: &mut RecoveryState) -> Result<(), StorageError> {
    // Phase 1: 读取并过滤 journal entries
    state
        .journal
        .bch2_blacklist_table_initialize(&state.superblock.journal_seq_blacklist);
    let mut journal_start = JournalStartInfo::default();
    let all_jsets = state
        .journal
        .bch2_journal_read(&mut journal_start)
        .await
        .map_err(|e| StorageError::NotFound(format!("journal_read: {}", e)))?;

    // 按 seq 排序确保顺序
    let mut jsets: Vec<Jset> = all_jsets.into_iter().map(|(_, jset)| jset).collect();
    jsets.sort_by_key(|j| j.header.seq);

    state.jsets = jsets;

    // Phase 2: 从 journal + superblock 加载 btree roots
    // 合并 journal roots 与 superblock roots（journal 覆盖 superblock）
    let preloaded = state.jsets.iter().cloned().map(|jset| (0, jset)).collect();
    let replayer = JournalReplayer::from_jsets(&state.journal, preloaded);
    let journal_roots = replayer.read_btree_roots().await?;

    for ty in BTREE_ID_NR {
        let idx = ty as usize;
        let mut addr = state.superblock.root_addrs.get(idx).copied().unwrap_or(0);
        let mut level = state.superblock.root_levels.get(idx).copied().unwrap_or(0);
        if let Some(&(_, journal_addr, journal_level)) =
            journal_roots.iter().find(|(t, _, _)| *t == ty)
        {
            addr = journal_addr;
            if journal_level > 0 {
                level = journal_level;
            }
        }
        if addr > 0 {
            state.vol.load_root(ty, addr, Some(level)).await?;
        }
    }

    state.recovered_roots = journal_roots;
    Ok(())
}
