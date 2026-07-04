use crate::btree::BTREE_ID_NR;
use crate::journal::JournalReplayer;
use crate::recovery::RecoveryState;
use crate::types::StorageError;

/// Pass: 从 journal 中提取 btree root 信息并加载 root 节点
///
/// 对应 bcachefs 的 btree root recovery 阶段：
/// 1. 从 journal 的 BtreeRoot 条目中获取 root 指针
/// 2. 与 superblock 的 root_addrs/root_levels 合并（journal 覆盖 superblock）
/// 3. 调用 load_root() 加载根节点
pub async fn run(state: &mut RecoveryState) -> Result<(), StorageError> {
    let replayer = JournalReplayer::new(&state.journal);
    let journal_roots = replayer.read_btree_roots().await?;

    // 合并 superblock roots + journal roots（journal 覆盖 superblock）
    // root_levels 来自 superblock，journal 条目暂不携带 level 信息
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
            state
                .vol
                .load_root(ty, addr, Some(level))
                .await?;
        }
    }

    state.recovered_roots = journal_roots;
    Ok(())
}
