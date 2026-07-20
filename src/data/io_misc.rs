use crate::bcachefs::*;
use crate::bcachefs_format::*;
use crate::btree::types::*;
use crate::errcode::BchResult;

pub fn bch2_extent_fallocate(
    _trans: *mut BtreeTrans,
    _inum: (u32, u64),
    _iter: *mut BtreeIter,
    _sectors: u64,
    _opts: u64,
    _i_sectors_delta: *mut i64,
    _write_point: u64,
) -> BchResult<i32> {
    todo!()
}

pub fn bch2_fpunch(
    _c: &BchFs,
    _inum: (u32, u64),
    _start: u64,
    _end: u64,
    _i_sectors_delta: *mut i64,
) -> BchResult<i32> {
    todo!()
}

pub fn bch2_truncate(
    _c: &BchFs,
    _inum: (u32, u64),
    _new_size: u64,
    _new_i_size: *mut u64,
) -> BchResult<i32> {
    todo!()
}

pub fn bch2_fcollapse_finsert(
    _c: &BchFs,
    _inum: (u32, u64),
    _start: u64,
    _end: u64,
    _insert: bool,
    _i_sectors_delta: *mut i64,
) -> BchResult<i32> {
    todo!()
}

pub fn bch2_fpunch_at(
    _trans: *mut BtreeTrans,
    _iter: *mut BtreeIter,
    _inum: (u32, u64),
    _sectors: u64,
    _i_sectors_delta: *mut i64,
) -> BchResult<i32> {
    todo!()
}
