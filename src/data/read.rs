use crate::bcachefs::*;
use crate::bcachefs_format::*;
use crate::btree::types::*;
use crate::errcode::BchResult;
use crate::opts::Printbuf;

pub const BCH_READ_RETRY_IF_STALE: u16 = 1;
pub const BCH_READ_MAY_PROMOTE: u16 = 2;
pub const BCH_READ_USER_MAPPED: u16 = 4;
pub const BCH_READ_IN_RETRY: u16 = 256;

pub fn bch2_read(
    _trans: *mut std::ffi::c_void,
    _rbio: *mut std::ffi::c_void,
    _bvec_iter: *mut std::ffi::c_void,
    _inum: (u32, u64),
    _failed: *mut std::ffi::c_void,
    _prev_read: *mut std::ffi::c_void,
    _flags: u16,
) -> BchResult<i64> {
    todo!()
}

pub fn bch2_read_extent(
    _trans: *mut std::ffi::c_void,
    _rbio: *mut std::ffi::c_void,
    _read_pos: Bpos,
    _data_btree: u8,
    _k: (),
    _offset_into_extent: u32,
    _flags: u16,
) {
    todo!()
}

pub fn bch2_read_indirect_extent(
    _trans: *mut std::ffi::c_void,
    _data_btree: &mut u8,
    _offset_into_extent: &mut i64,
    _extent: *mut std::ffi::c_void,
) -> BchResult<()> {
    todo!()
}

pub fn bch2_dev_congested_to_text(_out: &mut Printbuf, _ca: *mut std::ffi::c_void) {
    todo!()
}

pub fn bch2_read_bio_to_text(_out: &mut Printbuf, _c: &BchFs, _rbio: &std::ffi::c_void) {
    todo!()
}

pub fn bch2_fs_io_read_init(_c: &BchFs) -> BchResult<()> {
    todo!()
}

pub fn bch2_fs_io_read_exit(_c: &BchFs) {
    todo!()
}
