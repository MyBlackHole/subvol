#[cfg(test)]
use crate::alloc::btree::BCH_ALLOC_V4_ZERO;
use crate::alloc::btree::BchAllocEntry;
use crate::alloc::bucket::BchDataType;
use crate::alloc::bucket_to_sector;
use crate::alloc::foreground::bch2_alloc_wake_all;
use crate::bch_vol::BchVol;
use crate::storage::superblock::BchMemberState;
use crate::types::StorageError;
use std::sync::atomic::Ordering;

const LRU_TIME_BITS: u32 = 48;
const LRU_TIME_MAX: u64 = (1u64 << LRU_TIME_BITS) - 1;
const FRAGMENTATION_LRU_SCALE: u64 = 1 << 31;

/// 对应本地 `bch2_fs_capacity_exit()` (`fs/alloc/background.c:1736-1745`)。
pub fn bch2_fs_capacity_exit(c: &BchVol) {
    let capacity = unsafe { &mut *c.capacity.get() };
    let _mark_lock = capacity.mark_lock.write().unwrap();
    let online_reserved = capacity
        .pcpu
        .iter()
        .fold(0u64, |v, usage| v.wrapping_add(usage.online_reserved));
    if online_reserved != 0 {
        tracing::warn!("online_reserved not 0 at shutdown: {}", online_reserved);
    }

    capacity.pcpu.clear();
}

/// 对应本地 `bch2_fs_capacity_init()` (`fs/alloc/background.c:1747-1757`)。
pub fn bch2_fs_capacity_init(c: &BchVol) -> Result<(), StorageError> {
    let capacity = unsafe { &mut *c.capacity.get() };
    let _mark_lock = capacity.mark_lock.write().unwrap();
    capacity.pcpu = vec![crate::alloc::BchFsCapacityPcpu::default()];
    Ok(())
}

/// 对应本地 `bch2_recalc_capacity()` (`fs/alloc/background.c`)。
/// 调用方必须持有 `c.state_lock`。
pub fn bch2_recalc_capacity(c: &BchVol) {
    let mut capacity = 0;
    let mut reserved_sectors = 0;
    let mut bucket_size_max = 0;

    for dev_idx in c.device_registry.dev_indices() {
        let Some(ca) = c.device_registry.resolve_bch_dev(dev_idx) else {
            continue;
        };
        // `for_each_member_device_rcu(..., &c->devs_online)` in the local
        // bcachefs `fs/sb/members.h:127-132` excludes devices whose IO ref
        // domain has gone offline before checking member metadata.
        if !ca.is_online() {
            continue;
        }
        let mi = unsafe { &*ca.mi.get() };

        if mi.state != BchMemberState::Rw as u8 {
            continue;
        }

        if mi.durability == 0 {
            continue;
        }

        let mut dev_reserve = 0;

        dev_reserve += ca.nr_btree_reserve.load(Ordering::Acquire) * 2;
        dev_reserve += mi.nbuckets >> 6;

        dev_reserve += 1;
        dev_reserve += 1;
        dev_reserve += 1;

        dev_reserve *= u64::from(mi.bucket_size);

        capacity += bucket_to_sector(&ca, (mi.nbuckets - u64::from(mi.first_bucket)) as usize);

        reserved_sectors += dev_reserve * 2;

        bucket_size_max = bucket_size_max.max(u32::from(mi.bucket_size));
    }

    // 本地 NO_BCACHEFS_FS 构建下 `bch2_set_ra_pages()` 为空操作。

    let gc_reserve_percent = c
        .superblock()
        .storage_config
        .clone()
        .unwrap_or_default()
        .gc_reserve_percent;
    let gc_reserve = capacity * u64::from(gc_reserve_percent) / 100;

    reserved_sectors = gc_reserve.max(reserved_sectors);

    reserved_sectors = reserved_sectors.min(capacity);

    let capacity_state = unsafe { &mut *c.capacity.get() };
    let _mark_lock = capacity_state.mark_lock.write().unwrap();
    capacity_state.reserved = reserved_sectors;
    capacity_state.capacity = capacity - reserved_sectors;

    capacity_state.bucket_size_max = bucket_size_max;

    bch2_alloc_wake_all(c);
}

/// 对应本地 `bch2_min_rw_member_capacity()`。
pub fn bch2_min_rw_member_capacity(c: &BchVol) -> u64 {
    let mut ret = u64::MAX;

    for dev_idx in c.device_registry.dev_indices() {
        let Some(ca) = c.device_registry.resolve_bch_dev(dev_idx) else {
            continue;
        };
        // `for_each_rw_member_rcu()` (`fs/sb/members.h:134-135`) iterates
        // the published online RW mask, not merely the persisted state bit.
        if !ca.is_online() {
            continue;
        }
        let mi = unsafe { &*ca.mi.get() };
        if mi.state == BchMemberState::Rw as u8 {
            ret = ret.min(mi.nbuckets * u64::from(mi.bucket_size));
        }
    }
    ret
}

fn data_type_movable(data_type: BchDataType) -> bool {
    matches!(
        data_type,
        BchDataType::Btree | BchDataType::User | BchDataType::Stripe
    )
}

    /// 在只读 flush 时确保 0 号 bucket 的 mark 已初始化
///
/// 读 LRU 索引——对应 bcachefs `alloc_lru_idx_read()`
pub fn alloc_lru_idx_read(entry: &BchAllocEntry) -> u64 {
    if entry.data_type == BchDataType::Cached as u8 {
        entry.io_time[0] & LRU_TIME_MAX
    } else {
        0
    }
}

/// 片段 LRU 索引——对应 bcachefs `alloc_lru_idx_fragmentation()`
pub fn alloc_lru_idx_fragmentation(entry: &BchAllocEntry, bucket_size: u64) -> u64 {
    let data_type = BchDataType::from_raw(entry.data_type).unwrap_or(BchDataType::Free);
    if bucket_size == 0 || !data_type_movable(data_type) {
        return 0;
    }

    let used = u64::from(entry.dirty_sectors);
    if used == 0 {
        return 0;
    }

    let capped = used.min(bucket_size);
    capped.saturating_mul(FRAGMENTATION_LRU_SCALE) / bucket_size
}

/// 外部 backpointer 数量访问器——未来 backpointer 检查可直接复用
pub fn alloc_nr_external_backpointers(entry: &BchAllocEntry) -> u32 {
    entry.nr_external_backpointers
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alloc::bch2_dev_buckets_resize;
    use crate::bch_vol::VolumeConfig;
    use crate::block_device::{BchDev, MockBlockDevice};
    use crate::storage::superblock::{member_bits, BchSb, BchSbMember};
    use std::future::Future;
    use std::path::PathBuf;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    use std::task::{Context, Poll};

    fn capacity_vol(members: &[(u64, u16, BchMemberState, u8)]) -> (BchVol, Vec<Arc<BchDev>>) {
        let mut sb = BchSb::new();
        sb.storage_config = Some(crate::config::StorageConfig::default());
        sb.primary_dev_idx = 0;
        let mut devices = Vec::new();

        for (dev_idx, &(nbuckets, bucket_size, state, durability)) in members.iter().enumerate() {
            let mut member = BchSbMember::new(dev_idx as u8, format!("dev-{dev_idx}"));
            member.mark_alive([dev_idx as u8 + 1; 16]);
            member.nbuckets = nbuckets;
            member.first_bucket = 8;
            member.bucket_size = bucket_size;
            member.set_state(state);
            member.flags |= u64::from(durability) << member_bits::DURABILITY_SHIFT;
            sb.members.push(member);
            devices.push(Arc::new(BchDev::new(
                Arc::new(MockBlockDevice::new()),
                dev_idx as u8,
            )));
        }

        sb.capacity = members
            .iter()
            .map(|&(nbuckets, bucket_size, _, _)| nbuckets * u64::from(bucket_size) * 512)
            .sum();
        let config = VolumeConfig {
            capacity: sb.capacity,
            ..VolumeConfig::default()
        };
        let vol = BchVol::alloc_with_devices(
            sb,
            devices.iter().cloned(),
            config,
            String::new(),
            PathBuf::new(),
        );
        (vol, devices)
    }

    #[test]
    fn test_recalc_capacity_device_reserve_and_multiple_devices() {
        let (vol, devices) = capacity_vol(&[
            (128, 1024, BchMemberState::Rw, 2),
            (72, 512, BchMemberState::Rw, 2),
        ]);
        for ca in &devices {
            ca.nr_btree_reserve.store(0, Ordering::Release);
        }

        let _state_lock = vol.state_lock.lock().unwrap();
        bch2_recalc_capacity(&vol);
        let capacity = unsafe { &*vol.capacity.get() };

        let raw_capacity = (128 - 8) * 1024 + (72 - 8) * 512;
        let device_reserve = ((128 >> 6) + 3) * 1024 * 2 + ((72 >> 6) + 3) * 512 * 2;
        assert_eq!(capacity.reserved, device_reserve);
        assert_eq!(capacity.capacity, raw_capacity - device_reserve);
        assert_eq!(capacity.bucket_size_max, 1024);
    }

    #[test]
    fn test_recalc_capacity_filters_state_and_zero_durability() {
        let (vol, devices) = capacity_vol(&[
            (128, 1024, BchMemberState::Rw, 2),
            (256, 1024, BchMemberState::Ro, 2),
            (512, 1024, BchMemberState::Evacuating, 2),
            (1024, 1024, BchMemberState::Spare, 2),
            (2048, 1024, BchMemberState::Rw, 1),
        ]);
        devices[0].nr_btree_reserve.store(0, Ordering::Release);

        let _state_lock = vol.state_lock.lock().unwrap();
        bch2_recalc_capacity(&vol);
        let capacity = unsafe { &*vol.capacity.get() };

        assert_eq!(capacity.reserved, ((128 >> 6) + 3) * 1024 * 2);
        assert_eq!(capacity.capacity, (128 - 8) * 1024 - capacity.reserved);
        assert_eq!(capacity.bucket_size_max, 1024);
    }

    #[test]
    fn test_recalc_capacity_excludes_offline_rw_member() {
        let (vol, devices) = capacity_vol(&[
            (128, 1024, BchMemberState::Rw, 2),
            (72, 512, BchMemberState::Rw, 2),
        ]);
        devices[0].nr_btree_reserve.store(0, Ordering::Release);
        devices[1].nr_btree_reserve.store(0, Ordering::Release);
        devices[0].set_offline();

        let _state_lock = vol.state_lock.lock().unwrap();
        bch2_recalc_capacity(&vol);
        let capacity = unsafe { &*vol.capacity.get() };

        let raw_capacity = (72 - 8) * 512;
        let device_reserve = ((72 >> 6) + 3) * 512 * 2;
        assert_eq!(capacity.reserved, device_reserve);
        assert_eq!(capacity.capacity, raw_capacity - device_reserve);
        assert_eq!(capacity.bucket_size_max, 512);
        assert_eq!(bch2_min_rw_member_capacity(&vol), 72 * 512);
    }

    #[test]
    fn test_recalc_capacity_gc_reserve_wins_and_clamps() {
        let (vol, devices) = capacity_vol(&[(128, 1024, BchMemberState::Rw, 2)]);
        devices[0].nr_btree_reserve.store(0, Ordering::Release);
        vol.superblock_mut()
            .storage_config
            .as_mut()
            .unwrap()
            .gc_reserve_percent = 20;

        let _state_lock = vol.state_lock.lock().unwrap();
        bch2_recalc_capacity(&vol);
        let capacity = unsafe { &*vol.capacity.get() };
        assert_eq!(capacity.reserved, (128 - 8) * 1024 * 20 / 100);

        devices[0].nr_btree_reserve.store(1000, Ordering::Release);
        bch2_recalc_capacity(&vol);
        let capacity = unsafe { &*vol.capacity.get() };
        assert_eq!(capacity.reserved, (128 - 8) * 1024);
        assert_eq!(capacity.capacity, 0);
    }

    #[test]
    fn test_min_rw_member_capacity_and_empty_result() {
        let (vol, _) = capacity_vol(&[
            (128, 1024, BchMemberState::Rw, 2),
            (72, 512, BchMemberState::Rw, 2),
            (16, 512, BchMemberState::Ro, 2),
        ]);
        assert_eq!(bch2_min_rw_member_capacity(&vol), 72 * 512);

        let (vol, _) = capacity_vol(&[(16, 512, BchMemberState::Ro, 2)]);
        assert_eq!(bch2_min_rw_member_capacity(&vol), u64::MAX);
    }

    #[test]
    fn test_resize_recalculates_capacity_and_invalid_resize_does_not_publish() {
        let (vol, devices) = capacity_vol(&[(128, 1024, BchMemberState::Rw, 2)]);
        let ca = &devices[0];
        ca.nr_btree_reserve.store(0, Ordering::Release);
        vol.superblock_mut().members[0].nbuckets = 160;
        bch2_dev_buckets_resize(&vol, ca, 160).unwrap();
        let capacity = unsafe { &*vol.capacity.get() };
        assert_eq!(capacity.capacity + capacity.reserved, (160 - 8) * 1024);

        let published = (
            capacity.capacity,
            capacity.reserved,
            capacity.bucket_size_max,
        );
        assert!(bch2_dev_buckets_resize(&vol, ca, 0).is_err());
        let capacity = unsafe { &*vol.capacity.get() };
        assert_eq!(
            (
                capacity.capacity,
                capacity.reserved,
                capacity.bucket_size_max
            ),
            published
        );
    }

    #[test]
    fn test_alloc_wake_all_notifies_global_and_each_device_allocator() {
        let (vol, devices) = capacity_vol(&[
            (128, 1024, BchMemberState::Rw, 2),
            (72, 512, BchMemberState::Rw, 2),
        ]);
        let mut notified = Box::pin(vol.allocator().freelist_wait.notified());
        let waker = futures::task::noop_waker();
        let mut context = Context::from_waker(&waker);
        assert_eq!(notified.as_mut().poll(&mut context), Poll::Pending);
        let before: Vec<_> = devices
            .iter()
            .map(|ca| ca.alloc_wake_counter.load(Ordering::Acquire))
            .collect();

        let _state_lock = vol.state_lock.lock().unwrap();
        bch2_recalc_capacity(&vol);

        for (ca, before) in devices.iter().zip(before) {
            assert_eq!(ca.alloc_wake_counter.load(Ordering::Acquire), before + 1);
        }
        assert_eq!(notified.as_mut().poll(&mut context), Poll::Ready(()));
    }

    #[test]
    fn test_member_state_transition_recalculates_and_preserves_logical_capacity() {
        let (vol, devices) = capacity_vol(&[(128, 1024, BchMemberState::Rw, 2)]);
        let ca = &devices[0];
        ca.nr_btree_reserve.store(0, Ordering::Release);
        let logical_capacity = vol.capacity();

        for state in [
            BchMemberState::Ro,
            BchMemberState::Evacuating,
            BchMemberState::Spare,
        ] {
            crate::bch_vol::bch2_dev_set_state(&vol, ca, state).unwrap();
            assert_eq!(vol.superblock().member_state(0), Some(state));
            assert_eq!(ca.member_state(), state);
            assert_eq!(unsafe { &*ca.mi.get() }.state, state as u8);
            let capacity = unsafe { &*vol.capacity.get() };
            assert_eq!(capacity.capacity, 0);
            assert_eq!(capacity.reserved, 0);
            assert_eq!(capacity.bucket_size_max, 0);
        }

        crate::bch_vol::bch2_dev_set_state(&vol, ca, BchMemberState::Rw).unwrap();
        assert_eq!(vol.superblock().member_state(0), Some(BchMemberState::Rw));
        assert_eq!(ca.member_state(), BchMemberState::Rw);
        assert_eq!(unsafe { &*ca.mi.get() }.state, BchMemberState::Rw as u8);
        assert_ne!(unsafe { &*vol.capacity.get() }.capacity, 0);
        assert_eq!(vol.capacity(), logical_capacity);
        assert_eq!(vol.capacity(), logical_capacity);
    }

    #[test]
    fn test_capacity_init_and_exit() {
        let (vol, _) = capacity_vol(&[(128, 1024, BchMemberState::Rw, 2)]);
        let capacity = unsafe { &mut *vol.capacity.get() };
        assert_eq!(capacity.pcpu.len(), 1);
        assert_eq!(capacity.pcpu[0].usage.hidden, 0);
        assert_eq!(capacity.pcpu[0].usage.btree, 0);
        assert_eq!(capacity.pcpu[0].usage.data, 0);
        assert_eq!(capacity.pcpu[0].usage.cached, 0);
        assert_eq!(capacity.pcpu[0].usage.reserved, 0);
        assert_eq!(capacity.pcpu[0].sectors_available, 0);
        assert_eq!(capacity.pcpu[0].online_reserved, 0);

        capacity.pcpu[0].online_reserved = 1;
        bch2_fs_capacity_exit(&vol);
        assert!(unsafe { &*vol.capacity.get() }.pcpu.is_empty());

        bch2_fs_capacity_init(&vol).unwrap();
        let capacity = unsafe { &*vol.capacity.get() };
        assert_eq!(capacity.pcpu.len(), 1);
        assert_eq!(capacity.pcpu[0].online_reserved, 0);
    }

    #[test]
    fn test_alloc_lru_idx_read_uses_cached_time() {
        let mut entry = BCH_ALLOC_V4_ZERO;
        entry.data_type = BchDataType::Cached as u8;
        entry.io_time[0] = LRU_TIME_MAX | 0x1234_0000_0000_0000;
        assert_eq!(alloc_lru_idx_read(&entry), LRU_TIME_MAX);
    }

    #[test]
    fn test_alloc_lru_idx_fragmentation_scales_used_bytes() {
        let mut entry = BCH_ALLOC_V4_ZERO;
        entry.data_type = BchDataType::User as u8;
        entry.dirty_sectors = 64 * crate::alloc::SECTORS_PER_BLOCK as u32;
        entry.cached_sectors = 32 * crate::alloc::SECTORS_PER_BLOCK as u32;
        let idx = alloc_lru_idx_fragmentation(&entry, 256 * crate::alloc::SECTORS_PER_BLOCK);
        assert!(idx > 0);
        assert!(idx <= FRAGMENTATION_LRU_SCALE);
    }

    #[test]
    fn test_alloc_nr_external_backpointers_accessor() {
        let mut entry = BCH_ALLOC_V4_ZERO;
        entry.nr_external_backpointers = 9;
        assert_eq!(alloc_nr_external_backpointers(&entry), 9);
    }
}
