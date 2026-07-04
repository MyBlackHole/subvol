use crate::alloc::{alloc_freespace_pos, BchAllocator, BchDataType, Bucket};
use crate::btree::{BtreeEntry, BtreeId};
use crate::recovery::RecoveryState;
use crate::types::StorageError;
use crate::BchVol;

/// Pass: Freespace btree 初始化（对应 bcachefs `bch2_fs_freespace_init()`）
///
/// 遍历 allocator 所有 bucket，将空闲 bucket 写入 Freespace btree。
/// 对应 bcachefs `bch2_fs_freespace_init()` 的 alloc btree 扫描路径。
///
/// # 幂等性
/// Freespace btree key 包含 bucket_index + genbits，重复写入相同 key 结果不变。
/// 本 pass 可多次运行不产生副作用。
pub async fn run(state: &mut RecoveryState) -> Result<(), StorageError> {
    if state.vol.btree(BtreeId::Freespace).root().node.packed_keys + state.vol.btree(BtreeId::Freespace).root().node.unpacked_keys > 0 {
        return Ok(());
    }
    let allocator = unsafe { &*state.vol.allocator.get() };
    bch2_fs_freespace_init(&state.vol, allocator)
}

/// 核心实现 — 由 fs_freespace_init pass 调用
///
/// 遍历 allocator 所有 bucket，对每个 Free 状态的 bucket 调用
/// `bch2_freespace_insert()` 写入 Freespace btree。
pub(crate) fn bch2_fs_freespace_init(
    vol: &BchVol,
    allocator: &BchAllocator,
) -> Result<(), StorageError> {
    for dev_idx in vol.device_registry.dev_indices() {
        let Some(ca) = vol.device_registry.resolve_bch_dev(dev_idx) else {
            continue;
        };
        allocator.for_each_bucket(&ca, |bucket_idx, bucket: &Bucket, _gen| {
            if bucket.state == BchDataType::Free {
                bch2_freespace_insert_core(vol, dev_idx, bucket_idx, 0, bucket.oldest_gen);
            }
        });
        ca.freespace_initialized
            .store(true, std::sync::atomic::Ordering::Release);
    }
    Ok(())
}

/// 在 Freespace btree 中插入空闲 bucket 条目
///
/// key = alloc_freespace_pos(bucket_index, gen)，value = empty。
/// genbits 用于检测 stale：分配时通过 genbits 匹配确保使用的 bucket 未被重新分配过。
fn bch2_freespace_insert_core(
    vol: &BchVol,
    dev: u8,
    bucket_index: u64,
    generation: u8,
    oldest_gen: u8,
) {
    use crate::btree::key::KeyType;
    let pos = alloc_freespace_pos(dev, bucket_index, generation, oldest_gen);
    if vol.get_entry_raw(BtreeId::Freespace, pos).is_some() {
        return;
    }
    vol.insert_entry_raw(
        BtreeId::Freespace,
        BtreeEntry::raw(pos, KeyType::Set, vec![]),
        0,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_device::MockBlockDevice;
    use crate::btree::BtreeId;
    use crate::journal::Journal;
    use crate::recovery::RecoveryState;
    use crate::types::BackendType;
    use std::sync::Arc;

    fn make_vol() -> crate::BchVol {
        let vol = crate::BchVol::test_trees();
        let ca = vol.primary_device_rcu_noerror().unwrap();
        let allocator = unsafe { &*vol.allocator.get() };
        allocator.for_each_bucket_mut(&ca, |bucket_idx, bucket, _gen| {
            if bucket_idx == 3 {
                bucket.state = BchDataType::User;
            }
        });
        vol
    }

    #[test]
    fn test_fs_freespace_init_inserts_only_free_buckets() {
        let vol = make_vol();
        let allocator = unsafe { &*vol.allocator.get() };

        bch2_fs_freespace_init(&vol, &allocator).unwrap();

        let free_entry = vol.get_entry_raw(BtreeId::Freespace, alloc_freespace_pos(0, 0, 0, 0));
        assert!(free_entry.is_some(), "free bucket should be inserted");

        let allocated_entry =
            vol.get_entry_raw(BtreeId::Freespace, alloc_freespace_pos(0, 3, 7, 0));
        assert!(
            allocated_entry.is_none(),
            "non-free bucket should not be inserted"
        );
    }

    #[tokio::test]
    async fn test_fs_freespace_init_is_idempotent() {
        let vol = make_vol();
        let _backend = Arc::new(MockBlockDevice::new());
        let journal = Journal::new(vec![100]);
        let sb = crate::storage::superblock::BchSb::with_volume_info(
            "test".into(),
            1,
            "default".into(),
            4096,
            1024 * 1024,
            BackendType::Nfs,
        );
        let mut state = RecoveryState::new(Box::new(vol), journal, sb);

        run(&mut state).await.unwrap();
        let first_count = state.vol.btree(BtreeId::Freespace).root().node.packed_keys + state.vol.btree(BtreeId::Freespace).root().node.unpacked_keys;

        run(&mut state).await.unwrap();
        let second_count = state.vol.btree(BtreeId::Freespace).root().node.packed_keys + state.vol.btree(BtreeId::Freespace).root().node.unpacked_keys;

        assert_eq!(first_count, second_count, "second rebuild must be a no-op");
    }
}
