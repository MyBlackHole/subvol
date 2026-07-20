use crate::bcachefs::*;
use crate::bcachefs_format::*;
use crate::errcode::*;
use crate::opts::BchOpts;

pub const JOURNAL_SEQ_MAX: u64 = (1u64 << 56) - 1;
pub const JOURNAL_STATE_BUF_BITS: u32 = 2;
pub const JOURNAL_STATE_BUF_NR: u32 = 1 << JOURNAL_STATE_BUF_BITS;
pub const JOURNAL_STATE_BUF_MASK: u32 = JOURNAL_STATE_BUF_NR - 1;
pub const JOURNAL_STATE_BUF_COUNT_BITS: u32 = 10;
pub const JOURNAL_STATE_BUF_COUNT_MAX: u32 = (1 << JOURNAL_STATE_BUF_COUNT_BITS) - 1;
pub const JOURNAL_STATE_BUF0_SHIFT: u32 = 24;

pub const JOURNAL_ENTRY_SIZE_MIN: u32 = 64 << 10;
pub const JOURNAL_ENTRY_SIZE_MAX: u32 = 4 << 20;

pub const JOURNAL_ENTRY_OFFSET_MAX: u32 = (1 << 22) - 1;
pub const JOURNAL_ENTRY_BLOCKED_VAL: u32 = JOURNAL_ENTRY_OFFSET_MAX - 2;
pub const JOURNAL_ENTRY_CLOSED_VAL: u32 = JOURNAL_ENTRY_OFFSET_MAX - 1;
pub const JOURNAL_ENTRY_ERROR_VAL: u32 = JOURNAL_ENTRY_OFFSET_MAX;

pub const JOURNAL_BUF_NOT_IN_FLIGHT: u64 = 1;
pub const JOURNAL_BUF_NOFLUSH: u64 = 2;
pub const JOURNAL_BUF_FLUSH_NO_WAIT: u64 = 3;

pub const JOURNAL_PIN: usize = 32 * 1024;

#[derive(Clone, Copy, Debug)]
pub struct JournalBuf {
    pub data: *mut u8,
    pub buf_size: u32,
    pub sectors: u32,
    pub disk_sectors: u32,
    pub u64s_reserved: u32,
    pub last_seq: u64,
    pub flush_picked: bool,
    pub flush: bool,
    pub separate_flush: bool,
    pub need_flush_to_write_buffer: bool,
    pub write_started: bool,
    pub write_allocated: bool,
    pub write_done: bool,
    pub empty: bool,
    pub has_overwrites: bool,
    pub devs_written: [u8; BCH_REPLICAS_MAX as usize],
    pub devs_written_nr: u8,
}

impl JournalBuf {
    pub fn new() -> Self {
        JournalBuf {
            data: std::ptr::null_mut(),
            buf_size: 0,
            sectors: 0,
            disk_sectors: 0,
            u64s_reserved: 0,
            last_seq: 0,
            flush_picked: false,
            flush: false,
            separate_flush: false,
            need_flush_to_write_buffer: false,
            write_started: false,
            write_allocated: false,
            write_done: false,
            empty: false,
            has_overwrites: false,
            devs_written: [0; BCH_REPLICAS_MAX as usize],
            devs_written_nr: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct JournalRes {
    pub ref_: bool,
    pub has_overwrites: bool,
    pub u64s: u16,
    pub offset: u32,
    pub seq: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct JournalResState {
    pub v: u64,
}

impl JournalResState {
    pub fn new() -> Self {
        JournalResState { v: 0 }
    }

    pub fn cur_entry_offset(&self) -> u32 {
        (self.v & 0x3fffff) as u32
    }

    pub fn set_cur_entry_offset(&mut self, v: u32) {
        self.v = (self.v & !0x3fffff) | (v as u64);
    }

    pub fn idx(&self) -> u32 {
        ((self.v >> 22) & 3) as u32
    }

    pub fn set_idx(&mut self, v: u32) {
        self.v = (self.v & !(3u64 << 22)) | ((v as u64) << 22);
    }

    pub fn buf_count(&self, idx: u32) -> u32 {
        ((self.v >> (JOURNAL_STATE_BUF0_SHIFT as u64 + idx as u64 * JOURNAL_STATE_BUF_COUNT_BITS as u64))
            & JOURNAL_STATE_BUF_COUNT_MAX as u64) as u32
    }

    pub fn set_buf_count(&mut self, idx: u32, count: u32) {
        let shift = JOURNAL_STATE_BUF0_SHIFT + idx * JOURNAL_STATE_BUF_COUNT_BITS;
        self.v = (self.v & !((JOURNAL_STATE_BUF_COUNT_MAX as u64) << shift))
            | ((count as u64) << shift);
    }
}

#[derive(Clone, Copy, Debug)]
pub enum JournalSpaceFrom {
    Discarded,
    CleanOndisk,
    Clean,
    Total,
    Nr,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct JournalSpace {
    pub next_entry: u32,
    pub total: u32,
}

#[derive(Clone, Copy, Debug)]
pub enum JournalPinType {
    Btree3,
    Btree2,
    Btree1,
    Btree0,
    KeyCache,
    Other,
    Nr,
}

#[derive(Clone, Debug)]
pub struct JournalEntryPinList {
    pub count: u32,
    pub unflushed: [Vec<JournalEntryPin>; 6],
    pub flushed: Vec<JournalEntryPin>,
    pub unreplayed: bool,
    pub devs: Vec<u8>,
    pub bytes: u32,
}

impl JournalEntryPinList {
    pub fn new() -> Self {
        JournalEntryPinList {
            count: 0,
            unflushed: [Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new()],
            flushed: Vec::new(),
            unreplayed: false,
            devs: Vec::new(),
            bytes: 0,
        }
    }

    pub fn init(&mut self, count: u32) {
        for list in &mut self.unflushed {
            list.clear();
        }
        self.flushed.clear();
        self.count = count;
        self.unreplayed = false;
        self.devs.clear();
        self.bytes = 0;
    }
}

#[derive(Clone, Debug)]
pub struct JournalEntryPin {
    pub flush: Option<JournalPinFlushFn>,
    pub seq: u64,
    pub list_idx: isize,
}

pub type JournalPinFlushFn = fn(&mut Journal, &mut JournalEntryPin, u64) -> Result<(), BchError>;

#[derive(Clone, Copy, Debug)]
pub enum JournalCycleFlags {
    MustClose = 1,
    MustOpen = 2,
    ForceClose = 4,
}

#[derive(Clone, Copy, Debug)]
pub enum JournalFlags {
    Degraded,
    ReplayDone,
    Running,
    MaySkipFlush,
    NeedFlushWrite,
    MedOnSpace,
    LowOnSpace,
    LowOnPin,
    LowOnWb,
}

#[derive(Clone, Debug)]
pub struct JournalRewindRange {
    pub from: u64,
    pub to: u64,
}

pub fn jset_u64s(u64s: u32) -> u32 {
    u64s + (std::mem::size_of::<JsetEntry>() as u32) / 8
}

pub fn journal_entry_overhead(j: &Journal) -> u32 {
    (std::mem::size_of::<Jset>() as u32) / 8 + j.entry_u64s_reserved
}

pub fn journal_state_count(s: JournalResState, idx: u32) -> u32 {
    s.buf_count(idx)
}

pub fn journal_state_seq_count(j: &Journal, s: JournalResState, seq: u64) -> u32 {
    if journal_cur_seq(j) - seq < JOURNAL_STATE_BUF_NR as u64 {
        journal_state_count(s, seq as u32 & JOURNAL_STATE_BUF_MASK)
    } else {
        0
    }
}

pub fn journal_state_inc(s: &mut JournalResState) -> bool {
    let cnt = journal_state_count(*s, s.idx());
    if cnt == JOURNAL_STATE_BUF_COUNT_MAX {
        return false;
    }
    let shift = JOURNAL_STATE_BUF0_SHIFT + s.idx() * JOURNAL_STATE_BUF_COUNT_BITS;
    s.v += 1u64 << shift;
    true
}

pub fn journal_state_buf_put(j: &mut Journal, idx: u32) -> JournalResState {
    let shift = JOURNAL_STATE_BUF0_SHIFT as u64 + idx as u64 * JOURNAL_STATE_BUF_COUNT_BITS as u64;
    let old = atomic64_sub_return(1u64 << shift, &mut j.reservations.v);
    JournalResState { v: old }
}

pub fn journal_cur_seq(j: &Journal) -> u64 {
    atomic64_read(&j.seq)
}

pub fn journal_entry_is_open(j: &Journal) -> bool {
    j.reservations.cur_entry_offset() < JOURNAL_ENTRY_CLOSED_VAL
}

pub fn journal_entry_init(entry: &mut JsetEntry, type_: BchJsetEntryType, id: u8, level: u8, u64s: u16) -> u32 {
    entry.u64s = u64s;
    entry.btree_id = id;
    entry.level = level;
    entry.type_ = type_;
    jset_u64s(u64s as u32)
}

pub fn journal_entry_set(entry: &mut JsetEntry, type_: BchJsetEntryType, id: u8, level: u8, data: &[u64], u64s: u16) -> u32 {
    let ret = journal_entry_init(entry, type_, id, level, u64s);
    for (i, v) in data.iter().enumerate() {
        entry._data()[i] = *v;
    }
    ret
}

pub fn journal_buf_must_flush(buf: &JournalBuf) -> bool {
    buf.flush_picked || buf.flush
}

pub fn journal_buf_must_not_flush(buf: &JournalBuf) -> bool {
    buf.empty && !buf.flush
}

pub fn journal_entry_empty(j: &Jset) -> bool {
    if j.seq != j.last_seq {
        return false;
    }
    for entry in &j.entries {
        if entry.type_ == BchJsetEntryType::BtreeKeys && entry.u64s > 0 {
            return false;
        }
    }
    true
}

pub fn bch2_journal_error(j: &Journal) -> Result<(), BchError> {
    if j.reservations.cur_entry_offset() == JOURNAL_ENTRY_ERROR_VAL {
        Err(BchError::from_raw(-1))
    } else {
        Ok(())
    }
}

pub fn journal_res_get_fast(j: &mut Journal, res: &mut JournalRes, flags: u32) -> bool {
    let mut old_v = atomic64_read(&j.reservations.v);

    loop {
        let mut new = JournalResState { v: old_v };

        std::sync::atomic::fence(std::sync::atomic::Ordering::Acquire);

        if new.cur_entry_offset() + res.u64s as u32 > j.cur_entry_u64s {
            return false;
        }

        if journal_state_count(new, new.idx()) == 0 {
            return false;
        }

        if (flags & BCH_WATERMARK_MASK as u32) < j.watermark as u32 {
            return false;
        }

        new.set_cur_entry_offset(new.cur_entry_offset() + res.u64s as u32);

        if !journal_state_inc(&mut new) {
            return false;
        }

        if flags & (1 << __JOURNAL_RES_GET_CHECK) != 0 {
            return true;
        }

        match atomic64_compare_exchange(&mut j.reservations.v, old_v, new.v) {
            Ok(_) => {
                res.ref_ = true;
                res.offset = JournalResState { v: old_v }.cur_entry_offset();
                res.seq = journal_cur_seq(j);
                res.seq -= (res.seq - JournalResState { v: old_v }.idx() as u64) & (JOURNAL_STATE_BUF_MASK as u64);
                return true;
            }
            Err(v) => old_v = v,
        }
    }
}

pub fn __bch2_journal_buf_put(j: &mut Journal, seq: u64) {
    let idx = seq as u32 & JOURNAL_STATE_BUF_MASK;
    let s = journal_state_buf_put(j, idx);
    if journal_state_count(s, idx) == 0 {
        bch2_journal_buf_put_final(j, seq);
    }
}

pub fn bch2_journal_buf_put(j: &mut Journal, seq: u64) {
    let idx = seq as u32 & JOURNAL_STATE_BUF_MASK;
    let s = journal_state_buf_put(j, idx);
    if journal_state_count(s, idx) == 0 {
        bch2_journal_buf_put_final(j, seq);
    }
}

pub fn bch2_journal_res_put(j: &mut Journal, res: &mut JournalRes) {
    if !res.ref_ {
        return;
    }

    while res.u64s > 0 {
        bch2_journal_add_entry(j, res, BchJsetEntryType::BtreeKeys, 0, 0, 0);
    }

    bch2_journal_buf_put(j, res.seq);
    res.ref_ = false;
}

pub fn bch2_journal_add_entry(j: &Journal, res: &mut JournalRes, type_: BchJsetEntryType, btree_id: u8, level: u8, u64s: u16) {
    let entry = journal_res_entry(j, res);
    let actual = journal_entry_init(entry, type_, btree_id, level, u64s);
    res.offset += actual;
    res.u64s -= actual as u16;
}

pub fn journal_res_entry(j: &Journal, res: &JournalRes) -> &mut JsetEntry {
    let data = journal_res_data(j, res);
    let offset = res.offset as usize;
    unsafe {
        &mut *(data.as_ptr().add(offset) as *mut JsetEntry)
    }
}

pub fn journal_res_data(j: &Journal, res: &JournalRes) -> Vec<u64> {
    Vec::new()
}

pub fn journal_res_buf(j: &Journal, res: &JournalRes) -> &JournalBuf {
    &j.ring[res.seq as usize & JOURNAL_STATE_BUF_MASK as usize]
}

pub fn journal_cur_buf(j: &Journal) -> &JournalBuf {
    &j.ring[j.reservations.idx() as usize]
}

pub fn __journal_entry_close_one(j: &mut Journal, closed_val: u32) {
    let buf = journal_cur_buf(j);
    let old_v = atomic64_read(&j.reservations.v);
    let mut old = JournalResState { v: old_v };

    loop {
        let mut new = old;
        new.set_cur_entry_offset(closed_val);

        if old.cur_entry_offset() == JOURNAL_ENTRY_ERROR_VAL || old.cur_entry_offset() == new.cur_entry_offset() {
            return;
        }

        match atomic64_compare_exchange(&mut j.reservations.v, old.v, new.v) {
            Ok(_) => break,
            Err(v) => {
                old.v = v;
                continue;
            }
        }
    }

    if !(old.cur_entry_offset() < JOURNAL_ENTRY_CLOSED_VAL) {
        return;
    }

    let seq = journal_cur_seq(j);
    let mut entry_offset = old.cur_entry_offset();
    if entry_offset == JOURNAL_ENTRY_BLOCKED_VAL {
        entry_offset = j.cur_entry_offset_if_blocked;
    }

    buf.last_seq = j.last_seq;
    buf.data.last_seq = buf.last_seq;

    if closed_val != JOURNAL_ENTRY_ERROR_VAL {
        __bch2_journal_buf_put(j, seq);
    } else {
        let idx = seq as u32 & JOURNAL_STATE_BUF_MASK;
        let s = journal_state_buf_put(j, idx);
        if journal_state_count(s, idx) == 0 {
            bch2_journal_do_writes_locked(j);
        }
    }
}

pub fn __journal_entry_open_one(j: &mut Journal) -> Result<(), BchError> {
    if journal_entry_is_open(j) {
        return Ok(());
    }

    if j.blocked > 0 {
        return Err(BchError::from_raw(-1));
    }

    if j.cur_entry_error != 0 {
        return Err(BchError::from_raw(j.cur_entry_error));
    }

    try!(bch2_journal_error(j));

    if j.pin.len() >= j.pin_size {
        return Err(BchError::from_raw(-1));
    }

    if j.pin_size - j.pin.len() < 2 {
        return Err(BchError::from_raw(-1));
    }

    if journal_state_count(j.reservations, (journal_cur_seq(j) + 1) as u32 & JOURNAL_STATE_BUF_MASK) > 0 {
        return Err(BchError::from_raw(-1));
    }

    if journal_cur_seq(j) >= JOURNAL_SEQ_MAX {
        return Err(BchError::from_raw(-1));
    }

    if j.free_buf.is_none() {
        return Err(BchError::from_raw(-1));
    }

    let sectors = j.cur_entry_sectors.min(j.free_buf_size >> 9);
    let u64s = ((sectors as i32) << 9) / 8 - journal_entry_overhead(j) as i32;
    let u64s = u64s.max(0).min((JOURNAL_ENTRY_CLOSED_VAL - 1) as i32) as u32;

    let was_empty = j.pin.is_empty();

    let seq = atomic64_inc_return(&mut j.seq);

    let mut pin_list = JournalEntryPinList::new();
    pin_list.init(1);
    j.pin.push(pin_list);

    let mut buf = JournalBuf::new();
    let free_buf = j.free_buf.take().unwrap();
    std::mem::swap(&mut buf.data, &mut free_buf);
    std::mem::swap(&mut buf.buf_size, &mut j.free_buf_size);
    buf.u64s_reserved = j.entry_u64s_reserved;
    buf.disk_sectors = j.cur_entry_sectors;
    buf.sectors = sectors;
    buf.has_overwrites = j.journal_transaction_names;
    buf.need_flush_to_write_buffer = true;

    let mut data = Jset::default();
    data.seq = seq;
    data.u64s = 0;
    buf.data = data;

    let ring_idx = (seq & JOURNAL_STATE_BUF_MASK as u64) as usize;
    j.ring[ring_idx] = buf;
    j.ring_data[ring_idx] = data;

    if !j.early_journal_entries.is_empty() {
        let n = j.early_journal_entries.len();
        for (i, v) in j.early_journal_entries.drain(..).enumerate() {
            j.ring_data[ring_idx].entries.push(JsetEntry {
                u64s: 0,
                btree_id: 0,
                level: 0,
                type_: BchJsetEntryType::BtreeKeys,
            });
        }
    }

    j.cur_entry_u64s = u64s;

    let mut old = JournalResState { v: atomic64_read(&j.reservations.v) };
    loop {
        let mut new = old;
        new.set_idx(new.idx() + 1);
        journal_state_inc(&mut new);
        new.set_cur_entry_offset(j.ring_data[ring_idx].u64s);

        match atomic64_compare_exchange(&mut j.reservations.v, old.v, new.v) {
            Ok(_) => break,
            Err(v) => old.v = v,
        }
    }

    if was_empty && j.reclaim_thread.is_some() {
        // wake reclaim
    }

    Ok(())
}

pub fn bch2_journal_cycle_locked(j: &mut Journal, flags: u32) -> Result<(), BchError> {
    loop {
        let close = flags & (JournalCycleFlags::MustClose as u32) != 0
            || flags & (JournalCycleFlags::MustOpen as u32) != 0
            || (journal_entry_is_open(j)
                && journal_buf_must_flush(journal_cur_buf(j))
                && (flags & (JournalCycleFlags::ForceClose as u32) != 0
                    || j.in_flight <= 1));

        if close {
            __journal_entry_close_one(j, JOURNAL_ENTRY_CLOSED_VAL);
        }

        let should_open = !journal_entry_is_open(j)
            && (flags & (JournalCycleFlags::MustOpen as u32) != 0);

        if !should_open {
            return Ok(());
        }

        try!(__journal_entry_open_one(j));
    }
}

pub fn bch2_journal_cycle(j: &mut Journal, flags: u32) -> Result<(), BchError> {
    bch2_journal_cycle_locked(j, flags)
}

pub fn bch2_journal_halt_locked(j: &mut Journal) {
    __journal_entry_close_one(j, JOURNAL_ENTRY_ERROR_VAL);
    if j.err_seq == 0 {
        j.err_seq = journal_cur_seq(j);
    }
}

pub fn bch2_journal_halt(j: &mut Journal) {
    bch2_journal_halt_locked(j);
}

pub fn bch2_journal_quiesce(j: &mut Journal) {
    while atomic64_read(&j.seq) != j.seq_ondisk {
        bch2_journal_cycle_locked(j, JournalCycleFlags::MustClose as u32).ok();
        if atomic64_read(&j.seq) == j.seq_ondisk {
            break;
        }
    }
}

pub fn bch2_journal_write_work(j: &mut Journal) {
    bch2_journal_flush_async(j, None);
}

pub fn __bch2_journal_res_get(j: &mut Journal, res: &mut JournalRes, flags: u32) -> Result<(), BchError> {
    loop {
        if journal_res_get_fast(j, res, flags) {
            return Ok(());
        }

        try!(bch2_journal_error(j));

        if j.blocked > 0 {
            return Err(BchError::from_raw(-1));
        }

        if (flags & BCH_WATERMARK_MASK as u32) < j.watermark as u32 {
            return Err(BchError::from_raw(-1));
        }

        journal_buf_prealloc(j);

        if journal_res_get_fast(j, res, flags) {
            return Ok(());
        }

        let buf = journal_cur_buf(j);
        if journal_entry_is_open(j)
            && (buf.buf_size >> 9) < buf.disk_sectors
            && buf.buf_size < JOURNAL_ENTRY_SIZE_MAX
        {
            j.buf_size_want = j.buf_size_want.max(buf.buf_size << 1);
        }

        let ret = bch2_journal_cycle_locked(j, JournalCycleFlags::MustOpen as u32);
        if ret.is_ok() {
            return Err(BchError::from_raw(-1));
        }
    }
}

pub fn bch2_journal_res_get_slowpath(j: &mut Journal, res: &mut JournalRes, flags: u32) -> Result<(), BchError> {
    __bch2_journal_res_get(j, res, flags)
}

pub fn bch2_journal_entry_res_resize(j: &mut Journal, res: &mut JournalEntryRes, new_u64s: u32) {
    let d = new_u64s as i32 - res.u64s as i32;
    j.entry_u64s_reserved = (j.entry_u64s_reserved as i32 + d) as u32;
    res.u64s = new_u64s;

    if d <= 0 {
        return;
    }

    j.cur_entry_u64s = (j.cur_entry_u64s as i32 - d).max(0) as u32;
    let state = j.reservations;

    if state.cur_entry_offset() >= JOURNAL_ENTRY_CLOSED_VAL {
        return;
    }

    if state.cur_entry_offset() > j.cur_entry_u64s {
        j.cur_entry_u64s = (j.cur_entry_u64s as i32 + d) as u32;
        bch2_journal_cycle_locked(j, JournalCycleFlags::MustClose as u32 | JournalCycleFlags::ForceClose as u32).ok();
    } else {
        // journal_cur_buf(j).u64s_reserved += d
    }
}

pub fn __bch2_journal_flush_seq_async(j: &mut Journal, seq: u64, closure: Option<&mut dyn FnMut()>) {
    let mut current = seq;
    loop {
        if current > journal_cur_seq(j) {
            break;
        }

        let buf = &j.ring[(current & JOURNAL_STATE_BUF_MASK as u64) as usize];
        if buf.write_done {
            if current >= j.seq_ondisk {
                break;
            }
            current += 1;
            continue;
        }

        if buf.write_started {
            if let Some(ref mut cl) = closure {
                cl();
            }
            return;
        }

        current += 1;
    }
}

pub fn bch2_journal_flush_seq_async(j: &mut Journal, seq: u64, closure: Option<&mut dyn FnMut()>) -> Result<i32, BchError> {
    let flushed_seq_ondisk = j.flushed_seq_ondisk;
    let cur_seq = journal_cur_seq(j);

    if seq <= flushed_seq_ondisk {
        return Ok(1);
    }

    if seq > cur_seq {
        return Ok(0);
    }

    if j.err_seq != 0 && seq > j.flushed_seq_ondisk {
        return Err(BchError::from_raw(-1));
    }

    let seq = seq.max(j.seq_ondisk + 1);

    if closure.is_none() {
        return Ok(0);
    }

    __bch2_journal_flush_seq_async(j, seq, closure);

    if let Err(e) = bch2_journal_error(j) {
        return Err(e);
    }

    Ok(0)
}

pub fn bch2_journal_flush_async(j: &mut Journal, closure: Option<&mut dyn FnMut()>) {
    bch2_journal_flush_seq_async(j, atomic64_read(&j.seq), closure).ok();
}

pub fn bch2_journal_flush(j: &mut Journal) -> Result<(), BchError> {
    let seq = atomic64_read(&j.seq);
    bch2_journal_flush_seq_async(j, seq, None).ok();
    Ok(())
}

pub fn bch2_journal_noflush_seq(j: &mut Journal, start: u64, end: u64) -> bool {
    if start > j.flushed_seq_ondisk {
        return false;
    }
    true
}

pub fn bch2_journal_advance_rewind_seq(j: &mut Journal, seq: u64) {
    j.rewind_seq = j.rewind_seq.max(seq);
}

pub fn __bch2_journal_meta(j: &mut Journal) -> Result<(), BchError> {
    let mut res = JournalRes::default();
    try!(bch2_journal_res_get_slowpath(j, &mut res, 0));
    bch2_journal_res_put(j, &mut res);
    Ok(())
}

pub fn bch2_journal_meta(j: &mut Journal) -> Result<(), BchError> {
    __bch2_journal_meta(j)
}

pub fn bch2_journal_unblock(j: &mut Journal) {
    j.blocked = j.blocked.saturating_sub(1);
    if j.blocked == 0
        && j.cur_entry_offset_if_blocked < JOURNAL_ENTRY_CLOSED_VAL
        && j.reservations.cur_entry_offset() == JOURNAL_ENTRY_BLOCKED_VAL
    {
        let mut old = JournalResState { v: atomic64_read(&j.reservations.v) };
        loop {
            let mut new = old;
            new.set_cur_entry_offset(j.cur_entry_offset_if_blocked);
            match atomic64_compare_exchange(&mut j.reservations.v, old.v, new.v) {
                Ok(_) => break,
                Err(v) => old.v = v,
            }
        }
    }
}

fn __bch2_journal_block(j: &mut Journal) {
    if j.blocked == 0 {
        let mut old = JournalResState { v: atomic64_read(&j.reservations.v) };
        loop {
            j.cur_entry_offset_if_blocked = old.cur_entry_offset();
            if j.cur_entry_offset_if_blocked >= JOURNAL_ENTRY_CLOSED_VAL {
                break;
            }
            let mut new = old;
            new.set_cur_entry_offset(JOURNAL_ENTRY_BLOCKED_VAL);
            match atomic64_compare_exchange(&mut j.reservations.v, old.v, new.v) {
                Ok(_) => break,
                Err(v) => old.v = v,
            }
        }
    }
    j.blocked += 1;
}

pub fn bch2_journal_block(j: &mut Journal) {
    __bch2_journal_block(j);
}

pub fn bch2_journal_do_writes_locked(j: &mut Journal) {
    if j.in_flight == 0 {
        return;
    }
}

pub fn bch2_journal_buf_put_final(j: &mut Journal, seq: u64) {
    bch2_journal_update_last_seq(j);
    bch2_journal_do_writes_locked(j);
}

pub fn journal_buf_prealloc(j: &mut Journal) {
    if j.free_buf.is_some() && j.free_buf_size >= j.buf_size_want {
        return;
    }
    let buf_size = j.buf_size_want;
    j.free_buf = Some(Jset::default());
    j.free_buf_size = buf_size;
}

pub fn bch2_journal_update_last_seq(j: &mut Journal) {
    let old = j.last_seq;
    while j.last_seq < j.pin.len() as u64
        && j.last_seq <= j.seq_ondisk
    {
        let idx = j.last_seq as usize;
        if idx >= j.pin.len() {
            break;
        }
        if j.pin[idx].count > 0 {
            break;
        }
        j.last_seq += 1;
    }
}

use crate::alloc::buckets::*;

fn atomic64_read(v: &u64) -> u64 {
    std::sync::atomic::AtomicU64::new(*v).load(std::sync::atomic::Ordering::Relaxed)
}

fn atomic64_inc_return(v: &mut u64) -> u64 {
    let atomic = std::sync::atomic::AtomicU64::new(*v);
    let r = atomic.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    *v = r + 1;
    r + 1
}

fn atomic64_sub_return(sub: u64, v: &mut u64) -> u64 {
    let atomic = std::sync::atomic::AtomicU64::new(*v);
    let r = atomic.fetch_sub(sub, std::sync::atomic::Ordering::SeqCst);
    *v = r - sub;
    r - sub
}

fn atomic64_compare_exchange(v: &mut u64, old: u64, new: u64) -> Result<u64, u64> {
    let atomic = std::sync::atomic::AtomicU64::new(*v);
    match atomic.compare_exchange(old, new, std::sync::atomic::Ordering::SeqCst, std::sync::atomic::Ordering::Relaxed) {
        Ok(_) => {
            *v = new;
            Ok(new)
        }
        Err(actual) => {
            *v = actual;
            Err(actual)
        }
    }
}

pub const __JOURNAL_RES_GET_NONBLOCK: u32 = BCH_WATERMARK_BITS;
pub const __JOURNAL_RES_GET_CHECK: u32 = BCH_WATERMARK_BITS + 1;
