use crate::bcachefs::*;
use crate::bcachefs_format::*;
use crate::alloc::buckets::*;
use crate::btree::types::*;
use crate::errcode::*;

pub const WRITE_POINT_STATE_NR: usize = 5;

pub struct WritePointSpecifier {
    pub v: u64,
}

#[derive(Clone, Debug)]
pub struct DevAllocList {
    pub nr: u32,
    pub data: [u8; BCH_SB_MEMBERS_MAX],
}

impl DevAllocList {
    pub fn new() -> Self {
        DevAllocList {
            nr: 0,
            data: [0; BCH_SB_MEMBERS_MAX],
        }
    }
}

#[derive(Clone, Debug)]
pub struct AllocTraceEntry {
    pub dev: u8,
    pub new_stripe_alloc: bool,
    pub will_retry_all_devices: bool,
    pub will_retry_target_devices: bool,
    pub will_retry_set_devices: bool,
    pub copygc_can_make_progress: bool,
    pub have_cl: bool,
    pub err: i16,
    pub wake_counter_snapshot: u32,
    pub free_buckets: u64,
}

#[derive(Clone, Debug)]
pub enum BtreeBitmap {
    No,
    Yes,
    Any,
}

#[derive(Clone, Debug)]
pub struct AllocCounters {
    pub buckets_seen: u64,
    pub skipped_open: u64,
    pub skipped_need_journal_commit: u64,
    pub need_journal_commit: u64,
    pub skipped_nocow: u64,
    pub skipped_nouse: u64,
    pub skipped_mi_btree_bitmap: u64,
}

#[derive(Clone, Debug)]
pub struct AllocRequest {
    pub nr_replicas: u8,
    pub ec_replicas: u8,
    pub ec_max_data_blocks: u8,
    pub target: u32,
    pub ec: bool,
    pub new_stripe_alloc: bool,
    pub will_retry_all_devices: bool,
    pub will_retry_target_devices: bool,
    pub will_retry_set_devices: bool,
    pub copygc_can_make_progress: bool,
    pub trace_alloc_failed: bool,
    pub watermark: BchWatermark,
    pub data_type: BchDataType,
    pub ptrs: OpenBuckets,
    pub nr_effective: u32,
    pub have_cache: bool,
    pub devs_may_alloc: BchDevsMask,
    pub devs_sorted: DevAllocList,
    pub usage: BchDevUsage,
    pub btree_bitmap: BtreeBitmap,
    pub counters: AllocCounters,
    pub trace: Vec<AllocTraceEntry>,
}

impl AllocRequest {
    pub fn new() -> Self {
        AllocRequest {
            nr_replicas: 0,
            ec_replicas: 0,
            ec_max_data_blocks: 0,
            target: 0,
            ec: false,
            new_stripe_alloc: false,
            will_retry_all_devices: false,
            will_retry_target_devices: false,
            will_retry_set_devices: false,
            copygc_can_make_progress: false,
            trace_alloc_failed: false,
            watermark: BchWatermark::Normal,
            data_type: BchDataType::User,
            ptrs: OpenBuckets::new(),
            nr_effective: 0,
            have_cache: false,
            devs_may_alloc: BchDevsMask::new(),
            devs_sorted: DevAllocList::new(),
            usage: BchDevUsage::default(),
            btree_bitmap: BtreeBitmap::Any,
            counters: AllocCounters {
                buckets_seen: 0,
                skipped_open: 0,
                skipped_need_journal_commit: 0,
                need_journal_commit: 0,
                skipped_nocow: 0,
                skipped_nouse: 0,
                skipped_mi_btree_bitmap: 0,
            },
            trace: Vec::new(),
        }
    }
}

pub fn writepoint_hashed(v: u64) -> WritePointSpecifier {
    WritePointSpecifier { v: v | 1 }
}

pub fn writepoint_ptr(wp: &WritePoint) -> WritePointSpecifier {
    WritePointSpecifier { v: wp as *const WritePoint as u64 }
}

pub fn ob_push(c: &mut BchFs, obs: &mut OpenBuckets, ob: &OpenBucket) {
    let ob_idx = (ob as *const OpenBucket as usize
        - c.allocator.open_buckets.as_ptr() as usize)
        / std::mem::size_of::<OpenBucket>();
    obs.v[obs.nr as usize] = ob_idx as OpenBucketIdx;
    obs.nr += 1;
}

pub fn open_bucket_hashslot(c: &BchFs, dev: u32, bucket: u64) -> usize {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    dev.hash(&mut hasher);
    bucket.hash(&mut hasher);
    let hash = hasher.finish() as usize;
    hash & (OPEN_BUCKETS_COUNT - 1)
}

pub fn bucket_is_open(c: &BchFs, dev: u32, bucket: u64) -> Option<&OpenBucket> {
    let slot = open_bucket_hashslot(c, dev, bucket);
    let mut idx = c.allocator.open_buckets_hash[slot];
    while idx != 0 {
        let ob = &c.allocator.open_buckets[idx as usize];
        if ob.dev as u32 == dev && ob.bucket == bucket {
            return Some(ob);
        }
        idx = ob.hash;
    }
    None
}

pub fn ob_dev<'a>(c: &'a BchFs, ob: &OpenBucket) -> &'a BchDev {
    &c.devs[ob.dev as usize].as_ref().unwrap()
}

pub fn ob_ptr(c: &BchFs, ob: &OpenBucket) -> BchExtentPtr {
    let ca = ob_dev(c, ob);
    BchExtentPtr {
        dev: ob.dev as u32,
        gen: ob.gen as u32,
        offset: bucket_to_sector(ca, ob.bucket) + ca.bucket_size as u64 - ob.sectors_free as u64,
    }
}

pub fn alloc_sectors_append_ptrs_inlined(
    c: &mut BchFs,
    wp: &mut WritePoint,
    _k: &mut BkeyI,
    sectors: u32,
    _cached: bool,
) {
    wp.sectors_free = wp.sectors_free.saturating_sub(sectors);
    wp.sectors_allocated += sectors as u64;

    for i in 0..wp.ptrs.nr as usize {
        let ob_idx = wp.ptrs.v[i] as usize;
        let ob = &mut c.allocator.open_buckets[ob_idx];
        ob.sectors_free = ob.sectors_free.saturating_sub(sectors);
    }
}

pub fn alloc_sectors_done_inlined(c: &mut BchFs, wp: &mut WritePoint) {
    let mut ptrs = OpenBuckets::new();
    let mut keep = OpenBuckets::new();

    for i in 0..wp.ptrs.nr as usize {
        let ob_idx = wp.ptrs.v[i] as usize;
        let ob = &c.allocator.open_buckets[ob_idx];
        let block_sectors = c.block_sectors();
        if ob.sectors_free < block_sectors {
            ob_push(c, &mut ptrs, ob);
        } else {
            ob_push(c, &mut keep, ob);
        }
    }
    wp.ptrs = keep;
    wp.sectors_free = wp.prev_sectors_free - wp.sectors_free;
}

pub fn open_bucket_get(c: &mut BchFs, wp: &mut WritePoint, ptrs: &mut OpenBuckets) {
    for i in 0..wp.ptrs.nr as usize {
        let ob_idx = wp.ptrs.v[i] as usize;
        let ob = &mut c.allocator.open_buckets[ob_idx];
        ob.data_type = wp.data_type;
        ob_push(c, ptrs, ob);
    }
}

pub fn open_buckets_put(c: &mut BchFs, ptrs: &mut OpenBuckets) {
    ptrs.nr = 0;
}

pub fn ec_open_bucket<'a>(c: &'a BchFs, obs: &OpenBuckets) -> Option<&'a OpenBucket> {
    for i in 0..obs.nr as usize {
        let ob = &c.allocator.open_buckets[obs.v[i] as usize];
        if ob.ec_idx != 0 {
            return Some(ob);
        }
    }
    None
}

pub fn open_bucket_for_each<'a, 'b: 'a>(
    c: &'a BchFs,
    obs: &'b OpenBuckets,
) -> impl Iterator<Item = &'a OpenBucket> + 'a {
    (0..obs.nr as usize).map(move |i| &c.allocator.open_buckets[obs.v[i] as usize])
}

pub fn alloc_trace_add(
    req: &mut AllocRequest,
    dev: u8,
    err: i16,
    wake_counter_snapshot: u32,
    free_buckets: u64,
    copygc_can_make_progress: bool,
) -> i16 {
    req.trace.push(AllocTraceEntry {
        dev,
        new_stripe_alloc: req.new_stripe_alloc,
        will_retry_all_devices: req.will_retry_all_devices,
        will_retry_target_devices: req.will_retry_target_devices,
        will_retry_set_devices: req.will_retry_set_devices,
        copygc_can_make_progress,
        have_cl: false,
        err,
        wake_counter_snapshot,
        free_buckets,
    });
    if req.trace.len() > 16 {
        req.trace_alloc_failed = true;
    }
    err
}
