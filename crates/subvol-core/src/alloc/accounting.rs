// Disk accounting — bcachefs `struct disk_accounting_pos` 对齐
//
// 编码：key 为 Bpos（20 字节），byte0 = type，byte1+ = 子类型字段，
// 通过 reinterpret + LE byte order 与 bcachefs 兼容。

use serde::{Deserialize, Serialize};

use crate::alloc::btree::BchAllocV4;
use crate::alloc::bucket::data_type_is_empty;
use crate::alloc::BchDataType;
use crate::bch_vol::BchVol;
use crate::block_device::BchDev;
use crate::btree::iter::UpdateTriggerFlags;
use crate::btree::key::{Bpos, BtreeEntry, KeyType, KeyValue};
use crate::btree::BtreeId;
use crate::types::StorageError;

// ── bcachefs BCH_DISK_ACCOUNTING_TYPES (accounting_format.h:107-118) ──
pub const BCH_DISK_ACCOUNTING_nr_inodes: u8 = 0;
pub const BCH_DISK_ACCOUNTING_persistent_reserved: u8 = 1;
pub const BCH_DISK_ACCOUNTING_replicas: u8 = 2;
pub const BCH_DISK_ACCOUNTING_dev_data_type: u8 = 3;
pub const BCH_DISK_ACCOUNTING_compression: u8 = 4;
pub const BCH_DISK_ACCOUNTING_snapshot: u8 = 5;
pub const BCH_DISK_ACCOUNTING_btree: u8 = 6;
pub const BCH_DISK_ACCOUNTING_rebalance_work: u8 = 7;
pub const BCH_DISK_ACCOUNTING_inum: u8 = 8;
pub const BCH_DISK_ACCOUNTING_reconcile_work: u8 = 9;
pub const BCH_DISK_ACCOUNTING_dev_leaving: u8 = 10;

/// bcachefs 对齐: `struct disk_accounting_pos` 编码为 Bpos
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum AcctType {
    /// type=2: replicas — inode byte0=type, byte1=nr_devs, byte2+=devs[]
    /// 参数为 (dev_index, nr_devs)
    Replicas(u8, u8),
    /// type=3: dev_data_type — inode={type|dev<<8|data_type<<16}
    DevDataType(u8, u8),
    /// type=4: compression — inode={type|ctype<<8}
    Compression(u8),
    /// type=5: snapshot — inode={type|id<<8}
    Snapshot(u32),
    /// type=6: btree — inode={type|id<<8}
    Btree(u32),
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct AcctEntry {
    pub counters: [u64; 3],
}

/// bcachefs 对齐: disk_accounting_pos → Bpos
fn acct_key(ty: AcctType) -> Bpos {
    match ty {
        AcctType::Replicas(dev, nr_devs) => {
            // struct bch_replicas_entry_v1: byte0=type(2), byte1=nr_devs, byte2+=devs[]
            // 单设备优化: devs[0]=dev, rest=0
            let inode =
                BCH_DISK_ACCOUNTING_replicas as u64 | (nr_devs as u64) << 8 | (dev as u64) << 16;
            Bpos::new(inode, 0, 0)
        }
        AcctType::DevDataType(dev, data_type) => {
            // packing: byte0=type(3), byte1=dev, byte2=data_type, rest=0
            let inode = BCH_DISK_ACCOUNTING_dev_data_type as u64
                | (dev as u64) << 8
                | (data_type as u64) << 16;
            Bpos::new(inode, 0, 0)
        }
        AcctType::Compression(ctype) => {
            // byte0=type(4), byte1=ctype
            let inode = BCH_DISK_ACCOUNTING_compression as u64 | (ctype as u64) << 8;
            Bpos::new(inode, 0, 0)
        }
        AcctType::Snapshot(snap_id) => {
            // byte0=type(5), byte1-4=snap_id LE
            let inode = BCH_DISK_ACCOUNTING_snapshot as u64 | (snap_id as u64) << 8;
            Bpos::new(inode, 0, 0)
        }
        AcctType::Btree(btree_id) => {
            // byte0=type(6), byte1-4=btree_id LE
            let inode = BCH_DISK_ACCOUNTING_btree as u64 | (btree_id as u64) << 8;
            Bpos::new(inode, 0, 0)
        }
    }
}

pub fn bch2_disk_accounting_mod(
    vol: &BchVol,
    ty: AcctType,
    delta: &[i64; 3],
    _gc: bool,
) -> Result<(), StorageError> {
    let key_bpos = acct_key(ty);
    let mut counters = [0u64; 3];

    if let Some(entry) = vol.get_entry_raw(BtreeId::Accounting, key_bpos) {
        match &entry.value {
            KeyValue::Raw(bytes) => {
                if let Ok(existing) = bincode::deserialize::<AcctEntry>(bytes) {
                    counters = existing.counters;
                }
            }
            _ => {}
        }
    }

    for i in 0..3 {
        counters[i] = counters[i].wrapping_add_signed(delta[i]);
    }

    let entry = AcctEntry { counters };
    let bytes = bincode::serialize(&entry)
        .map_err(|e| StorageError::Transaction(format!("serialize acct entry: {}", e)))?;
    vol.btree(BtreeId::Accounting)
        .bch2_btree_bset_insert_key_wrapper(BtreeEntry::raw(key_bpos, KeyType::Normal, bytes), 0);

    Ok(())
}

/// Local bcachefs `bch2_dev_usage_init()` (`fs/alloc/accounting.c:1257-1289`).
/// Set the free-device counters to the member geometry instead of adding, so
/// restarting a partially completed device add remains idempotent.
pub fn bch2_dev_usage_init(vol: &BchVol, ca: &BchDev, gc: bool) -> Result<(), StorageError> {
    let member = vol
        .superblock()
        .member(ca.dev_idx)
        .ok_or_else(|| StorageError::NotFound(format!("member {} not found", ca.dev_idx)))?;
    let ty = AcctType::DevDataType(ca.dev_idx, BchDataType::Free as u8);
    let target = [member.nbuckets - u64::from(member.first_bucket), 0, 0];
    let mut current = [0; 3];

    if !gc {
        if let Some(entry) = vol.get_entry_raw(BtreeId::Accounting, acct_key(ty)) {
            if let KeyValue::Raw(bytes) = entry.value {
                if let Ok(existing) = bincode::deserialize::<AcctEntry>(&bytes) {
                    current = existing.counters;
                }
            }
        }
    }

    let delta = std::array::from_fn(|i| target[i].wrapping_sub(current[i]) as i64);
    bch2_disk_accounting_mod(vol, ty, &delta, gc)
}

/// Local bcachefs `bch2_bucket_sectors_total()` (`fs/alloc/background.h:79-82`).
#[inline]
#[allow(dead_code)]
fn bch2_bucket_sectors_total(a: BchAllocV4) -> i64 {
    i64::from(a.stripe_sectors) + i64::from(a.dirty_sectors) + i64::from(a.cached_sectors)
}

/// Local bcachefs `bch2_bucket_sectors_dirty()` (`fs/alloc/background.h:84-87`).
#[inline]
fn bch2_bucket_sectors_dirty(a: BchAllocV4) -> i64 {
    i64::from(a.stripe_sectors) + i64::from(a.dirty_sectors)
}

/// Local bcachefs `bch2_bucket_sectors()` (`fs/alloc/background.h:89-94`).
#[inline]
fn bch2_bucket_sectors(a: BchAllocV4) -> i64 {
    if a.data_type == BchDataType::Cached as u8 {
        i64::from(a.cached_sectors)
    } else {
        bch2_bucket_sectors_dirty(a)
    }
}

/// Local bcachefs `bch2_bucket_sectors_fragmented()` (`fs/alloc/background.h:96-106`).
#[inline]
fn bch2_bucket_sectors_fragmented(bucket_size: i64, a: BchAllocV4) -> i64 {
    let d = bch2_bucket_sectors(a);

    if d != 0 {
        (bucket_size - d).max(0)
    } else if !BchDataType::from_raw(a.data_type).is_some_and(data_type_is_empty) {
        bucket_size
    } else {
        0
    }
}

/// Local bcachefs `bch2_bucket_sectors_unstriped()` (`fs/alloc/background.h:108-111`).
#[inline]
fn bch2_bucket_sectors_unstriped(a: BchAllocV4) -> i64 {
    if a.data_type == BchDataType::Stripe as u8 {
        i64::from(a.dirty_sectors)
    } else {
        0
    }
}

/// Local bcachefs `bch2_dev_data_type_accounting_mod()`
/// (`fs/alloc/background.c:1168-1180`).
#[inline]
fn bch2_dev_data_type_accounting_mod(
    vol: &BchVol,
    ca: &BchDev,
    data_type: u8,
    delta_buckets: i64,
    delta_sectors: i64,
    delta_fragmented: i64,
    flags: UpdateTriggerFlags,
) -> Result<(), StorageError> {
    let d = [delta_buckets, delta_sectors, delta_fragmented];

    bch2_disk_accounting_mod(
        vol,
        AcctType::DevDataType(ca.dev_idx, data_type),
        &d,
        flags.contains(UpdateTriggerFlags::GC),
    )
}

/// Local bcachefs `bch2_alloc_key_to_dev_counters()`
/// (`fs/alloc/background.c:1182-1213`).
pub fn bch2_alloc_key_to_dev_counters(
    vol: &BchVol,
    ca: &BchDev,
    old: &BchAllocV4,
    new: &BchAllocV4,
    flags: UpdateTriggerFlags,
) -> Result<(), StorageError> {
    let bucket_size = i64::from(
        vol.superblock()
            .member(ca.dev_idx)
            .ok_or_else(|| StorageError::NotFound(format!("member {} not found", ca.dev_idx)))?
            .bucket_size,
    );

    let old_sectors = bch2_bucket_sectors(*old);
    let new_sectors = bch2_bucket_sectors(*new);
    if old.data_type != new.data_type {
        bch2_dev_data_type_accounting_mod(
            vol,
            ca,
            new.data_type,
            1,
            new_sectors,
            bch2_bucket_sectors_fragmented(bucket_size, *new),
            flags,
        )?;
        bch2_dev_data_type_accounting_mod(
            vol,
            ca,
            old.data_type,
            -1,
            -old_sectors,
            -bch2_bucket_sectors_fragmented(bucket_size, *old),
            flags,
        )?;
    } else if old_sectors != new_sectors {
        bch2_dev_data_type_accounting_mod(
            vol,
            ca,
            new.data_type,
            0,
            new_sectors - old_sectors,
            bch2_bucket_sectors_fragmented(bucket_size, *new)
                - bch2_bucket_sectors_fragmented(bucket_size, *old),
            flags,
        )?;
    }

    let old_unstriped = bch2_bucket_sectors_unstriped(*old);
    let new_unstriped = bch2_bucket_sectors_unstriped(*new);
    if old_unstriped != new_unstriped {
        bch2_dev_data_type_accounting_mod(
            vol,
            ca,
            BchDataType::Unstriped as u8,
            i64::from(new_unstriped != 0) - i64::from(old_unstriped != 0),
            new_unstriped - old_unstriped,
            0,
            flags,
        )?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alloc::btree::BCH_ALLOC_V4_ZERO;

    fn acct_counters(vol: &BchVol, ty: AcctType) -> [u64; 3] {
        let entry = vol
            .get_entry_raw(BtreeId::Accounting, acct_key(ty))
            .expect("accounting entry");
        let KeyValue::Raw(bytes) = entry.value else {
            panic!("accounting value is not raw");
        };
        bincode::deserialize::<AcctEntry>(&bytes).unwrap().counters
    }

    #[test]
    fn test_acct_key_replicas_packing() {
        // Replicas(dev=0, nr_devs=1): type=2 byte0, nr_devs=1 byte1, dev=0 byte2
        let key = acct_key(AcctType::Replicas(0, 1));
        assert_eq!(key.inode, 2 | (1u64 << 8) | (0u64 << 16));
        assert_eq!(key.offset, 0);
    }

    #[test]
    fn test_acct_entry_serialize() {
        let e = AcctEntry {
            counters: [100, 200, 300],
        };
        let bytes = bincode::serialize(&e).unwrap();
        let e2: AcctEntry = bincode::deserialize(&bytes).unwrap();
        assert_eq!(e2.counters[0], 100);
        assert_eq!(e2.counters[2], 300);
    }

    #[test]
    fn test_acct_key_dev_data_type_packing() {
        // type=3, dev=5, data_type=2
        let key = acct_key(AcctType::DevDataType(5, 2));
        assert_eq!(key.inode, 3 | (5u64 << 8) | (2u64 << 16));
        assert_eq!(key.offset, 0);
        assert_eq!(key.snapshot, 0);
    }

    #[test]
    fn test_acct_key_snapshot_packing() {
        let key = acct_key(AcctType::Snapshot(0x12345678));
        assert_eq!(key.inode, 5 | (0x12345678u64 << 8));
        assert_eq!(key.offset, 0);
    }

    #[test]
    fn test_acct_key_btree_packing() {
        let key = acct_key(AcctType::Btree(0xFF));
        assert_eq!(key.inode, 6 | (0xFFu64 << 8));
    }

    #[test]
    fn test_acct_key_compression_packing() {
        let key = acct_key(AcctType::Compression(3));
        assert_eq!(key.inode, 4 | (3u64 << 8));
    }

    #[test]
    fn test_bucket_sector_helpers_match_local_bcachefs() {
        let a = BchAllocV4 {
            data_type: BchDataType::User as u8,
            stripe_sectors: 10,
            dirty_sectors: 20,
            cached_sectors: 40,
            ..BCH_ALLOC_V4_ZERO
        };
        assert_eq!(bch2_bucket_sectors_total(a), 70);
        assert_eq!(bch2_bucket_sectors_dirty(a), 30);
        assert_eq!(bch2_bucket_sectors(a), 30);
        assert_eq!(bch2_bucket_sectors_fragmented(100, a), 70);
        assert_eq!(bch2_bucket_sectors_unstriped(a), 0);

        let cached = BchAllocV4 {
            data_type: BchDataType::Cached as u8,
            ..a
        };
        assert_eq!(bch2_bucket_sectors(cached), 40);

        let stripe = BchAllocV4 {
            data_type: BchDataType::Stripe as u8,
            ..a
        };
        assert_eq!(bch2_bucket_sectors_unstriped(stripe), 20);

        let nonempty_zero = BchAllocV4 {
            data_type: BchDataType::User as u8,
            ..BCH_ALLOC_V4_ZERO
        };
        assert_eq!(bch2_bucket_sectors_fragmented(100, nonempty_zero), 100);
        assert_eq!(bch2_bucket_sectors_fragmented(100, BCH_ALLOC_V4_ZERO), 0);
    }

    #[test]
    fn test_alloc_key_to_dev_counters_type_transition_new_then_old() {
        let vol = BchVol::test_trees();
        let ca = vol.device_rcu_noerror(0).unwrap();
        bch2_disk_accounting_mod(
            &vol,
            AcctType::DevDataType(0, BchDataType::Free as u8),
            &[1, 0, 0],
            true,
        )
        .unwrap();

        let new = BchAllocV4 {
            data_type: BchDataType::User as u8,
            dirty_sectors: 100,
            ..BCH_ALLOC_V4_ZERO
        };
        bch2_alloc_key_to_dev_counters(&vol, &ca, &BCH_ALLOC_V4_ZERO, &new, UpdateTriggerFlags::GC)
            .unwrap();

        assert_eq!(
            acct_counters(&vol, AcctType::DevDataType(0, BchDataType::User as u8)),
            [1, 100, 1948]
        );
        assert_eq!(
            acct_counters(&vol, AcctType::DevDataType(0, BchDataType::Free as u8)),
            [0, 0, 0]
        );
    }

    #[test]
    fn test_alloc_key_to_dev_counters_same_type_delta() {
        let vol = BchVol::test_trees();
        let ca = vol.device_rcu_noerror(0).unwrap();
        bch2_disk_accounting_mod(
            &vol,
            AcctType::DevDataType(0, BchDataType::User as u8),
            &[1, 100, 1948],
            false,
        )
        .unwrap();

        let old = BchAllocV4 {
            data_type: BchDataType::User as u8,
            dirty_sectors: 100,
            ..BCH_ALLOC_V4_ZERO
        };
        let new = BchAllocV4 {
            dirty_sectors: 300,
            ..old
        };
        bch2_alloc_key_to_dev_counters(&vol, &ca, &old, &new, UpdateTriggerFlags::TRANSACTIONAL)
            .unwrap();

        assert_eq!(
            acct_counters(&vol, AcctType::DevDataType(0, BchDataType::User as u8)),
            [1, 300, 1748]
        );
    }

    #[test]
    fn test_alloc_key_to_dev_counters_uses_member_bucket_size() {
        let vol = BchVol::test_trees();
        vol.superblock_mut().member_mut(0).unwrap().bucket_size = 100;
        let ca = vol.device_rcu_noerror(0).unwrap();
        bch2_disk_accounting_mod(
            &vol,
            AcctType::DevDataType(0, BchDataType::Free as u8),
            &[1, 0, 0],
            false,
        )
        .unwrap();

        let new = BchAllocV4 {
            data_type: BchDataType::User as u8,
            dirty_sectors: 30,
            ..BCH_ALLOC_V4_ZERO
        };
        bch2_alloc_key_to_dev_counters(
            &vol,
            &ca,
            &BCH_ALLOC_V4_ZERO,
            &new,
            UpdateTriggerFlags::TRANSACTIONAL,
        )
        .unwrap();

        assert_eq!(
            acct_counters(&vol, AcctType::DevDataType(0, BchDataType::User as u8)),
            [1, 30, 70]
        );
    }

    #[test]
    fn test_alloc_key_to_dev_counters_stripe_unstriped_delta() {
        let vol = BchVol::test_trees();
        let ca = vol.device_rcu_noerror(0).unwrap();
        bch2_disk_accounting_mod(
            &vol,
            AcctType::DevDataType(0, BchDataType::Free as u8),
            &[1, 0, 0],
            true,
        )
        .unwrap();

        let stripe = BchAllocV4 {
            data_type: BchDataType::Stripe as u8,
            dirty_sectors: 100,
            stripe_sectors: 200,
            ..BCH_ALLOC_V4_ZERO
        };
        bch2_alloc_key_to_dev_counters(
            &vol,
            &ca,
            &BCH_ALLOC_V4_ZERO,
            &stripe,
            UpdateTriggerFlags::GC | UpdateTriggerFlags::INSERT,
        )
        .unwrap();

        assert_eq!(
            acct_counters(&vol, AcctType::DevDataType(0, BchDataType::Stripe as u8)),
            [1, 300, 1748]
        );
        assert_eq!(
            acct_counters(&vol, AcctType::DevDataType(0, BchDataType::Unstriped as u8)),
            [1, 100, 0]
        );

        bch2_alloc_key_to_dev_counters(
            &vol,
            &ca,
            &stripe,
            &BCH_ALLOC_V4_ZERO,
            UpdateTriggerFlags::GC,
        )
        .unwrap();
        assert_eq!(
            acct_counters(&vol, AcctType::DevDataType(0, BchDataType::Unstriped as u8)),
            [0, 0, 0]
        );
    }
}
