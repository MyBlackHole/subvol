pub mod btree;
pub mod bucket_io;
pub mod cache;
pub mod gc;
pub mod interior;
pub mod io;
pub mod iter;
pub mod key;
pub mod key_cache;
pub mod node;
pub mod op;
pub mod search;
pub mod transaction;
pub mod types;
pub(crate) mod update;
pub mod write_buffer;
pub(crate) mod writer;

pub use btree::Btree;
pub use cache::{
    bch2_btree_node_mem_free, bch2_btree_node_transition_state,
    bch2_btree_node_transition_state_locked, bch2_btree_node_write_done_clean, BtreeCache,
    BTREE_FOREGROUND_MERGE_HIGHER, BTREE_FOREGROUND_MERGE_HYSTERESIS,
    BTREE_FOREGROUND_MERGE_THRESHOLD, BTREE_SPLIT_THRESHOLD, BTREE_WRITE_IO_LIMIT, MAX_CLEAN,
    MAX_DIRTY,
};
pub use gc::{
    bch2_check_allocations, bch2_check_topology, bch2_fs_btree_gc_init_early, bch2_gc_alloc_done,
    bch2_gc_alloc_start, bch2_gc_btrees, bch2_gc_gen, bch2_gc_mark_key, bch2_gc_pos_from_sb,
    bch2_gc_pos_to_sb, bch2_gc_pos_to_text, bch2_presplit_shard_boundaries, gc_phase, gc_pos_btree,
    gc_pos_cmp, gc_visited, BtreeGc, GcPhase, GcPos,
};
pub use io::{
    __bch2_btree_node_write, bch2_btree_cancel_all_writes, bch2_btree_flush_all_reads,
    bch2_btree_flush_all_writes, bch2_btree_init_next, bch2_btree_node_io_lock,
    bch2_btree_node_io_unlock, bch2_btree_node_read_done, bch2_btree_node_wait_on_read,
    bch2_btree_node_wait_on_write, bch2_btree_node_write, bch2_btree_post_write_cleanup,
    bch2_validate_bset,
};
pub use iter::BtreeIter;
pub use key::KEY_TYPE_BTREE_PTR_V3;
pub use key::{Addr48, BchVal, Bpos, BtreeEntry, BtreeKey, KeyType, KeyValue};
pub use key_cache::KeyCache;
pub use node::BtreeNode;
pub use node::{
    bch2_btree_node_iter_advance, bch2_btree_node_iter_init, bch2_btree_node_iter_init_from_start,
    bch2_btree_node_iter_next_all, bch2_btree_node_iter_peek, bch2_btree_node_iter_peek_all,
    bch2_btree_node_iter_set_drop, bch2_btree_node_iter_sort,
};
pub use node::{bset, bset_u64s, btree_bset_first, btree_bset_last, for_each_bset};
pub use node::{BsetAuxTreeType, BtreeNodeIter, BtreeNodeIterSet, BSET_CACHELINE, MAX_BSETS};
pub use node::{NODE_ACCESSED, NODE_NEED_REWRITE};
pub use transaction::BtreeTrans;
pub use types::{
    BtreeNodeLockedType, BtreePathError, BtreePathLevel, BtreePathNode, BtreePtrV2, BtreeRoot,
    NodeCache, PendingRootJournal, BTREE_MAX_DEPTH,
};
pub use write_buffer::{
    bch2_btree_write_buffer_flush_going_ro, bch2_btree_write_buffer_flush_sync,
    bch2_btree_write_buffer_maybe_flush, bch2_btree_write_buffer_must_wait,
    bch2_btree_write_buffer_to_text, bch2_btree_write_buffer_tryflush,
    bch2_fs_btree_write_buffer_exit, bch2_fs_btree_write_buffer_init,
    bch2_fs_btree_write_buffer_init_early, bch2_journal_key_to_wb, bch_wb_btree_idx,
    bch_wb_btree_to_btree_id, wb_key_cmp, wb_maybe_flush_exit, wb_maybe_flush_inc, BchWbBtree,
    BtreeWriteBuffer, BtreeWriteBufferKeys, BtreeWriteBufferedKey, WbMaybeFlush,
};

// ---------------------------------------------------------------------------
// BtreeId — bcachefs 对齐的多 btree 架构
// ---------------------------------------------------------------------------

/// 每个 btree 实例处理一种元数据类型（受 bcachefs `enum btree_id` 启发）。
///
/// 所有 type 共享相同的 `Btree` 实现，但各自拥有独立的根节点和 key 空间。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BtreeId {
    /// 数据 extent 映射：bpos(vol_id, lba, snapshot) -> BchVal
    Extents,
    Inodes = 1,
    Dirents = 2,
    Xattrs = 3,
    Stripes = 6,
    Reflink = 7,
    /// 子卷记录：bpos(subvol_id, 0, snapshot) -> Subvolume
    Subvolumes = 8,
    /// 快照树节点：bpos(snapshot_id, 0, 0) -> SnapshotNode
    Snapshots = 9,
    /// 快照树元信息：bpos(tree_id, 0, 0) -> SnapshotTree
    SnapshotTrees = 15,
    /// 空间分配状态：bpos(bucket_index, 0, 0) -> BchAllocEntry
    Alloc = 4,
    /// 空闲 bucket 索引：bpos(0, bucket_index, gen) -> empty value
    ///
    /// 对应 bcachefs BTREE_ID_freespace。由 Alloc btree trigger 自动维护：
    /// - bucket 变为 Free → insert
    /// - bucket 变为 Allocated → delete
    Freespace = 11,
    /// 等待 discard/TRIM 的 bucket 索引：bpos(journal_seq_empty, bucket) -> KEY_TYPE_set
    ///
    /// 对应本地 bcachefs `BTREE_ID_need_discard = 12`。
    NeedDiscard = 12,
    /// bucket generation 索引：bpos(device, chunk, 0) -> BchBucketGens
    BucketGens = 14,
    /// 配额条目：bpos(qid, counter_type, 0) -> BchQuota
    ///
    /// 对应 bcachefs BTREE_ID_quotas。
    /// qid = (type << 56) | subvol_id，counter_type = 0(spc) / 1(ino)。
    Quotas = 5,
    /// 反向指针：bpos(dev, bucket_index, level) -> BchBackpointer
    ///
    /// 对应 bcachefs BTREE_ID_backpointers（8）。
    /// 记录每个 bucket 中每个 extent 的引用，用于 GC 时检测泄漏。
    Backpointers = 13,
    /// 磁盘用量统计：bpos(type, 0, 0) -> BchDiskAccounting
    ///
    /// 对应 bcachefs BTREE_ID_accounting（9）。
    /// 记录各类型（数据/压缩/btree）的磁盘使用量，
    /// 通过 trigger 在每次 extent 变更时自动更新。
    Accounting = 20,
    /// 子卷路径父子索引：bpos(parent_subvol, child_subvol, 0) -> KEY_TYPE_set
    ///
    /// 对应本地 bcachefs `BTREE_ID_subvolume_children`（ID 19）。
    SubvolumeChildren = 19,
    Lru = 10,
    DeletedInodes = 16,
    LoggedOps = 17,
    ReconcileWork = 18,
    ReconcileHipri = 21,
    ReconcilePending = 22,
    ReconcileScan = 23,
    ReconcileWorkPhys = 24,
    ReconcileHipriPhys = 25,
    BucketToStripe = 26,
    StripeBackpointers = 27,
}

impl BtreeId {
    /// 从 u8 表示反解 BtreeId（用于 WAL replay 等反序列化场景）
    ///
    /// 使用本地 bcachefs `BCH_BTREE_IDS` 的持久化 type 编号；未表示的 type 返回 None。
    pub fn from_u8(v: u8) -> Option<BtreeId> {
        match v {
            0 => Some(BtreeId::Extents), 1 => Some(BtreeId::Inodes),
            2 => Some(BtreeId::Dirents), 3 => Some(BtreeId::Xattrs),
            4 => Some(BtreeId::Alloc), 5 => Some(BtreeId::Quotas),
            6 => Some(BtreeId::Stripes), 7 => Some(BtreeId::Reflink),
            8 => Some(BtreeId::Subvolumes), 9 => Some(BtreeId::Snapshots),
            10 => Some(BtreeId::Lru), 11 => Some(BtreeId::Freespace),
            12 => Some(BtreeId::NeedDiscard), 13 => Some(BtreeId::Backpointers),
            14 => Some(BtreeId::BucketGens), 15 => Some(BtreeId::SnapshotTrees),
            16 => Some(BtreeId::DeletedInodes), 17 => Some(BtreeId::LoggedOps),
            18 => Some(BtreeId::ReconcileWork), 19 => Some(BtreeId::SubvolumeChildren),
            20 => Some(BtreeId::Accounting), 21 => Some(BtreeId::ReconcileHipri),
            22 => Some(BtreeId::ReconcilePending), 23 => Some(BtreeId::ReconcileScan),
            24 => Some(BtreeId::ReconcileWorkPhys), 25 => Some(BtreeId::ReconcileHipriPhys),
            26 => Some(BtreeId::BucketToStripe), 27 => Some(BtreeId::StripeBackpointers),
            _ => None,
        }
    }
}

/// 所有 btree type 的完整列表（bcachefs 对齐的 BTREE_ID_NR）
pub const BTREE_ID_NR: [BtreeId; 28] = [
    BtreeId::Extents,
    BtreeId::Inodes,
    BtreeId::Dirents,
    BtreeId::Xattrs,
    BtreeId::Alloc,
    BtreeId::Quotas,
    BtreeId::Stripes,
    BtreeId::Reflink,
    BtreeId::Subvolumes,
    BtreeId::Snapshots,
    BtreeId::Lru,
    BtreeId::Freespace,
    BtreeId::NeedDiscard,
    BtreeId::Backpointers,
    BtreeId::BucketGens,
    BtreeId::SnapshotTrees,
    BtreeId::DeletedInodes,
    BtreeId::LoggedOps,
    BtreeId::ReconcileWork,
    BtreeId::SubvolumeChildren,
    BtreeId::Accounting,
    BtreeId::ReconcileHipri,
    BtreeId::ReconcilePending,
    BtreeId::ReconcileScan,
    BtreeId::ReconcileWorkPhys,
    BtreeId::ReconcileHipriPhys,
    BtreeId::BucketToStripe,
    BtreeId::StripeBackpointers,
];

/// 在 btree 上执行 bit_mod 操作（插入 KEY_TYPE_set 或 KEY_TYPE_deleted）。
///
/// 对应 bcachefs `btree/update.c:bch2_btree_bit_mod()`。
/// `set=true` → 插入 KEY_TYPE_set（标记该位置存在/空白）
/// `set=false` → 插入 KEY_TYPE_Deleted（标记该位置已分配/删除）
///
/// 这是 freespace btree 的核心操作 —— 在 bcachefs 中 alloc trigger 通过
/// `bch2_btree_bit_mod` 维护 freespace btree 与 alloc btree 的同步。
pub fn bch2_btree_bit_mod(vol: &crate::BchVol, btree: BtreeId, pos: Bpos, set: bool) -> bool {
    let key_type = if set {
        KeyType::Set
    } else {
        KeyType::Deleted
    };
    let entry = BtreeEntry::raw(pos, key_type, vec![]);
    vol.btree(btree)
        .bch2_btree_bset_insert_key_wrapper(entry, 0)
}

/// 批量操作类型
pub enum BatchEntry {
    Insert { pos: Bpos, data: Vec<u8> },
    Delete { pos: Bpos },
}

// ---------------------------------------------------------------------------
// 单元测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_device::BlockDevice;

    #[test]
    fn test_btree_type_all_coverage() {
        // 验证 ALL 列表覆盖所有变体且无遗漏
        let mut set = std::collections::HashSet::new();
        for ty in BTREE_ID_NR {
            assert!(set.insert(ty), "duplicate BtreeId variant in ALL");
        }
        assert_eq!(set.len(), BTREE_ID_NR.len());
    }

    #[test]
    fn test_btree_type_index_roundtrip() {
        // 每个 type 到索引再回来应该唯一
        use std::collections::HashSet;
        let mut indices = HashSet::new();
        for ty in BTREE_ID_NR {
            let idx = ty as usize;
            assert!(
                idx < BTREE_ID_NR.len(),
                "index {} >= count {}",
                idx,
                BTREE_ID_NR.len()
            );
            assert!(indices.insert(idx), "duplicate index {}", idx);
        }
    }

    #[test]
    fn test_btree_vol_new_all_initialized() {
        let vol = crate::BchVol::test_trees();
        // 确认每个 type 都有独立的 btree 实例（通过 root 指针区分）
        let roots: std::collections::HashSet<*const _> = BTREE_ID_NR
            .iter()
            .map(|ty| vol.btree(*ty).root() as *const _)
            .collect();
        assert_eq!(
            roots.len(),
            BTREE_ID_NR.len(),
            "each BtreeId must have a distinct Btree instance"
        );
    }

    #[test]
    fn test_btree_vol_insert_and_get() {
        let vol = crate::BchVol::test_trees();
        let key = BtreeKey::from_bpos(Bpos::new(0, 100, 42), KeyType::Normal);
        let val = BchVal::new(0x1234, 1);

        assert!(vol.insert_entry_raw(BtreeId::Extents, BtreeEntry::from((key, val)), 0));
        let got = vol.get_entry(BtreeId::Extents, &key);
        assert_eq!(got, Some((key, val)));
    }

    #[test]
    fn test_btree_vol_types_independent() {
        // 验证不同 type 的 btree 互相隔离
        let vol = crate::BchVol::test_trees();

        let ext_key = BtreeKey::from_bpos(Bpos::new(0, 10, 0), KeyType::Normal);
        let ext_val = BchVal::new(0x100, 1);
        vol.insert_entry_raw(BtreeId::Extents, BtreeEntry::from((ext_key, ext_val)), 0);

        let snap_key = BtreeKey::from_bpos(Bpos::new(2, 20, 1), KeyType::Normal);
        let snap_val = BchVal::new(0x200, 2);
        vol.insert_entry_raw(
            BtreeId::Snapshots,
            BtreeEntry::from((snap_key, snap_val)),
            0,
        );

        // 隔离验证：Extents 中查不到 Snapshots 的 key
        assert_eq!(
            vol.get_entry(BtreeId::Extents, &snap_key),
            None,
            "btree types must be isolated"
        );
        assert_eq!(
            vol.get_entry(BtreeId::Snapshots, &ext_key),
            None,
            "btree types must be isolated"
        );
    }

    #[test]
    fn test_btree_vol_delete() {
        let vol = crate::BchVol::test_trees();
        let key = BtreeKey::from_bpos(Bpos::new(0, 50, 0), KeyType::Normal);
        let val = BchVal::new(0xABC, 1);

        assert!(vol.insert_entry_raw(BtreeId::Extents, BtreeEntry::from((key, val)), 0));
        assert_eq!(vol.get_entry(BtreeId::Extents, &key), Some((key, val)));

        assert!(
            futures::executor::block_on(vol.btree(BtreeId::Extents).bch2_btree_delete(
                &NoopWriter,
                &key,
                0
            ))
            .unwrap_or(false)
        );
        assert_eq!(vol.get_entry(BtreeId::Extents, &key), None);

        // 删除不存在的 key 应返回 false
        assert!(
            !futures::executor::block_on(vol.btree(BtreeId::Extents).bch2_btree_delete(
                &NoopWriter,
                &key,
                0
            ))
            .unwrap_or(false)
        );
    }

    #[test]
    fn test_btree_vol_for_each() {
        let vol = crate::BchVol::test_trees();
        let mut count = 0;
        vol.for_each(|ty, bt| {
            count += 1;
            assert_eq!(bt.root().node.packed_keys + bt.root().node.unpacked_keys, 0, "btree {:?} not empty", ty);
        });
        assert_eq!(count, BTREE_ID_NR.len());
    }

    #[test]
    fn test_btree_vol_default() {
        let vol = crate::BchVol::test_trees();
        assert!(vol.btree(BtreeId::Alloc).root().node.packed_keys + vol.btree(BtreeId::Alloc).root().node.unpacked_keys == 0);
    }

    
    use crate::block_device::{BchDev, MockBlockDevice};
    use crate::btree::key::KeyValue;
    use crate::btree::writer::NoopWriter;
    use crate::journal::reclaim::{JournalEntryPin, JournalPinType};
    use crate::journal::Journal;
    use crate::recovery::{self, RecoveryState};
    use crate::storage::superblock::BchSb;
    use crate::types::{BackendType, BlockAddr};
    
    use std::sync::Arc;

    /// 创建最小 BchSb 用于测试
    fn test_superblock() -> BchSb {
        BchSb::with_volume_info(
            "test-vol".into(),
            1,
            "pool".into(),
            4096,
            1024 * 1024,
            BackendType::Nfs,
        )
    }

    #[tokio::test]
    async fn test_recovery_pass_journal_read_and_replay() {
        let backend = Arc::new(MockBlockDevice::new());
        let vol = crate::BchVol::test_trees();
        let journal = Journal::new(vec![256]);
        journal.set_test_device(Arc::new(BchDev::new(backend.clone(), 0)));

        journal
            .append(
                BtreeId::Extents,
                &[
                    BtreeEntry::new(
                        Bpos::new(0, 10, 0),
                        KeyType::Normal,
                        KeyValue::extent(0x100, 1, 0),
                    ),
                    BtreeEntry::new(
                        Bpos::new(0, 20, 0),
                        KeyType::Normal,
                        KeyValue::extent(0x200, 1, 0),
                    ),
                ],
                false,
            )
            .await
            .unwrap();
        journal.bch2_journal_flush().await.unwrap();

        let sb = test_superblock();
        let mut state = RecoveryState::new(Box::new(vol), journal, sb);
        recovery::bch2_fs_recovery(&mut state).await.unwrap();

        let k1 = BtreeKey::from_bpos(Bpos::new(0, 10, 0), KeyType::Normal);
        assert!(
            state.vol.get_entry(BtreeId::Extents, &k1).is_some(),
            "key 10 should exist"
        );

        let k2 = BtreeKey::from_bpos(Bpos::new(0, 20, 0), KeyType::Normal);
        assert!(
            state.vol.get_entry(BtreeId::Extents, &k2).is_some(),
            "key 20 should exist"
        );

        // Verify passes tracked
        assert!(state.passes_complete > 0, "passes should have completed");
        assert!(!state.jsets.is_empty(), "jsets should be populated");
    }

    #[tokio::test]
    async fn test_recovery_pass_btree_roots() {
        let backend = Arc::new(MockBlockDevice::new());
        let dev = Arc::new(crate::block_device::BchDev::new(backend.clone(), 0));
        let vol = {
            let vol = Arc::new(crate::BchVol::alloc(
                test_superblock(),
                dev,
                crate::bch_vol::VolumeConfig::default(),
                "test-vol".to_string(),
                std::path::PathBuf::from("/tmp/test-vol"),
            ));
            vol.attach_tree_refs(&vol);
            Arc::try_unwrap(vol).unwrap()
        };

        let mut root_node = BtreeNode::new_leaf();
        assert!(root_node.insert(
            BtreeKey::new(42, 1, KeyType::Normal),
            BchVal::new(0xDEAD, 1)
        ));
        let node_bytes = root_node.serialize_to_bucket(0xABCD).unwrap();
        backend
            .write_block(BlockAddr::new(0xABCD), &node_bytes)
            .await
            .unwrap();

        let journal = Journal::new(vec![256]);
        journal.set_test_device(Arc::new(BchDev::new(backend.clone(), 0)));
        journal
            .append_btree_root(BtreeId::Extents, 0xABCD, 0, false)
            .await
            .unwrap();
        let root_pin = JournalEntryPin::new(None, JournalPinType::Btree0);
        journal.bch2_journal_pin_add(1, &root_pin, None);
        journal.bch2_journal_flush().await.unwrap();
        journal
            .append(
                BtreeId::Extents,
                &[BtreeEntry::new(
                    Bpos::new(0, 10, 0),
                    KeyType::Normal,
                    KeyValue::extent(0x100, 1, 0),
                )],
                false,
            )
            .await
            .unwrap();
        journal.bch2_journal_flush().await.unwrap();

        let sb = test_superblock();
        let mut state = RecoveryState::new(Box::new(vol), journal, sb);
        recovery::bch2_fs_recovery(&mut state).await.unwrap();

        let root_key = BtreeKey::new(42, 1, KeyType::Normal);
        assert!(
            state.vol.get_entry(BtreeId::Extents, &root_key).is_some(),
            "root-loaded key 42 should exist"
        );

        let jkey = BtreeKey::from_bpos(Bpos::new(0, 10, 0), KeyType::Normal);
        assert!(
            state.vol.get_entry(BtreeId::Extents, &jkey).is_some(),
            "journal key 10 should exist"
        );
    }

    #[tokio::test]
    async fn test_recovery_empty_journal() {
        let backend = Arc::new(MockBlockDevice::new());
        let vol = crate::BchVol::test_trees();
        let journal = Journal::new(vec![]);
        journal.set_test_device(Arc::new(BchDev::new(backend.clone(), 0)));
        let sb = test_superblock();

        let mut state = RecoveryState::new(Box::new(vol), journal, sb);
        let result = recovery::bch2_fs_recovery(&mut state).await;
        assert!(result.is_ok(), "empty journal should not error");
    }

    #[test]
    fn test_insert_entry_raw_flushes_dirty_key_cache() {
        let vol = crate::BchVol::test_trees();
        let dirty_pos = Bpos::new(0, 50, 0);
        let dirty_entry =
            BtreeEntry::new(dirty_pos, KeyType::Normal, KeyValue::extent(0x111, 1, 0));

        vol.btree(BtreeId::Extents)
            .key_cache
            .bch2_btree_insert_key_cached(dirty_pos, dirty_entry, 0);
        assert_eq!(vol.btree(BtreeId::Extents).key_cache.nr_dirty_keys(), 1);

        let write_pos = Bpos::new(0, 60, 0);
        let write_entry =
            BtreeEntry::new(write_pos, KeyType::Normal, KeyValue::extent(0x222, 1, 0));

        assert!(vol.insert_entry_raw(BtreeId::Extents, write_entry, 0));
        assert_eq!(vol.btree(BtreeId::Extents).key_cache.nr_dirty_keys(), 0);

        let cached_key = BtreeKey::from_bpos(dirty_pos, KeyType::Normal);
        let got = vol.get_entry(BtreeId::Extents, &cached_key);
        assert!(
            got.is_some(),
            "dirty key should still be readable after flush"
        );
        assert_eq!(got.unwrap().1.paddr.get(), 0x111);
    }

    #[test]
    fn test_insert_entry_raw_flushes_dirty_key_cache_with_preexisting_dirty_entry() {
        let vol = crate::BchVol::test_trees();
        let dirty_pos = Bpos::new(0, 70, 0);
        let dirty_entry =
            BtreeEntry::new(dirty_pos, KeyType::Normal, KeyValue::extent(0x333, 1, 0));

        vol.btree(BtreeId::Extents)
            .key_cache
            .bch2_btree_insert_key_cached(dirty_pos, dirty_entry, 0);
        assert_eq!(vol.btree(BtreeId::Extents).key_cache.nr_dirty_keys(), 1);

        let batch_pos = Bpos::new(0, 80, 0);
        let batch_entry =
            BtreeEntry::new(batch_pos, KeyType::Normal, KeyValue::extent(0x444, 1, 0));

        assert!(vol.insert_entry_raw(BtreeId::Extents, batch_entry, 0));
        assert_eq!(vol.btree(BtreeId::Extents).key_cache.nr_dirty_keys(), 0);

        let cached_key = BtreeKey::from_bpos(dirty_pos, KeyType::Normal);
        let got = vol.get_entry(BtreeId::Extents, &cached_key);
        assert!(
            got.is_some(),
            "dirty key should remain readable after batch flush"
        );
        assert_eq!(got.unwrap().1.paddr.get(), 0x333);
    }

    #[tokio::test]
    async fn test_recovery_superblock_roots() {
        let backend = Arc::new(MockBlockDevice::new());
        let dev = Arc::new(crate::block_device::BchDev::new(backend.clone(), 0));
        let vol = {
            let vol = Arc::new(crate::BchVol::alloc(
                test_superblock(),
                dev,
                crate::bch_vol::VolumeConfig::default(),
                "test-vol".to_string(),
                std::path::PathBuf::from("/tmp/test-vol"),
            ));
            vol.attach_tree_refs(&vol);
            Arc::try_unwrap(vol).unwrap()
        };

        let mut alloc_node = BtreeNode::new_leaf();
        assert!(alloc_node.insert(
            BtreeKey::new(100, 1, KeyType::Normal),
            BchVal::new(0xCAFE, 1)
        ));
        let node_bytes = alloc_node.serialize_to_bucket(0xBBBB).unwrap();
        backend
            .write_block(BlockAddr::new(0xBBBB), &node_bytes)
            .await
            .unwrap();

        let journal = Journal::new(vec![256]);
        journal.set_test_device(Arc::new(BchDev::new(backend.clone(), 0)));
        journal
            .append(
                BtreeId::Extents,
                &[BtreeEntry::new(
                    Bpos::new(0, 99, 0),
                    KeyType::Normal,
                    KeyValue::extent(0x999, 1, 0),
                )],
                false,
            )
            .await
            .unwrap();
        journal.bch2_journal_flush().await.unwrap();

        let mut sb = test_superblock();
        // Set root_addrs so btree_roots pass finds the Alloc root
        while sb.root_addrs.len() < 5 {
            sb.root_addrs.push(0);
        }
        sb.root_addrs[4] = 0xBBBB; // Alloc type index

        let mut state = RecoveryState::new(Box::new(vol), journal, sb);
        recovery::bch2_fs_recovery(&mut state).await.unwrap();

        let alloc_key = BtreeKey::new(100, 1, KeyType::Normal);
        assert!(
            state.vol.get_entry(BtreeId::Alloc, &alloc_key).is_some(),
            "superblock-loaded Alloc key 100 should exist"
        );

        let ext_key = BtreeKey::from_bpos(Bpos::new(0, 99, 0), KeyType::Normal);
        assert!(
            state.vol.get_entry(BtreeId::Extents, &ext_key).is_some(),
            "journal key 99 should exist"
        );
    }
}
