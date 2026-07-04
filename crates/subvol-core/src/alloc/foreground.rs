use crate::alloc::format::{AllocEntry, DataType, FreespaceEntry};
use crate::btree::key::{Bpos, BtreeEntry};
use crate::btree::transaction::BtreeTrans;
use crate::btree::tree::BtreeIter;
use crate::btree::types::{BTREE_ID_ALLOC, BTREE_ID_FREESPACE};
use crate::data::extents_format::BLOCK_SIZE;
use crate::engine::Allocator;
use crate::types::StorageError;

/// 通过事务读取 freespace 条目
fn read_freespace(alloc: &Allocator, pos: Bpos) -> Result<FreespaceEntry, StorageError> {
    let mut tx = BtreeTrans::new(&alloc.vol);
    let iter = tx.iter(alloc, BTREE_ID_FREESPACE, pos, false);
    let entry = iter.peek().ok_or(StorageError::NotFound)?;
    FreespaceEntry::from_bytes(&entry.payload)
        .ok_or_else(|| StorageError::Internal("bad freespace entry".into()))
}

/// 通过事务读取 alloc 条目
fn read_alloc(alloc: &Allocator, pos: Bpos) -> Result<AllocEntry, StorageError> {
    let mut tx = BtreeTrans::new(&alloc.vol);
    let iter = tx.iter(alloc, BTREE_ID_ALLOC, pos, false);
    let entry = iter.peek().ok_or(StorageError::NotFound)?;
    AllocEntry::from_bytes(&entry.payload)
        .ok_or_else(|| StorageError::Internal("bad alloc entry".into()))
}

impl Allocator {
    pub async fn init(
        &mut self,
        total_bytes: u64,
        reserved_blocks: &[u64],
    ) -> Result<(), StorageError> {
        let total_blocks = (total_bytes / BLOCK_SIZE) as u64;
        let used: std::collections::HashSet<u64> = reserved_blocks.iter().copied().collect();
        crate::log_info!(
            "alloc_init: total={} bytes blocks={} reserved={}",
            total_bytes,
            total_blocks,
            reserved_blocks.len()
        );

        // The alloc btree stores non-default (allocated) blocks only.  Free
        // space is represented by the freespace extents below; materializing
        // one "free" key for every device block needlessly exhausts a node
        // during format on larger devices.
        let mut used_blocks: Vec<u64> = used.iter().copied().filter(|b| *b < total_blocks).collect();
        used_blocks.sort_unstable();
        const BATCH: usize = 512;
        for chunk_start in (0..used_blocks.len()).step_by(BATCH) {
            let mut alloc_tx = BtreeTrans::new(&self.vol);
            let end = (chunk_start + BATCH).min(used_blocks.len());
            for &b in &used_blocks[chunk_start..end] {
                let pos = Bpos {
                    inode: 0,
                    offset: b,
                    snapshot: 0,
                };
                let entry = AllocEntry {
                    gen: 1,
                    data_type: DataType::Dirty as u8,
                    dirty_sectors: (BLOCK_SIZE / 512) as u32,
                    cached_sectors: 0,
                };
                let iter = alloc_tx.iter(self, BTREE_ID_ALLOC, pos, true);
                alloc_tx.update_from_iter(&iter, 0, entry.to_bytes());
            }
            alloc_tx.commit(self).await.map_err(|err| {
                StorageError::Internal(format!("alloc batch {}-{} failed: {}", chunk_start, end, err))
            })?;
            crate::log_verbose!(
                "alloc_init: 批次提交成功 end={} blocks={}",
                end,
                end - chunk_start
            );
        }

        let mut free_regions: Vec<(u64, u64)> = Vec::new();
        let mut start = 0u64;
        while start < total_blocks {
            if used.contains(&start) {
                start += 1;
                continue;
            }
            let mut end = start + 1;
            while end < total_blocks && !used.contains(&end) {
                end += 1;
            }
            let region_start = start * BLOCK_SIZE;
            let region_len = (end - start) * BLOCK_SIZE;
            free_regions.push((region_start, region_len));
            start = end;
        }

        let mut fs_tx = BtreeTrans::new(&self.vol);
        for (off, len) in &free_regions {
            let pos = Bpos {
                inode: 0,
                offset: *off,
                snapshot: 0,
            };
            let entry = FreespaceEntry::new(*len);
            let iter = fs_tx.iter(self, BTREE_ID_FREESPACE, pos, true);
            fs_tx.update_from_iter(&iter, 0, entry.to_bytes());
        }
        fs_tx.commit(self).await
    }

    pub async fn allocate(&mut self, size: u64) -> Result<(u64, u64), StorageError> {
        let aligned = round_up(size, BLOCK_SIZE);
        let need_blocks = (aligned / BLOCK_SIZE) as u32;

        let free_pos = self.find_freespace(aligned)?;
        crate::log_verbose!(
            "allocate: size={} aligned={} blocks={} free_pos=({},{},{})",
            size,
            aligned,
            need_blocks,
            free_pos.inode,
            free_pos.offset,
            free_pos.snapshot
        );

        let freespace = read_freespace(self, free_pos)?;

        // Keep the freespace reservation and alloc records in one btree
        // transaction.  The allocator path in bcachefs stages both index
        // updates under the caller's transaction; splitting them would make
        // a failed second commit expose a block as both free and allocated.
        let mut alloc_tx = BtreeTrans::new(&self.vol);
        {
            let iter = alloc_tx.iter(self, BTREE_ID_FREESPACE, free_pos, true);
            alloc_tx.update_from_iter(&iter, 1, vec![]);
        }

        let remain = freespace.len.saturating_sub(aligned);
        if remain >= BLOCK_SIZE {
            let remain_pos = Bpos {
                inode: 0,
                offset: free_pos.offset + aligned,
                snapshot: 0,
            };
            let remain_entry = FreespaceEntry::new(remain);
            {
                let iter = alloc_tx.iter(self, BTREE_ID_FREESPACE, remain_pos, true);
                alloc_tx.update_from_iter(&iter, 0, remain_entry.to_bytes());
            }
        }

        for b in 0..need_blocks {
            let block = free_pos.offset / BLOCK_SIZE + b as u64;
            let pos = Bpos {
                inode: 0,
                offset: block,
                snapshot: 0,
            };
            let entry = AllocEntry {
                gen: 1,
                data_type: DataType::Dirty as u8,
                dirty_sectors: (BLOCK_SIZE / 512) as u32,
                cached_sectors: 0,
            };
            {
                let iter = alloc_tx.iter(self, BTREE_ID_ALLOC, pos, true);
                alloc_tx.update_from_iter(&iter, 0, entry.to_bytes());
            }
        }
        alloc_tx.commit(self).await?;

        Ok((free_pos.offset, aligned))
    }

    /// 在现有事务中分配磁盘空间（同步，不创建新事务）
    ///
    /// 将分配操作（freespace 删除/拆分 + alloc 插入）添加到 `tx` 中，
    /// 由调用者统一提交。避免嵌套事务。
    pub fn allocate_in_trans(
        &mut self,
        tx: &mut BtreeTrans,
        size: u64,
    ) -> Result<(u64, u64), StorageError> {
        let aligned = round_up(size, BLOCK_SIZE);
        let need_blocks = (aligned / BLOCK_SIZE) as u32;

        let free_pos = self.find_freespace(aligned)?;
        crate::log_verbose!(
            "allocate_in_trans: size={} aligned={} blocks={} free_pos=({},{},{})",
            size,
            aligned,
            need_blocks,
            free_pos.inode,
            free_pos.offset,
            free_pos.snapshot
        );

        let freespace = read_freespace(self, free_pos)?;

        // 删除原 freespace 条目
        let f_iter = tx.iter(self, BTREE_ID_FREESPACE, free_pos, true);
        tx.update_from_iter(&f_iter, 1, vec![]);

        let remain = freespace.len.saturating_sub(aligned);
        if remain >= BLOCK_SIZE {
            let remain_pos = Bpos {
                inode: 0,
                offset: free_pos.offset + aligned,
                snapshot: 0,
            };
            let remain_entry = FreespaceEntry::new(remain);
            let r_iter = tx.iter(self, BTREE_ID_FREESPACE, remain_pos, true);
            tx.update_from_iter(&r_iter, 0, remain_entry.to_bytes());
        }

        for b in 0..need_blocks {
            let block = free_pos.offset / BLOCK_SIZE + b as u64;
            let pos = Bpos {
                inode: 0,
                offset: block,
                snapshot: 0,
            };
            let entry = AllocEntry {
                gen: 1,
                data_type: DataType::Dirty as u8,
                dirty_sectors: (BLOCK_SIZE / 512) as u32,
                cached_sectors: 0,
            };
            let a_iter = tx.iter(self, BTREE_ID_ALLOC, pos, true);
            tx.update_from_iter(&a_iter, 0, entry.to_bytes());
        }

        Ok((free_pos.offset, aligned))
    }

    pub async fn free(&mut self, offset: u64, size: u64) -> Result<(), StorageError> {
        let aligned = round_up(size, BLOCK_SIZE);
        let blocks = (aligned / BLOCK_SIZE) as u32;
        crate::log_verbose!(
            "free: off={} size={} aligned={} blocks={}",
            offset,
            size,
            aligned,
            blocks
        );

        let free_pos = Bpos {
            inode: 0,
            offset,
            snapshot: 0,
        };
        let prev = self.find_prev_freespace(free_pos);
        let next_pos = Bpos {
            inode: 0,
            offset: offset.saturating_add(aligned),
            snapshot: 0,
        };
        let next = read_freespace(self, next_pos).ok();
        let mut merged_len = aligned;
        let merged_offset = if let Some(prev_entry) = &prev {
            let prev_fs = FreespaceEntry::from_bytes(&prev_entry.payload)
                .ok_or(StorageError::Internal("bad freespace entry".into()))?;
            if prev_entry.pos.offset.saturating_add(prev_fs.len) == offset {
                merged_len = merged_len.saturating_add(prev_fs.len);
                prev_entry.pos.offset
            } else {
                offset
            }
        } else {
            offset
        };
        if let Some(next_fs) = &next {
            merged_len = merged_len.saturating_add(next_fs.len);
        }

        let mut free_tx = BtreeTrans::new(&self.vol);
        for b in 0..blocks {
            let block = offset / BLOCK_SIZE + b as u64;
            let pos = Bpos {
                inode: 0,
                offset: block,
                snapshot: 0,
            };
            let entry = AllocEntry::free();
            {
                let iter = free_tx.iter(self, BTREE_ID_ALLOC, pos, true);
                free_tx.update_from_iter(&iter, 0, entry.to_bytes());
            }
        }
        if let Some(prev_entry) = prev {
            let prev_fs = FreespaceEntry::from_bytes(&prev_entry.payload)
                .ok_or(StorageError::Internal("bad freespace entry".into()))?;
            if prev_entry.pos.offset.saturating_add(prev_fs.len) == offset {
                let iter = free_tx.iter(self, BTREE_ID_FREESPACE, prev_entry.pos, true);
                free_tx.update_from_iter(&iter, 1, vec![]);
            }
        }
        if next.is_some() {
            let iter = free_tx.iter(self, BTREE_ID_FREESPACE, next_pos, true);
            free_tx.update_from_iter(&iter, 1, vec![]);
        }
        let entry = FreespaceEntry::new(merged_len);
        let merged_pos = Bpos {
            inode: 0,
            offset: merged_offset,
            snapshot: 0,
        };
        {
            let iter = free_tx.iter(self, BTREE_ID_FREESPACE, merged_pos, true);
            free_tx.update_from_iter(&iter, 0, entry.to_bytes());
        }
        free_tx.commit(self).await
    }
}

impl Allocator {
    fn find_freespace(&self, size: u64) -> Result<Bpos, StorageError> {
        crate::log_verbose!("find_freespace: need={}", size);
        for entry in BtreeIter::new(&self.freespace_tree, Bpos::MIN) {
            let fs = FreespaceEntry::from_bytes(&entry.payload)
                .ok_or(StorageError::Internal("bad freespace entry".into()))?;
            if fs.len >= size {
                return Ok(entry.pos);
            }
        }
        Err(StorageError::NotFound)
    }

    fn find_prev_freespace(&self, pos: Bpos) -> Option<BtreeEntry> {
        let mut prev: Option<BtreeEntry> = None;
        for entry in BtreeIter::new(&self.freespace_tree, Bpos::MIN) {
            if entry.pos.offset < pos.offset {
                match &prev {
                    None => prev = Some(entry.clone()),
                    Some(p) if entry.pos.offset > p.pos.offset => {
                        prev = Some(entry.clone());
                    }
                    _ => {}
                }
            }
        }
        prev
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
    use crate::block_device::BchDev;
    use crate::bch_vol::BchVol;
    use crate::btree::key::Bpos;
    use crate::btree::types::BTREE_ID_FREESPACE;
    use std::sync::Arc;

    #[test]
    fn allocate_updates_freespace_and_alloc_atomically() {
        let stub = Arc::new(BchVol::new());
        let dev = Arc::new(BchDev::with_size(stub, 1 << 20));
        let vol = BchVol::with_dev(dev.clone(), Vec::new());
        let mut alloc = Allocator::new(&vol, &dev);
        let free_pos = Bpos { inode: 0, offset: 0, snapshot: 0 };

        let runtime = tokio::runtime::Runtime::new().unwrap();
        let mut setup = BtreeTrans::new(&vol);
        let iter = setup.iter(&alloc, BTREE_ID_FREESPACE, free_pos, true);
        setup.update_from_iter(
            &iter,
            0,
            FreespaceEntry::new(BLOCK_SIZE * 4).to_bytes(),
        );
        runtime.block_on(setup.commit(&mut alloc)).unwrap();
        let (offset, size) = runtime.block_on(alloc.allocate(BLOCK_SIZE * 2)).unwrap();
        assert_eq!((offset, size), (0, BLOCK_SIZE * 2));

        let remainder = BtreeIter::new(
            &alloc.freespace_tree,
            Bpos { inode: 0, offset: BLOCK_SIZE * 2, snapshot: 0 },
        )
        .peek()
        .expect("remaining free extent");
        assert_eq!(FreespaceEntry::from_bytes(&remainder.payload).unwrap().len, BLOCK_SIZE * 2);

        for block in 0..2 {
            let entry = BtreeIter::new(
                &alloc.alloc_tree,
                Bpos { inode: 0, offset: block, snapshot: 0 },
            )
            .peek()
            .expect("allocated block entry");
            assert!(!AllocEntry::from_bytes(&entry.payload).unwrap().is_free());
        }
    }
}
