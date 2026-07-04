use crate::btree::gc::bch2_check_alloc_info;
#[cfg(test)]
use crate::alloc::btree::serialize_alloc_entry;
#[cfg(test)]
use crate::alloc::{BchAllocEntry, BchAllocator, BchDataType};
#[cfg(test)]
use crate::btree::{Bpos, BtreeEntry, BtreeId, KeyType};
use crate::recovery::RecoveryState;
use crate::types::StorageError;

/// Pass: alloc-info 一致性检查（对应 bcachefs `bch2_check_alloc_info()`）
pub async fn run(state: &mut RecoveryState) -> Result<(), StorageError> {
    let allocator = unsafe { &*state.vol.allocator.get() };
    let discrepancies = bch2_check_alloc_info(&state.vol, allocator)?;
    if discrepancies.is_empty() {
        return Ok(());
    }

    Err(StorageError::InvalidData(format!(
        "alloc-info inconsistencies: {}",
        discrepancies.join("; ")
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alloc::alloc_freespace_pos;
    use crate::journal::Journal;
    use crate::storage::superblock::BchSb;
    use crate::types::BackendType;

    fn seed_alloc_state(vol: &crate::BchVol, allocator: &BchAllocator, skip_free: Option<u64>) {
        let ca = vol.primary_device_rcu_noerror().unwrap();
        allocator.for_each_bucket(&ca, |bucket_idx, bucket, _gen| {
            let alloc_entry = BchAllocEntry {
                journal_seq_nonempty: bucket.journal_seq_nonempty,
                journal_seq_empty: bucket.journal_seq_empty,
                stripe_refcount: 0,
                stripe_sectors: bucket.stripe_sectors,
                dirty_sectors: bucket.dirty_sectors,
                cached_sectors: bucket.cached_sectors,
                data_type: bucket.state as u8,
                flags: 0,
                gen: 0,
                oldest_gen: bucket.oldest_gen,
                stripe_redundancy_obsolete: 0,
                io_time: [0; 2],
                nr_external_backpointers: 0,
                pad: 0,
            };
            let alloc_pos = Bpos::new(0, bucket_idx, 0);
            let alloc_value = serialize_alloc_entry(&alloc_entry);
            vol.btree(BtreeId::Alloc)
                .bch2_btree_bset_insert_key_wrapper(
                    BtreeEntry::raw(alloc_pos, KeyType::Normal, alloc_value),
                    0,
                );

            if bucket.state == BchDataType::Free && skip_free != Some(bucket_idx) {
                let freespace_pos =
                    alloc_freespace_pos(ca.dev_idx, bucket_idx, 0, bucket.oldest_gen);
                vol.btree(BtreeId::Freespace)
                    .bch2_btree_bset_insert_key_wrapper(
                        BtreeEntry::raw(freespace_pos, KeyType::Normal, vec![]),
                        0,
                    );
            }
        });
    }

    fn make_state(vol: crate::BchVol) -> RecoveryState {
        let sb = BchSb::with_volume_info(
            "test".into(),
            1,
            "default".into(),
            4096,
            1024 * 1024,
            BackendType::Nfs,
        );
        RecoveryState::new(Box::new(vol), Journal::new(vec![1]), sb)
    }

    #[tokio::test]
    async fn test_check_alloc_info_passes_for_consistent_state() {
        let vol = crate::BchVol::test_trees();
        let ca = vol.primary_device_rcu_noerror().unwrap();
        let allocator = unsafe { &*vol.allocator.get() };
        allocator.for_each_bucket_mut(&ca, |bucket_idx, bucket, _gen| {
            if bucket_idx == 1 {
                bucket.state = BchDataType::User;
                bucket.dirty_sectors = crate::alloc::SECTORS_PER_BLOCK as u32;
            }
        });

        seed_alloc_state(&vol, allocator, None);

        let mut state = make_state(vol);
        run(&mut state).await.unwrap();
    }

    #[tokio::test]
    async fn test_check_alloc_info_fails_when_free_entry_missing() {
        let vol = crate::BchVol::test_trees();
        let ca = vol.primary_device_rcu_noerror().unwrap();
        let allocator = unsafe { &*vol.allocator.get() };
        allocator.for_each_bucket_mut(&ca, |bucket_idx, bucket, _gen| {
            if bucket_idx == 1 {
                bucket.state = BchDataType::User;
                bucket.dirty_sectors = crate::alloc::SECTORS_PER_BLOCK as u32;
            }
        });

        seed_alloc_state(&vol, allocator, Some(0));

        let mut state = make_state(vol);
        let err = run(&mut state).await.unwrap_err();
        match err {
            StorageError::InvalidData(msg) => {
                assert!(msg.contains("missing freespace entry"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_check_alloc_info_fails_on_stale_freespace_entry() {
        let vol = crate::BchVol::test_trees();
        let ca = vol.primary_device_rcu_noerror().unwrap();
        vol.superblock_mut()
            .member_mut(ca.dev_idx)
            .unwrap()
            .nbuckets = 2;
        crate::alloc::bch2_dev_buckets_resize(&vol, &ca, 2).unwrap();
        let allocator = unsafe { &*vol.allocator.get() };
        allocator.for_each_bucket_mut(&ca, |bucket_idx, bucket, _gen| {
            if bucket_idx == 1 {
                bucket.state = BchDataType::User;
                bucket.dirty_sectors = crate::alloc::SECTORS_PER_BLOCK as u32;
            }
        });

        seed_alloc_state(&vol, allocator, None);
        vol.btree(BtreeId::Freespace)
            .bch2_btree_bset_insert_key_wrapper(
                BtreeEntry::raw(alloc_freespace_pos(0, 1, 16, 0), KeyType::Normal, vec![]),
                0,
            );

        let mut state = make_state(vol);
        let err = run(&mut state).await.unwrap_err();
        match err {
            StorageError::InvalidData(msg) => {
                assert!(msg.contains("stale freespace entry"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }
}
