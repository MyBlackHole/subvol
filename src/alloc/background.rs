use crate::bcachefs::*;
use crate::bcachefs_format::*;
use crate::alloc::buckets::*;
use crate::btree::types::*;
use crate::errcode::*;

pub fn dev_bucket_exists(c: &BchFs, pos: Bpos) -> bool {
    if let Some(ca) = c.devs.get(pos.inode as usize).and_then(|d| d.as_ref()) {
        bucket_valid(ca, pos.offset)
    } else {
        false
    }
}

pub fn bucket_to_u64(bucket: Bpos) -> u64 {
    (bucket.inode << 48) | (bucket.offset & !(!0u64 << 48))
}

pub fn u64_to_bucket(v: u64) -> Bpos {
    Bpos::pos(v >> 48, v & !(!0u64 << 48))
}

pub fn alloc_gc_gen(a: &BchAllocV4) -> u8 {
    a.gen.wrapping_sub(a.oldest_gen)
}

pub fn bucket_data_type(data_type: BchDataType) -> BchDataType {
    match data_type {
        BchDataType::Cached | BchDataType::Stripe => BchDataType::User,
        _ => data_type,
    }
}

pub fn bucket_data_type_mismatch(bucket: BchDataType, ptr: BchDataType) -> bool {
    !data_type_is_empty(bucket) && bucket_data_type(bucket) != bucket_data_type(ptr)
}

pub fn bch2_bucket_sectors_total(a: &BchAllocV4) -> i64 {
    a.stripe_sectors as i64 + a.dirty_sectors as i64 + a.cached_sectors as i64
}

pub fn bch2_bucket_sectors_dirty(a: &BchAllocV4) -> i64 {
    a.stripe_sectors as i64 + a.dirty_sectors as i64
}

pub fn bucket_sectors(a: &BchAllocV4) -> i64 {
    if a.data_type() == BchDataType::Cached {
        a.cached_sectors as i64
    } else {
        bch2_bucket_sectors_dirty(a)
    }
}

pub fn bucket_sectors_fragmented(ca: &BchDev, a: &BchAllocV4) -> i64 {
    let d = bucket_sectors(a);
    if d > 0 {
        let bucket_size = ca.bucket_size as i64;
        std::cmp::max(0, bucket_size - d)
    } else if !data_type_is_empty(a.data_type()) {
        ca.bucket_size as i64
    } else {
        0
    }
}

pub fn bucket_sectors_unstriped(a: &BchAllocV4) -> i64 {
    if a.data_type() == BchDataType::Stripe {
        a.dirty_sectors as i64
    } else {
        0
    }
}

pub fn alloc_data_type(a: &BchAllocV4, data_type: BchDataType) -> BchDataType {
    if a.stripe_refcount > 0 {
        if data_type == BchDataType::Parity {
            data_type
        } else {
            BchDataType::Stripe
        }
    } else if bch2_bucket_sectors_dirty(a) > 0 {
        bucket_data_type(data_type)
    } else if a.cached_sectors > 0 {
        BchDataType::Cached
    } else if data_type == BchDataType::NeedDiscard {
        BchDataType::NeedDiscard
    } else if alloc_gc_gen(a) >= BUCKET_GC_GEN_MAX {
        BchDataType::NeedGcGens
    } else {
        BchDataType::Free
    }
}

pub fn alloc_data_type_set(a: &mut BchAllocV4, data_type: BchDataType) {
    a.data_type = alloc_data_type(a, data_type) as u8;
}

pub fn alloc_lru_idx_read(a: &BchAllocV4) -> u64 {
    if a.data_type() == BchDataType::Cached {
        a.io_time[0] & LRU_TIME_MAX
    } else {
        0
    }
}

pub fn alloc_lru_idx_fragmentation(a: &BchAllocV4, ca: &BchDev) -> u64 {
    if a.data_type as usize >= BCH_DATA_NR {
        return 0;
    }
    if !data_type_movable(a.data_type()) || bucket_sectors_fragmented(ca, a) == 0 {
        return 0;
    }
    let d = std::cmp::min(bch2_bucket_sectors_dirty(a), ca.bucket_size as i64);
    (d as u64 * (1u64 << 31)) / ca.bucket_size as u64
}

pub fn alloc_freespace_genbits(a: &BchAllocV4) -> u64 {
    ((alloc_gc_gen(a) as u64) >> 4) << 56
}

pub fn alloc_freespace_pos(pos: Bpos, a: &BchAllocV4) -> Bpos {
    let mut p = pos;
    p.offset |= alloc_freespace_genbits(a);
    p
}

pub fn bch2_bucket_io_time_reset(
    _trans: &mut BtreeTrans,
    _dev: u32,
    _bucket: u64,
    _io_type: i32,
) -> BchResult<()> {
    Ok(())
}

pub fn bch2_alloc_read(_c: &mut BchFs) -> BchResult<()> {
    Ok(())
}

pub fn bch2_recalc_capacity(_c: &mut BchFs) {}

pub fn bch2_fs_ra_pages(_c: &BchFs) -> u64 {
    0
}

pub fn bch2_min_rw_member_capacity(_c: &BchFs) -> u64 {
    0
}

pub fn bch2_dev_allocator_set_rw(_c: &mut BchFs, _ca: &mut BchDev, _rw: bool) {}

pub fn bch2_dev_allocator_remove(_c: &mut BchFs, _ca: &mut BchDev) {}

pub fn bch2_dev_allocator_add(_c: &mut BchFs, _ca: &mut BchDev) {}

pub fn bch2_fs_allocator_background_init(_c: &mut BchFs) {}

pub fn bch2_fs_capacity_exit(_c: &mut BchFs) {}

pub fn bch2_fs_capacity_init(_c: &mut BchFs) -> BchResult<()> {
    Ok(())
}
