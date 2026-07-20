use crate::bcachefs::*;
use crate::bcachefs_format::*;
use crate::opts::Printbuf;
use crate::alloc::buckets::*;
use crate::btree::types::*;
use crate::errcode::*;

pub fn lru_pos_id(pos: Bpos) -> u64 {
    pos.inode >> LRU_TIME_BITS
}

pub fn lru_pos_time(pos: Bpos) -> u64 {
    pos.inode & !(!0u64 << LRU_TIME_BITS)
}

pub fn lru_pos(lru_id: u16, dev_bucket: u64, time: u64) -> Bpos {
    Bpos::spos(
        ((lru_id as u64) << LRU_TIME_BITS) | time,
        dev_bucket,
        0,
    )
}

pub fn lru_start(lru_id: u16) -> Bpos {
    lru_pos(lru_id, 0, 0)
}

pub fn lru_end(lru_id: u16) -> Bpos {
    lru_pos(lru_id, u64::MAX, LRU_TIME_MAX)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BchLruType {
    Read,
    Fragmentation,
    Stripes,
}

pub fn lru_type(k: ()) -> BchLruType {
    BchLruType::Read
}

pub fn bch2_lru_validate(
    _c: &BchFs,
    _k: (),
    _ctx: &(),
) -> BchResult<()> {
    Ok(())
}

pub fn bch2_lru_to_text(_buf: &mut Printbuf, _c: &BchFs, _k: ()) {
}

pub fn bch2_lru_pos_to_text(_buf: &mut Printbuf, _pos: Bpos) {
}

pub fn __bch2_lru_change(
    _trans: &mut BtreeTrans,
    _lru_id: u16,
    _dev_bucket: u64,
    _old_time: u64,
    _new_time: u64,
) -> BchResult<()> {
    Ok(())
}

pub fn bch2_lru_change(
    trans: &mut BtreeTrans,
    lru_id: u16,
    dev_bucket: u64,
    old_time: u64,
    new_time: u64,
) -> BchResult<()> {
    if old_time != new_time {
        __bch2_lru_change(trans, lru_id, dev_bucket, old_time, new_time)
    } else {
        Ok(())
    }
}

pub fn bch2_dev_remove_lrus(_c: &BchFs, _ca: &BchDev) -> BchResult<()> {
    Ok(())
}

pub fn bch2_lru_check_set(
    _trans: &mut BtreeTrans,
    _lru_id: u16,
    _dev_bucket: u64,
    _time: u64,
    _k: (),
    _flush: &mut (),
) -> BchResult<()> {
    Ok(())
}

pub fn bch2_check_lrus(_c: &BchFs) -> BchResult<()> {
    Ok(())
}
