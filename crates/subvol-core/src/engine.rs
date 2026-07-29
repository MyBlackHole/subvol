use std::sync::Arc;

use crate::block_device::superblock::BtreeRootEntry;
use crate::block_device::BchDev;
use crate::btree::key::{Bpos, BtreeEntry};
use crate::btree::transaction::{BtreeProvider, BtreeTrans};
use crate::btree::tree::Btree;
use crate::btree::types::{BtreeId, BTREE_ID_ALLOC, BTREE_ID_DATA_INDEX, BTREE_ID_FREESPACE};
use crate::journal::JsetOverlay;
use crate::types::StorageError;
use crate::BchVol;

pub struct Allocator {
    pub(crate) vol: Arc<BchVol>,
    pub(crate) freespace_tree: Btree,
    pub(crate) alloc_tree: Btree,
    pub(crate) data_tree: Btree,
    pub(crate) dev: Arc<BchDev>,
    pub(crate) next_inode: u64,
}

impl Allocator {
    pub fn new(vol: &Arc<BchVol>, dev: &Arc<BchDev>) -> Self {
        Allocator {
            vol: vol.clone(),
            freespace_tree: Btree::new(BTREE_ID_FREESPACE, dev),
            alloc_tree: Btree::new(BTREE_ID_ALLOC, dev),
            data_tree: Btree::new(BTREE_ID_DATA_INDEX, dev),
            dev: dev.clone(),
            next_inode: 1,
        }
    }

    pub fn new_with_dev(vol: &Arc<BchVol>, dev: &Arc<BchDev>) -> Self {
        Self::new(vol, dev)
    }

    pub fn key_counts(&self) -> (u32, u32, u32) {
        (
            self.freespace_tree.total_key_count(),
            self.alloc_tree.total_key_count(),
            self.data_tree.total_key_count(),
        )
    }

    pub fn root_entries(&self) -> Vec<BtreeRootEntry> {
        vec![
            BtreeRootEntry {
                btree_id: BTREE_ID_FREESPACE.0,
                level: self.freespace_tree.root.level,
                root_offset: 0,
            },
            BtreeRootEntry {
                btree_id: BTREE_ID_ALLOC.0,
                level: self.alloc_tree.root.level,
                root_offset: 0,
            },
            BtreeRootEntry {
                btree_id: BTREE_ID_DATA_INDEX.0,
                level: self.data_tree.root.level,
                root_offset: 0,
            },
        ]
    }

    pub async fn persist_roots(
        &mut self,
        dev: &BchDev,
        root_area_start: u64,
    ) -> Result<Vec<BtreeRootEntry>, StorageError> {
        use crate::data::extents_format::BLOCK_SIZE;

        let persist_tree_root = |tree: &mut Btree| {
            let child_offsets: Vec<u64> =
                tree.child_nodes.iter().map(|child| child.disk_offset).collect();
            let saved = tree.root.rewrite_ptrs_for_write(&child_offsets);
            let data = tree.persist();
            tree.root.restore_ptr_offsets(&saved);
            (data, tree.root.level)
        };
        let (freespace_data, freespace_level) = persist_tree_root(&mut self.freespace_tree);
        let (alloc_data, alloc_level) = persist_tree_root(&mut self.alloc_tree);
        let (data_index_data, data_index_level) = persist_tree_root(&mut self.data_tree);
        let roots_serialized: [(BtreeId, Vec<u8>, u8); 3] = [
            (BTREE_ID_FREESPACE, freespace_data, freespace_level),
            (BTREE_ID_ALLOC, alloc_data, alloc_level),
            (BTREE_ID_DATA_INDEX, data_index_data, data_index_level),
        ];

        let mut entries = Vec::with_capacity(3);
        let mut offset = root_area_start;
        for (btree_id, data, level) in &roots_serialized {
            let alloc_size = round_up(data.len() as u64, BLOCK_SIZE);
            dev.write_at(offset, data).await?;
            crate::log_verbose!(
                "persist_root: id={} level={} offset={} size={}",
                btree_id.0,
                level,
                offset,
                data.len()
            );
            entries.push(BtreeRootEntry {
                btree_id: btree_id.0,
                level: *level,
                root_offset: offset,
            });
            offset += alloc_size;
        }
        Ok(entries)
    }

    pub async fn from_roots(
        vol: &Arc<BchVol>,
        dev: &Arc<BchDev>,
        roots: &[BtreeRootEntry],
    ) -> Result<Self, StorageError> {
        let mut ft = Option::<Btree>::None;
        let mut at = Option::<Btree>::None;
        let mut dt = Option::<Btree>::None;

        for entry in roots {
            if entry.root_offset == 0 {
                continue;
            }
            let data = dev
                .read_at(entry.root_offset, crate::btree::types::NODE_SIZE as usize)
                .await?;
            let mut tree = Btree::from_persisted_with_device(
                &data,
                BtreeId::from_u8(entry.btree_id),
                dev,
                entry.root_offset,
            )
            .await?
            .ok_or_else(|| StorageError::Internal("failed to deserialize btree tree".into()))?;
            tree.root.level = entry.level;
            match entry.btree_id {
                id if id == BTREE_ID_FREESPACE.0 => {
                    ft = Some(tree);
                }
                id if id == BTREE_ID_ALLOC.0 => {
                    at = Some(tree);
                }
                id if id == BTREE_ID_DATA_INDEX.0 => {
                    dt = Some(tree);
                }
                _ => {}
            }
        }

        let ft = ft.unwrap_or_else(|| Btree::new(BTREE_ID_FREESPACE, dev));
        let at = at.unwrap_or_else(|| Btree::new(BTREE_ID_ALLOC, dev));
        let dt = dt.unwrap_or_else(|| Btree::new(BTREE_ID_DATA_INDEX, dev));

        // Scan leaf maxima directly when restoring the inode counter.  A
        // cursor walk from POS_MIN would visit every missing inode position
        // in a sparse tree instead of advancing to the next stored key.
        let max_inode = if dt.root.level == 0 {
            dt.root.last_entry().map(|entry| entry.pos.inode).unwrap_or(0)
        } else {
            dt.child_nodes
                .iter()
                .filter(|node| node.level == 0)
                .filter_map(|node| node.last_entry())
                .map(|entry| entry.pos.inode)
                .max()
                .unwrap_or(0)
        };

        Ok(Allocator {
            vol: vol.clone(),
            freespace_tree: ft,
            alloc_tree: at,
            data_tree: dt,
            dev: dev.clone(),
            next_inode: max_inode + 1,
        })
    }

    /// 设置 journal overlay 查询回调（重播阶段使用）
    ///
    /// 所有 BtreeIter::peek() 将通过 overlay 优先返回，
    /// 使得重播阶段有一致的数据查询窗口。
    /// 重播完成后调用 `clear_journal_overlay()` 清除。
    pub fn set_journal_overlay(&mut self, overlay: Arc<JsetOverlay>) {
        let trees: [(u8, &mut Btree); 3] = [
            (BTREE_ID_FREESPACE.0, &mut self.freespace_tree),
            (BTREE_ID_ALLOC.0, &mut self.alloc_tree),
            (BTREE_ID_DATA_INDEX.0, &mut self.data_tree),
        ];
        for (bt, tree) in trees {
            let overlay = overlay.clone();
            tree.set_lookup_overlay(Some(Arc::new(move |pos: &Bpos| {
                overlay
                    .get_with_type(bt, pos)
                    .map(|(entry_type, level, payload)| BtreeEntry {
                        btree_type: bt,
                        level,
                        entry_type,
                        pos: *pos,
                        payload: payload.clone(),
                    })
            })));
        }
    }

    /// 从 overlay 重放条目到 btree（重播完成阶段使用）
    ///
    /// 对应 bcachefs 中 replay 结束后将 overlay journal entries 应用到 btree。
    /// 返回重放的条目数。
    pub async fn replay_from_overlay(
        &mut self,
        overlay: &mut JsetOverlay,
    ) -> Result<usize, StorageError> {
        let mut entries: Vec<_> = overlay.drain_with_type().collect();
        entries.sort_by_key(|((bt, pos), _)| (*bt, *pos));

        let mut count = 0;
        for accounting in [true, false] {
            let mut tx = BtreeTrans::new_replay(&self.vol);
            for ((bt, pos), (entry_type, _level, payload)) in entries.iter().filter(|((bt, _), _)| {
                (*bt == BTREE_ID_FREESPACE.0 || *bt == BTREE_ID_ALLOC.0) == accounting
            }) {
                let iter = tx.iter(self, BtreeId(*bt), *pos, true);
                tx.update_from_iter(&iter, *entry_type, payload.clone());
                count += 1;
            }
            if !tx.is_empty() {
                tx.commit(self).await?;
            }
        }
        Ok(count)
    }

    /// 更新根节点记录（从 journal root_records 加载磁盘上的新根）
    pub async fn apply_root_records(
        &mut self,
        root_records: &[(u8, u8, u64)],
    ) -> Result<(), StorageError> {
        for (bt, level, off) in root_records {
            if *off == 0 {
                continue;
            }
            let data = self
                .dev
                .read_at(*off, crate::btree::types::NODE_SIZE as usize)
                .await?;
            let mut tree = Btree::from_persisted_with_device(
                &data,
                BtreeId::from_u8(*bt),
                &self.dev,
                *off,
            )
            .await?
            .ok_or_else(|| {
                StorageError::Internal(format!(
                    "failed to load root from disk: bt={} off={}",
                    bt, off
                ))
            })?;
            tree.root.level = *level;
            let target = self.get_btree(BtreeId::from_u8(*bt));
            *target = tree;
            crate::log_info!("apply_root_records: bt={} level={} off={}", bt, level, off);
        }
        Ok(())
    }

    /// 清除 journal overlay 查询回调（重播完成后调用）
    ///
    /// 清除后所有查询回到正常 btree 数据路径。
    pub fn clear_journal_overlay(&mut self) {
        self.freespace_tree.set_lookup_overlay(None);
        self.alloc_tree.set_lookup_overlay(None);
        self.data_tree.set_lookup_overlay(None);
    }

    /// 获取 volume 引用
    pub fn vol_ref(&self) -> &Arc<BchVol> {
        &self.vol
    }

    /// 通过 btree_id 获取 &Btree 引用（读路径使用）
    pub fn get_btree_ref(&self, id: BtreeId) -> &Btree {
        match id {
            id if id == BTREE_ID_ALLOC => &self.alloc_tree,
            id if id == BTREE_ID_FREESPACE => &self.freespace_tree,
            id if id == BTREE_ID_DATA_INDEX => &self.data_tree,
            _ => panic!("unknown btree id: {}", id.0),
        }
    }
}

impl BtreeProvider for Allocator {
    fn get_btree(&mut self, id: BtreeId) -> &mut Btree {
        match id {
            id if id == BTREE_ID_ALLOC => &mut self.alloc_tree,
            id if id == BTREE_ID_FREESPACE => &mut self.freespace_tree,
            id if id == BTREE_ID_DATA_INDEX => &mut self.data_tree,
            _ => panic!("unknown btree id: {}", id.0),
        }
    }
}

fn round_up(x: u64, align: u64) -> u64 {
    if align == 0 {
        x
    } else {
        (x + align - 1) & !(align - 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btree::key::Bpos;
    use crate::btree::tree::BtreeIter;

    #[test]
    fn replay_commit_does_not_append_to_journal() {
        let stub = Arc::new(BchVol::new());
        let dev = Arc::new(BchDev::with_size(stub, 1 << 20));
        let vol = BchVol::with_dev(dev.clone(), Vec::new());
        let mut alloc = Allocator::new(&vol, &dev);
        let mut overlay = JsetOverlay::new();
        overlay.set_entry(
            BTREE_ID_DATA_INDEX.0,
            Bpos {
                inode: 1,
                offset: 0,
                snapshot: 0,
            },
            0,
            vec![1, 2, 3],
        );
        let before = vol.journal_ref().bch2_journal_cur_seq();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let applied = runtime
            .block_on(alloc.replay_from_overlay(&mut overlay))
            .unwrap();
        assert_eq!(applied, 1);
        assert_eq!(vol.journal_ref().bch2_journal_cur_seq(), before);
        assert_eq!(alloc.data_tree.total_key_count(), 1);
    }

    #[test]
    #[should_panic(expected = "btree update requires a transaction iterator")]
    fn transaction_rejects_standalone_iterator_updates() {
        let stub = Arc::new(BchVol::new());
        let dev = Arc::new(BchDev::with_size(stub, 1 << 20));
        let vol = BchVol::with_dev(dev.clone(), Vec::new());
        let mut alloc = Allocator::new(&vol, &dev);
        let mut tx = BtreeTrans::new(&vol);
        let iter = BtreeIter::new(&alloc.data_tree, Bpos::MIN);
        tx.update_from_iter(&iter, 0, vec![1]);
    }

    #[test]
    fn persisted_internal_root_restores_levels_and_child_data() {
        let stub = Arc::new(BchVol::new());
        let dev = Arc::new(BchDev::with_size(stub, 1 << 22));
        let vol = BchVol::with_dev(dev.clone(), Vec::new());
        let mut alloc = Allocator::new(&vol, &dev);
        let root_area = crate::block_device::superblock::data_area_offset(0, 0);
        let root_offset = root_area + 2 * crate::btree::types::NODE_SIZE;
        let child_offset = root_area + 3 * crate::btree::types::NODE_SIZE;

        let mut leaf = crate::btree::node::BtreeNode::new_leaf(BTREE_ID_DATA_INDEX);
        leaf.disk_offset = child_offset;
        leaf.disk_size = crate::btree::types::NODE_SIZE as u32;
        leaf.insert_key(BtreeEntry {
            btree_type: BTREE_ID_DATA_INDEX.0,
            level: 0,
            entry_type: 0,
            pos: Bpos { inode: 7, offset: 0, snapshot: 0 },
            payload: vec![7, 8, 9],
        })
        .unwrap();

        let mut root = crate::btree::node::BtreeNode::new(1, BTREE_ID_DATA_INDEX);
        root.disk_offset = root_offset;
        root.disk_size = crate::btree::types::NODE_SIZE as u32;
        root.insert_key(BtreeEntry {
            btree_type: BTREE_ID_DATA_INDEX.0,
            level: 1,
            entry_type: crate::data::extents_format::ENTRY_TYPE_BTREE_PTR,
            pos: Bpos { inode: 7, offset: 0, snapshot: 0 },
            payload: crate::data::extents_format::BtreePtr {
                offset: 0,
                child_level: 0,
            }
            .to_bytes(),
        })
        .unwrap();
        alloc.data_tree.root = root;
        alloc.data_tree.child_nodes.push(leaf);

        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            alloc.data_tree.flush_pending_writes().await.unwrap();
            let roots = alloc.persist_roots(&dev, root_area).await.unwrap();
            let reopened = Allocator::from_roots(&vol, &dev, &roots).await.unwrap();
            assert_eq!(reopened.data_tree.root.level, 1);
            assert_eq!(reopened.data_tree.child_nodes.len(), 1);
            assert_eq!(reopened.data_tree.child_nodes[0].level, 0);
            assert_eq!(
                BtreeIter::new(
                    &reopened.data_tree,
                    Bpos { inode: 7, offset: 0, snapshot: 0 },
                )
                .peek()
                .unwrap()
                .payload,
                vec![7, 8, 9]
            );
        });
    }

    #[test]
    fn transaction_handles_more_than_one_leaf_capacity() {
        let stub = Arc::new(BchVol::new());
        let dev = Arc::new(BchDev::with_size(stub, 1 << 20));
        let vol = BchVol::with_dev(dev.clone(), Vec::new());
        let mut alloc = Allocator::new(&vol, &dev);
        let mut tx = BtreeTrans::new(&vol);
        for inode in 0..300 {
            let iter = tx.iter(
                &alloc,
                BTREE_ID_DATA_INDEX,
                Bpos {
                    inode,
                    offset: 0,
                    snapshot: 0,
                },
                true,
            );
            tx.update_from_iter(&iter, 0, vec![inode as u8]);
        }
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime
            .block_on(tx.commit(&mut alloc))
            .expect("large transaction should commit");
        assert_eq!(alloc.data_tree.total_key_count(), 300);
    }

    #[test]
    fn transaction_retries_oversized_batch_in_bounded_commits() {
        let stub = Arc::new(BchVol::new());
        let dev = Arc::new(BchDev::with_size(stub, 1 << 20));
        let vol = BchVol::with_dev(dev.clone(), Vec::new());
        let mut alloc = Allocator::new(&vol, &dev);
        let mut tx = BtreeTrans::new(&vol);
        for inode in 0..3_000 {
            let iter = tx.iter(
                &alloc,
                BTREE_ID_DATA_INDEX,
                Bpos { inode, offset: 0, snapshot: 0 },
                true,
            );
            tx.update_from_iter(&iter, 0, vec![inode as u8]);
        }
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime
            .block_on(tx.commit(&mut alloc))
            .expect("oversized transaction should retry in bounded commits");
        assert_eq!(alloc.data_tree.root.level, 1);
        for offset in [0, 1_500, 2_999] {
            assert!(
                BtreeIter::new(&alloc.data_tree, Bpos { inode: offset, offset: 0, snapshot: 0 })
                    .peek()
                    .is_some(),
                "missing inode {}",
                offset
            );
        }
    }

    #[test]
    fn oversized_update_fails_before_partial_batch_commit() {
        let stub = Arc::new(BchVol::new());
        let dev = Arc::new(BchDev::with_size(stub, 1 << 20));
        let vol = BchVol::with_dev(dev.clone(), Vec::new());
        let mut alloc = Allocator::new(&vol, &dev);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let result = runtime.block_on(async {
            let mut tx = BtreeTrans::new(&vol);
            for inode in 0..513 {
                let iter = tx.iter(
                    &alloc,
                    BTREE_ID_DATA_INDEX,
                    Bpos { inode, offset: 0, snapshot: 0 },
                    true,
                );
                let payload = if inode == 512 {
                    vec![0; u16::MAX as usize - 20]
                } else {
                    vec![inode as u8]
                };
                tx.update_from_iter(&iter, 0, payload);
            }
            tx.commit(&mut alloc).await
        });
        assert!(result.is_err());
        assert_eq!(alloc.data_tree.total_key_count(), 0);
    }

    #[test]
    fn transaction_flushes_wal_before_btree_mutation() {
        let buckets = vec![4096, 4096 + crate::journal::JSET_BLOCK_SIZE as u64 * 8];
        let stub = Arc::new(BchVol::new());
        let dev = Arc::new(BchDev::with_size(
            stub,
            buckets[1] + crate::journal::JSET_BLOCK_SIZE as u64 * 8,
        ));
        let vol = BchVol::with_dev(dev.clone(), buckets);
        let mut alloc = Allocator::new(&vol, &dev);
        let pos = Bpos { inode: 91, offset: 0, snapshot: 0 };
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let mut tx = BtreeTrans::new(&vol);
            let iter = tx.iter(&alloc, BTREE_ID_DATA_INDEX, pos, true);
            tx.update_from_iter(&iter, 0, vec![9, 1, 4]);
            tx.commit(&mut alloc).await.unwrap();

            let mut info = crate::journal::JournalStartInfo::default();
            let jsets = vol.journal_ref().bch2_journal_read(&mut info).await.unwrap();
            assert_eq!(jsets.len(), 1);
            assert_eq!(jsets[0].1.header.flags & crate::journal::JSET_CSUM_TYPE_MASK,
                crate::journal::CSUM_TYPE_CRC32C as u32);
        });
    }

    #[test]
    fn transaction_delete_removes_existing_key() {
        let stub = Arc::new(BchVol::new());
        let dev = Arc::new(BchDev::with_size(stub, 1 << 20));
        let vol = BchVol::with_dev(dev.clone(), Vec::new());
        let mut alloc = Allocator::new(&vol, &dev);
        let pos = Bpos {
            inode: 42,
            offset: 0,
            snapshot: 0,
        };
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime
            .block_on(async {
                let mut insert = BtreeTrans::new(&vol);
                let iter = insert.iter(&alloc, BTREE_ID_DATA_INDEX, pos, true);
                insert.update_from_iter(&iter, 0, vec![7, 8, 9]);
                insert.commit(&mut alloc).await?;

                let mut delete = BtreeTrans::new(&vol);
                let iter = delete.iter(&alloc, BTREE_ID_DATA_INDEX, pos, true);
                delete.update_from_iter(&iter, 1, Vec::new());
                delete.commit(&mut alloc).await
            })
            .unwrap();
        assert_eq!(alloc.data_tree.total_key_count(), 0);
        assert!(BtreeIter::new(&alloc.data_tree, pos).peek().is_none());
    }

    // ── 测试帮助：通过事务填充数据 ──

    fn insert_one(alloc: &mut Allocator, vol: &Arc<BchVol>, inode: u64, payload: Vec<u8>) {
        let pos = Bpos { inode, offset: 0, snapshot: 0 };
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let mut tx = BtreeTrans::new(vol);
            let iter = tx.iter(alloc, BTREE_ID_DATA_INDEX, pos, true);
            tx.update_from_iter(&iter, 0, payload);
            tx.commit(alloc).await.unwrap();
        });
    }

    fn insert_many(alloc: &mut Allocator, vol: &Arc<BchVol>, entries: &[(u64, Vec<u8>)]) {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let mut tx = BtreeTrans::new(vol);
            for (inode, payload) in entries {
                let pos = Bpos { inode: *inode, offset: 0, snapshot: 0 };
                let iter = tx.iter(alloc, BTREE_ID_DATA_INDEX, pos, true);
                tx.update_from_iter(&iter, 0, payload.clone());
            }
            tx.commit(alloc).await.unwrap();
        });
    }

    // ═══════════════════════════════════════════════════════════════════
    // Btree 迭代器测试（通过事务构建数据）
    // ═══════════════════════════════════════════════════════════════════

    #[test]
    fn test_single_entry() {
        let stub = Arc::new(BchVol::new());
        let dev = Arc::new(BchDev::with_size(stub, 1 << 20));
        let vol = BchVol::with_dev(dev.clone(), Vec::new());
        let mut alloc = Allocator::new(&vol, &dev);
        insert_one(&mut alloc, &vol, 100, vec![42]);

        let mut iter = BtreeIter::new(
            &alloc.data_tree,
            Bpos { inode: 100, offset: 0, snapshot: 0 },
        );
        let entry = iter.next().expect("should find entry");
        assert_eq!(entry.payload, vec![42]);

        let mut iter_99 = BtreeIter::new(
            &alloc.data_tree,
            Bpos { inode: 99, offset: 0, snapshot: 0 },
        );
        assert_eq!(
            iter_99.next(),
            Some(BtreeEntry {
                btree_type: BTREE_ID_DATA_INDEX.0,
                level: 0,
                entry_type: 0,
                pos: Bpos { inode: 100, offset: 0, snapshot: 0 },
                payload: vec![42],
            })
        );

        let mut iter_101 = BtreeIter::new(
            &alloc.data_tree,
            Bpos { inode: 101, offset: 0, snapshot: 0 },
        );
        assert!(iter_101.next().is_none());
    }

    #[test]
    fn test_forward_iteration() {
        let stub = Arc::new(BchVol::new());
        let dev = Arc::new(BchDev::with_size(stub, 1 << 20));
        let vol = BchVol::with_dev(dev.clone(), Vec::new());
        let mut alloc = Allocator::new(&vol, &dev);
        let entries: Vec<_> = (0..10).map(|i| (i * 100, vec![i as u8])).collect();
        insert_many(&mut alloc, &vol, &entries);

        let mut iter = BtreeIter::new(
            &alloc.data_tree,
            Bpos { inode: 0, offset: 0, snapshot: 0 },
        );
        for i in 0..10 {
            let entry = iter.next().expect("should have entry");
            assert_eq!(entry.pos.inode, i * 100);
            assert_eq!(entry.payload, vec![i as u8]);
        }
        assert!(iter.next().is_none());
    }

    #[test]
    fn test_iter_from_mid() {
        let stub = Arc::new(BchVol::new());
        let dev = Arc::new(BchDev::with_size(stub, 1 << 20));
        let vol = BchVol::with_dev(dev.clone(), Vec::new());
        let mut alloc = Allocator::new(&vol, &dev);
        let entries: Vec<_> = (0..10).map(|i| (i * 100, vec![i as u8])).collect();
        insert_many(&mut alloc, &vol, &entries);

        let mut iter = BtreeIter::new(
            &alloc.data_tree,
            Bpos { inode: 250, offset: 0, snapshot: 0 },
        );
        let entry = iter.next().expect("should find >= 250");
        assert_eq!(entry.pos.inode, 300);
    }

    #[test]
    fn test_prev_iteration() {
        let stub = Arc::new(BchVol::new());
        let dev = Arc::new(BchDev::with_size(stub, 1 << 20));
        let vol = BchVol::with_dev(dev.clone(), Vec::new());
        let mut alloc = Allocator::new(&vol, &dev);
        let entries: Vec<_> = (0..5).map(|i| (i * 100, vec![i as u8])).collect();
        insert_many(&mut alloc, &vol, &entries);

        let mut iter = BtreeIter::new(
            &alloc.data_tree,
            Bpos { inode: 400, offset: 0, snapshot: 0 },
        );
        let entry = iter.next().expect("should find >= 400");
        assert_eq!(entry.pos.inode, 400);

        let prev = iter.prev();
        assert!(prev.is_some());
        assert_eq!(prev.unwrap().pos.inode, 300);
    }

    #[test]
    fn test_peek_no_advance() {
        let stub = Arc::new(BchVol::new());
        let dev = Arc::new(BchDev::with_size(stub, 1 << 20));
        let vol = BchVol::with_dev(dev.clone(), Vec::new());
        let mut alloc = Allocator::new(&vol, &dev);
        insert_one(&mut alloc, &vol, 100, vec![1]);

        let iter = BtreeIter::new(
            &alloc.data_tree,
            Bpos { inode: 100, offset: 0, snapshot: 0 },
        );
        let p1 = iter.peek();
        let p2 = iter.peek();
        assert!(p1.is_some());
        assert!(p2.is_some());
    }

    #[test]
    fn test_seek_reset() {
        let stub = Arc::new(BchVol::new());
        let dev = Arc::new(BchDev::with_size(stub, 1 << 20));
        let vol = BchVol::with_dev(dev.clone(), Vec::new());
        let mut alloc = Allocator::new(&vol, &dev);
        let entries: Vec<_> = (0..10).map(|i| (i * 100, vec![i as u8])).collect();
        insert_many(&mut alloc, &vol, &entries);

        let mut iter = BtreeIter::new(
            &alloc.data_tree,
            Bpos { inode: 0, offset: 0, snapshot: 0 },
        );
        iter.next();
        iter.seek(Bpos { inode: 500, offset: 0, snapshot: 0 });
        let entry = iter.next().expect("should find after seek");
        assert_eq!(entry.pos.inode, 500);
    }

    #[test]
    fn test_peek_upto() {
        let stub = Arc::new(BchVol::new());
        let dev = Arc::new(BchDev::with_size(stub, 1 << 20));
        let vol = BchVol::with_dev(dev.clone(), Vec::new());
        let mut alloc = Allocator::new(&vol, &dev);
        let entries: Vec<_> = (0..10).map(|i| (i * 100, vec![i as u8])).collect();
        insert_many(&mut alloc, &vol, &entries);

        let iter = BtreeIter::new(
            &alloc.data_tree,
            Bpos { inode: 200, offset: 0, snapshot: 0 },
        );
        let entry = iter.peek_upto(Bpos { inode: 500, offset: 0, snapshot: 0 });
        assert!(entry.is_some());
        assert_eq!(entry.unwrap().pos.inode, 200);

        let entry = iter.peek_upto(Bpos { inode: 300, offset: 0, snapshot: 0 });
        assert!(entry.is_some());
        assert_eq!(entry.unwrap().pos.inode, 200);

        let entry = iter.peek_upto(Bpos { inode: 200, offset: 0, snapshot: 0 });
        assert!(entry.is_none());

        let entry = iter.peek_upto(Bpos { inode: 50, offset: 0, snapshot: 0 });
        assert!(entry.is_none());
    }

    #[test]
    fn test_advance_between_entries() {
        let stub = Arc::new(BchVol::new());
        let dev = Arc::new(BchDev::with_size(stub, 1 << 20));
        let vol = BchVol::with_dev(dev.clone(), Vec::new());
        let mut alloc = Allocator::new(&vol, &dev);
        let entries: Vec<_> = (0..5).map(|i| (i * 100, vec![i as u8])).collect();
        insert_many(&mut alloc, &vol, &entries);

        let mut iter = BtreeIter::new(
            &alloc.data_tree,
            Bpos { inode: 0, offset: 0, snapshot: 0 },
        );
        let entry = iter.next().unwrap();
        assert_eq!(entry.pos.inode, 0);

        assert!(iter.advance());
        let entry = iter.next().unwrap();
        assert_eq!(entry.pos.inode, 100);

        assert!(iter.advance());
        let entry = iter.next().unwrap();
        assert_eq!(entry.pos.inode, 200);
    }

    #[test]
    fn test_advance_then_rewind_cycle() {
        let stub = Arc::new(BchVol::new());
        let dev = Arc::new(BchDev::with_size(stub, 1 << 20));
        let vol = BchVol::with_dev(dev.clone(), Vec::new());
        let mut alloc = Allocator::new(&vol, &dev);
        insert_one(&mut alloc, &vol, 100, vec![1]);
        insert_one(&mut alloc, &vol, 200, vec![2]);

        let mut iter = BtreeIter::new(
            &alloc.data_tree,
            Bpos { inode: 100, offset: 0, snapshot: 0 },
        );
        let entry = iter.next().unwrap();
        assert_eq!(entry.pos.inode, 100);

        assert!(iter.advance());
        let entry = iter.peek_upto(Bpos { inode: 300, offset: 0, snapshot: 0 });
        assert!(entry.is_some());
        assert_eq!(entry.unwrap().pos.inode, 200);

        assert!(iter.rewind());
        let entry = iter.peek_upto(Bpos { inode: 150, offset: 0, snapshot: 0 });
        assert!(entry.is_some());
        assert_eq!(entry.unwrap().pos.inode, 100);
    }

    #[test]
    fn test_traverse() {
        let stub = Arc::new(BchVol::new());
        let dev = Arc::new(BchDev::with_size(stub, 1 << 20));
        let vol = BchVol::with_dev(dev.clone(), Vec::new());
        let mut alloc = Allocator::new(&vol, &dev);
        insert_one(&mut alloc, &vol, 100, vec![1]);
        insert_one(&mut alloc, &vol, 200, vec![2]);

        let mut iter = BtreeIter::new(
            &alloc.data_tree,
            Bpos { inode: 100, offset: 0, snapshot: 0 },
        );
        let entry = iter.next().unwrap();
        assert_eq!(entry.pos.inode, 100);

        iter.set_pos(Bpos { inode: 200, offset: 0, snapshot: 0 });
        iter.traverse();
        let entry = iter.next().unwrap();
        assert_eq!(entry.pos.inode, 200);
    }

    #[test]
    fn test_set_pos() {
        let stub = Arc::new(BchVol::new());
        let dev = Arc::new(BchDev::with_size(stub, 1 << 20));
        let vol = BchVol::with_dev(dev.clone(), Vec::new());
        let mut alloc = Allocator::new(&vol, &dev);
        insert_one(&mut alloc, &vol, 100, vec![1]);
        insert_one(&mut alloc, &vol, 200, vec![2]);

        let mut iter = BtreeIter::new(
            &alloc.data_tree,
            Bpos { inode: 0, offset: 0, snapshot: 0 },
        );
        iter.next();
        iter.set_pos(Bpos { inode: 200, offset: 0, snapshot: 0 });
        let entry = iter.next().expect("should find entry at 200");
        assert_eq!(entry.pos.inode, 200);
    }

    #[test]
    fn test_advance_then_peek_upto() {
        let stub = Arc::new(BchVol::new());
        let dev = Arc::new(BchDev::with_size(stub, 1 << 20));
        let vol = BchVol::with_dev(dev.clone(), Vec::new());
        let mut alloc = Allocator::new(&vol, &dev);
        let entries: Vec<_> = (0..10).map(|i| (i * 100, vec![i as u8])).collect();
        insert_many(&mut alloc, &vol, &entries);

        let mut iter = BtreeIter::new(
            &alloc.data_tree,
            Bpos { inode: 100, offset: 0, snapshot: 0 },
        );
        let entry = iter.next().unwrap();
        assert_eq!(entry.pos.inode, 100);

        iter.advance();
        let entry = iter.peek_upto(Bpos { inode: 500, offset: 0, snapshot: 0 });
        assert!(entry.is_some());
        assert_eq!(entry.unwrap().pos.inode, 200);

        let entry = iter.next().unwrap();
        assert_eq!(entry.pos.inode, 200);
    }

    #[test]
    fn test_from_impl() {
        let stub = Arc::new(BchVol::new());
        let dev = Arc::new(BchDev::with_size(stub, 1 << 20));
        let vol = BchVol::with_dev(dev.clone(), Vec::new());
        let mut alloc = Allocator::new(&vol, &dev);
        let entries: Vec<_> = (0..3).map(|i| (i * 100, vec![i as u8])).collect();
        insert_many(&mut alloc, &vol, &entries);

        let iter: BtreeIter = (&alloc.data_tree).into();
        let count = iter.count();
        assert_eq!(count, 3);
    }

    /// 与 persisted_internal_root_restores_levels_and_child_data 不同，
    /// 此测试通过事务提交大量条目触发自动 split，验证多级树构造正确。
    #[test]
    fn test_split_creates_multi_level_tree() {
        let stub = Arc::new(BchVol::new());
        let dev = Arc::new(BchDev::with_size(stub, 1 << 22));
        let vol = BchVol::with_dev(dev.clone(), Vec::new());
        let mut alloc = Allocator::new(&vol, &dev);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let mut tx = BtreeTrans::new(&vol);
            for inode in 0..3_000u64 {
                let pos = Bpos { inode, offset: 0, snapshot: 0 };
                let iter = tx.iter(&alloc, BTREE_ID_DATA_INDEX, pos, true);
                tx.update_from_iter(&iter, 0, vec![(inode & 0xff) as u8]);
            }
            tx.commit(&mut alloc).await.unwrap();
        });
        assert_eq!(alloc.data_tree.root.level, 1);
        for inode in [0, 1_500, 2_999] {
            let mut iter = BtreeIter::new(
                &alloc.data_tree,
                Bpos { inode, offset: 0, snapshot: 0 },
            );
            assert_eq!(
                iter.next().map(|entry| entry.pos.inode),
                Some(inode)
            );
        }
    }

    /// 验证在事务中 delete (entry_type=1) 后的树状态
    #[test]
    fn test_delete_entry_removes_key() {
        let stub = Arc::new(BchVol::new());
        let dev = Arc::new(BchDev::with_size(stub, 1 << 20));
        let vol = BchVol::with_dev(dev.clone(), Vec::new());
        let mut alloc = Allocator::new(&vol, &dev);
        let pos = Bpos { inode: 100, offset: 0, snapshot: 0 };
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let mut insert = BtreeTrans::new(&vol);
            let iter = insert.iter(&alloc, BTREE_ID_DATA_INDEX, pos, true);
            insert.update_from_iter(&iter, 0, vec![42]);
            insert.commit(&mut alloc).await.unwrap();

            let mut delete = BtreeTrans::new(&vol);
            let iter = delete.iter(&alloc, BTREE_ID_DATA_INDEX, pos, true);
            delete.update_from_iter(&iter, 1, Vec::new());
            delete.commit(&mut alloc).await.unwrap();
        });
        assert!(BtreeIter::new(&alloc.data_tree, pos).peek().is_none());
    }

    #[test]
    fn test_next_entry_prev_entry() {
        let stub = Arc::new(BchVol::new());
        let dev = Arc::new(BchDev::with_size(stub, 1 << 20));
        let vol = BchVol::with_dev(dev.clone(), Vec::new());
        let mut alloc = Allocator::new(&vol, &dev);
        let entries: Vec<_> = (0..5).map(|i| (i * 100, vec![i as u8])).collect();
        insert_many(&mut alloc, &vol, &entries);

        let next = alloc.data_tree.root.next_entry(&Bpos { inode: 100, offset: 0, snapshot: 0 });
        assert!(next.is_some());
        assert_eq!(next.unwrap().pos.inode, 200);

        let prev = alloc.data_tree.root.prev_entry(&Bpos { inode: 300, offset: 0, snapshot: 0 });
        assert!(prev.is_some());
        assert_eq!(prev.unwrap().pos.inode, 200);
    }

    #[test]
    fn test_first_last_entry() {
        let stub = Arc::new(BchVol::new());
        let dev = Arc::new(BchDev::with_size(stub, 1 << 20));
        let vol = BchVol::with_dev(dev.clone(), Vec::new());
        let mut alloc = Allocator::new(&vol, &dev);
        let entries: Vec<_> = (0..5).map(|i| (i * 100, vec![i as u8])).collect();
        insert_many(&mut alloc, &vol, &entries);

        let first = alloc.data_tree.root.first_entry();
        assert!(first.is_some());
        assert_eq!(first.unwrap().pos.inode, 0);

        let last = alloc.data_tree.root.last_entry();
        assert!(last.is_some());
        assert_eq!(last.unwrap().pos.inode, 400);
    }
}
