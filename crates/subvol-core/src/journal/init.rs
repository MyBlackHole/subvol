//! Journal bucket allocation — 对应本地 `fs/journal/init.c`。

use crate::alloc::{
    bch2_disk_reservation_get, bch2_disk_reservation_put, bch2_trans_mark_metadata_bucket,
    AllocRequest, BchDataType, BchReservationFlags, SECTORS_PER_BLOCK,
};
use crate::bch_vol::BchVol;
use crate::block_device::{BchDev, BchDevIoRefKind};
use crate::btree::iter::UpdateTriggerFlags;
use crate::btree::transaction::{BtreeTrans, UsageField};
use crate::storage::superblock::{feature_bits, member_bits};
use crate::types::{StorageError, Watermark, SECTOR_SIZE};

const BCH_JOURNAL_BUCKETS_MIN: u64 = 8;

/// 对应本地 `bch2_set_nr_journal_buckets_iter()`
/// (`fs/journal/init.c:19-142`)。
fn bch2_set_nr_journal_buckets_iter(
    c: &BchVol,
    ca: &BchDev,
    nr: u64,
    new_fs: bool,
    watermark: Watermark,
) -> Result<(), StorageError> {
    let nr_current = u64::from(ca.journal.lock().unwrap().nr);
    assert!(nr > nr_current);
    let nr_want = nr - nr_current;
    let member = c
        .superblock()
        .member(ca.dev_idx)
        .ok_or_else(|| StorageError::NotFound(format!("member {} not found", ca.dev_idx)))?;
    let bucket_blocks = u64::from(member.bucket_size) / SECTORS_PER_BLOCK;
    let bucket_sectors = u32::from(member.bucket_size);
    let allocator = unsafe { &*c.allocator.get() };

    let mut block_addrs = Vec::with_capacity(nr_want as usize);
    let mut buckets = Vec::with_capacity(nr_want as usize);
    let mut ret = None;

    for _ in 0..nr_want {
        let mut reservation = if new_fs {
            None
        } else {
            let mut reservation = crate::alloc::bch2_disk_reservation_init(c, 1);
            bch2_disk_reservation_get(
                c,
                &mut reservation,
                u64::from(bucket_sectors),
                1,
                BchReservationFlags::None,
            )?;
            Some(reservation)
        };
        let request = AllocRequest::new(watermark, BchDataType::Journal);
        let block_addr = match allocator.bch2_bucket_alloc_new_fs(c, ca, &request, None) {
            Ok(block_addr) => block_addr,
            Err(err) => {
                if let Some(reservation) = reservation.as_mut() {
                    bch2_disk_reservation_put(c, reservation);
                }
                ret = Some(StorageError::from(err));
                break;
            }
        };
        let bucket = block_addr / bucket_blocks;

        if let Err(err) = bch2_trans_mark_metadata_bucket(
            c,
            ca,
            bucket,
            BchDataType::Journal,
            bucket_sectors,
            UpdateTriggerFlags::TRANSACTIONAL,
        ) {
            if let Some(reservation) = reservation.as_mut() {
                bch2_disk_reservation_put(c, reservation);
            }
            allocator.bch2_open_bucket_put(ca, block_addr);
            let _ = bch2_trans_mark_metadata_bucket(
                c,
                ca,
                bucket,
                BchDataType::Free,
                0,
                UpdateTriggerFlags::TRANSACTIONAL,
            );
            ret = Some(err);
            break;
        }

        if let Some(reservation) = reservation {
            let mut trans = BtreeTrans::new(c);
            trans.set_disk_reservation(reservation);
            trans.fs_usage_add(UsageField::Reserved, i64::from(bucket_sectors));
            trans.bch2_trans_account_disk_usage_change();
        }

        block_addrs.push(block_addr);
        buckets.push(bucket);
    }

    if buckets.is_empty() {
        return Err(ret.expect("journal allocation made no progress without an error"));
    }

    // Don't return an error if we successfully allocated some buckets.
    ret = None;

    let persist_result = {
        let _journal_block = c.journal_ref().bch2_journal_block();
        let (mut new_buckets, mut new_bucket_seq, pos, new_nr) = {
            let journal = ca.journal.lock().unwrap();
            assert!(journal.discard_idx <= journal.nr);
            let pos = if journal.discard_idx != 0 {
                journal.discard_idx as usize
            } else {
                journal.nr as usize
            };

            (
                journal.buckets.clone(),
                journal.bucket_seq.clone(),
                pos,
                journal.nr + buckets.len() as u32,
            )
        };

        new_buckets.splice(pos..pos, buckets.iter().copied());
        new_bucket_seq.splice(pos..pos, std::iter::repeat_n(0, buckets.len()));

        let disk_sb = {
            let mut disk_sb = ca.disk_sb.lock().unwrap().clone();
            disk_sb.journal_buckets = new_buckets
                .iter()
                .map(|bucket| bucket * bucket_blocks)
                .collect();
            disk_sb.journal_bucket_seq.clone_from(&new_bucket_seq);
            disk_sb
        };

        let write_result = std::thread::scope(|scope| {
            scope
                .spawn(move || {
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(StorageError::Io)?
                        .block_on(disk_sb.write_to_device(ca))
                        .map(|_| disk_sb)
                })
                .join()
                .map_err(|_| StorageError::Transaction("superblock writer panicked".into()))?
        });

        match write_result {
            Ok(disk_sb) => {
                *ca.disk_sb.lock().unwrap() = disk_sb;
                let mut journal = ca.journal.lock().unwrap();
                journal.buckets = new_buckets;
                journal.bucket_seq = new_bucket_seq;
                journal.nr = new_nr;
                if pos <= journal.discard_idx as usize {
                    journal.discard_idx = (journal.discard_idx + buckets.len() as u32) % journal.nr;
                }
                if pos <= journal.dirty_idx_ondisk as usize {
                    journal.dirty_idx_ondisk =
                        (journal.dirty_idx_ondisk + buckets.len() as u32) % journal.nr;
                }
                if pos <= journal.dirty_idx as usize {
                    journal.dirty_idx = (journal.dirty_idx + buckets.len() as u32) % journal.nr;
                }
                if pos <= journal.cur_idx as usize {
                    journal.cur_idx = (journal.cur_idx + buckets.len() as u32) % journal.nr;
                }
                Ok(())
            }
            Err(err) => Err(err),
        }
    };

    if let Err(err) = persist_result {
        ret = Some(err);
        for &bucket in &buckets {
            let _ = bch2_trans_mark_metadata_bucket(
                c,
                ca,
                bucket,
                BchDataType::Free,
                0,
                UpdateTriggerFlags::TRANSACTIONAL,
            );
        }
    }

    for block_addr in block_addrs {
        allocator.bch2_open_bucket_put(ca, block_addr);
    }

    match ret {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// 对应本地 `bch2_set_nr_journal_buckets_loop()`
/// (`fs/journal/init.c:144-180`)。
fn bch2_set_nr_journal_buckets_loop(
    c: &BchVol,
    ca: &BchDev,
    nr: u64,
    new_fs: bool,
) -> Result<(), StorageError> {
    let watermark = if new_fs {
        Watermark::Btree
    } else {
        Watermark::Normal
    };

    if nr < u64::from(ca.journal.lock().unwrap().nr) {
        return Ok(());
    }

    loop {
        let nr_current = u64::from(ca.journal.lock().unwrap().nr);
        if nr_current >= nr {
            return Ok(());
        }

        let ret = bch2_set_nr_journal_buckets_iter(c, ca, nr, new_fs, watermark);
        ret?;
    }
}

/// 对应本地 `bch2_dev_journal_alloc()` (`fs/journal/init.c:263-302`)。
///
/// Rust 的 `BchDev` 不持有可能因值移动而失效的 `bch_fs *` 裸指针，因此
/// `c` 显式传入；其余参数和控制流与本地函数一致。
pub fn bch2_dev_journal_alloc(c: &BchVol, ca: &BchDev, new_fs: bool) -> Result<(), StorageError> {
    let member = c
        .superblock()
        .member(ca.dev_idx)
        .ok_or_else(|| StorageError::NotFound(format!("member {} not found", ca.dev_idx)))?;
    let data_allowed = (member.flags >> member_bits::DATA_ALLOWED_SHIFT) & 0x1f;
    if data_allowed & (1 << BchDataType::Journal as u8) == 0 {
        return Ok(());
    }

    if c.superblock().feature_test(feature_bits::SMALL_IMAGE) {
        return Err(StorageError::InvalidData(
            "cannot allocate journal, filesystem is an unresized image file".into(),
        ));
    }

    let pages = unsafe { libc::sysconf(libc::_SC_PHYS_PAGES) }.max(0) as u64;
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(0) as u64;
    let bucket_bytes = u64::from(member.bucket_size) * SECTOR_SIZE as u64;
    let upper = pages
        .saturating_mul(page_size)
        .checked_div(4)
        .and_then(|bytes| bytes.checked_div(bucket_bytes))
        .unwrap_or(0);
    let nr = (member.nbuckets >> 7)
        .max(BCH_JOURNAL_BUCKETS_MIN)
        .min(upper);

    bch2_set_nr_journal_buckets_loop(c, ca, nr, new_fs)
}

/// 对应本地 `bch2_fs_journal_alloc()` (`fs/journal/init.c:305-320`)。
pub fn bch2_fs_journal_alloc(c: &BchVol) -> Result<(), StorageError> {
    let mut ca = None;
    loop {
        ca = c
            .device_registry
            .bch2_get_next_online_dev(ca, u32::MAX, BchDevIoRefKind::Read);
        let Some(current) = ca.as_ref() else {
            break;
        };

        if current.journal.lock().unwrap().nr != 0 {
            continue;
        }

        if let Err(ret) = bch2_dev_journal_alloc(c, current, true) {
            drop(ca.take());
            return Err(ret);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Arc;

    use async_trait::async_trait;

    use crate::alloc::{
        BLOCKS_PER_BUCKET, DEFAULT_BLOCK_SIZE, DEFAULT_BTREE_NODE_SIZE,
    };
    use crate::block_device::{BlockDevice, MockBlockDevice};
    use crate::storage::superblock::{BchSb, BchSbMember};
    use crate::types::{BlockAddr, HealthStatus};

    fn make_vol(nbuckets: u64, backends: Vec<Arc<dyn BlockDevice>>) -> (BchVol, Vec<Arc<BchDev>>) {
        let capacity = nbuckets * BLOCKS_PER_BUCKET * DEFAULT_BLOCK_SIZE;
        let mut sb = BchSb::new();
        sb.block_size = DEFAULT_BLOCK_SIZE as u32;
        sb.capacity = capacity;
        sb.primary_dev_idx = 0;
        sb.members = backends
            .iter()
            .enumerate()
            .map(|(dev_idx, _)| {
                let mut member = BchSbMember::new(dev_idx as u8, format!("dev-{dev_idx}"));
                member.mark_alive([dev_idx as u8 + 1; 16]);
                member.nbuckets = nbuckets;
                member.bucket_size = (BLOCKS_PER_BUCKET * SECTORS_PER_BLOCK) as u16;
                member.flags |=
                    (1 << BchDataType::Journal as u8) << member_bits::DATA_ALLOWED_SHIFT;
                member
            })
            .collect();
        let devices: Vec<_> = backends
            .into_iter()
            .enumerate()
            .map(|(dev_idx, backend)| Arc::new(BchDev::new(backend, dev_idx as u8)))
            .collect();
        let vol = BchVol::alloc_with_devices(
            sb,
            devices.clone(),
            crate::bch_vol::VolumeConfig {
                block_size: DEFAULT_BLOCK_SIZE as u32,
                capacity,
                btree_node_size: DEFAULT_BTREE_NODE_SIZE,
                ..crate::bch_vol::VolumeConfig::default()
            },
            "journal-init-test".into(),
            PathBuf::from("/tmp/journal-init-test"),
        );
        (vol, devices)
    }

    #[test]
    fn allocates_and_persists_default_device_journal() {
        let (vol, devices) = make_vol(1024, vec![Arc::new(MockBlockDevice::new())]);
        let ca = &devices[0];

        bch2_dev_journal_alloc(&vol, ca, true).unwrap();

        let journal = ca.journal.lock().unwrap();
        assert_eq!(journal.nr, 8);
        assert_eq!(journal.buckets.len(), 8);
        assert_eq!(journal.bucket_seq, vec![0; 8]);
        let bucket_blocks = BLOCKS_PER_BUCKET;
        assert_eq!(
            ca.disk_sb.lock().unwrap().journal_buckets,
            journal
                .buckets
                .iter()
                .map(|bucket| bucket * bucket_blocks)
                .collect::<Vec<_>>()
        );
        drop(journal);
        assert_eq!(unsafe { &*vol.allocator.get() }.open_buckets.nr_open(), 0);
        assert_eq!(
            ca.nr_open_buckets
                .load(std::sync::atomic::Ordering::Acquire),
            0
        );
    }

    #[test]
    fn inserts_at_discard_and_rotates_all_runtime_indices() {
        let (vol, devices) = make_vol(1024, vec![Arc::new(MockBlockDevice::new())]);
        let ca = &devices[0];
        {
            let mut journal = ca.journal.lock().unwrap();
            journal.buckets = vec![500, 501, 502];
            journal.bucket_seq = vec![11, 12, 13];
            journal.nr = 3;
            journal.discard_idx = 1;
            journal.dirty_idx_ondisk = 0;
            journal.dirty_idx = 2;
            journal.cur_idx = 1;
        }

        bch2_set_nr_journal_buckets_iter(&vol, ca, 5, true, Watermark::Btree).unwrap();

        let journal = ca.journal.lock().unwrap();
        assert_eq!(journal.nr, 5);
        assert_eq!(&journal.buckets[..1], &[500]);
        assert_eq!(&journal.buckets[3..], &[501, 502]);
        assert_eq!(&journal.bucket_seq[..1], &[11]);
        assert_eq!(&journal.bucket_seq[1..3], &[0, 0]);
        assert_eq!(&journal.bucket_seq[3..], &[12, 13]);
        assert_eq!(journal.discard_idx, 3);
        assert_eq!(journal.dirty_idx_ondisk, 0);
        assert_eq!(journal.dirty_idx, 4);
        assert_eq!(journal.cur_idx, 3);
    }

    #[test]
    fn partial_allocation_commits_progress_and_suppresses_allocation_error() {
        let (vol, devices) = make_vol(4, vec![Arc::new(MockBlockDevice::new())]);
        let ca = &devices[0];

        bch2_set_nr_journal_buckets_iter(&vol, ca, 8, true, Watermark::Btree).unwrap();

        let journal = ca.journal.lock().unwrap();
        assert!(journal.nr > 0);
        assert!(journal.nr < 8);
        assert_eq!(journal.nr as usize, journal.buckets.len());
        assert_eq!(journal.nr as usize, journal.bucket_seq.len());
    }

    #[test]
    fn zero_progress_returns_allocation_error_without_state_change() {
        let (vol, devices) = make_vol(0, vec![Arc::new(MockBlockDevice::new())]);
        let ca = &devices[0];

        let ret = bch2_set_nr_journal_buckets_iter(&vol, ca, 1, true, Watermark::Btree);

        assert!(matches!(
            ret,
            Err(StorageError::AddressSpaceExhausted { .. })
        ));
        assert_eq!(ca.journal.lock().unwrap().nr, 0);
        assert!(ca.disk_sb.lock().unwrap().journal_buckets.is_empty());
    }

    #[derive(Debug)]
    struct WriteFailDevice {
        inner: MockBlockDevice,
    }

    #[async_trait]
    impl BlockDevice for WriteFailDevice {
        async fn read_block(
            &self,
            addr: BlockAddr,
            buf: &mut [u8],
        ) -> crate::block_device::Result<()> {
            self.inner.read_block(addr, buf).await
        }

        async fn write_block(
            &self,
            _addr: BlockAddr,
            _buf: &[u8],
        ) -> crate::block_device::Result<()> {
            Err(StorageError::Unreachable(
                "injected superblock write failure".into(),
            ))
        }

        async fn delete_block(&self, addr: BlockAddr) -> crate::block_device::Result<()> {
            self.inner.delete_block(addr).await
        }

        async fn trim_block(&self, addr: BlockAddr) -> crate::block_device::Result<()> {
            self.inner.trim_block(addr).await
        }

        async fn flush(&self) -> crate::block_device::Result<()> {
            Ok(())
        }

        async fn health_check(&self) -> crate::block_device::Result<HealthStatus> {
            Ok(HealthStatus::Healthy)
        }

        async fn used_space(&self) -> crate::block_device::Result<u64> {
            self.inner.used_space().await
        }
    }

    #[test]
    fn persistence_failure_rolls_back_metadata_runtime_and_open_buckets() {
        let backend = Arc::new(WriteFailDevice {
            inner: MockBlockDevice::new(),
        });
        let (vol, devices) = make_vol(256, vec![backend]);
        let ca = &devices[0];

        let ret = bch2_set_nr_journal_buckets_iter(&vol, ca, 2, true, Watermark::Btree);

        assert!(matches!(ret, Err(StorageError::Unreachable(_))));
        assert_eq!(ca.journal.lock().unwrap().nr, 0);
        assert!(ca.disk_sb.lock().unwrap().journal_buckets.is_empty());
        assert_eq!(ca.allocated.load(std::sync::atomic::Ordering::Acquire), 0);
        assert_eq!(
            ca.nr_open_buckets
                .load(std::sync::atomic::Ordering::Acquire),
            0
        );
        assert_eq!(unsafe { &*vol.allocator.get() }.open_buckets.nr_open(), 0);
        let groups = unsafe { &*ca.groups.get() };
        assert!(groups.iter().all(|group| group
            .lock()
            .unwrap()
            .buckets
            .iter()
            .all(|bucket| bucket.state == BchDataType::Free)));
    }

    #[test]
    fn fs_allocation_processes_online_empty_devices_and_balances_read_refs() {
        let (vol, devices) = make_vol(
            1024,
            vec![
                Arc::new(MockBlockDevice::new()),
                Arc::new(MockBlockDevice::new()),
                Arc::new(MockBlockDevice::new()),
            ],
        );
        {
            let mut journal = devices[1].journal.lock().unwrap();
            journal.nr = 1;
            journal.buckets = vec![700];
            journal.bucket_seq = vec![9];
        }
        devices[2].set_offline();

        bch2_fs_journal_alloc(&vol).unwrap();

        assert_eq!(devices[0].journal.lock().unwrap().nr, 8);
        assert_eq!(devices[1].journal.lock().unwrap().buckets, vec![700]);
        assert_eq!(devices[2].journal.lock().unwrap().nr, 0);
        for dev in devices {
            assert_eq!(dev.io_ref_count(BchDevIoRefKind::Read), 0);
        }
    }

    #[test]
    fn runtime_allocation_reservation_is_released() {
        let (vol, devices) = make_vol(1024, vec![Arc::new(MockBlockDevice::new())]);
        let ca = &devices[0];
        let before = unsafe { (&*vol.capacity.get()).pcpu[0].online_reserved };

        bch2_set_nr_journal_buckets_loop(&vol, ca, 2, false).unwrap();

        let after = unsafe { (&*vol.capacity.get()).pcpu[0].online_reserved };
        assert_eq!(after, before);
    }
}
