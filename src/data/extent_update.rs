use crate::bcachefs::*;
use crate::btree::types::*;
use crate::errcode::BchResult;

pub fn bch2_extent_update(
    _trans: *mut BtreeTrans,
    _inum: (u32, u64),
    _iter: *mut BtreeIter,
    _k: *mut std::ffi::c_void,
    _k_buf_u64s: u32,
    _disk_res: *mut std::ffi::c_void,
    _new_i_size: u64,
    _i_sectors_delta_total: *mut i64,
    _check_enospc: bool,
    _change_cookie: u32,
    _flush: *mut std::ffi::c_void,
) -> BchResult<i32> {
    todo!()
}

pub fn bch2_extent_trim_atomic(
    _trans: *mut BtreeTrans,
    _iter: *mut BtreeIter,
    _k: *mut std::ffi::c_void,
) -> BchResult<i32> {
    todo!()
}
