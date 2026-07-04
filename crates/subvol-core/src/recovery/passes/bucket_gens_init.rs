use crate::alloc::{BchAllocator, BchBucketGens, BUCKET_GENS_PER_KEY};
use crate::btree::{Bpos, BtreeEntry, BtreeId, KeyType};
use crate::recovery::RecoveryState;
use crate::types::StorageError;
use std::collections::HashMap;

/// Pass: bucket_gen 初始化（对应 bcachefs `bch2_bucket_gen_init()`）
///
/// 扫描 allocator 的所有 bucket，按 `(group, bucket_idx / 256)` 聚合，
/// 为每个 chunk 写入一个 `bucket_gen` 记录。
///
/// # 幂等性
/// 该 pass 每次运行都会先重置 `BucketGens` btree，再根据 allocator 状态
/// 重新生成内容，因此可重复执行且不会累积旧 key。
pub async fn run(state: &mut RecoveryState) -> Result<(), StorageError> {
    state.vol.btree(BtreeId::BucketGens).clear();
    let allocator = unsafe { &*state.vol.allocator.get() };
    bch2_bucket_gen_init(&state.vol, allocator)
}

/// 核心实现：将 allocator 的 bucket 版本写入 bucket_gen btree。
pub(crate) fn bch2_bucket_gen_init(
    vol: &crate::BchVol,
    allocator: &BchAllocator,
) -> Result<(), StorageError> {
    let mut grouped: HashMap<(u8, u64), BchBucketGens> = HashMap::new();

    for dev_idx in vol.device_registry.dev_indices() {
        let Some(ca) = vol.device_registry.resolve_bch_dev(dev_idx) else {
            continue;
        };
        allocator.for_each_bucket(&ca, |bucket_idx, _bucket, gen| {
            let chunk_idx = bucket_idx / BUCKET_GENS_PER_KEY as u64;
            let slot = (bucket_idx % BUCKET_GENS_PER_KEY as u64) as usize;
            grouped
                .entry((dev_idx, chunk_idx))
                .or_insert_with(BchBucketGens::new)
                .set(slot, *gen);
        });
    }

    for ((dev, chunk_idx), gens) in grouped {
        let pos = Bpos::new(dev as u64, chunk_idx, 0);
        let bytes = bincode::serialize(&gens)
            .map_err(|e| StorageError::InvalidData(format!("serialize bucket gens: {e}")))?;
        vol.btree(BtreeId::BucketGens)
            .bch2_btree_bset_insert_key_wrapper(BtreeEntry::raw(pos, KeyType::Normal, bytes), 0);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::Journal;
    use crate::recovery::RecoveryState;
    use crate::storage::superblock::BchSb;
    use crate::types::BackendType;

    fn make_state() -> RecoveryState {
        let sb = BchSb::with_volume_info(
            "test".into(),
            1,
            "default".into(),
            4096,
            1024 * 1024,
            BackendType::Nfs,
        );
        RecoveryState::new(
            Box::new(crate::BchVol::test_trees()),
            Journal::new(vec![1]),
            sb,
        )
    }

    #[test]
    fn test_bucket_gen_init_writes_chunked_entries() {
        let state = make_state();
        let ca = state.vol.primary_device_rcu_noerror().unwrap();
        state
            .vol
            .superblock_mut()
            .member_mut(ca.dev_idx)
            .unwrap()
            .nbuckets = 512;
        crate::alloc::bch2_dev_buckets_resize(&state.vol, &ca, 512).unwrap();
        let allocator = unsafe { &*state.vol.allocator.get() };
        allocator.for_each_bucket_mut(&ca, |bucket_idx, _bucket, gen| match bucket_idx {
            0 => *gen = 1,
            255 => *gen = 7,
            256 => *gen = 3,
            511 => *gen = 9,
            _ => {}
        });

        bch2_bucket_gen_init(&state.vol, allocator).unwrap();

        let first = state
            .vol
            .get_entry_raw(BtreeId::BucketGens, Bpos::new(0, 0, 0))
            .expect("chunk 0 should exist");
        let second = state
            .vol
            .get_entry_raw(BtreeId::BucketGens, Bpos::new(0, 1, 0))
            .expect("chunk 1 should exist");

        let first_gen: BchBucketGens = bincode::deserialize(&first.value.to_bytes()).unwrap();
        let second_gen: BchBucketGens = bincode::deserialize(&second.value.to_bytes()).unwrap();

        assert_eq!(first_gen.gens[0], 1);
        assert_eq!(first_gen.gens[255], 7);
        assert_eq!(second_gen.gens[0], 3);
        assert_eq!(second_gen.gens[255], 9);
    }

    #[tokio::test]
    async fn test_bucket_gen_init_is_idempotent() {
        let mut state = make_state();

        run(&mut state).await.unwrap();
        let first_count = state.vol.btree(BtreeId::BucketGens).root().node.packed_keys + state.vol.btree(BtreeId::BucketGens).root().node.unpacked_keys;

        run(&mut state).await.unwrap();
        let second_count = state.vol.btree(BtreeId::BucketGens).root().node.packed_keys + state.vol.btree(BtreeId::BucketGens).root().node.unpacked_keys;

        assert_eq!(first_count, second_count);
    }
}
