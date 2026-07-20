use crate::bcachefs::*;
use crate::bcachefs_format::*;
use crate::btree::types::*;
use crate::errcode::BchResult;
use crate::opts::Printbuf;

pub fn bch2_remap_range(
    _c: &BchFs,
    _dst_inum: (u32, u64),
    _dst_offset: u64,
    _src_inum: (u32, u64),
    _src_offset: u64,
    _sectors: u64,
    _new_i_size: u64,
    _i_sectors_delta: *mut i64,
    _copy_range: bool,
) -> i64 {
    todo!()
}

pub fn bch2_reflink_p_validate(
    _c: &BchFs,
    _k: (),
    _from: *const std::ffi::c_void,
) -> BchResult<i32> {
    todo!()
}

pub fn bch2_reflink_p_to_text(
    _out: &mut Printbuf,
    _c: &BchFs,
    _k: (),
) {
    todo!()
}

pub fn bch2_reflink_v_validate(
    _c: &BchFs,
    _k: (),
    _from: *const std::ffi::c_void,
) -> BchResult<i32> {
    todo!()
}

pub fn bch2_reflink_v_to_text(
    _out: &mut Printbuf,
    _c: &BchFs,
    _k: (),
) {
    todo!()
}

pub fn bch2_make_extent_indirect(
    _trans: *mut BtreeTrans,
    _iter: *mut BtreeIter,
    _k: *mut std::ffi::c_void,
    _allow_copy_range: bool,
) -> BchResult<i32> {
    todo!()
}

pub fn bch2_lookup_indirect_extent(
    _trans: *mut BtreeTrans,
    _iter: *mut BtreeIter,
    _offset_into_extent: *mut i64,
    _p: (),
    _check: bool,
    _flags: u32,
) {
    todo!()
}
