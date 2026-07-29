use core::cell::UnsafeCell;
use std::collections::VecDeque;
use std::sync::atomic::{
    AtomicBool, AtomicI32, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering,
};
use std::sync::Mutex;

pub const JOURNAL_SEQ_MAX: u64 = (1u64 << 56) - 1;
pub const JOURNAL_STATE_BUF_BITS: u32 = 2;
pub const JOURNAL_STATE_BUF_NR: usize = 1 << JOURNAL_STATE_BUF_BITS;
pub const JOURNAL_STATE_BUF_MASK: u64 = JOURNAL_STATE_BUF_NR as u64 - 1;
pub const JOURNAL_ENTRY_SIZE_MIN: usize = 64 << 10;
pub const JOURNAL_ENTRY_OFFSET_MAX: u32 = (1 << 22) - 1;
pub const JSET_MAGIC: u64 = 0x245235c1a3625032;
pub const BCH_JSET_ENTRY_btree_keys: u8 = 0;
pub const BCH_JSET_ENTRY_btree_root: u8 = 1;
pub const BCH_JSET_ENTRY_overwrite: u8 = 10;
pub const BCH_JSET_ENTRY_write_buffer_keys: u8 = 11;
pub const BCH_JSET_ENTRY_log: u8 = 9;
pub const BCH_JSET_ENTRY_log_bkey: u8 = 13;
pub const JSET_KEYS_U64s: u32 = 1;
pub const JSET_HEADER_U64S: usize = 7;
pub const JOURNAL_degraded: usize = 0;
pub const JOURNAL_replay_done: usize = 1;
pub const JOURNAL_running: usize = 2;
pub const JOURNAL_may_skip_flush: usize = 3;
pub const JOURNAL_need_flush_write: usize = 4;
pub const JOURNAL_med_on_space: usize = 5;
pub const JOURNAL_low_on_space: usize = 6;
pub const JOURNAL_low_on_pin: usize = 7;
pub const JOURNAL_low_on_wb: usize = 8;
pub const JOURNAL_PIN: usize = 32 * 1024;
pub const BCH_WATERMARK_BITS: u32 = 3;
pub const BCH_WATERMARK_MASK: u32 = (1 << BCH_WATERMARK_BITS) - 1;

#[repr(C, align(8))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct bch_csum {
    pub lo: u64,
    pub hi: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct jset_entry {
    pub u64s: u16,
    pub btree_id: u8,
    pub level: u8,
    pub type_: u8,
    pub pad: [u8; 3],
}

#[repr(C, align(8))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct jset {
    pub csum: bch_csum,
    pub magic: u64,
    pub seq: u64,
    pub version: u32,
    pub flags: u32,
    pub u64s: u32,
    pub _read_clock: u16,
    pub _write_clock: u16,
    pub last_seq: u64,
}

#[allow(non_snake_case)]
pub const fn JSET_CSUM_TYPE(j: &jset) -> u32 {
    j.flags & 0xf
}

#[allow(non_snake_case)]
pub const fn JSET_BIG_ENDIAN(j: &jset) -> u32 {
    (j.flags >> 4) & 1
}

#[allow(non_snake_case)]
pub const fn JSET_NO_FLUSH(j: &jset) -> u32 {
    (j.flags >> 5) & 1
}

#[allow(non_snake_case)]
pub fn SET_JSET_NO_FLUSH(j: &mut jset, value: u32) {
    j.flags = (j.flags & !(1 << 5)) | ((value & 1) << 5);
}

pub unsafe fn journal_entry_empty(j: *const jset) -> bool {
    if (*j).seq != (*j).last_seq {
        return false;
    }

    let mut entry = j.cast::<u64>().add(JSET_HEADER_U64S).cast::<jset_entry>();
    let end = j
        .cast::<u64>()
        .add(JSET_HEADER_U64S + (*j).u64s as usize)
        .cast::<jset_entry>();
    while entry < end {
        if (*entry).type_ == BCH_JSET_ENTRY_btree_keys && (*entry).u64s != 0 {
            return false;
        }
        entry = entry
            .cast::<u64>()
            .add(jset_u64s((*entry).u64s as u32) as usize)
            .cast();
    }
    true
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct journal_res {
    pub ref_: bool,
    pub has_overwrites: bool,
    pub u64s: u16,
    pub offset: u32,
    pub seq: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct journal_start_info {
    pub last_seq: u64,
    pub replay_end: u64,
    pub cur_seq: u64,
    pub clean: bool,
}

#[repr(u8)]
#[allow(non_camel_case_types)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum journal_space_from {
    journal_space_discarded,
    journal_space_clean_ondisk,
    journal_space_clean,
    journal_space_total,
    journal_space_nr,
}

#[repr(C)]
#[allow(non_camel_case_types)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct journal_space {
    pub next_entry: u32,
    pub total: u32,
}

#[repr(u8)]
#[allow(non_camel_case_types)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum bch_watermark {
    BCH_WATERMARK_stripe,
    BCH_WATERMARK_normal,
    BCH_WATERMARK_copygc,
    BCH_WATERMARK_btree,
    BCH_WATERMARK_btree_copygc,
    BCH_WATERMARK_reclaim,
    BCH_WATERMARK_interior_updates,
}

#[allow(non_camel_case_types)]
#[derive(Debug, Default)]
pub struct journal_device {
    pub bucket_seq: Vec<u64>,
    pub sectors_free: u32,
    pub discard_idx: u32,
    pub dirty_idx_ondisk: u32,
    pub dirty_idx: u32,
    pub cur_idx: u32,
    pub nr: u32,
    pub buckets: Vec<u64>,
    pub highest_seq_found: u64,
}

#[repr(u8)]
#[allow(non_camel_case_types)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum journal_pin_type {
    JOURNAL_PIN_TYPE_btree3,
    JOURNAL_PIN_TYPE_btree2,
    JOURNAL_PIN_TYPE_btree1,
    JOURNAL_PIN_TYPE_btree0,
    JOURNAL_PIN_TYPE_key_cache,
    JOURNAL_PIN_TYPE_other,
}

pub const JOURNAL_PIN_TYPE_NR: usize = 6;

#[allow(non_camel_case_types)]
#[derive(Debug, Default)]
pub struct journal_entry_pin_list {
    pub count: u32,
    pub unflushed: [Vec<usize>; JOURNAL_PIN_TYPE_NR],
    pub flushed: Vec<usize>,
    pub unreplayed: bool,
    pub devs_nr: u8,
    pub devs: [u8; crate::btree::types::BCH_BKEY_PTRS_MAX],
    pub bytes: u32,
}

pub struct journal_buf {
    pub data: UnsafeCell<Box<[u64]>>,
    pub seq: AtomicU64,
    pub has_overwrites: AtomicBool,
}

unsafe impl Sync for journal_buf {}

impl Default for journal_buf {
    fn default() -> Self {
        Self {
            data: UnsafeCell::new(
                vec![0; JOURNAL_ENTRY_SIZE_MIN / core::mem::size_of::<u64>()].into_boxed_slice(),
            ),
            seq: AtomicU64::new(0),
            has_overwrites: AtomicBool::new(false),
        }
    }
}

pub struct journal {
    pub reservations: AtomicU64,
    pub seq: AtomicU64,
    pub cur_entry_u64s: AtomicU32,
    pub ring: [journal_buf; JOURNAL_STATE_BUF_NR],
    pub closed: Mutex<Vec<Vec<u64>>>,
    pub cycle_lock: Mutex<()>,
    pub disk_sb: AtomicPtr<crate::sb::bch_sb_handle>,
    pub device: Mutex<journal_device>,
    pub pin: Mutex<(u64, VecDeque<journal_entry_pin_list>)>,
    pub seq_ondisk: AtomicU64,
    pub last_seq_ondisk: AtomicU64,
    pub last_seq: AtomicU64,
    pub reclaim_lock: Mutex<()>,
    pub flush_in_progress: AtomicUsize,
    pub flush_in_progress_dropped: AtomicBool,
    pub nr_direct_reclaim: AtomicU64,
    pub nr_background_reclaim: AtomicU64,
    pub flags: AtomicUsize,
    pub watermark: AtomicU32,
    pub space: Mutex<[journal_space; journal_space_from::journal_space_nr as usize]>,
    pub cur_entry_sectors: AtomicU32,
    pub cur_entry_error: AtomicI32,
    pub reclaim_kicked: AtomicBool,
}

impl Default for journal {
    fn default() -> Self {
        let ring = core::array::from_fn(|_| journal_buf::default());
        let seq = 1u64;
        ring[seq as usize & (JOURNAL_STATE_BUF_NR - 1)]
            .seq
            .store(seq, Ordering::Relaxed);
        let idx = seq & JOURNAL_STATE_BUF_MASK;
        let state = idx << 22 | 1u64 << (24 + idx * 10);
        let mut pin = VecDeque::new();
        pin.push_back(journal_entry_pin_list {
            count: 1,
            ..Default::default()
        });
        Self {
            reservations: AtomicU64::new(state),
            seq: AtomicU64::new(seq),
            cur_entry_u64s: AtomicU32::new(
                (JOURNAL_ENTRY_SIZE_MIN / core::mem::size_of::<u64>() - JSET_HEADER_U64S) as u32,
            ),
            ring,
            closed: Mutex::new(Vec::new()),
            cycle_lock: Mutex::new(()),
            disk_sb: AtomicPtr::new(core::ptr::null_mut()),
            device: Mutex::new(journal_device::default()),
            pin: Mutex::new((seq, pin)),
            seq_ondisk: AtomicU64::new(0),
            last_seq_ondisk: AtomicU64::new(seq),
            last_seq: AtomicU64::new(seq),
            reclaim_lock: Mutex::new(()),
            flush_in_progress: AtomicUsize::new(0),
            flush_in_progress_dropped: AtomicBool::new(false),
            nr_direct_reclaim: AtomicU64::new(0),
            nr_background_reclaim: AtomicU64::new(0),
            flags: AtomicUsize::new(0),
            watermark: AtomicU32::new(bch_watermark::BCH_WATERMARK_stripe as u32),
            space: Mutex::new(
                [journal_space::default(); journal_space_from::journal_space_nr as usize],
            ),
            cur_entry_sectors: AtomicU32::new(0),
            cur_entry_error: AtomicI32::new(0),
            reclaim_kicked: AtomicBool::new(false),
        }
    }
}

fn journal_space_from(ja: &journal_device, from: journal_space_from) -> u32 {
    match from {
        journal_space_from::journal_space_discarded => ja.discard_idx,
        journal_space_from::journal_space_clean_ondisk => ja.dirty_idx_ondisk,
        journal_space_from::journal_space_clean => ja.dirty_idx,
        _ => unreachable!(),
    }
}

pub fn bch2_journal_dev_buckets_available(
    _j: &journal,
    ja: &journal_device,
    from: journal_space_from,
) -> u32 {
    if ja.nr == 0 {
        return 0;
    }

    let mut available = (journal_space_from(ja, from) + ja.nr - ja.cur_idx - 1) % ja.nr;

    if available != 0 && ja.dirty_idx_ondisk == ja.dirty_idx {
        available -= 1;
    }

    available
}

pub fn journal_pin_list_init(p: &mut journal_entry_pin_list, count: u32) {
    *p = journal_entry_pin_list {
        count,
        ..Default::default()
    };
}

pub fn journal_med_on_space(j: &journal) -> bool {
    j.flags.load(Ordering::Acquire) & (1usize << JOURNAL_med_on_space) != 0
}

pub fn journal_low_on_space(j: &journal) -> bool {
    j.flags.load(Ordering::Acquire)
        & ((1usize << JOURNAL_low_on_space) | (1usize << JOURNAL_low_on_pin))
        != 0
}

pub fn journal_reclaim_kick(j: &journal) {
    j.reclaim_kicked.store(true, Ordering::Release);
}

pub fn bch2_journal_set_watermark(j: &journal) {
    let space = j.space.lock().unwrap();
    let clean = space[journal_space_from::journal_space_clean as usize].total as u64;
    let total = space[journal_space_from::journal_space_total as usize].total as u64;
    drop(space);

    let (med_on_space, low_on_space) = if total != 0 {
        (clean * 4 <= total * 3, clean * 4 <= total)
    } else {
        (false, false)
    };
    let pin = j.pin.lock().unwrap();
    let low_on_pin = JOURNAL_PIN.saturating_sub(pin.1.len()) < JOURNAL_PIN / 4;
    drop(pin);

    let mut flags = j.flags.load(Ordering::Acquire);
    for (bit, value) in [
        (JOURNAL_med_on_space, med_on_space),
        (JOURNAL_low_on_space, low_on_space),
        (JOURNAL_low_on_pin, low_on_pin),
    ] {
        if value {
            flags |= 1usize << bit;
        } else {
            flags &= !(1usize << bit);
        }
    }
    j.flags.store(flags, Ordering::Release);
    j.watermark.store(
        if low_on_space || low_on_pin {
            bch_watermark::BCH_WATERMARK_reclaim as u32
        } else {
            bch_watermark::BCH_WATERMARK_stripe as u32
        },
        Ordering::Release,
    );
    if med_on_space {
        journal_reclaim_kick(j);
    }
}

pub fn bch2_journal_space_available(j: &journal) {
    let last_seq = j.last_seq.load(Ordering::Acquire);
    let last_seq_ondisk = j.last_seq_ondisk.load(Ordering::Acquire);
    let mut ja = j.device.lock().unwrap();
    if ja.nr == 0 {
        drop(ja);
        *j.space.lock().unwrap() =
            [journal_space::default(); journal_space_from::journal_space_nr as usize];
        j.cur_entry_sectors.store(0, Ordering::Release);
        j.cur_entry_error.store(0, Ordering::Release);
        bch2_journal_set_watermark(j);
        return;
    }

    while ja.dirty_idx != ja.cur_idx && ja.bucket_seq[ja.dirty_idx as usize] < last_seq {
        ja.dirty_idx = (ja.dirty_idx + 1) % ja.nr;
    }

    while ja.dirty_idx_ondisk != ja.dirty_idx
        && ja.bucket_seq[ja.dirty_idx_ondisk as usize] < last_seq_ondisk
    {
        ja.dirty_idx_ondisk = (ja.dirty_idx_ondisk + 1) % ja.nr;
    }

    let disk_sb = j.disk_sb.load(Ordering::Acquire);
    let (bucket_size, block_sectors) = unsafe {
        if disk_sb.is_null() || (*disk_sb).sb.is_null() {
            (0, 1)
        } else {
            let sb = (*disk_sb).sb;
            (
                crate::sb::io::bch2_sb_member_get(sb, (*sb).dev_idx as usize).bucket_size as u32,
                (*sb).block_size.max(1) as u32,
            )
        }
    };
    let bucket_size_aligned = bucket_size / block_sectors * block_sectors;
    let mut spaces = [journal_space::default(); journal_space_from::journal_space_nr as usize];
    if bucket_size_aligned != 0 {
        spaces[journal_space_from::journal_space_total as usize] = journal_space {
            next_entry: bucket_size_aligned,
            total: bucket_size_aligned.saturating_mul(ja.nr),
        };
        for from in [
            journal_space_from::journal_space_discarded,
            journal_space_from::journal_space_clean_ondisk,
            journal_space_from::journal_space_clean,
        ] {
            let mut buckets = bch2_journal_dev_buckets_available(j, &ja, from);
            let mut sectors = ja.sectors_free / block_sectors * block_sectors;
            if sectors < bucket_size && buckets != 0 {
                buckets -= 1;
                sectors = bucket_size_aligned;
            }
            spaces[from as usize] = journal_space {
                next_entry: sectors,
                total: sectors.saturating_add(buckets.saturating_mul(bucket_size_aligned)),
            };
        }
    }
    drop(ja);

    let clean = spaces[journal_space_from::journal_space_clean as usize].total;
    let clean_ondisk = spaces[journal_space_from::journal_space_clean_ondisk as usize];
    let total = spaces[journal_space_from::journal_space_total as usize].total;
    let may_skip_flush = clean_ondisk.next_entry < clean_ondisk.total
        && clean.saturating_sub(clean_ondisk.total) <= total / 8
        && clean_ondisk.total.saturating_mul(2) > clean;
    if may_skip_flush {
        j.flags
            .fetch_or(1usize << JOURNAL_may_skip_flush, Ordering::AcqRel);
    } else {
        j.flags
            .fetch_and(!(1usize << JOURNAL_may_skip_flush), Ordering::AcqRel);
    }
    let discarded = spaces[journal_space_from::journal_space_discarded as usize];
    *j.space.lock().unwrap() = spaces;
    j.cur_entry_sectors
        .store(discarded.next_entry, Ordering::Release);
    j.cur_entry_error.store(
        if discarded.next_entry != 0 { 0 } else { -9 },
        Ordering::Release,
    );
    bch2_journal_set_watermark(j);
}

pub fn bch2_journal_do_discards(j: &journal) {
    let mut ja = j.device.lock().unwrap();
    while ja.discard_idx != ja.dirty_idx_ondisk {
        ja.discard_idx = (ja.discard_idx + 1) % ja.nr;
    }
}

pub fn bch2_journal_update_last_seq(j: &journal) {
    let seq_ondisk = j.seq_ondisk.load(Ordering::Acquire);
    let mut pin = j.pin.lock().unwrap();
    while !pin.1.is_empty() && pin.0 <= seq_ondisk && pin.1.front().unwrap().count == 0 {
        pin.1.pop_front();
        pin.0 += 1;
    }
    let last_seq = pin.0;
    drop(pin);

    if last_seq != j.last_seq.swap(last_seq, Ordering::AcqRel) {
        bch2_journal_space_available(j);
        bch2_journal_do_discards(j);
    }
}

pub fn journal_pin_active(pin: &crate::btree::types::journal_entry_pin) -> bool {
    pin.seq != 0
}

unsafe fn journal_pin_type(
    pin: *mut crate::btree::types::journal_entry_pin,
    flush_fn: crate::btree::types::journal_pin_flush_fn,
) -> journal_pin_type {
    let flush = flush_fn as *const () as usize;
    let flush0 = crate::btree::update::bch2_btree_node_flush0 as *const () as usize;
    let flush1 = crate::btree::update::bch2_btree_node_flush1 as *const () as usize;
    if flush == flush0 || flush == flush1 {
        let idx = usize::from(flush == flush1);
        let b = pin
            .cast::<u8>()
            .sub(
                core::mem::offset_of!(crate::btree::types::btree, writes)
                    + idx * core::mem::size_of::<crate::btree::types::btree_write>()
                    + core::mem::offset_of!(crate::btree::types::btree_write, journal),
            )
            .cast::<crate::btree::types::btree>();
        return match (*b).c.level {
            0 => journal_pin_type::JOURNAL_PIN_TYPE_btree0,
            1 => journal_pin_type::JOURNAL_PIN_TYPE_btree1,
            2 => journal_pin_type::JOURNAL_PIN_TYPE_btree2,
            _ => journal_pin_type::JOURNAL_PIN_TYPE_btree3,
        };
    }
    journal_pin_type::JOURNAL_PIN_TYPE_other
}

pub unsafe fn bch2_journal_pin_set(
    j: &journal,
    new_seq: u64,
    pin: *mut crate::btree::types::journal_entry_pin,
    flush_fn: crate::btree::types::journal_pin_flush_fn,
) {
    let old_seq = (*pin).seq;
    let pin_addr = pin as usize;
    let pin_type = journal_pin_type(pin, flush_fn) as usize;
    if old_seq != 0 && j.flush_in_progress.load(Ordering::Acquire) == pin_addr {
        j.flush_in_progress_dropped.store(true, Ordering::Release);
    }
    let mut lists = j.pin.lock().unwrap();
    if old_seq != 0 {
        assert!(old_seq >= lists.0);
        let old = (old_seq - lists.0) as usize;
        assert!(old < lists.1.len());
        assert_ne!(lists.1[old].count, 0);
        lists.1[old].count -= 1;
        for pins in &mut lists.1[old].unflushed {
            pins.retain(|candidate| *candidate != pin_addr);
        }
        lists.1[old]
            .flushed
            .retain(|candidate| *candidate != pin_addr);
    }
    assert!(new_seq >= lists.0);
    let new = (new_seq - lists.0) as usize;
    assert!(new < lists.1.len());
    lists.1[new].count += 1;
    lists.1[new].unflushed[pin_type].push(pin_addr);
    (*pin).seq = new_seq;
    (*pin).flush = Some(flush_fn);
    drop(lists);

    if old_seq != 0 {
        bch2_journal_update_last_seq(j);
    }
}

pub unsafe fn bch2_journal_pin_add(
    j: &journal,
    seq: u64,
    pin: *mut crate::btree::types::journal_entry_pin,
    flush_fn: crate::btree::types::journal_pin_flush_fn,
) {
    if !journal_pin_active(&*pin) || (*pin).seq > seq {
        bch2_journal_pin_set(j, seq, pin, flush_fn);
    }
}

pub unsafe fn bch2_journal_pin_update(
    j: &journal,
    seq: u64,
    pin: *mut crate::btree::types::journal_entry_pin,
    flush_fn: crate::btree::types::journal_pin_flush_fn,
) {
    if !journal_pin_active(&*pin) || (*pin).seq < seq {
        bch2_journal_pin_set(j, seq, pin, flush_fn);
    }
}

pub unsafe fn bch2_journal_pin_drop(j: &journal, pin: *mut crate::btree::types::journal_entry_pin) {
    let seq = (*pin).seq;
    if seq == 0 {
        return;
    }

    let mut lists = j.pin.lock().unwrap();
    assert!(seq >= lists.0);
    let idx = (seq - lists.0) as usize;
    assert!(idx < lists.1.len());
    assert_ne!(lists.1[idx].count, 0);
    lists.1[idx].count -= 1;
    let pin_addr = pin as usize;
    for pins in &mut lists.1[idx].unflushed {
        pins.retain(|candidate| *candidate != pin_addr);
    }
    lists.1[idx]
        .flushed
        .retain(|candidate| *candidate != pin_addr);
    if j.flush_in_progress.load(Ordering::Acquire) == pin_addr {
        j.flush_in_progress_dropped.store(true, Ordering::Release);
    }
    (*pin).seq = 0;
    drop(lists);
    bch2_journal_update_last_seq(j);
}

pub fn bch2_journal_replay_pins_put(j: &journal, seq: u64) {
    let mut lists = j.pin.lock().unwrap();
    let end = seq.min(j.seq.load(Ordering::Acquire));
    for current in lists.0..end {
        let idx = (current - lists.0) as usize;
        if idx >= lists.1.len() || !lists.1[idx].unreplayed {
            continue;
        }
        lists.1[idx].unreplayed = false;
        assert_ne!(lists.1[idx].count, 0);
        lists.1[idx].count -= 1;
    }
    drop(lists);
    bch2_journal_update_last_seq(j);
}

unsafe fn journal_get_next_pin(
    j: &journal,
    seq_to_flush: u64,
    allowed_below_seq: u32,
    allowed_above_seq: u32,
) -> Option<(
    *mut crate::btree::types::journal_entry_pin,
    u64,
    crate::btree::types::journal_pin_flush_fn,
)> {
    let lists = j.pin.lock().unwrap();
    for (offset, pin_list) in lists.1.iter().enumerate() {
        let seq = lists.0 + offset as u64;
        if pin_list.unreplayed {
            break;
        }
        if seq > seq_to_flush && allowed_above_seq == 0 {
            break;
        }

        for pin_type in 0..JOURNAL_PIN_TYPE_NR {
            if (((1u32 << pin_type) & allowed_below_seq != 0 && seq <= seq_to_flush)
                || ((1u32 << pin_type) & allowed_above_seq != 0))
                && !pin_list.unflushed[pin_type].is_empty()
            {
                let pin =
                    pin_list.unflushed[pin_type][0] as *mut crate::btree::types::journal_entry_pin;
                let flush_fn = (*pin)
                    .flush
                    .expect("active journal pin without flush callback");
                assert_eq!(j.flush_in_progress.swap(pin as usize, Ordering::AcqRel), 0);
                j.flush_in_progress_dropped.store(false, Ordering::Release);
                return Some((pin, seq, flush_fn));
            }
        }
    }
    None
}

unsafe fn journal_flush_pins(
    j: &journal,
    seq_to_flush: u64,
    allowed_below_seq: u32,
    allowed_above_seq: u32,
    mut min_any: usize,
    _min_key_cache: usize,
) -> usize {
    let mut nr_flushed = 0usize;
    loop {
        let mut allowed_below = allowed_below_seq;
        let mut allowed_above = allowed_above_seq;
        if min_any != 0 {
            allowed_below = u32::MAX;
            allowed_above = u32::MAX;
        }
        let Some((pin, seq, flush_fn)) =
            journal_get_next_pin(j, seq_to_flush, allowed_below, allowed_above)
        else {
            break;
        };
        if min_any != 0 {
            min_any -= 1;
        }

        let ret = flush_fn(j as *const journal as *mut journal, pin, seq);
        let dropped = j.flush_in_progress_dropped.load(Ordering::Acquire);
        if ret == 0 && !dropped && (*pin).seq == seq {
            let pin_addr = pin as usize;
            let mut lists = j.pin.lock().unwrap();
            if seq >= lists.0 {
                let idx = (seq - lists.0) as usize;
                if idx < lists.1.len() {
                    for pins in &mut lists.1[idx].unflushed {
                        pins.retain(|candidate| *candidate != pin_addr);
                    }
                    if !lists.1[idx].flushed.contains(&pin_addr) {
                        lists.1[idx].flushed.push(pin_addr);
                    }
                }
            }
        }
        j.flush_in_progress.store(0, Ordering::Release);
        j.flush_in_progress_dropped.store(false, Ordering::Release);
        if ret != 0 {
            break;
        }
        nr_flushed += 1;
    }
    nr_flushed
}

fn journal_seq_to_flush(j: &journal) -> u64 {
    let ja = j.device.lock().unwrap();
    if ja.nr == 0 {
        return 0;
    }
    let bucket_to_flush = (ja.cur_idx + ja.nr / 2) % ja.nr;
    ja.bucket_seq[bucket_to_flush as usize]
}

pub fn bch2_journal_flush_pins(j: &journal, seq_to_flush: u64) -> bool {
    let _reclaim = j.reclaim_lock.lock().unwrap();
    let mut did_work = false;
    for pin_type in (0..JOURNAL_PIN_TYPE_NR).rev() {
        let nr = unsafe { journal_flush_pins(j, seq_to_flush, 1 << pin_type, 0, 0, 0) };
        did_work |= nr != 0;
    }
    did_work
}

pub(crate) fn __bch2_journal_reclaim(j: &journal, direct: bool, mut kicked: bool) -> i32 {
    loop {
        let seq_to_flush = journal_seq_to_flush(j);
        let min_nr = usize::from(kicked || journal_med_on_space(j));
        let nr = unsafe { journal_flush_pins(j, seq_to_flush, u32::MAX, 0, min_nr, 0) };
        if direct {
            j.nr_direct_reclaim.fetch_add(nr as u64, Ordering::Relaxed);
        } else {
            j.nr_background_reclaim
                .fetch_add(nr as u64, Ordering::Relaxed);
        }
        if direct || min_nr == 0 || nr == 0 {
            break;
        }
        kicked = false;
    }
    0
}

pub fn bch2_journal_reclaim(j: &journal) -> i32 {
    let _reclaim = j.reclaim_lock.lock().unwrap();
    __bch2_journal_reclaim(j, true, true)
}

const fn journal_state_offset(v: u64) -> u32 {
    (v & ((1 << 22) - 1)) as u32
}

const fn journal_state_idx(v: u64) -> u64 {
    (v >> 22) & JOURNAL_STATE_BUF_MASK
}

const fn journal_state_count(v: u64, idx: u64) -> u16 {
    ((v >> (24 + idx * 10)) & 0x3ff) as u16
}

const fn journal_state_set_offset(v: u64, offset: u32) -> u64 {
    (v & !((1 << 22) - 1)) | offset as u64
}

const fn journal_state_set_idx(v: u64, idx: u64) -> u64 {
    (v & !(JOURNAL_STATE_BUF_MASK << 22)) | (idx << 22)
}

const fn journal_state_inc(v: u64, idx: u64) -> Option<u64> {
    if journal_state_count(v, idx) == 0x3ff {
        None
    } else {
        Some(v + (1u64 << (24 + idx * 10)))
    }
}

const fn journal_state_dec(v: u64, idx: u64) -> u64 {
    assert!(journal_state_count(v, idx) != 0);
    v - (1u64 << (24 + idx * 10))
}

pub const fn jset_u64s(payload_u64s: u32) -> u32 {
    JSET_KEYS_U64s + payload_u64s
}

pub fn journal_res_get_fast(j: &journal, res: &mut journal_res, flags: u32) -> bool {
    let mut old = j.reservations.load(Ordering::Acquire);
    loop {
        let idx = journal_state_idx(old);
        let offset = journal_state_offset(old);
        if offset + res.u64s as u32 > j.cur_entry_u64s.load(Ordering::Acquire) {
            return false;
        }
        assert_ne!(journal_state_count(old, idx), 0);
        if (flags & BCH_WATERMARK_MASK) < j.watermark.load(Ordering::Acquire) {
            return false;
        }
        let Some(mut new) = journal_state_inc(old, idx) else {
            return false;
        };
        new = journal_state_set_offset(new, offset + res.u64s as u32);
        match j
            .reservations
            .compare_exchange_weak(old, new, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => {
                let mut seq = j.seq.load(Ordering::Acquire);
                seq -= (seq - idx) & JOURNAL_STATE_BUF_MASK;
                let buf = &j.ring[idx as usize];
                assert_eq!(buf.seq.load(Ordering::Acquire), seq);
                res.ref_ = true;
                res.offset = offset;
                res.seq = seq;
                res.has_overwrites = buf.has_overwrites.load(Ordering::Acquire);
                return true;
            }
            Err(v) => old = v,
        }
    }
}

pub fn bch2_journal_res_get(j: &journal, res: &mut journal_res, u64s: u16, flags: u32) -> i32 {
    assert!(!res.ref_);
    res.u64s = u64s;
    loop {
        if journal_res_get_fast(j, res, flags) {
            return 0;
        }
        if (flags & BCH_WATERMARK_MASK) < j.watermark.load(Ordering::Acquire) {
            let reclaimed = j.nr_direct_reclaim.load(Ordering::Acquire);
            let ret = bch2_journal_reclaim(j);
            if ret != 0 {
                return ret;
            }
            if j.nr_direct_reclaim.load(Ordering::Acquire) != reclaimed {
                continue;
            }
            return -9;
        }
        let ret = bch2_journal_flush(j);
        if ret == -9 {
            let reclaimed = j.nr_direct_reclaim.load(Ordering::Acquire);
            let reclaim_ret = bch2_journal_reclaim(j);
            if reclaim_ret != 0 {
                return reclaim_ret;
            }
            if j.nr_direct_reclaim.load(Ordering::Acquire) != reclaimed {
                continue;
            }
        }
        if ret != 0 {
            return ret;
        }
    }
}

pub unsafe fn journal_res_entry(j: &journal, res: &journal_res) -> *mut jset_entry {
    assert!(res.ref_);
    let idx = (res.seq & JOURNAL_STATE_BUF_MASK) as usize;
    let data = &mut *j.ring[idx].data.get();
    data.as_mut_ptr().add(res.offset as usize).cast()
}

pub unsafe fn journal_entry_init(
    entry: *mut jset_entry,
    type_: u8,
    id: u8,
    level: u8,
    u64s: u16,
) -> u16 {
    *entry = jset_entry {
        u64s,
        btree_id: id,
        level,
        type_,
        pad: [0; 3],
    };
    u64s + JSET_KEYS_U64s as u16
}

pub unsafe fn journal_entry_set(
    entry: *mut jset_entry,
    type_: u8,
    id: u8,
    level: u8,
    data: *const u64,
    u64s: u16,
) -> u16 {
    let ret = journal_entry_init(entry, type_, id, level, u64s);
    core::ptr::copy_nonoverlapping(data, entry.cast::<u64>().add(1), u64s as usize);
    ret
}

pub unsafe fn bch2_journal_add_entry(
    j: &journal,
    res: &mut journal_res,
    type_: u8,
    id: u8,
    level: u8,
    u64s: u16,
) -> *mut jset_entry {
    let entry = journal_res_entry(j, res);
    let actual = journal_entry_init(entry, type_, id, level, u64s);
    assert!(actual <= res.u64s);
    res.offset += actual as u32;
    res.u64s -= actual;
    entry
}

pub fn bch2_journal_res_put(j: &journal, res: &mut journal_res) {
    if !res.ref_ {
        return;
    }
    unsafe {
        while res.u64s != 0 {
            bch2_journal_add_entry(j, res, BCH_JSET_ENTRY_btree_keys, 0, 0, 0);
        }
    }
    let idx = res.seq & JOURNAL_STATE_BUF_MASK;
    j.reservations
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |v| {
            Some(journal_state_dec(v, idx))
        })
        .unwrap();
    res.ref_ = false;
}

pub fn bch2_journal_flush(j: &journal) -> i32 {
    let _guard = j.cycle_lock.lock().unwrap();
    let old_state = j.reservations.load(Ordering::Acquire);
    let old_idx = journal_state_idx(old_state);
    if journal_state_count(old_state, old_idx) != 1 {
        return -1;
    }
    let used = journal_state_offset(old_state) as usize;
    let old_seq = j.seq.load(Ordering::Acquire);
    let old_buf = &j.ring[old_idx as usize];
    assert_eq!(old_buf.seq.load(Ordering::Acquire), old_seq);

    let data = unsafe { &*old_buf.data.get() };
    let mut record = vec![0u64; JSET_HEADER_U64S + used];
    record[2] = JSET_MAGIC;
    record[3] = old_seq;
    record[5] = used as u64;
    record[6] = j.last_seq.load(Ordering::Acquire);
    record[JSET_HEADER_U64S..].copy_from_slice(&data[..used]);

    let disk_sb = j.disk_sb.load(Ordering::Acquire);
    if !disk_sb.is_null() {
        let ret = unsafe {
            use std::os::unix::fs::FileExt;

            if (*disk_sb).sb.is_null() || (*disk_sb).s_bdev_file.is_null() {
                -4
            } else {
                let sb = (*disk_sb).sb;
                let members =
                    crate::sb::io::bch2_sb_field_get_id(sb, crate::sb::BCH_SB_FIELD_members_v2);
                let field =
                    crate::sb::io::bch2_sb_field_get_id(sb, crate::sb::BCH_SB_FIELD_journal_v2);
                if members.is_null() || field.is_null() {
                    -4
                } else {
                    let member = crate::sb::io::bch2_sb_member_get(sb, (*sb).dev_idx as usize);
                    let bucket_size = member.bucket_size as u32;
                    let journal_field = field.cast::<crate::sb::bch_sb_field_journal_v2>();
                    let nr = crate::sb::io::bch2_sb_field_journal_v2_nr_entries(journal_field);
                    let entries = journal_field
                        .cast::<u8>()
                        .add(core::mem::size_of::<crate::sb::bch_sb_field_journal_v2>())
                        .cast::<crate::sb::bch_sb_field_journal_v2_entry>();
                    let mut total_buckets = 0u64;
                    for idx in 0..nr {
                        total_buckets = total_buckets.saturating_add((*entries.add(idx)).nr);
                    }
                    let block_bytes = ((*sb).block_size as usize).max(1) * 512;
                    let record_bytes = record.len() * 8;
                    let write_bytes = record_bytes.next_multiple_of(block_bytes);
                    let sectors = (write_bytes / 512) as u32;
                    if bucket_size == 0 || total_buckets == 0 || sectors > bucket_size {
                        -4
                    } else {
                        let mut ja = j.device.lock().unwrap();
                        if ja.nr != total_buckets as u32
                            || ja.buckets.len() != ja.nr as usize
                            || ja.bucket_seq.len() != ja.nr as usize
                        {
                            return -4;
                        }
                        let mut advanced = false;
                        if sectors > ja.sectors_free
                            && sectors <= bucket_size
                            && bch2_journal_dev_buckets_available(
                                j,
                                &ja,
                                journal_space_from::journal_space_discarded,
                            ) != 0
                        {
                            ja.cur_idx = (ja.cur_idx + 1) % ja.nr;
                            ja.sectors_free = bucket_size;
                            let cur_idx = ja.cur_idx as usize;
                            ja.bucket_seq[cur_idx] = old_seq;
                            advanced = true;
                        }
                        if sectors > ja.sectors_free {
                            return -9;
                        }
                        let bucket = ja.buckets[ja.cur_idx as usize];
                        let file = &*(*disk_sb).s_bdev_file.cast::<std::fs::File>();
                        let bucket_sector = bucket * bucket_size as u64;
                        if advanced {
                            let zeros = vec![0u8; bucket_size as usize * 512];
                            if file.write_at(&zeros, bucket_sector * 512).ok() != Some(zeros.len())
                            {
                                return -5;
                            }
                        }
                        let sector = bucket_sector + (bucket_size - ja.sectors_free) as u64;
                        let mut disk = vec![0u64; write_bytes / 8];
                        disk[..record.len()].copy_from_slice(&record);
                        let uuid_lo = u64::from_le_bytes((&(*sb).uuid)[..8].try_into().unwrap());
                        disk[0] = 0;
                        disk[1] = 0;
                        disk[2] = uuid_lo ^ JSET_MAGIC;
                        disk[4] = crate::sb::bcachefs_metadata_version_current as u64
                            | (crate::checksum::BCH_CSUM_xxhash as u64) << 32;
                        let checksum = crate::checksum::bch2_checksum(
                            crate::checksum::BCH_CSUM_xxhash,
                            core::slice::from_raw_parts(
                                disk.as_ptr().cast::<u8>().add(16),
                                record_bytes - 16,
                            ),
                        );
                        disk[0] = checksum.lo;
                        disk[1] = checksum.hi;
                        let bytes =
                            core::slice::from_raw_parts(disk.as_ptr().cast::<u8>(), write_bytes);
                        if sector * 512 + write_bytes as u64
                            > file.metadata().map(|m| m.len()).unwrap_or(0)
                            || file.write_at(bytes, sector * 512).ok() != Some(write_bytes)
                        {
                            -5
                        } else {
                            ja.sectors_free -= sectors;
                            let cur_idx = ja.cur_idx as usize;
                            ja.bucket_seq[cur_idx] = old_seq;
                            ja.highest_seq_found = old_seq;
                            0
                        }
                    }
                }
            }
        };
        if ret != 0 {
            return ret;
        }
    }
    let last_seq_wrote = record[6];
    let empty = unsafe { journal_entry_empty(record.as_ptr().cast::<jset>()) } as u64;
    j.closed.lock().unwrap().push(record);

    let new_seq = old_seq + 1;
    if new_seq > JOURNAL_SEQ_MAX {
        return -2;
    }
    let new_idx = new_seq & JOURNAL_STATE_BUF_MASK;
    if journal_state_count(old_state, new_idx) != 0 {
        return -3;
    }
    let new_buf = &j.ring[new_idx as usize];
    unsafe { (&mut *new_buf.data.get()).fill(0) };
    new_buf.seq.store(new_seq, Ordering::Release);
    let mut new_state = journal_state_dec(old_state, old_idx);
    new_state = journal_state_set_idx(new_state, new_idx);
    new_state = journal_state_set_offset(new_state, 0);
    new_state = journal_state_inc(new_state, new_idx).unwrap();
    j.seq.store(new_seq, Ordering::Release);
    j.reservations.store(new_state, Ordering::Release);

    {
        let mut lists = j.pin.lock().unwrap();
        assert_eq!(lists.0 + lists.1.len() as u64, new_seq);
        lists.1.push_back(journal_entry_pin_list {
            count: 1,
            ..Default::default()
        });
        let old = (old_seq - lists.0) as usize;
        assert_ne!(lists.1[old].count, 0);
        lists.1[old].count -= 1;
    }
    j.seq_ondisk.store(old_seq, Ordering::Release);
    j.last_seq_ondisk
        .fetch_max(last_seq_wrote + empty, Ordering::AcqRel);
    bch2_journal_update_last_seq(j);
    bch2_journal_space_available(j);
    bch2_journal_do_discards(j);
    let next_sectors = j.cur_entry_sectors.load(Ordering::Acquire);
    if next_sectors != 0 {
        j.cur_entry_u64s.store(
            (next_sectors as usize * 512 / core::mem::size_of::<u64>())
                .saturating_sub(JSET_HEADER_U64S)
                .min(JOURNAL_ENTRY_OFFSET_MAX as usize) as u32,
            Ordering::Release,
        );
    }
    if j.reclaim_kicked.swap(false, Ordering::AcqRel) {
        if let Ok(_reclaim) = j.reclaim_lock.try_lock() {
            __bch2_journal_reclaim(j, false, true);
        } else {
            j.reclaim_kicked.store(true, Ordering::Release);
        }
    }
    0
}

unsafe fn journal_entry_btree_root_validate(entry: *mut jset_entry) -> i32 {
    let key = entry
        .cast::<u64>()
        .add(1)
        .cast::<crate::btree::bkey::bkey_i>();
    if (*entry).u64s == 0 || (*entry).u64s != (*key).k.u64s as u16 {
        core::ptr::write_bytes(entry.cast::<u64>().add(1), 0, (*entry).u64s as usize);
        (*entry).u64s = 0;
        return 0;
    }
    if (*entry).btree_id as usize >= crate::btree::types::BTREE_ID_NR {
        return -4;
    }
    if (*entry).level >= crate::btree::bset::BTREE_MAX_DEPTH {
        return -5;
    }
    if (*key).k.format != crate::btree::bkey::KEY_FORMAT_CURRENT {
        return -6;
    }
    if (*key).k.type_ != crate::btree::bset::KEY_TYPE_btree_ptr_v2 {
        return -7;
    }
    if (*key).k.u64s as usize
        > crate::btree::bkey::BKEY_U64S as usize + crate::btree::types::BKEY_BTREE_PTR_VAL_U64S_MAX
    {
        return -8;
    }
    0
}

pub unsafe fn bch2_journal_read(
    c: *mut crate::btree::types::bch_fs,
    info: *mut journal_start_info,
) -> i32 {
    use std::os::unix::fs::FileExt;

    if c.is_null() || info.is_null() || (*c).disk_sb.sb.is_null() {
        return -1;
    }
    *info = journal_start_info::default();
    let handle = &mut (*c).disk_sb;
    if handle.s_bdev_file.is_null() {
        return -2;
    }
    let sb = handle.sb;
    let members = crate::sb::io::bch2_sb_field_get_id(sb, crate::sb::BCH_SB_FIELD_members_v2);
    let field = crate::sb::io::bch2_sb_field_get_id(sb, crate::sb::BCH_SB_FIELD_journal_v2);
    if members.is_null() || field.is_null() {
        return -3;
    }
    let member = crate::sb::io::bch2_sb_member_get(sb, (*sb).dev_idx as usize);
    let bucket_size = member.bucket_size as usize;
    if bucket_size == 0 {
        return -3;
    }
    let journal_field = field.cast::<crate::sb::bch_sb_field_journal_v2>();
    let nr = crate::sb::io::bch2_sb_field_journal_v2_nr_entries(journal_field);
    let entries = journal_field
        .cast::<u8>()
        .add(core::mem::size_of::<crate::sb::bch_sb_field_journal_v2>())
        .cast::<crate::sb::bch_sb_field_journal_v2_entry>();
    let mut total_buckets = 0u64;
    let mut buckets = Vec::new();
    for idx in 0..nr {
        let range = &*entries.add(idx);
        total_buckets = total_buckets.saturating_add(range.nr);
        for offset in 0..range.nr {
            buckets.push(range.start + offset);
        }
    }
    if total_buckets == 0 || total_buckets > u32::MAX as u64 {
        return -3;
    }

    let file = &*handle.s_bdev_file.cast::<std::fs::File>();
    let block_bytes = ((*sb).block_size as usize).max(1) * 512;
    let bucket_bytes = bucket_size * 512;
    let uuid_lo = u64::from_le_bytes((&(*sb).uuid)[..8].try_into().unwrap());
    let disk_magic = uuid_lo ^ JSET_MAGIC;
    let mut found: Vec<(u64, u32, u32, Vec<u64>)> = Vec::new();
    let mut bucket_seq = vec![0u64; total_buckets as usize];
    let mut logical_bucket = 0u32;
    for range_idx in 0..nr {
        let range = &*entries.add(range_idx);
        for range_offset in 0..range.nr {
            let bucket = range.start + range_offset;
            let disk_offset = bucket * bucket_size as u64 * 512;
            if disk_offset + bucket_bytes as u64 > file.metadata().map(|m| m.len()).unwrap_or(0) {
                return -4;
            }
            let mut data = vec![0u8; bucket_bytes];
            let mut read = 0usize;
            while read < data.len() {
                match file.read_at(&mut data[read..], disk_offset + read as u64) {
                    Ok(0) => return -4,
                    Ok(nr) => read += nr,
                    Err(_) => return -4,
                }
            }

            let mut offset = 0usize;
            let mut max_seq = 0u64;
            while offset + core::mem::size_of::<jset>() <= data.len() {
                let disk = data.as_ptr().add(offset).cast::<u64>();
                if *disk.add(2) != disk_magic {
                    break;
                }
                let seq = *disk.add(3);
                let version_flags = *disk.add(4);
                let version = version_flags as u32;
                let flags = (version_flags >> 32) as u32;
                let payload_u64s = *disk.add(5) as u32 as usize;
                let record_bytes = core::mem::size_of::<jset>() + payload_u64s * 8;
                if seq == 0
                    || seq > JOURNAL_SEQ_MAX
                    || version != crate::sb::bcachefs_metadata_version_current as u32
                    || flags & 0xf != crate::checksum::BCH_CSUM_xxhash
                    || flags & (1 << 4) != 0
                    || record_bytes > data.len() - offset
                {
                    return -5;
                }
                if seq < max_seq {
                    break;
                }
                max_seq = seq;
                bucket_seq[logical_bucket as usize] = seq;
                let sectors_bytes = record_bytes.next_multiple_of(block_bytes);
                if sectors_bytes > data.len() - offset {
                    return -5;
                }
                let expected = bch_csum {
                    lo: *disk,
                    hi: *disk.add(1),
                };
                let checksum = crate::checksum::bch2_checksum(
                    crate::checksum::BCH_CSUM_xxhash,
                    core::slice::from_raw_parts(data.as_ptr().add(offset + 16), record_bytes - 16),
                );
                if expected.lo != checksum.lo || expected.hi != checksum.hi {
                    return -6;
                }
                let words = record_bytes / 8;
                let mut record = vec![0u64; words];
                core::ptr::copy_nonoverlapping(disk, record.as_mut_ptr(), words);
                record[2] = JSET_MAGIC;
                found.push((
                    seq,
                    logical_bucket,
                    ((offset + sectors_bytes) / 512) as u32,
                    record,
                ));
                offset += sectors_bytes;
            }
            logical_bucket += 1;
        }
    }

    found.sort_by_key(|entry| entry.0);
    for pair in found.windows(2) {
        if pair[0].0 == pair[1].0 && pair[0].3 != pair[1].3 {
            return -7;
        }
    }
    found.dedup_by(|left, right| left.0 == right.0);
    let highest = found.last().map(|entry| (entry.0, entry.1, entry.2));
    let cur_seq = highest.map(|entry| entry.0 + 1).unwrap_or(1);
    let flush = found.iter().rev().find(|entry| {
        let header = unsafe { &*(entry.3.as_ptr().cast::<jset>()) };
        JSET_NO_FLUSH(header) == 0
    });
    let (last_seq, replay_end, clean) = if let Some(entry) = flush {
        let seq = entry.0;
        let last_seq = entry.3[6].min(seq);
        let mut empty = seq == last_seq;
        if empty {
            let mut offset = JSET_HEADER_U64S;
            let end = JSET_HEADER_U64S + entry.3[5] as usize;
            while offset < end {
                let journal_entry = &*(entry.3.as_ptr().add(offset).cast::<jset_entry>());
                if journal_entry.type_ == BCH_JSET_ENTRY_btree_keys && journal_entry.u64s != 0 {
                    empty = false;
                    break;
                }
                offset += jset_u64s(journal_entry.u64s as u32) as usize;
            }
        }
        (last_seq, seq, empty)
    } else {
        (0, 0, false)
    };
    if replay_end != 0 {
        let mut expected = last_seq;
        for entry in &found {
            if entry.0 < last_seq || entry.0 > replay_end {
                continue;
            }
            if entry.0 != expected {
                return -8;
            }
            expected += 1;
        }
        if expected != replay_end + 1 {
            return -8;
        }
    }
    let mut records = (*c).journal.closed.lock().unwrap();
    records.clear();
    records.extend(
        found
            .into_iter()
            .filter(|entry| entry.0 >= last_seq && entry.0 <= replay_end)
            .map(|entry| entry.3),
    );

    (*info).last_seq = last_seq;
    (*info).replay_end = replay_end;
    (*info).cur_seq = cur_seq;
    (*info).clean = clean;
    drop(records);

    for buf in &(*c).journal.ring {
        unsafe { (&mut *buf.data.get()).fill(0) };
        buf.seq.store(0, Ordering::Release);
        buf.has_overwrites.store(false, Ordering::Release);
    }
    let idx = cur_seq & JOURNAL_STATE_BUF_MASK;
    (*c).journal.ring[idx as usize]
        .seq
        .store(cur_seq, Ordering::Release);
    let state = idx << 22 | 1u64 << (24 + idx * 10);
    (*c).journal.seq.store(cur_seq, Ordering::Release);
    (*c).journal.reservations.store(state, Ordering::Release);
    let pin_front = if last_seq != 0 { last_seq } else { cur_seq };
    let mut pin = VecDeque::new();
    for _ in pin_front..cur_seq {
        pin.push_back(journal_entry_pin_list {
            count: 1,
            unreplayed: true,
            ..Default::default()
        });
    }
    pin.push_back(journal_entry_pin_list {
        count: 1,
        ..Default::default()
    });
    *(*c).journal.pin.lock().unwrap() = (pin_front, pin);
    (*c).journal
        .seq_ondisk
        .store(cur_seq - 1, Ordering::Release);
    (*c).journal.last_seq.store(pin_front, Ordering::Release);
    (*c).journal
        .last_seq_ondisk
        .store(pin_front, Ordering::Release);
    (*c).journal
        .disk_sb
        .store(&mut (*c).disk_sb, Ordering::Release);
    let (cur_idx, sectors_free, highest_seq_found) = highest
        .map(|(seq, bucket, used)| (bucket, bucket_size as u32 - used, seq))
        .unwrap_or((0, bucket_size as u32, 0));
    let mut dirty_idx = (cur_idx + 1) % total_buckets as u32;
    let live_from = if last_seq != 0 { last_seq } else { cur_seq };
    while dirty_idx != cur_idx && bucket_seq[dirty_idx as usize] < live_from {
        dirty_idx = (dirty_idx + 1) % total_buckets as u32;
    }
    *(*c).journal.device.lock().unwrap() = journal_device {
        bucket_seq,
        sectors_free,
        discard_idx: dirty_idx,
        dirty_idx_ondisk: dirty_idx,
        dirty_idx,
        cur_idx,
        nr: total_buckets as u32,
        buckets,
        highest_seq_found,
    };
    bch2_journal_space_available(&(*c).journal);
    let next_sectors = (*c).journal.cur_entry_sectors.load(Ordering::Acquire);
    if next_sectors != 0 {
        (*c).journal.cur_entry_u64s.store(
            (next_sectors as usize * 512 / core::mem::size_of::<u64>())
                .saturating_sub(JSET_HEADER_U64S)
                .min(JOURNAL_ENTRY_OFFSET_MAX as usize) as u32,
            Ordering::Release,
        );
    }
    0
}

pub unsafe fn bch2_journal_replay(c: *mut crate::btree::types::bch_fs) -> i32 {
    (*c).journal
        .flags
        .fetch_and(!(1usize << JOURNAL_replay_done), Ordering::AcqRel);
    bch2_journal_keys_clear(c);
    let records = (*c).journal.closed.lock().unwrap().clone();
    let mut ordered: Vec<(u64, Vec<u64>)> = Vec::with_capacity(records.len());
    for record in records {
        if record.len() < JSET_HEADER_U64S
            || record[2] != JSET_MAGIC
            || record[3] == 0
            || record[3] > JOURNAL_SEQ_MAX
            || record[5] as u32 as usize > record.len() - JSET_HEADER_U64S
        {
            return -1;
        }
        ordered.push((record[3], record));
    }
    ordered.sort_by_key(|entry| entry.0);

    let mut unique: Vec<(u64, Vec<u64>)> = Vec::with_capacity(ordered.len());
    for (seq, record) in ordered {
        if let Some((previous_seq, previous_record)) = unique.last() {
            if *previous_seq == seq {
                if previous_record.as_slice() != record.as_slice() {
                    return -7;
                }
                continue;
            }
        }
        unique.push((seq, record));
    }

    let Some((replay_start, replay_end)) = unique.iter().rev().find_map(|(seq, record)| {
        let header = unsafe { &*(record.as_ptr().cast::<jset>()) };
        (JSET_NO_FLUSH(header) == 0).then_some((header.last_seq.min(*seq), *seq))
    }) else {
        (*c).journal
            .flags
            .fetch_or(1usize << JOURNAL_replay_done, Ordering::AcqRel);
        return 0;
    };

    let mut selected: Vec<(u64, Vec<u64>)> = unique
        .into_iter()
        .filter(|(seq, _)| *seq >= replay_start && *seq <= replay_end)
        .collect();
    let mut expected = replay_start;
    for (seq, _) in &selected {
        if *seq != expected {
            return -8;
        }
        expected = match expected.checked_add(1) {
            Some(next) => next,
            None => return -8,
        };
    }
    if expected != replay_end.saturating_add(1) {
        return -8;
    }

    /*
     * This mirrors recovery.c's early replay pass: root records must become
     * visible before a later btree-key replay can traverse that root.
     */
    for (_, record) in selected.iter_mut() {
        let mut offset = JSET_HEADER_U64S;
        let end = JSET_HEADER_U64S + record[5] as u32 as usize;
        while offset < end {
            let entry = record.as_mut_ptr().add(offset).cast::<jset_entry>();
            let actual = jset_u64s((*entry).u64s as u32) as usize;
            if actual == 0 || offset + actual > end {
                return -2;
            }
            if (*entry).type_ == BCH_JSET_ENTRY_btree_root {
                let ret = journal_entry_btree_root_validate(entry);
                if ret != 0 {
                    return ret;
                }
                if (*entry).u64s != 0 {
                    crate::btree::interior::bch2_journal_entry_to_btree_root(c, entry);
                }
            }
            offset += actual;
        }
    }

    /* Build the journal overlay before replaying updates, as bcachefs does
     * while normal btree lookups are still allowed to observe journal keys. */
    for (_, record) in selected.iter_mut() {
        let mut offset = JSET_HEADER_U64S;
        let end = JSET_HEADER_U64S + record[5] as u32 as usize;
        while offset < end {
            let entry = record.as_mut_ptr().add(offset).cast::<jset_entry>();
            let actual = jset_u64s((*entry).u64s as u32) as usize;
            if actual == 0 || offset + actual > end {
                return -2;
            }
            if (*entry).type_ == BCH_JSET_ENTRY_btree_keys {
                let mut key_offset = 0usize;
                while key_offset < (*entry).u64s as usize {
                    let remaining = (*entry).u64s as usize - key_offset;
                    if remaining < crate::btree::bkey::BKEY_U64S as usize {
                        return -3;
                    }
                    let key = record
                        .as_ptr()
                        .add(offset + 1 + key_offset)
                        .cast::<crate::btree::bkey::bkey_i>();
                    let key_u64s = (*key).k.u64s as usize;
                    if key_u64s < crate::btree::bkey::BKEY_U64S as usize || key_u64s > remaining {
                        return -3;
                    }
                    let ret = bch2_journal_key_insert(c, (*entry).btree_id, (*entry).level, key);
                    if ret != 0 {
                        return ret;
                    }
                    key_offset += key_u64s;
                }
            }
            offset += actual;
        }
    }

    /* Replay the selected sequence in durable journal order.  This follows
     * recovery.c's bch2_journal_replay_key(): every key gets a node iterator
     * at its recorded level, is traversed before the update, and carries its
     * durable journal sequence into commit. */
    for (seq, record) in selected.iter_mut() {
        let mut offset = JSET_HEADER_U64S;
        let end = JSET_HEADER_U64S + record[5] as u32 as usize;
        while offset < end {
            let entry = record.as_mut_ptr().add(offset).cast::<jset_entry>();
            let actual = jset_u64s((*entry).u64s as u32) as usize;
            if actual == 0 || offset + actual > end {
                return -2;
            }
            if (*entry).type_ == BCH_JSET_ENTRY_btree_keys {
                let mut key_offset = 0usize;
                while key_offset < (*entry).u64s as usize {
                    let remaining = (*entry).u64s as usize - key_offset;
                    if remaining < crate::btree::bkey::BKEY_U64S as usize {
                        return -3;
                    }
                    let key = record
                        .as_mut_ptr()
                        .add(offset + 1 + key_offset)
                        .cast::<crate::btree::bkey::bkey_i>();
                    let key_u64s = (*key).k.u64s as usize;
                    if key_u64s < crate::btree::bkey::BKEY_U64S as usize || key_u64s > remaining {
                        return -3;
                    }
                    let ret =
                        bch2_journal_replay_key(c, (*entry).btree_id, (*entry).level, key, *seq);
                    if ret != 0 {
                        return ret;
                    }
                    key_offset += key_u64s;
                }
            }
            offset += actual;
        }
    }
    /* replay_journal_seq_end in journal/init.c is cur_seq (the first unused
     * sequence), not the last durable record.  journal.seq is that same
     * boundary after bch2_journal_read() reconstructs the journal state. */
    bch2_journal_replay_pins_put(&(*c).journal, (*c).journal.seq.load(Ordering::Acquire));
    (*c).journal
        .flags
        .fetch_or(1usize << JOURNAL_replay_done, Ordering::AcqRel);
    0
}

/*
 * The port equivalent of recovery.c's bch2_journal_replay_key(), invoked by
 * the same unbounded transaction-restart loop as commit_do().  A split may
 * have already changed tree topology when commit returns -4, so the next
 * attempt must begin a fresh transaction, retraverse from the root, and only
 * then stage the journal key again.
 */
unsafe fn bch2_journal_replay_key(
    c: *mut crate::btree::types::bch_fs,
    btree_id: u8,
    level: u8,
    key: *mut crate::btree::bkey::bkey_i,
    seq: u64,
) -> i32 {
    let mut trans = crate::btree::iter::btree_trans::default();
    crate::btree::iter::bch2_trans_init(&mut trans, c);

    loop {
        crate::btree::iter::bch2_trans_begin(&mut trans);
        trans.journal_replay_not_finished = true;
        trans.journal_res.seq = seq;

        let mut iter = crate::btree::iter::btree_iter::default();
        crate::btree::iter::bch2_trans_node_iter_init(
            &mut trans,
            &mut iter,
            btree_id,
            (*key).k.p,
            crate::btree::bset::BTREE_MAX_DEPTH,
            level,
            crate::btree::iter::BTREE_ITER_intent | crate::btree::iter::BTREE_ITER_not_extents,
        );
        let mut ret = crate::btree::iter::bch2_btree_iter_traverse(&mut iter);
        if ret == 0 {
            ret = crate::btree::update::bch2_trans_update(
                &mut trans,
                &mut iter,
                key,
                crate::btree::update::BTREE_UPDATE_nojournal
                    | crate::btree::update::BTREE_TRIGGER_norun,
            );
        }
        if ret == 0 {
            ret = crate::btree::update::bch2_trans_commit(&mut trans);
        }
        crate::btree::iter::bch2_trans_iter_exit(&mut iter);

        /* BCH_ERR_transaction_restart is represented as -4 by this port's
         * commit path.  As in commit_do(), retry it without a fixed limit. */
        if ret == -4 {
            continue;
        }
        return ret;
    }
}

unsafe fn journal_key_ptr(
    key: &crate::btree::types::journal_key,
) -> *const crate::btree::bkey::bkey_i {
    key.allocated_k
}

pub unsafe fn bch2_journal_keys_clear(c: *mut crate::btree::types::bch_fs) {
    if c.is_null() {
        return;
    }
    let keys = &mut (*c).journal_keys;
    for key in keys.data.drain(..).chain(keys.pre_sort.drain(..)) {
        if key.allocated && !key.allocated_k.is_null() {
            crate::btree::types::journal_key_free(key.allocated_k);
        }
    }
    keys.nr = 0;
    keys.size = 0;
    keys.gap = 0;
    keys.overwrites.clear();
}

unsafe fn __journal_keys_sort(keys: &mut crate::btree::types::journal_keys) {
    keys.data.sort_by(|left, right| {
        left.btree_id
            .cmp(&right.btree_id)
            .then(left.level.cmp(&right.level))
            .then_with(|| {
                crate::btree::bkey::bpos_cmp((*left.allocated_k).k.p, (*right.allocated_k).k.p)
                    .cmp(&0)
            })
    });

    keys.overwrites.clear();
    let mut idx = 0;
    while idx < keys.data.len() {
        keys.data[idx].overwritten_range = 0;
        if !keys.data[idx].overwritten {
            idx += 1;
            continue;
        }
        let start = idx;
        let btree_id = keys.data[idx].btree_id;
        let level = keys.data[idx].level;
        idx += 1;
        while idx < keys.data.len()
            && keys.data[idx].overwritten
            && keys.data[idx].btree_id == btree_id
            && keys.data[idx].level == level
        {
            idx += 1;
        }
        if keys.overwrites.is_empty() {
            keys.overwrites
                .push(crate::btree::types::journal_key_range_overwritten { start: 0, end: 0 });
        }
        let range = keys.overwrites.len();
        keys.overwrites
            .push(crate::btree::types::journal_key_range_overwritten { start, end: idx });
        for item in start..idx {
            keys.data[item].overwritten_range = range as u32;
        }
    }
}

pub unsafe fn bch2_journal_key_insert(
    c: *mut crate::btree::types::bch_fs,
    btree_id: u8,
    level: u8,
    key: *const crate::btree::bkey::bkey_i,
) -> i32 {
    if c.is_null() || key.is_null() {
        return -22;
    }
    let bytes = crate::btree::bkey::bkey_bytes(&(*key).k);
    if bytes < core::mem::size_of::<crate::btree::bkey::bkey_i>() {
        return -22;
    }
    let layout = match std::alloc::Layout::from_size_align(bytes, core::mem::align_of::<u64>()) {
        Ok(layout) => layout,
        Err(_) => return -12,
    };
    let copied = std::alloc::alloc(layout).cast::<crate::btree::bkey::bkey_i>();
    if copied.is_null() {
        return -12;
    }
    core::ptr::copy_nonoverlapping(key.cast::<u8>(), copied.cast::<u8>(), bytes);
    let keys = &mut (*c).journal_keys;
    if let Some(existing) = keys.data.iter_mut().find(|entry| {
        entry.btree_id == btree_id
            && entry.level == level
            && !entry.allocated_k.is_null()
            && (*entry.allocated_k).k.p == (*key).k.p
    }) {
        if existing.allocated && !existing.allocated_k.is_null() {
            crate::btree::types::journal_key_free(existing.allocated_k);
        }
        existing.allocated = true;
        existing.allocated_k = copied;
        existing.overwritten = false;
        existing.overwritten_range = 0;
        __journal_keys_sort(keys);
        return 0;
    }
    keys.data.push(crate::btree::types::journal_key {
        btree_id,
        level,
        allocated: true,
        allocated_k: copied,
        ..Default::default()
    });
    keys.nr = keys.data.len();
    keys.size = keys.nr;
    __journal_keys_sort(keys);
    0
}

pub unsafe fn bch2_journal_keys_peek_max(
    c: *mut crate::btree::types::bch_fs,
    btree_id: u8,
    level: u8,
    pos: crate::btree::bkey::bpos,
    end_pos: crate::btree::bkey::bpos,
    idx: &mut usize,
) -> *const crate::btree::bkey::bkey_i {
    if c.is_null() {
        return core::ptr::null();
    }
    let keys = &(*c).journal_keys.data;
    assert!(*idx <= keys.len());
    while *idx < keys.len() {
        let key = &keys[*idx];
        let k = journal_key_ptr(key);
        if key.btree_id == btree_id
            && key.level == level
            && crate::btree::bkey::bpos_cmp((*k).k.p, pos) >= 0
            && crate::btree::bkey::bpos_cmp((*k).k.p, end_pos) <= 0
        {
            if key.overwritten {
                let range = usize::try_from(key.overwritten_range).unwrap_or(0);
                *idx = if range != 0 && range < (*c).journal_keys.overwrites.len() {
                    (&(*c).journal_keys.overwrites)[range].end
                } else {
                    *idx + 1
                };
                continue;
            }
            return k;
        }
        *idx += 1;
    }
    core::ptr::null()
}

pub unsafe fn bch2_journal_keys_peek_slot(
    c: *mut crate::btree::types::bch_fs,
    btree_id: u8,
    level: u8,
    pos: crate::btree::bkey::bpos,
) -> *const crate::btree::bkey::bkey_i {
    let mut idx = 0;
    let key = bch2_journal_keys_peek_max(c, btree_id, level, pos, pos, &mut idx);
    if !key.is_null() && (*key).k.p == pos {
        key
    } else {
        core::ptr::null()
    }
}

pub unsafe fn bch2_journal_keys_peek_prev_min(
    c: *mut crate::btree::types::bch_fs,
    btree_id: u8,
    level: u8,
    pos: crate::btree::bkey::bpos,
    end_pos: crate::btree::bkey::bpos,
    idx: &mut usize,
) -> *const crate::btree::bkey::bkey_i {
    if c.is_null() {
        return core::ptr::null();
    }
    let keys = &(*c).journal_keys.data;
    assert!(*idx <= keys.len());
    let mut cursor = if *idx == 0 {
        keys.len()
    } else {
        (*idx).saturating_add(1).min(keys.len())
    };
    while cursor != 0 {
        cursor -= 1;
        let key = &keys[cursor];
        let k = journal_key_ptr(key);
        if key.btree_id == btree_id
            && key.level == level
            && crate::btree::bkey::bpos_cmp((*k).k.p, pos) <= 0
            && crate::btree::bkey::bpos_cmp((*k).k.p, end_pos) >= 0
        {
            if key.overwritten {
                let range = usize::try_from(key.overwritten_range).unwrap_or(0);
                cursor = if range != 0 && range < (*c).journal_keys.overwrites.len() {
                    (&(*c).journal_keys.overwrites)[range].start
                } else {
                    cursor
                };
                if cursor == 0 {
                    break;
                }
                continue;
            }
            *idx = cursor;
            return k;
        }
    }
    *idx = 0;
    core::ptr::null()
}

pub unsafe fn bch2_key_deleted_in_journal(
    trans: *mut crate::btree::iter::btree_trans,
    btree_id: u8,
    level: u8,
    pos: crate::btree::bkey::bpos,
) -> bool {
    if trans.is_null() || !(*trans).journal_replay_not_finished {
        return false;
    }
    let key = bch2_journal_keys_peek_slot((*trans).c, btree_id, level, pos);
    !key.is_null() && (*key).k.type_ == crate::btree::bset::KEY_TYPE_deleted
}

unsafe fn __bch2_journal_key_overwritten(keys: &mut crate::btree::types::journal_keys, idx: usize) {
    let btree_id = keys.data[idx].btree_id;
    let level = keys.data[idx].level;
    keys.data[idx].overwritten = true;

    let prev_idx = if idx > 0
        && keys.data[idx - 1].btree_id == btree_id
        && keys.data[idx - 1].level == level
        && keys.data[idx - 1].overwritten
    {
        Some(idx - 1)
    } else {
        None
    };
    let next_idx = if idx + 1 < keys.data.len()
        && keys.data[idx + 1].btree_id == btree_id
        && keys.data[idx + 1].level == level
        && keys.data[idx + 1].overwritten
    {
        Some(idx + 1)
    } else {
        None
    };
    let prev_range = prev_idx
        .and_then(|i| usize::try_from(keys.data[i].overwritten_range).ok())
        .filter(|&i| i != 0 && i < keys.overwrites.len());
    let next_range = next_idx
        .and_then(|i| usize::try_from(keys.data[i].overwritten_range).ok())
        .filter(|&i| i != 0 && i < keys.overwrites.len());

    match (prev_range, next_range) {
        (Some(prev), Some(next)) => {
            let next_end = keys.overwrites[next].end;
            keys.overwrites[prev].end = next_end;
            keys.data[idx].overwritten_range = prev as u32;
            for item in keys.overwrites[next].start..next_end {
                if item < keys.data.len() {
                    keys.data[item].overwritten_range = prev as u32;
                }
            }
        }
        (Some(prev), None) => {
            keys.overwrites[prev].end = keys.overwrites[prev].end.max(idx + 1);
            keys.data[idx].overwritten_range = prev as u32;
        }
        (None, Some(next)) => {
            keys.overwrites[next].start = keys.overwrites[next].start.min(idx);
            keys.data[idx].overwritten_range = next as u32;
        }
        (None, None) => {
            if keys.overwrites.is_empty() {
                keys.overwrites
                    .push(crate::btree::types::journal_key_range_overwritten { start: 0, end: 0 });
            }
            let range = keys.overwrites.len();
            keys.overwrites
                .push(crate::btree::types::journal_key_range_overwritten {
                    start: prev_idx.unwrap_or(idx),
                    end: next_idx.map_or(idx + 1, |i| i + 1),
                });
            keys.data[idx].overwritten_range = range as u32;
            if let Some(i) = prev_idx {
                keys.data[i].overwritten_range = range as u32;
            }
            if let Some(i) = next_idx {
                keys.data[i].overwritten_range = range as u32;
            }
        }
    }
}

pub unsafe fn bch2_journal_key_check_or_overwrite(
    c: *mut crate::btree::types::bch_fs,
    btree_id: u8,
    level: u8,
    pos: crate::btree::bkey::bpos,
    check: bool,
) -> i32 {
    if c.is_null() {
        return 0;
    }
    let keys = &mut (*c).journal_keys;
    let Some(idx) = keys.data.iter().position(|entry| {
        entry.btree_id == btree_id
            && entry.level == level
            && !entry.allocated_k.is_null()
            && (*entry.allocated_k).k.p == pos
    }) else {
        return 0;
    };
    if keys.data[idx].overwritten {
        return 0;
    }
    if check {
        0
    } else {
        __bch2_journal_key_overwritten(keys, idx);
        0
    }
}

#[cfg(test)]
mod journal_key_overlay_tests {
    use super::*;

    #[test]
    fn replaces_same_slot_and_reads_ranges() {
        unsafe {
            let mut c = crate::btree::types::bch_fs::default();
            let mut first = crate::btree::bkey::bkey_i::default();
            first.k.u64s = crate::btree::bkey::BKEY_U64S;
            first.k.p = crate::btree::bkey::SPOS(2, 3, 0);
            first.k.type_ = crate::btree::bset::KEY_TYPE_btree_ptr_v2;
            assert_eq!(bch2_journal_key_insert(&mut c, 1, 0, &first), 0);

            let mut replacement = first;
            replacement.k.type_ = crate::btree::bset::KEY_TYPE_deleted;
            assert_eq!(bch2_journal_key_insert(&mut c, 1, 0, &replacement), 0);

            let got = bch2_journal_keys_peek_slot(&mut c, 1, 0, first.k.p);
            assert!(!got.is_null());
            assert_eq!((*got).k.type_, crate::btree::bset::KEY_TYPE_deleted);
            assert_eq!(c.journal_keys.data[0].overwritten_range, 0);
            let mut idx = 0;
            assert!(
                bch2_journal_keys_peek_max(&mut c, 1, 0, first.k.p, first.k.p, &mut idx,) == got
            );
            let mut trans = crate::btree::iter::btree_trans::default();
            trans.c = &mut c;
            trans.journal_replay_not_finished = true;
            assert!(bch2_key_deleted_in_journal(&mut trans, 1, 0, first.k.p,));
            c.journal
                .flags
                .fetch_or(1usize << JOURNAL_replay_done, Ordering::AcqRel);
            crate::btree::iter::bch2_trans_begin(&mut trans);
            assert!(!trans.journal_replay_not_finished);
            let mut later = first;
            later.k.p = crate::btree::bkey::SPOS(5, 1, 0);
            later.k.type_ = crate::btree::bset::KEY_TYPE_btree_ptr_v2;
            assert_eq!(bch2_journal_key_insert(&mut c, 1, 0, &later), 0);
            let mut following = later;
            following.k.p = crate::btree::bkey::SPOS(5, 2, 0);
            assert_eq!(bch2_journal_key_insert(&mut c, 1, 0, &following), 0);
            let mut prev_idx = 0;
            let previous = bch2_journal_keys_peek_prev_min(
                &mut c,
                1,
                0,
                crate::btree::bkey::SPOS(6, 0, 0),
                crate::btree::bkey::POS_MIN,
                &mut prev_idx,
            );
            assert!(!previous.is_null());
            assert_eq!((*previous).k.p, following.k.p);
            let previous_again = bch2_journal_keys_peek_prev_min(
                &mut c,
                1,
                0,
                crate::btree::bkey::SPOS(6, 0, 0),
                crate::btree::bkey::POS_MIN,
                &mut prev_idx,
            );
            assert_eq!(previous_again, previous);
            assert_eq!(
                bch2_journal_key_check_or_overwrite(&mut c, 1, 0, later.k.p, true,),
                0
            );
            assert_eq!(
                bch2_journal_key_check_or_overwrite(&mut c, 1, 0, later.k.p, false,),
                0
            );
            assert_eq!(
                bch2_journal_key_check_or_overwrite(&mut c, 1, 0, following.k.p, false,),
                0
            );
            let later_entry = c
                .journal_keys
                .data
                .iter()
                .find(|entry| (*entry.allocated_k).k.p == later.k.p)
                .unwrap();
            let following_entry = c
                .journal_keys
                .data
                .iter()
                .find(|entry| (*entry.allocated_k).k.p == following.k.p)
                .unwrap();
            assert_eq!(
                later_entry.overwritten_range,
                following_entry.overwritten_range
            );
            assert!(later_entry.overwritten_range != 0);
            let mut skipped_idx = 0;
            assert!(bch2_journal_keys_peek_max(
                &mut c,
                1,
                0,
                later.k.p,
                following.k.p,
                &mut skipped_idx,
            )
            .is_null());
            let mut inserted_before = following;
            inserted_before.k.p = crate::btree::bkey::SPOS(4, 9, 0);
            assert_eq!(bch2_journal_key_insert(&mut c, 1, 0, &inserted_before), 0);
            let later_range = c
                .journal_keys
                .data
                .iter()
                .find(|entry| (*entry.allocated_k).k.p == later.k.p)
                .unwrap()
                .overwritten_range;
            let following_range = c
                .journal_keys
                .data
                .iter()
                .find(|entry| (*entry.allocated_k).k.p == following.k.p)
                .unwrap()
                .overwritten_range;
            assert_eq!(later_range, following_range);
            assert!(later_range != 0);
            assert_eq!(
                bch2_journal_key_check_or_overwrite(&mut c, 1, 0, later.k.p, true,),
                0
            );
            bch2_journal_keys_clear(&mut c);
            assert!(bch2_journal_keys_peek_slot(&mut c, 1, 0, first.k.p).is_null());
        }
    }

    #[test]
    fn copies_each_variable_length_journal_key() {
        unsafe {
            for u64s in crate::btree::bkey::BKEY_U64S..=crate::btree::bkey::BKEY_U64S + 16 {
                let mut c = crate::btree::types::bch_fs::default();
                let mut source = vec![0u64; u64s as usize];
                let key = source.as_mut_ptr().cast::<crate::btree::bkey::bkey_i>();
                (*key).k = crate::btree::bkey::bkey {
                    u64s,
                    format: crate::btree::bkey::KEY_FORMAT_CURRENT,
                    type_: crate::btree::bset::KEY_TYPE_btree_ptr_v2,
                    p: crate::btree::bkey::SPOS(7, u64::from(u64s), 0),
                    ..Default::default()
                };
                for (offset, word) in source
                    .iter_mut()
                    .enumerate()
                    .skip(crate::btree::bkey::BKEY_U64S as usize)
                {
                    *word = 0xa5a5_0000_0000_0000 | offset as u64;
                }

                assert_eq!(bch2_journal_key_insert(&mut c, 1, 0, key), 0);
                let copied = c.journal_keys.data[0].allocated_k;
                assert_ne!(copied, key);
                assert_eq!(
                    core::slice::from_raw_parts(copied.cast::<u64>(), u64s as usize),
                    source.as_slice(),
                );

                bch2_journal_keys_clear(&mut c);
                assert!(c.journal_keys.data.is_empty());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_device_round_trip_resumes_bucket_position_and_rejects_corruption() {
        use crate::btree::types::bch_fs;
        use crate::sb::io::{bch2_free_super, bch2_sb_field_resize_id, bch2_sb_realloc};
        use crate::sb::{
            bch_member, bch_sb_field_journal_v2, bch_sb_field_journal_v2_entry,
            bch_sb_field_members_v2, BCH_SB_FIELD_journal_v2, BCH_SB_FIELD_members_v2,
        };
        use std::os::unix::fs::FileExt;

        unsafe {
            let path =
                std::env::temp_dir().join(format!("subvol-journal-device-{}", std::process::id()));
            let file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .read(true)
                .write(true)
                .open(&path)
                .unwrap();
            file.set_len(128 * 512).unwrap();

            let setup = |c: &mut bch_fs| {
                c.disk_sb.s_bdev_file = Box::into_raw(Box::new(file.try_clone().unwrap())).cast();
                assert_eq!(bch2_sb_realloc(&mut c.disk_sb, 0), 0);
                (*c.disk_sb.sb).version = crate::sb::bcachefs_metadata_version_current;
                (*c.disk_sb.sb).uuid = [0x42; 16];
                (*c.disk_sb.sb).dev_idx = 0;
                (*c.disk_sb.sb).nr_devices = 1;
                (*c.disk_sb.sb).block_size = 1;

                let members_u64s = (core::mem::size_of::<bch_sb_field_members_v2>()
                    + core::mem::size_of::<bch_member>())
                .div_ceil(8) as u32;
                let members =
                    bch2_sb_field_resize_id(&mut c.disk_sb, BCH_SB_FIELD_members_v2, members_u64s)
                        .cast::<bch_sb_field_members_v2>();
                (*members).member_bytes = core::mem::size_of::<bch_member>() as u16;
                *members
                    .cast::<u8>()
                    .add(core::mem::size_of::<bch_sb_field_members_v2>())
                    .cast::<bch_member>() = bch_member {
                    nbuckets: 64,
                    first_bucket: 8,
                    bucket_size: 2,
                    ..Default::default()
                };

                let journal = bch2_sb_field_resize_id(&mut c.disk_sb, BCH_SB_FIELD_journal_v2, 3)
                    .cast::<bch_sb_field_journal_v2>();
                *journal
                    .cast::<u8>()
                    .add(core::mem::size_of::<bch_sb_field_journal_v2>())
                    .cast::<bch_sb_field_journal_v2_entry>() =
                    bch_sb_field_journal_v2_entry { start: 32, nr: 4 };
            };

            let mut source = bch_fs::default();
            setup(&mut source);
            let mut start = journal_start_info::default();
            assert_eq!(bch2_journal_read(&mut source, &mut start), 0);
            assert_eq!(start.cur_seq, 1);
            assert_eq!(
                *source.journal.space.lock().unwrap(),
                [
                    journal_space {
                        next_entry: 2,
                        total: 6
                    },
                    journal_space {
                        next_entry: 2,
                        total: 6
                    },
                    journal_space {
                        next_entry: 2,
                        total: 6
                    },
                    journal_space {
                        next_entry: 2,
                        total: 8
                    },
                ]
            );
            assert!(journal_med_on_space(&source.journal));
            assert!(!journal_low_on_space(&source.journal));
            assert_eq!(
                source.journal.watermark.load(Ordering::Acquire),
                bch_watermark::BCH_WATERMARK_stripe as u32
            );
            let mut source_btree = crate::btree::types::btree::default();
            source_btree.flags |= 1usize << crate::btree::io::BTREE_NODE_dirty;
            for value in [101u64, 202] {
                let mut res = journal_res::default();
                assert_eq!(bch2_journal_res_get(&source.journal, &mut res, 2, 0), 0);
                let entry = bch2_journal_add_entry(
                    &source.journal,
                    &mut res,
                    BCH_JSET_ENTRY_overwrite,
                    0,
                    0,
                    1,
                );
                *entry.cast::<u64>().add(1) = value;
                bch2_journal_pin_add(
                    &source.journal,
                    res.seq,
                    &mut source_btree.writes[0].journal,
                    crate::btree::update::bch2_btree_node_flush0,
                );
                bch2_journal_res_put(&source.journal, &mut res);
                assert_eq!(bch2_journal_flush(&source.journal), 0);
            }
            {
                let ja = source.journal.device.lock().unwrap();
                assert_eq!((ja.cur_idx, ja.sectors_free), (0, 0));
            }

            let mut resumed = bch_fs::default();
            setup(&mut resumed);
            assert_eq!(bch2_journal_read(&mut resumed, &mut start), 0);
            assert_eq!((start.last_seq, start.replay_end, start.cur_seq), (1, 2, 3));
            assert_eq!(resumed.journal.closed.lock().unwrap().len(), 2);
            {
                let ja = resumed.journal.device.lock().unwrap();
                assert_eq!((ja.cur_idx, ja.sectors_free), (0, 0));
            }

            let mut res = journal_res::default();
            assert_eq!(bch2_journal_res_get(&resumed.journal, &mut res, 2, 0), 0);
            let entry = bch2_journal_add_entry(
                &resumed.journal,
                &mut res,
                BCH_JSET_ENTRY_overwrite,
                0,
                0,
                1,
            );
            *entry.cast::<u64>().add(1) = 303;
            bch2_journal_res_put(&resumed.journal, &mut res);
            assert_eq!(bch2_journal_flush(&resumed.journal), 0);
            {
                let ja = resumed.journal.device.lock().unwrap();
                assert_eq!((ja.cur_idx, ja.sectors_free), (1, 1));
            }
            assert!(journal_low_on_space(&resumed.journal));
            assert_eq!(
                resumed.journal.watermark.load(Ordering::Acquire),
                bch_watermark::BCH_WATERMARK_reclaim as u32
            );
            let mut blocked = journal_res::default();
            assert_eq!(
                bch2_journal_res_get(&resumed.journal, &mut blocked, 1, 0),
                -9
            );
            assert!(!blocked.ref_);

            let mut reopened = bch_fs::default();
            setup(&mut reopened);
            assert_eq!(bch2_journal_read(&mut reopened, &mut start), 0);
            assert_eq!((start.replay_end, start.cur_seq), (3, 4));
            let records = reopened.journal.closed.lock().unwrap();
            assert_eq!(
                records.iter().map(|record| record[3]).collect::<Vec<_>>(),
                [1, 2, 3]
            );
            assert_eq!(
                records
                    .iter()
                    .map(|record| record[JSET_HEADER_U64S + 1])
                    .collect::<Vec<_>>(),
                [101, 202, 303]
            );
            drop(records);

            let third_offset = (33 * 2) * 512;
            let mut third_sector = vec![0u8; 512];
            assert_eq!(file.read_at(&mut third_sector, third_offset).unwrap(), 512);
            let third = &mut *third_sector.as_mut_ptr().cast::<jset>();
            SET_JSET_NO_FLUSH(third, 1);
            third.csum = bch_csum::default();
            let third_bytes = core::mem::size_of::<jset>() + third.u64s as usize * 8;
            let third_checksum = crate::checksum::bch2_checksum(
                crate::checksum::BCH_CSUM_xxhash,
                &third_sector[16..third_bytes],
            );
            third.csum = bch_csum {
                lo: third_checksum.lo,
                hi: third_checksum.hi,
            };
            assert_eq!(file.write_at(&third_sector, third_offset).unwrap(), 512);

            let mut noflush_tail = bch_fs::default();
            setup(&mut noflush_tail);
            assert_eq!(bch2_journal_read(&mut noflush_tail, &mut start), 0);
            assert_eq!((start.last_seq, start.replay_end, start.cur_seq), (1, 2, 4));
            assert_eq!(
                noflush_tail
                    .journal
                    .closed
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|record| record[3])
                    .collect::<Vec<_>>(),
                [1, 2]
            );

            let corrupt_offset = (32 * 2 + 1) * 512 + 64;
            let mut byte = [0u8; 1];
            assert_eq!(file.read_at(&mut byte, corrupt_offset).unwrap(), 1);
            byte[0] ^= 1;
            assert_eq!(file.write_at(&byte, corrupt_offset).unwrap(), 1);
            let mut corrupted = bch_fs::default();
            setup(&mut corrupted);
            assert_eq!(bch2_journal_read(&mut corrupted, &mut start), -6);

            byte[0] ^= 1;
            assert_eq!(file.write_at(&byte, corrupt_offset).unwrap(), 1);
            let third = &mut *third_sector.as_mut_ptr().cast::<jset>();
            SET_JSET_NO_FLUSH(third, 0);
            third.csum = bch_csum::default();
            let third_checksum = crate::checksum::bch2_checksum(
                crate::checksum::BCH_CSUM_xxhash,
                &third_sector[16..third_bytes],
            );
            third.csum = bch_csum {
                lo: third_checksum.lo,
                hi: third_checksum.hi,
            };
            assert_eq!(file.write_at(&third_sector, third_offset).unwrap(), 512);
            let second_magic_offset = (32 * 2 + 1) * 512 + 16;
            let mut second_magic = [0u8; 8];
            assert_eq!(
                file.read_at(&mut second_magic, second_magic_offset)
                    .unwrap(),
                8
            );
            assert_eq!(file.write_at(&[0u8; 8], second_magic_offset).unwrap(), 8);
            let mut missing = bch_fs::default();
            setup(&mut missing);
            assert_eq!(bch2_journal_read(&mut missing, &mut start), -8);

            assert_eq!(
                file.write_at(&second_magic, second_magic_offset).unwrap(),
                8
            );
            let mut protected = bch_fs::default();
            setup(&mut protected);
            assert_eq!(bch2_journal_read(&mut protected, &mut start), 0);
            assert_eq!((start.last_seq, start.replay_end, start.cur_seq), (1, 3, 4));
            for (value, expected) in [(404u64, 0), (505, 0), (606, 0), (707, -9)] {
                let mut res = journal_res::default();
                assert_eq!(
                    bch2_journal_res_get(
                        &protected.journal,
                        &mut res,
                        2,
                        bch_watermark::BCH_WATERMARK_reclaim as u32,
                    ),
                    0
                );
                let entry = bch2_journal_add_entry(
                    &protected.journal,
                    &mut res,
                    BCH_JSET_ENTRY_overwrite,
                    0,
                    0,
                    1,
                );
                *entry.cast::<u64>().add(1) = value;
                bch2_journal_res_put(&protected.journal, &mut res);
                assert_eq!(bch2_journal_flush(&protected.journal), expected);
            }
            let ja = protected.journal.device.lock().unwrap();
            assert_eq!((ja.cur_idx, ja.sectors_free), (2, 0));
            assert_eq!(&ja.bucket_seq[..3], &[2, 4, 6]);
            drop(ja);

            let mut full_reopen = bch_fs::default();
            setup(&mut full_reopen);
            assert_eq!(bch2_journal_read(&mut full_reopen, &mut start), 0);
            assert_eq!((start.last_seq, start.replay_end, start.cur_seq), (1, 6, 7));
            assert_eq!(
                full_reopen
                    .journal
                    .closed
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|record| record[3])
                    .collect::<Vec<_>>(),
                [1, 2, 3, 4, 5, 6]
            );

            assert_eq!(bch2_journal_replay(&mut protected), 0);
            assert_ne!(
                protected.journal.flags.load(Ordering::Acquire) & (1usize << JOURNAL_replay_done),
                0
            );
            assert_eq!(protected.journal.last_seq.load(Ordering::Acquire), 7);
            assert_eq!(protected.journal.last_seq_ondisk.load(Ordering::Acquire), 1);
            {
                let ja = protected.journal.device.lock().unwrap();
                assert_eq!(
                    (ja.discard_idx, ja.dirty_idx_ondisk, ja.dirty_idx),
                    (0, 0, 2)
                );
            }
            assert_eq!(bch2_journal_flush(&protected.journal), 0);
            assert_eq!(protected.journal.last_seq_ondisk.load(Ordering::Acquire), 8);
            for value in [808u64, 909] {
                let mut res = journal_res::default();
                assert_eq!(
                    bch2_journal_res_get(
                        &protected.journal,
                        &mut res,
                        2,
                        bch_watermark::BCH_WATERMARK_reclaim as u32,
                    ),
                    0
                );
                let entry = bch2_journal_add_entry(
                    &protected.journal,
                    &mut res,
                    BCH_JSET_ENTRY_overwrite,
                    0,
                    0,
                    1,
                );
                *entry.cast::<u64>().add(1) = value;
                bch2_journal_res_put(&protected.journal, &mut res);
                assert_eq!(bch2_journal_flush(&protected.journal), 0);
            }
            {
                let ja = protected.journal.device.lock().unwrap();
                assert_eq!((ja.cur_idx, ja.sectors_free), (0, 1));
                assert_eq!(ja.bucket_seq[0], 9);
            }

            let mut reused_reopen = bch_fs::default();
            setup(&mut reused_reopen);
            assert_eq!(bch2_journal_read(&mut reused_reopen, &mut start), 0);
            assert_eq!(
                (start.last_seq, start.replay_end, start.cur_seq),
                (9, 9, 10)
            );
            assert!(start.clean);
            assert_eq!(
                reused_reopen.journal.closed.lock().unwrap()[0][JSET_HEADER_U64S + 1],
                909
            );

            bch2_free_super(&mut reused_reopen.disk_sb);
            bch2_free_super(&mut full_reopen.disk_sb);
            bch2_free_super(&mut protected.disk_sb);
            bch2_free_super(&mut missing.disk_sb);
            bch2_free_super(&mut noflush_tail.disk_sb);
            bch2_free_super(&mut corrupted.disk_sb);
            bch2_free_super(&mut reopened.disk_sb);
            bch2_free_super(&mut resumed.disk_sb);
            bch2_free_super(&mut source.disk_sb);
            drop(file);
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn journal_disk_layout_matches_local_format() {
        assert_eq!(core::mem::size_of::<bch_csum>(), 16);
        assert_eq!(core::mem::size_of::<jset_entry>(), 8);
        assert_eq!(core::mem::size_of::<jset>(), 56);
        assert_eq!(core::mem::size_of::<journal_res>(), 16);
    }

    #[test]
    fn direct_reclaim_keeps_btree_pin_unflushed_after_write_error() {
        unsafe {
            let mut c = crate::btree::types::bch_fs::default();
            let mut b = crate::btree::types::btree::default();
            b.flags |= 1usize << crate::btree::io::BTREE_NODE_dirty;
            crate::btree::update::bch2_btree_add_journal_pin(&mut c, &mut b, 1);
            assert_eq!(b.writes[0].journal.seq, 1);
            assert_eq!(bch2_journal_flush(&c.journal), 0);
            assert_eq!(c.journal.last_seq.load(Ordering::Acquire), 1);

            assert_eq!(bch2_journal_reclaim(&c.journal), 0);
            assert_eq!(c.journal.nr_direct_reclaim.load(Ordering::Acquire), 0);
            {
                let _reclaim = c.journal.reclaim_lock.lock().unwrap();
                assert_eq!(__bch2_journal_reclaim(&c.journal, false, true), 0);
            }
            assert_eq!(c.journal.nr_background_reclaim.load(Ordering::Acquire), 0);
            assert_eq!(b.writes[0].journal.seq, 1);
            assert_eq!(c.journal.flush_in_progress.load(Ordering::Acquire), 0);
            let lists = c.journal.pin.lock().unwrap();
            assert_eq!(lists.1[0].count, 1);
            assert_eq!(
                lists.1[0].unflushed[3],
                [(&mut b.writes[0].journal as *mut _) as usize]
            );
            assert!(lists.1[0].flushed.is_empty());
        }
    }

    #[test]
    fn reservations_encode_entries_and_cycle_sequence() {
        let j = journal::default();
        let mut res = journal_res::default();
        assert_eq!(bch2_journal_res_get(&j, &mut res, 6, 0), 0);
        assert_eq!(res.seq, 1);
        unsafe {
            let entry = bch2_journal_add_entry(&j, &mut res, BCH_JSET_ENTRY_btree_keys, 3, 0, 5);
            let payload = (entry as *mut u64).add(1);
            for i in 0..5 {
                *payload.add(i) = 10 + i as u64;
            }
        }
        bch2_journal_res_put(&j, &mut res);
        assert_eq!(bch2_journal_flush(&j), 0);
        assert_eq!(j.seq.load(Ordering::Acquire), 2);
        let closed = j.closed.lock().unwrap();
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0][2], JSET_MAGIC);
        assert_eq!(closed[0][3], 1);
        assert_eq!(closed[0][5], 6);
        let entry = unsafe {
            &*(closed[0]
                .as_ptr()
                .add(JSET_HEADER_U64S)
                .cast::<jset_entry>())
        };
        assert_eq!(entry.u64s, 5);
        assert_eq!(entry.btree_id, 3);
        assert_eq!(entry.type_, BCH_JSET_ENTRY_btree_keys);
        assert_eq!(&closed[0][JSET_HEADER_U64S + 1..], &[10, 11, 12, 13, 14]);
    }

    #[test]
    fn btree_roots_round_trip_through_current_journal_entry() {
        use crate::btree::bkey::{bkey, KEY_FORMAT_CURRENT, SPOS};
        use crate::btree::bset::{bkey_i_to_btree_ptr_v2, KEY_TYPE_btree_ptr_v2};
        use crate::btree::types::bch_fs;

        unsafe {
            let mut source = bch_fs::default();
            let root = &mut source.btree.cache.roots_known[3];
            root.alive = 1;
            root.level = 2;
            root.key.k = bkey {
                u64s: 10,
                format: KEY_FORMAT_CURRENT,
                type_: KEY_TYPE_btree_ptr_v2,
                p: SPOS(7, 11, 0),
                ..Default::default()
            };
            let root_ptr = bkey_i_to_btree_ptr_v2(&mut root.key);
            (*root_ptr).v.mem_ptr = 0xfeed_beef;
            (*root_ptr).v.seq = 41;
            (*root_ptr).v.sectors_written = 8;

            let mut record = vec![0u64; JSET_HEADER_U64S + 11];
            record[2] = JSET_MAGIC;
            record[3] = 9;
            record[5] = 11;
            record[6] = 9;
            let entry = record
                .as_mut_ptr()
                .add(JSET_HEADER_U64S)
                .cast::<jset_entry>();
            let end =
                crate::btree::interior::bch2_btree_roots_to_journal_entries(&mut source, entry, 0);
            assert_eq!(end.cast::<u64>().offset_from(entry.cast::<u64>()), 11);
            assert_eq!((*entry).type_, BCH_JSET_ENTRY_btree_root);
            assert_eq!((*entry).btree_id, 3);
            assert_eq!((*entry).level, 2);

            let mut replay = bch_fs::default();
            replay.journal.closed.lock().unwrap().push(record);
            assert_eq!(bch2_journal_replay(&mut replay), 0);
            let replayed = &mut replay.btree.cache.roots_known[3];
            assert_eq!(replayed.alive, 1);
            assert_eq!(replayed.level, 2);
            assert_eq!(replayed.key.k.p, SPOS(7, 11, 0));
            let replayed_ptr = bkey_i_to_btree_ptr_v2(&mut replayed.key);
            assert_eq!((*replayed_ptr).v.mem_ptr, 0);
            assert_eq!((*replayed_ptr).v.seq, 41);
            assert_eq!((*replayed_ptr).v.sectors_written, 8);

            let mut skipped = [0u64; 11];
            let begin = skipped.as_mut_ptr().cast::<jset_entry>();
            assert_eq!(
                crate::btree::interior::bch2_btree_roots_to_journal_entries(
                    &mut source,
                    begin,
                    1 << 3,
                ),
                begin
            );

            let mut bad_size = vec![0u64; JSET_HEADER_U64S + 11];
            bad_size[2] = JSET_MAGIC;
            bad_size[3] = 10;
            bad_size[5] = 11;
            bad_size[6] = 10;
            let bad_entry = bad_size
                .as_mut_ptr()
                .add(JSET_HEADER_U64S)
                .cast::<jset_entry>();
            journal_entry_init(bad_entry, BCH_JSET_ENTRY_btree_root, 3, 2, 10);
            let bad_key = bad_entry
                .cast::<u64>()
                .add(1)
                .cast::<crate::btree::bkey::bkey_i>();
            (*bad_key).k = bkey {
                u64s: 9,
                format: KEY_FORMAT_CURRENT,
                type_: KEY_TYPE_btree_ptr_v2,
                ..Default::default()
            };
            let mut repaired = bch_fs::default();
            repaired.journal.closed.lock().unwrap().push(bad_size);
            assert_eq!(bch2_journal_replay(&mut repaired), 0);
            assert_eq!(repaired.btree.cache.roots_known[3].alive, 0);

            let mut oversized = vec![0u64; JSET_HEADER_U64S + 22];
            oversized[2] = JSET_MAGIC;
            oversized[3] = 11;
            oversized[5] = 22;
            oversized[6] = 11;
            let oversized_entry = oversized
                .as_mut_ptr()
                .add(JSET_HEADER_U64S)
                .cast::<jset_entry>();
            journal_entry_init(oversized_entry, BCH_JSET_ENTRY_btree_root, 3, 2, 21);
            let oversized_key = oversized_entry
                .cast::<u64>()
                .add(1)
                .cast::<crate::btree::bkey::bkey_i>();
            (*oversized_key).k = bkey {
                u64s: 21,
                format: KEY_FORMAT_CURRENT,
                type_: KEY_TYPE_btree_ptr_v2,
                ..Default::default()
            };
            let mut rejected = bch_fs::default();
            rejected.journal.closed.lock().unwrap().push(oversized);
            assert_eq!(bch2_journal_replay(&mut rejected), -8);
        }
    }

    #[test]
    fn replay_uses_the_newest_flushed_boundary_before_replaying_roots() {
        use crate::btree::bkey::{bkey, BKEY_U64S, KEY_FORMAT_CURRENT, SPOS};
        use crate::btree::bset::KEY_TYPE_btree_ptr_v2;
        use crate::btree::types::bch_fs;

        unsafe {
            let root_u64s = BKEY_U64S + 5;
            let make_root = |seq: u64, pos| {
                let mut record =
                    vec![0u64; JSET_HEADER_U64S + jset_u64s(root_u64s as u32) as usize];
                record[2] = JSET_MAGIC;
                record[3] = seq;
                record[5] = jset_u64s(root_u64s as u32) as u64;
                record[6] = 1;
                let entry = record
                    .as_mut_ptr()
                    .add(JSET_HEADER_U64S)
                    .cast::<jset_entry>();
                journal_entry_init(entry, BCH_JSET_ENTRY_btree_root, 3, 2, root_u64s as u16);
                let key = entry
                    .cast::<u64>()
                    .add(1)
                    .cast::<crate::btree::bkey::bkey_i>();
                (*key).k = bkey {
                    u64s: root_u64s,
                    format: KEY_FORMAT_CURRENT,
                    type_: KEY_TYPE_btree_ptr_v2,
                    p: pos,
                    ..Default::default()
                };
                record
            };

            let durable = make_root(1, SPOS(7, 11, 0));
            let mut unflushed_tail = make_root(2, SPOS(7, 99, 0));
            SET_JSET_NO_FLUSH(&mut *unflushed_tail.as_mut_ptr().cast::<jset>(), 1);

            let mut replay = bch_fs::default();
            replay
                .journal
                .closed
                .lock()
                .unwrap()
                .extend([unflushed_tail, durable]);
            assert_eq!(bch2_journal_replay(&mut replay), 0);
            let root = &replay.btree.cache.roots_known[3];
            assert_eq!(root.alive, 1);
            assert_eq!(root.key.k.p, SPOS(7, 11, 0));
            assert_ne!(
                replay.journal.flags.load(Ordering::Acquire) & (1usize << JOURNAL_replay_done),
                0,
            );
        }
    }

    #[test]
    fn replay_rejects_conflicting_duplicate_journal_records() {
        unsafe {
            let mut record = vec![0u64; JSET_HEADER_U64S];
            record[2] = JSET_MAGIC;
            record[3] = 1;
            record[6] = 1;
            let mut conflict = record.clone();
            conflict[0] = 1;

            let mut replay = crate::btree::types::bch_fs::default();
            replay
                .journal
                .closed
                .lock()
                .unwrap()
                .extend([record, conflict]);
            assert_eq!(bch2_journal_replay(&mut replay), -7);
        }
    }

    #[test]
    fn replay_restarts_after_a_leaf_split() {
        use crate::btree::bkey::{
            bkey, bkey_format_key_bits, BKEY_FORMAT_CURRENT, BKEY_U64S, KEY_FORMAT_CURRENT,
            POS_MIN, SPOS, SPOS_MAX,
        };
        use crate::btree::bset::{bset as disk_bset, btree_node as disk_btree_node};
        use crate::btree::iter::{
            bch2_btree_iter_next, bch2_btree_iter_peek, bch2_trans_init, bch2_trans_iter_exit,
            bch2_trans_iter_init, btree_iter, btree_trans,
        };
        use crate::btree::types::{
            bch2_btree_id_root_set, bch_fs, bset_tree, BSET_NO_AUX_TREE_VAL,
        };
        use crate::sb::io::{bch2_free_super, bch2_sb_realloc};

        unsafe {
            let mut words = vec![0u64; 64];
            let mut leaf = Box::new(crate::btree::types::btree::default());
            leaf.data = words.as_mut_ptr().cast::<disk_btree_node>();
            leaf.byte_order = 9;
            leaf.format = BKEY_FORMAT_CURRENT;
            leaf.nr_key_bits = bkey_format_key_bits(&leaf.format) as u8;
            leaf.nsets = 1;
            (*leaf.data).min_key = POS_MIN;
            (*leaf.data).max_key = SPOS_MAX;
            let disk_set = words.as_mut_ptr().add(17).cast::<disk_bset>();
            (*disk_set).u64s = 40;
            for idx in 0..8 {
                *words.as_mut_ptr().add(20 + idx * 5).cast::<bkey>() = bkey {
                    u64s: BKEY_U64S,
                    format: KEY_FORMAT_CURRENT,
                    type_: 6,
                    p: SPOS(1, idx as u64 + 1, 0),
                    ..Default::default()
                };
            }
            leaf.set[0] = bset_tree {
                size: 0,
                extra: BSET_NO_AUX_TREE_VAL,
                data_offset: 17,
                aux_data_offset: u16::MAX,
                end_offset: 60,
            };
            leaf.nr.live_u64s = 40;
            leaf.nr.bset_u64s[0] = 40;
            leaf.nr.unpacked_keys = 8;

            let mut replay = bch_fs::default();
            assert_eq!(bch2_sb_realloc(&mut replay.disk_sb, 0), 0);
            (*replay.disk_sb.sb).flags[0] = 1 << 12;
            bch2_btree_id_root_set(&mut replay, 0, &mut *leaf);

            let key_u64s = BKEY_U64S as u32;
            let mut record = vec![0u64; JSET_HEADER_U64S + jset_u64s(key_u64s) as usize];
            record[2] = JSET_MAGIC;
            record[3] = 1;
            record[5] = jset_u64s(key_u64s) as u64;
            record[6] = 1;
            let entry = record
                .as_mut_ptr()
                .add(JSET_HEADER_U64S)
                .cast::<jset_entry>();
            journal_entry_init(entry, BCH_JSET_ENTRY_btree_keys, 0, 0, key_u64s as u16);
            let key = entry
                .cast::<u64>()
                .add(1)
                .cast::<crate::btree::bkey::bkey_i>();
            (*key).k = bkey {
                u64s: key_u64s as u8,
                format: KEY_FORMAT_CURRENT,
                type_: 6,
                p: SPOS(1, 9, 0),
                ..Default::default()
            };
            replay.journal.closed.lock().unwrap().push(record);

            assert_eq!(bch2_journal_replay(&mut replay), 0);
            let root = crate::btree::types::bch2_btree_id_root_b(&replay, 0);
            assert_eq!((*root).c.level, 1);
            assert!(replay.journal_keys.data.iter().all(|key| key.overwritten));

            let mut trans = btree_trans::default();
            bch2_trans_init(&mut trans, &mut replay);
            let mut iter = btree_iter::default();
            bch2_trans_iter_init(&mut trans, &mut iter, 0, SPOS(1, 0, 0), 0);
            let mut seen = Vec::new();
            let mut key = bch2_btree_iter_peek(&mut iter);
            while !key.k.is_null() {
                seen.push((*key.k).p.offset);
                key = bch2_btree_iter_next(&mut iter);
            }
            bch2_trans_iter_exit(&mut iter);
            assert_eq!(seen, (1..=9).collect::<Vec<_>>());

            bch2_free_super(&mut replay.disk_sb);
        }
    }
}
