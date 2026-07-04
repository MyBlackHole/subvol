use crate::btree::key::Bpos;
use crate::btree::transaction::BtreeTrans;
use crate::btree::tree::BtreeIter;
use crate::btree::types::BTREE_ID_DATA_INDEX;
use crate::data::extents_format::{calc_csum, ExtentEntry};
use crate::engine::Allocator;
use crate::types::StorageError;

impl Allocator {
    pub async fn write_extent(
        &mut self,
        inode: u64,
        offset: u64,
        data: &[u8],
    ) -> Result<(), StorageError> {
        let len = data.len() as u64;
        let key_pos = Bpos {
            inode,
            offset,
            snapshot: 0,
        };
        let old_extent = BtreeIter::new(&self.data_tree, key_pos)
            .peek()
            .and_then(|entry| ExtentEntry::from_bytes(&entry.payload));
        let mut tx = BtreeTrans::new(&self.vol);
        let (phys_offset, _alloc_len) = self.allocate_in_trans(&mut tx, len)?;
        let csum = calc_csum(data);
        self.dev.write_at(phys_offset, data).await?;

        let mut extent = ExtentEntry::new();
        extent.add_ptr(0, phys_offset / 512, len as u32, csum);

        {
            let iter = tx.iter(self, BTREE_ID_DATA_INDEX, key_pos, true);
            tx.update_from_iter(&iter, 0, extent.to_bytes());
        }
        tx.commit(self).await?;

        // The new index is durable before the old extent is reclaimed.  A
        // failed reclaim therefore leaks space but cannot expose a reused
        // block through the live index.
        if let Some(old_extent) = old_extent {
            for ptr in old_extent.ptrs {
                self.free(ptr.block * 512, ptr.len as u64).await?;
            }
        }

        Ok(())
    }

    pub async fn read_extent(&self, inode: u64, offset: u64) -> Result<Vec<u8>, StorageError> {
        let key_pos = Bpos {
            inode,
            offset,
            snapshot: 0,
        };
        let mut tx = BtreeTrans::new(&self.vol);
        let iter = tx.iter(self, BTREE_ID_DATA_INDEX, key_pos, false);
        let entry = iter.peek().ok_or(StorageError::NotFound)?;

        let extent = ExtentEntry::from_bytes(&entry.payload)
            .ok_or(StorageError::Internal("bad extent entry".into()))?;

        let ptr = extent
            .ptrs
            .first()
            .ok_or(StorageError::Internal("extent has no pointers".into()))?;

        let phys_offset = ptr.block * 512;
        let data_len = ptr.len as usize;

        if data_len == 0 {
            return Ok(Vec::new());
        }

        let data = self.dev.read_at(phys_offset, data_len).await?;

        let actual_csum = calc_csum(&data);
        if actual_csum != ptr.csum {
            return Err(StorageError::Internal(format!(
                "checksum mismatch: expected={:#x} actual={:#x}",
                ptr.csum, actual_csum
            )));
        }

        Ok(data)
    }

    pub async fn create_inode(&mut self) -> Result<u64, StorageError> {
        let new_inode = self.next_inode;
        if new_inode == 0 || new_inode == u64::MAX {
            return Err(StorageError::Internal("inode space exhausted".into()));
        }
        self.next_inode += 1;
        crate::log_info!("create_inode: allocated inode={}", new_inode);
        Ok(new_inode)
    }

    pub async fn delete_inode(&mut self, inode: u64) -> Result<(), StorageError> {
        let entries: Vec<(Bpos, Vec<u8>)> = BtreeIter::new(&self.data_tree, Bpos::MIN)
            .filter(|e| e.pos.inode == inode)
            .map(|e| (e.pos, e.payload.clone()))
            .collect();

        if entries.is_empty() {
            return Err(StorageError::NotFound);
        }

        crate::log_info!("delete_inode: inode={} extents={}", inode, entries.len());

        for (pos, payload) in &entries {
            let mut tx = BtreeTrans::new(&self.vol);
            {
                let iter = tx.iter(self, BTREE_ID_DATA_INDEX, *pos, true);
                tx.update_from_iter(&iter, 1, vec![]);
            }
            tx.commit(self).await?;

            // Remove the index first.  If freeing the physical extent then
            // fails, the result is a leak rather than a live index pointing
            // at a block that can be reused by another write.
            if let Some(extent) = ExtentEntry::from_bytes(payload) {
                for ptr in &extent.ptrs {
                    let phys_offset = ptr.block * 512;
                    self.free(phys_offset, ptr.len as u64).await?;
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_device::BchDev;
    use crate::BchVol;
    use std::sync::Arc;

    #[test]
    fn extent_write_and_delete_keep_index_and_allocator_consistent() {
        let stub = Arc::new(BchVol::new());
        let dev = Arc::new(BchDev::with_size(stub, 1 << 20));
        let vol = BchVol::with_dev(dev.clone(), Vec::new());
        let mut alloc = Allocator::new(&vol, &dev);
        let runtime = tokio::runtime::Runtime::new().unwrap();

        runtime.block_on(async {
            alloc.init(dev.size(), &[]).await.unwrap();
            let payload = b"extent transaction";
            alloc.write_extent(7, 0, payload).await.unwrap();
            assert_eq!(alloc.read_extent(7, 0).await.unwrap(), payload);

            let replacement = b"replacement extent";
            alloc.write_extent(7, 0, replacement).await.unwrap();
            assert_eq!(alloc.read_extent(7, 0).await.unwrap(), replacement);

            alloc.delete_inode(7).await.unwrap();
            assert!(matches!(alloc.read_extent(7, 0).await, Err(StorageError::NotFound)));
        });
    }
}
