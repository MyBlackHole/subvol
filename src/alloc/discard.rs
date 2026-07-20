use crate::bcachefs::*;
use crate::bcachefs_format::*;
use crate::opts::Printbuf;
use crate::alloc::buckets::*;
use crate::btree::types::*;
use crate::errcode::*;

pub fn should_invalidate_buckets(ca: &BchDev, u: &BchDevUsage) -> u64 {
    let want_free = ca.nbuckets >> 5;
    let free = {
        let f = u.buckets[BchDataType::Free as usize] as i64
            - dev_buckets_reserved(ca, BchWatermark::Stripe) as i64;
        if f < 0 { 0 } else { f as u64 }
    };
    let val = (want_free as i64 - free as i64);
    let need = if val < 0 { 0 } else { val as u64 };
    std::cmp::min(need, u.buckets[BchDataType::Cached as usize])
}

pub fn bch2_fast_discard_bucket_del(_ca: &mut BchDev, _bucket: u64) {
}

pub fn bch2_fast_discard_bucket_add(_ca: &mut BchDev, _bucket: u64) {
}

pub fn bch2_fast_discards_to_text(_buf: &mut Printbuf, _ca: &BchDev) {
}

pub fn bch2_discards_to_text(_buf: &mut Printbuf, _c: &BchFs, _s: &DiscardState) {
}

pub fn bch2_dev_do_discards(_ca: &mut BchDev) {
}

pub fn bch2_do_discards_going_ro(_c: &mut BchFs) {
}

pub fn bch2_do_discards_async(_c: &mut BchFs) {
}

pub fn bch2_do_discards_work(_work: *mut std::ffi::c_void) {
}

pub fn bch2_do_discards_fast_work(_work: *mut std::ffi::c_void) {
}

pub fn bch2_do_invalidates_work(_work: *mut std::ffi::c_void) {
}

pub fn bch2_dev_do_invalidates(_ca: &mut BchDev) {
}

pub fn bch2_do_invalidates(_c: &mut BchFs) {
}

pub fn bch2_dev_discards_exit(_ca: &mut BchDev) {
}

pub fn bch2_dev_discards_init(_ca: &mut BchDev) -> BchResult<()> {
    Ok(())
}

pub fn bch2_fs_discards_exit(_c: &mut BchFs) {
}

pub fn bch2_fs_discards_init(_c: &mut BchFs) -> BchResult<()> {
    Ok(())
}

pub fn bch2_fs_discards_init_early(_c: &mut BchFs) {
}

#[derive(Clone, Debug)]
pub struct DiscardState {
    pub seen: u64,
    pub not_rw: u64,
    pub eexist: u64,
    pub eagain: u64,
    pub open: u64,
    pub need_journal_commit: u64,
    pub need_rewind_advance: u64,
    pub bad_data_type: u64,
    pub discarded: u64,
    pub committed: u64,
    pub pos: Bpos,
}
