use crate::bcachefs::*;
use crate::bcachefs_format::*;
use crate::errcode::*;

pub const BUCKET_JOURNAL_SEQ_BITS: u32 = 16;

pub const BCH_WATERMARK_BITS: u32 = 3;
pub const BCH_WATERMARK_MASK: u32 = !(!0u32 << BCH_WATERMARK_BITS);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BchWatermark {
    Stripe,
    Normal,
    Copygc,
    Btree,
    BtreeCopygc,
    Reclaim,
    InteriorUpdates,
}

pub const BCH_WATERMARK_NR: usize = 7;

pub const BUCKET_GC_GEN_MAX: u8 = 96;

pub const OPEN_BUCKETS_COUNT: usize = 4096;

pub const WRITE_POINT_HASH_NR: usize = 32;
pub const WRITE_POINT_MAX: usize = 32;

#[derive(Clone, Debug)]
pub struct BucketGens {
    pub first_bucket: u16,
    pub nbuckets: usize,
    pub nbuckets_minus_first: usize,
    pub b: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DiskReservation {
    pub sectors: u64,
    pub gen: u32,
    pub nr_replicas: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct BchFsCapacity {
    pub capacity: u64,
    pub reserved: u64,
    pub capacity_gen: u32,
    pub bucket_size_max: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct DevStripeState {
    pub next_alloc: [u64; BCH_SB_MEMBERS_MAX],
    pub cached_devs: BchDevsMask,
}

impl DevStripeState {
    pub fn new() -> Self {
        DevStripeState {
            next_alloc: [0; BCH_SB_MEMBERS_MAX],
            cached_devs: BchDevsMask::new(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct OpenBuckets {
    pub nr: OpenBucketIdx,
    pub v: [OpenBucketIdx; BCH_BKEY_PTRS_MAX as usize],
}

impl OpenBuckets {
    pub fn new() -> Self {
        OpenBuckets {
            nr: 0,
            v: [0; BCH_BKEY_PTRS_MAX as usize],
        }
    }
}

pub fn sector_to_bucket(ca: &BchDev, s: u64) -> u64 {
    s / ca.bucket_size as u64
}

pub fn bucket_to_sector(ca: &BchDev, b: u64) -> u64 {
    b * ca.bucket_size as u64
}

pub fn bucket_remainder(ca: &BchDev, s: u64) -> u64 {
    s % ca.bucket_size as u64
}

pub fn sector_to_bucket_and_offset(ca: &BchDev, s: u64, offset: &mut u32) -> u64 {
    let bucket_size = ca.bucket_size as u64;
    let b = s / bucket_size;
    *offset = (s - b * bucket_size) as u32;
    b
}

pub fn bucket_valid(ca: &BchDev, b: u64) -> bool {
    b >= ca.first_bucket && b < ca.nbuckets
}

pub fn ptr_bucket_nr(ca: &BchDev, ptr: &BchExtentPtr) -> u64 {
    sector_to_bucket(ca, ptr.offset)
}

pub fn ptr_bucket_pos(ca: &BchDev, ptr: &BchExtentPtr) -> Bpos {
    Bpos::pos(ptr.dev as u64, ptr_bucket_nr(ca, ptr))
}

pub fn gen_cmp(a: u8, b: u8) -> i8 {
    (a as i8).wrapping_sub(b as i8)
}

pub fn gen_after(a: u8, b: u8) -> u8 {
    let cmp = gen_cmp(a, b);
    if cmp > 0 { cmp as u8 } else { 0 }
}

pub fn alloc_to_bucket(dst: &mut Bucket, src: &BchAllocV4) {
    dst.gen = src.gen;
    dst.data_type = src.data_type;
    dst.dirty_sectors = src.dirty_sectors as u16;
    dst.cached_sectors = src.cached_sectors as u16;
    dst.stripe = src.stripe_refcount as u64;
    dst.nr_extents = src.nr_external_backpointers;
}

pub fn bucket_m_to_alloc(src: &Bucket) -> BchAllocV4 {
    let mut ret = BchAllocV4::default();
    ret.gen = src.gen;
    ret.data_type = src.data_type;
    ret.dirty_sectors = src.dirty_sectors as u32;
    ret.cached_sectors = src.cached_sectors as u32;
    ret.stripe_refcount = src.stripe as u32;
    ret.nr_external_backpointers = src.nr_extents;
    ret
}

pub fn ptr_data_type(k: &Bkey, _ptr: &BchExtentPtr) -> BchDataType {
    use crate::bcachefs_format::BchBkeyType;
    if matches!(k.type_, BchBkeyType::BtreePtr | BchBkeyType::BtreePtrV2) {
        BchDataType::Btree
    } else {
        BchDataType::User
    }
}

pub fn dev_ptr_stale(ca: &BchDev, ptr: &BchExtentPtr) -> i32 {
    let bucket_nr = ptr_bucket_nr(ca, ptr);
    if !bucket_valid(ca, bucket_nr) {
        return -1;
    }
    let gen = bucket_gen(ca, bucket_nr);
    match gen {
        Some(g) => gen_after(g, ptr.gen) as i32,
        None => -1,
    }
}

pub fn bucket_gen(ca: &BchDev, b: u64) -> Option<u8> {
    if b < ca.first_bucket || b >= ca.nbuckets {
        return None;
    }
    Some(0)
}

pub fn dev_buckets_reserved(ca: &BchDev, watermark: BchWatermark) -> u64 {
    let mut reserved: i64 = 0;
    match watermark {
        BchWatermark::Stripe => {
            reserved += (ca.nbuckets >> 6) as i64;
            reserved += (ca.nbuckets >> 6) as i64;
            reserved += ca.nr_btree_reserve as i64;
            reserved += ca.nr_btree_reserve as i64;
        }
        BchWatermark::Normal => {
            reserved += (ca.nbuckets >> 6) as i64;
            reserved += ca.nr_btree_reserve as i64;
            reserved += ca.nr_btree_reserve as i64;
        }
        BchWatermark::Copygc => {
            reserved += ca.nr_btree_reserve as i64;
            reserved += ca.nr_btree_reserve as i64;
        }
        BchWatermark::Btree => {
            reserved += ca.nr_btree_reserve as i64;
        }
        BchWatermark::BtreeCopygc | BchWatermark::Reclaim | BchWatermark::InteriorUpdates => {}
    }
    if reserved < 0 { 0 } else { reserved as u64 }
}

pub fn __dev_buckets_free(ca: &BchDev, usage: &BchDevUsage, watermark: BchWatermark) -> u64 {
    let free = usage.buckets[BchDataType::Free as usize] as i64;
    let reserved = dev_buckets_reserved(ca, watermark) as i64;
    let nr_open = ca.nr_open_buckets as i64;
    let val = free - nr_open - reserved;
    if val < 0 { 0 } else { val as u64 }
}

pub fn __dev_buckets_available(ca: &BchDev, usage: &BchDevUsage, watermark: BchWatermark) -> u64 {
    let free = usage.buckets[BchDataType::Free as usize] as i64;
    let cached = usage.buckets[BchDataType::Cached as usize] as i64;
    let need_gc_gens = usage.buckets[BchDataType::NeedGcGens as usize] as i64;
    let need_discard = usage.buckets[BchDataType::NeedDiscard as usize] as i64;
    let reserved = dev_buckets_reserved(ca, watermark) as i64;
    let nr_open = ca.nr_open_buckets as i64;
    let val = free + cached + need_gc_gens + need_discard - nr_open - reserved;
    if val < 0 { 0 } else { val as u64 }
}

pub fn disk_reservation_init(_c: &BchFs, nr_replicas: u32) -> DiskReservation {
    DiskReservation {
        sectors: 0,
        gen: 0,
        nr_replicas,
    }
}

pub fn disk_reservation_put(c: &mut BchFs, res: &mut DiskReservation) {
    if res.sectors > 0 {
        c.capacity.reserved = c.capacity.reserved.saturating_sub(res.sectors);
        res.sectors = 0;
    }
}

pub const RESERVE_FACTOR: u64 = 6;

pub fn avail_factor(r: u64) -> u64 {
    (r << RESERVE_FACTOR) / ((1 << RESERVE_FACTOR) + 1)
}

pub fn data_type_is_empty(data_type: BchDataType) -> bool {
    matches!(data_type, BchDataType::Free | BchDataType::NeedGcGens | BchDataType::NeedDiscard)
}

pub fn data_type_is_hidden(data_type: BchDataType) -> bool {
    matches!(data_type, BchDataType::Sb | BchDataType::Journal)
}

pub fn data_type_movable(data_type: BchDataType) -> bool {
    matches!(data_type, BchDataType::Btree | BchDataType::User | BchDataType::Stripe)
}

pub const DATA_TYPES_MOVABLE: u32 = (1 << 3) | (1 << 5) | (1 << 7);

pub mod watermark {
    use super::*;

    pub fn open_buckets_reserved(watermark: BchWatermark) -> usize {
        match watermark {
            BchWatermark::InteriorUpdates => 0,
            BchWatermark::Reclaim => OPEN_BUCKETS_COUNT / 6,
            BchWatermark::Btree | BchWatermark::BtreeCopygc => OPEN_BUCKETS_COUNT / 4,
            BchWatermark::Copygc => OPEN_BUCKETS_COUNT / 3,
            _ => OPEN_BUCKETS_COUNT / 2,
        }
    }
}
