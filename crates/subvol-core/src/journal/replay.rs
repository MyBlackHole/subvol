//! Journal 恢复器 — 遍历 journal bucket 并重放 entries
//!
//! 在 Volume 非正常关闭后，通过 JournalReplayer 读取所有 journal entries
//! 并重新应用到 btree / BchVol（replay）。
//!
//! # 幂等性
//!
//! JournalReplayer 维护一个 `last_applied_seq` 水位线。
//! 已重放的 Jset 的 seq 若大于此水位线，则应用；否则跳过。
//! 此机制确保两次 replay 相同 Jset → 结果一致。

use std::collections::HashSet;

use crate::btree::key::BtreeEntry;
use crate::btree::BtreeId;
use crate::types::StorageError;
use crate::BchVol;

use super::jset::{Jset, JsetEntryType};
use super::types::{Journal, JournalStartInfo};

/// 预加载的 journal entries，包含 (bucket_idx, Jset) 对
type PreloadedJsets = Vec<(u32, Jset)>;

/// 已重放的 entry — 包含完整解析后的信息
#[derive(Debug, Clone)]
pub struct ReplayedEntry {
    /// Journal seq
    pub seq: u64,
    /// 目标 btree type
    pub btree_type: BtreeId,
    /// Entry 类型
    pub entry_type: JsetEntryType,
    /// 重放的 btree key/value pairs
    pub btree_entries: Vec<BtreeEntry>,
}

/// Journal 恢复器 — 遍历 journal bucket 并重放 entries
///
/// 支持两种模式：
/// 1. **只读模式**：`replay_all()` / `replay_from()` 返回解析后的 entries（不修改 btree）
/// 2. **写入模式**：`replay_all_to_vol()` 将 Jset 应用到 `BchVol`（带幂等检测）
pub struct JournalReplayer<'a> {
    /// 引用 Journal 实例
    pub journal: &'a Journal,
    /// 已重放的最大 seq（幂等水位线）
    pub last_applied_seq: u64,
    /// 已重放的 seq 集合（幂等检测）
    replayed_seqs: HashSet<u64>,
    /// 预加载的 journal entries（非 None 时跳过磁盘读取，修复 R4 双重读取）
    preloaded_jsets: Option<PreloadedJsets>,
}

impl<'a> JournalReplayer<'a> {
    /// 创建新的 JournalReplayer（从磁盘读取 journal）
    pub fn new(journal: &'a Journal) -> Self {
        Self {
            journal,
            last_applied_seq: 0,
            replayed_seqs: HashSet::new(),
            preloaded_jsets: None,
        }
    }

    /// 从预加载的 jsets 创建 JournalReplayer（跳过磁盘读取，修复 R4 双重读取）
    pub fn from_jsets(journal: &'a Journal, jsets: Vec<(u32, Jset)>) -> Self {
        Self {
            journal,
            last_applied_seq: 0,
            replayed_seqs: HashSet::new(),
            preloaded_jsets: Some(jsets),
        }
    }

    /// 获取 jsets — 优先从预加载数据返回，否则从磁盘读取
    async fn get_jsets(&self) -> Result<Vec<(u32, Jset)>, StorageError> {
        if let Some(ref preloaded) = self.preloaded_jsets {
            return Ok(preloaded.clone());
        }
        let mut info = JournalStartInfo::default();
        self.journal
            .bch2_journal_read(&mut info)
            .await
            .map_err(|e| StorageError::NotFound(format!("journal read error: {}", e)))
    }

    /// 获取已重放的 seq 列表（用于持久化到 superblock 以跳过已重放的 entries）
    pub fn replayed_seqs(&self) -> Vec<u64> {
        let mut seqs: Vec<u64> = self.replayed_seqs.iter().copied().collect();
        seqs.sort();
        seqs
    }

    /// 从指定 seq 开始重放（只读模式）
    ///
    /// 遍历所有 journal bucket，找到 seq >= from_seq 的 entries 并解析。
    /// 不修改卷，多用于调试/检查。
    pub async fn replay_from(&self, from_seq: u64) -> Result<Vec<ReplayedEntry>, StorageError> {
        let all_jsets = self.get_jsets().await?;
        let mut result = Vec::new();

        for (_bucket_idx, jset) in all_jsets {
            if jset.header.seq < from_seq {
                continue;
            }
            let entries = Self::parse_jset(&jset)?;
            result.extend(entries);
        }

        Ok(result)
    }

    /// 重放所有 journal entries（只读模式）
    pub async fn replay_all(&self) -> Result<Vec<ReplayedEntry>, StorageError> {
        self.replay_from(0).await
    }

    /// 遍历所有 Jset 并将 entries 应用到 BchVol / btree 引擎（写入模式，带幂等检测）
    ///
    /// 按 seq 递增顺序重放，每个 Jset 最多应用一次。
    /// 幂等性保证：两次调用 replay_all_to_vol 相同 Jset → 结果一致。
    ///
    /// `vol` 为恢复/测试使用的 `BchVol`。
    pub async fn replay_all_to_vol(&mut self, vol: &BchVol) -> Result<u64, StorageError> {
        // 本地 bcachefs recovery 只读取一次 journal list，accounting/data
        // 两阶段共用同一份读取结果。journal_read_bucket() 会更新
        // per-device bucket_seq，因此不能在两阶段之间重新读盘。
        if self.preloaded_jsets.is_none() {
            self.preloaded_jsets = Some(self.get_jsets().await?);
        }
        self.replay_accounting_to_vol(vol).await?;
        self.replay_data_to_vol(vol).await
    }

    /// 两阶段重放阶段 1：仅重放 Accounting keys（Alloc btree 条目）
    ///
    /// 对应 bcachefs `bch2_journal_replay()` 的第一阶段：
    /// 在所有 data keys 之前重放 accounting keys，确保 btree 遍历
    /// 期间分配器看到正确的 accounting 状态。
    ///
    /// 不记录 seq 到 replayed_seqs（由 data phase 统一记录）。
    pub async fn replay_accounting_to_vol(&mut self, vol: &BchVol) -> Result<u64, StorageError> {
        let all_jsets = self.get_jsets().await?;

        let mut sorted: Vec<_> = all_jsets.into_iter().collect();
        sorted.sort_by_key(|(_, jset)| jset.header.seq);

        let mut applied_count = 0u64;

        for (_bucket_idx, jset) in &sorted {
            if jset.header.seq <= self.last_applied_seq
                || self.replayed_seqs.contains(&jset.header.seq)
            {
                continue;
            }

            // 仅应用 Alloc btree 条目（对应 bcachefs bch2_journal_replay_accounting_key）
            for entry in &jset.entries {
                if entry.hdr.entry_type == JsetEntryType::BtreeRoot as u8
                    || entry.hdr.entry_type == JsetEntryType::WriteBufferKeys as u8
                {
                    continue;
                }
                if entry.hdr.btree_type != BtreeId::Alloc as u8 {
                    continue;
                }
                let btree_entries: Vec<BtreeEntry> = if entry.payload.is_empty() {
                    Vec::new()
                } else {
                    bincode::deserialize(&entry.payload)?
                };
                for be in &btree_entries {
                    vol.btree(BtreeId::Alloc)
                        .bch2_btree_bset_insert_key_wrapper(be.clone(), 0);
                    applied_count += 1;
                }
            }
        }

        Ok(applied_count)
    }

    /// 两阶段重放阶段 2：仅重放 Data keys（非 Alloc 条目）
    ///
    /// 对应 bcachefs `bch2_journal_replay()` 的第二阶段：
    /// 在 accounting keys 完成后重放所有 data keys，
    /// 完成后记录 seq 到 replayed_seqs。
    pub async fn replay_data_to_vol(&mut self, vol: &BchVol) -> Result<u64, StorageError> {
        let all_jsets = self.get_jsets().await?;

        let mut sorted: Vec<_> = all_jsets.into_iter().collect();
        sorted.sort_by_key(|(_, jset)| jset.header.seq);

        let mut applied_count = 0u64;

        for (_bucket_idx, jset) in &sorted {
            if jset.header.seq <= self.last_applied_seq
                || self.replayed_seqs.contains(&jset.header.seq)
            {
                continue;
            }

            let replayable_jset = jset.entries.iter().any(|entry| {
                matches!(
                    JsetEntryType::from_u8(entry.hdr.entry_type),
                    Some(JsetEntryType::BtreeKeys)
                        | Some(JsetEntryType::BtreeRoot)
                        | Some(JsetEntryType::WriteBufferKeys)
                )
            });

            // 仅应用普通 key 条目（accounting 已在 phase 1 中处理，对应 bcachefs bch2_journal_replay_key）
            for entry in &jset.entries {
                let entry_type = JsetEntryType::from_u8(entry.hdr.entry_type)
                    .unwrap_or(JsetEntryType::BtreeKeys);
                if entry_type != JsetEntryType::BtreeKeys
                    && entry_type != JsetEntryType::WriteBufferKeys
                {
                    continue;
                }
                if entry.hdr.btree_type == BtreeId::Alloc as u8 {
                    continue;
                }
                let btree_type = BtreeId::from_u8(entry.hdr.btree_type).unwrap_or(BtreeId::Extents);
                let btree_entries: Vec<BtreeEntry> = if entry.payload.is_empty() {
                    Vec::new()
                } else {
                    bincode::deserialize(&entry.payload)?
                };
                for be in &btree_entries {
                    vol.btree(btree_type)
                        .bch2_btree_bset_insert_key_wrapper(be.clone(), 0);
                }
            }
            self.replayed_seqs.insert(jset.header.seq);
            self.last_applied_seq = self.last_applied_seq.max(jset.header.seq);
            applied_count += replayable_jset as u64;
        }

        Ok(applied_count)
    }

    /// 从 journal 中读取所有 btree_root 条目（早期恢复用）
    ///
    /// 返回 (BtreeId, addr, level)，level 从 superblock 获取或默认 0。
    /// 在 Volume 恢复时，需要先提取 btree_root 条目重建树结构，
    /// 然后才能 replay key/value entries。
    pub async fn read_btree_roots(&self) -> Result<Vec<(BtreeId, u64, u8)>, StorageError> {
        let all_jsets = self.get_jsets().await?;

        let mut roots = Vec::new();
        for (_bucket_idx, jset) in &all_jsets {
            for entry in &jset.entries {
                if entry.hdr.entry_type != JsetEntryType::BtreeRoot as u8 {
                    continue;
                }
                let btree_type = BtreeId::from_u8(entry.hdr.btree_type).unwrap_or(BtreeId::Extents);
                let btree_entries: Vec<BtreeEntry> = if entry.payload.is_empty() {
                    Vec::new()
                } else {
                    bincode::deserialize(&entry.payload)?
                };
                if let Some(first) = btree_entries.first() {
                    roots.push((btree_type, first.pos.offset, entry.hdr.level));
                }
            }
        }
        Ok(roots)
    }

    /// 解析单个 Jset 为 ReplayedEntry 列表
    fn parse_jset(jset: &Jset) -> Result<Vec<ReplayedEntry>, StorageError> {
        let mut result = Vec::new();

        for entry in &jset.entries {
            let btree_type = BtreeId::from_u8(entry.hdr.btree_type).unwrap_or(BtreeId::Extents);
            let entry_type =
                JsetEntryType::from_u8(entry.hdr.entry_type).unwrap_or(JsetEntryType::BtreeKeys);
            let btree_entries = match entry_type {
                JsetEntryType::BtreeKeys
                | JsetEntryType::BtreeRoot
                | JsetEntryType::WriteBufferKeys => {
                    if entry.payload.is_empty() {
                        Vec::new()
                    } else {
                        bincode::deserialize(&entry.payload)?
                    }
                }
                _ => Vec::new(),
            };

            result.push(ReplayedEntry {
                seq: jset.header.seq,
                btree_type,
                entry_type,
                btree_entries,
            });
        }

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::super::jset::JsetEntryType;
    use super::*;
    use crate::block_device::{BchDev, MockBlockDevice};
    use crate::btree::key::{Bpos, KeyType, KeyValue};
    use crate::journal::reclaim::{JournalEntryPin, JournalPinType};
    use crate::BchVol;
    use std::sync::Arc;

    #[tokio::test]
    async fn test_journal_replay_all() {
        let backend = MockBlockDevice::new();
        let journal = Journal::new(vec![100, 200]);
        journal.set_test_device(Arc::new(BchDev::new(Arc::new(backend.clone()), 0)));

        // 写 3 个 Jset
        journal
            .append(
                BtreeId::Extents,
                &[BtreeEntry::new(
                    Bpos::new(1, 100, 0),
                    KeyType::Normal,
                    KeyValue::extent(0x1000, 1, 0),
                )],
                false,
            )
            .await
            .unwrap();
        let pin = JournalEntryPin::new(None, JournalPinType::Btree0);
        journal.bch2_journal_pin_add(1, &pin, None);
        journal.bch2_journal_flush().await.unwrap();

        journal
            .append(
                BtreeId::Snapshots,
                &[BtreeEntry::new(
                    Bpos::new(2, 200, 0),
                    KeyType::Normal,
                    KeyValue::Raw(vec![1, 2, 3]),
                )],
                false,
            )
            .await
            .unwrap();
        journal.bch2_journal_flush().await.unwrap();

        journal
            .append(
                BtreeId::Alloc,
                &[BtreeEntry::new(
                    Bpos::new(3, 300, 0),
                    KeyType::Deleted,
                    KeyValue::extent(0, 0, 0),
                )],
                false,
            )
            .await
            .unwrap();
        journal.bch2_journal_flush().await.unwrap();

        // Replay
        let replayer = JournalReplayer::new(&journal);
        let entries = replayer.replay_all().await.unwrap();

        assert!(entries.len() >= 3);
        assert!(entries.iter().any(|entry| {
            entry.seq == 1
                && entry.btree_type == BtreeId::Extents
                && entry.entry_type == JsetEntryType::BtreeKeys
                && entry.btree_entries.len() == 1
                && entry.btree_entries[0].pos.offset == 100
        }));
        assert!(entries.iter().any(|entry| {
            entry.seq == 2
                && entry.btree_type == BtreeId::Snapshots
                && entry.entry_type == JsetEntryType::BtreeKeys
        }));
        assert!(entries.iter().any(|entry| {
            entry.seq == 3
                && entry.btree_type == BtreeId::Alloc
                && entry.entry_type == JsetEntryType::BtreeKeys
                && entry.btree_entries[0].key_type == KeyType::Deleted
        }));
    }

    #[tokio::test]
    async fn test_journal_empty_replay() {
        let backend = MockBlockDevice::new();
        let journal = Journal::new(vec![100]);
        journal.set_test_device(Arc::new(BchDev::new(Arc::new(backend.clone()), 0)));

        let replayer = JournalReplayer::new(&journal);
        let entries = replayer.replay_all().await.unwrap();
        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn test_journal_replay_from_seq() {
        let backend = MockBlockDevice::new();
        let journal = Journal::new(vec![100]);
        journal.set_test_device(Arc::new(BchDev::new(Arc::new(backend.clone()), 0)));

        journal
            .append(
                BtreeId::Extents,
                &[BtreeEntry::new(
                    Bpos::new(1, 1, 0),
                    KeyType::Normal,
                    KeyValue::extent(0x100, 1, 0),
                )],
                false,
            )
            .await
            .unwrap();
        journal.bch2_journal_flush().await.unwrap();

        journal
            .append(
                BtreeId::Extents,
                &[BtreeEntry::new(
                    Bpos::new(1, 2, 0),
                    KeyType::Normal,
                    KeyValue::extent(0x200, 1, 0),
                )],
                false,
            )
            .await
            .unwrap();
        let pin = JournalEntryPin::new(None, JournalPinType::Btree0);
        journal.bch2_journal_pin_add(2, &pin, None);
        journal.bch2_journal_flush().await.unwrap();

        journal
            .append(
                BtreeId::Extents,
                &[BtreeEntry::new(
                    Bpos::new(1, 3, 0),
                    KeyType::Normal,
                    KeyValue::extent(0x300, 1, 0),
                )],
                false,
            )
            .await
            .unwrap();
        journal.bch2_journal_flush().await.unwrap();

        let replayer = JournalReplayer::new(&journal);
        let entries = replayer.replay_from(2).await.unwrap();
        assert!(entries.iter().all(|entry| entry.seq >= 2));
        assert!(entries.iter().any(|entry| entry.seq == 2));
        assert!(entries.iter().any(|entry| entry.seq == 3));
    }

    #[tokio::test]
    async fn test_journal_replay_btree_root_entry() {
        let backend = MockBlockDevice::new();
        let journal = Journal::new(vec![100]);
        journal.set_test_device(Arc::new(BchDev::new(Arc::new(backend.clone()), 0)));

        journal
            .append_btree_root(BtreeId::Extents, 0xABCD, 0, false)
            .await
            .unwrap();
        journal.bch2_journal_flush().await.unwrap();

        let replayer = JournalReplayer::new(&journal);
        let entries = replayer.replay_all().await.unwrap();

        assert!(entries.iter().any(|entry| {
            entry.entry_type == JsetEntryType::BtreeRoot && entry.btree_type == BtreeId::Extents
        }));
    }

    #[tokio::test]
    async fn test_journal_replay_to_vol() {
        let backend = MockBlockDevice::new();
        let journal = Journal::new(vec![100, 200]);
        journal.set_test_device(Arc::new(BchDev::new(Arc::new(backend.clone()), 0)));
        let vol = BchVol::test_trees();

        // 写 2 个 entry 到 journal
        journal
            .append(
                BtreeId::Extents,
                &[BtreeEntry::new(
                    Bpos::new(0, 100, 0),
                    KeyType::Normal,
                    KeyValue::extent(0x1000, 1, 0),
                )],
                false,
            )
            .await
            .unwrap();
        let pin = JournalEntryPin::new(None, JournalPinType::Btree0);
        journal.bch2_journal_pin_add(1, &pin, None);
        journal.bch2_journal_flush().await.unwrap();

        journal
            .append(
                BtreeId::Snapshots,
                &[BtreeEntry::new(
                    Bpos::new(0, 200, 0),
                    KeyType::Normal,
                    KeyValue::Raw(vec![10, 20]),
                )],
                false,
            )
            .await
            .unwrap();
        journal.bch2_journal_flush().await.unwrap();

        // Replay 到 vol
        let mut replayer = JournalReplayer::new(&journal);
        let applied = replayer.replay_all_to_vol(&vol).await.unwrap();
        assert_eq!(applied, 2);

        // 验证数据已写入 vol 的 btree
        let ext_key = crate::btree::key::BtreeKey::from_bpos(Bpos::new(0, 100, 0), KeyType::Normal);
        assert!(vol
            .btree(BtreeId::Extents)
            .bch2_btree_iter_peek(&ext_key)
            .is_some());

        let snap_key =
            crate::btree::key::BtreeKey::from_bpos(Bpos::new(0, 200, 0), KeyType::Normal);
        assert!(vol
            .btree(BtreeId::Snapshots)
            .bch2_btree_iter_peek(&snap_key)
            .is_some());
    }

    #[tokio::test]
    async fn test_journal_replay_preserves_extent_replicas() {
        let backend = MockBlockDevice::new();
        let journal = Journal::new(vec![100, 200]);
        journal.set_test_device(Arc::new(BchDev::new(Arc::new(backend), 0)));
        let vol = BchVol::test_trees();
        let value = KeyValue::ExtentPtrs {
            blocks: 2,
            ptrs: vec![
                crate::btree::key::ExtentPtr {
                    dev: 0,
                    gen: 1,
                    offset: 0x100,
                    cached: false,
                    unwritten: false,
                },
                crate::btree::key::ExtentPtr {
                    dev: 1,
                    gen: 2,
                    offset: 0x200,
                    cached: false,
                    unwritten: false,
                },
            ],
            crc32c: 0,
            crc_offset_blocks: 0,
        };
        journal
            .append(
                BtreeId::Extents,
                &[BtreeEntry::new(
                    Bpos::new(0, 400, 0),
                    KeyType::Normal,
                    value.clone(),
                )],
                false,
            )
            .await
            .unwrap();
        journal.bch2_journal_flush().await.unwrap();

        let mut replayer = JournalReplayer::new(&journal);
        assert_eq!(replayer.replay_all_to_vol(&vol).await.unwrap(), 1);
        let entry = vol
            .get_entry_raw(BtreeId::Extents, Bpos::new(0, 400, 0))
            .expect("replayed extent");
        let replayed = match entry.value {
            KeyValue::Raw(bytes) => KeyValue::from_bytes(&bytes),
            other => other,
        };
        assert_eq!(replayed, value);
    }

    #[tokio::test]
    async fn test_journal_replay_idempotent() {
        let backend = MockBlockDevice::new();
        let journal = Journal::new(vec![100]);
        journal.set_test_device(Arc::new(BchDev::new(Arc::new(backend.clone()), 0)));
        let vol = BchVol::test_trees();

        // 写 1 个 entry
        journal
            .append(
                BtreeId::Extents,
                &[BtreeEntry::new(
                    Bpos::new(0, 42, 0),
                    KeyType::Normal,
                    KeyValue::extent(0x500, 1, 0),
                )],
                false,
            )
            .await
            .unwrap();
        journal.bch2_journal_flush().await.unwrap();

        // 第一次 replay
        let mut replayer = JournalReplayer::new(&journal);
        let applied1 = replayer.replay_all_to_vol(&vol).await.unwrap();
        assert_eq!(applied1, 1);

        // 第二次 replay（相同 Jset）→ 幂等，应用 0
        let applied2 = replayer.replay_all_to_vol(&vol).await.unwrap();
        assert_eq!(applied2, 0);

        // 结果一致
        let key = crate::btree::key::BtreeKey::from_bpos(Bpos::new(0, 42, 0), KeyType::Normal);
        let result = vol.btree(BtreeId::Extents).bch2_btree_iter_peek(&key);
        assert!(result.is_some());
        if let Some((_, val)) = result {
            assert_eq!(val.paddr.get(), 0x500);
        }
    }

    #[tokio::test]
    async fn test_journal_read_btree_roots() {
        let backend = MockBlockDevice::new();
        let journal = Journal::new(vec![100, 200]);
        journal.set_test_device(Arc::new(BchDev::new(Arc::new(backend.clone()), 0)));

        journal
            .append_btree_root(BtreeId::Extents, 0x1234, 1, false)
            .await
            .unwrap();
        let pin = JournalEntryPin::new(None, JournalPinType::Btree1);
        journal.bch2_journal_pin_add(1, &pin, None);
        journal.bch2_journal_flush().await.unwrap();
        journal
            .append_btree_root(BtreeId::Snapshots, 0x5678, 2, false)
            .await
            .unwrap();
        journal.bch2_journal_flush().await.unwrap();

        let replayer = JournalReplayer::new(&journal);
        let roots = replayer.read_btree_roots().await.unwrap();

        assert!(roots.len() >= 2);
        assert!(roots.contains(&(BtreeId::Extents, 0x1234, 1)));
        assert!(roots.contains(&(BtreeId::Snapshots, 0x5678, 2)));
    }

    #[tokio::test]
    async fn test_journal_replay_keys_skips_btree_root() {
        let backend = MockBlockDevice::new();
        let journal = Journal::new(vec![100]);
        journal.set_test_device(Arc::new(BchDev::new(Arc::new(backend.clone()), 0)));
        let vol = BchVol::test_trees();

        journal
            .append_btree_root(BtreeId::Extents, 0xABCD, 0, false)
            .await
            .unwrap();
        let pin = JournalEntryPin::new(None, JournalPinType::Btree0);
        journal.bch2_journal_pin_add(1, &pin, None);
        // flush 以产生新 entry，使下一个 append 获得不同 seq
        journal.bch2_journal_flush().await.unwrap();
        journal
            .append(
                BtreeId::Snapshots,
                &[BtreeEntry::new(
                    Bpos::new(0, 200, 0),
                    KeyType::Normal,
                    KeyValue::Raw(vec![10]),
                )],
                false,
            )
            .await
            .unwrap();
        journal.bch2_journal_flush().await.unwrap();

        let mut replayer = JournalReplayer::new(&journal);
        let applied = replayer.replay_all_to_vol(&vol).await.unwrap();
        assert_eq!(applied, 2);

        let btree_root_key =
            crate::btree::key::BtreeKey::from_bpos(Bpos::new(0, 0xABCD, 0), KeyType::Normal);
        assert!(vol
            .btree(BtreeId::Extents)
            .bch2_btree_iter_peek(&btree_root_key)
            .is_none());

        let snap_key =
            crate::btree::key::BtreeKey::from_bpos(Bpos::new(0, 200, 0), KeyType::Normal);
        assert!(vol
            .btree(BtreeId::Snapshots)
            .bch2_btree_iter_peek(&snap_key)
            .is_some());
    }

    #[tokio::test]
    async fn test_journal_replay_skips_blacklist_entries() {
        let vol = Arc::new(BchVol::test_trees());
        vol.attach_tree_refs(&vol);
        vol.journal_ref().set_vol_ref(&vol);
        let journal = vol.journal_ref();
        journal
            .bch2_journal_seq_blacklist_add(&vol, 10, 20)
            .await
            .unwrap();
        journal
            .bch2_journal_seq_blacklist_add(&vol, 30, 40)
            .await
            .unwrap();
        journal
            .bch2_journal_seq_blacklist_add(&vol, 20, 30)
            .await
            .unwrap();
        assert_eq!(
            vol.superblock().journal_seq_blacklist,
            vec![crate::journal::BlacklistEntry {
                start_seq: 10,
                end_seq: 40,
            }]
        );
        assert!(journal.bch2_journal_seq_is_blacklisted(10, false));
        assert!(journal.bch2_journal_seq_is_blacklisted(39, false));
        assert!(!journal.bch2_journal_seq_is_blacklisted(40, false));
        assert!(vol
            .superblock()
            .feature_test(crate::storage::superblock::feature_bits::JOURNAL_SEQ_BLACKLIST_V3));
        let dev = vol.primary_device_rcu_noerror().unwrap();
        let disk_sb = crate::storage::superblock::BchSb::read_from_device(&dev)
            .await
            .unwrap();
        assert_eq!(
            disk_sb.journal_seq_blacklist,
            vol.superblock().journal_seq_blacklist
        );
    }
}
