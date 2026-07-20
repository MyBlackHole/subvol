use crate::bcachefs::*;
use crate::bcachefs_format::*;
use crate::btree::types::*;
use crate::errcode::BchResult;

pub const BCH_WRITE_SYNC: u16 = 512;
pub const BCH_WRITE_ONLY_SPECIFIED_DEVS: u16 = 32;
pub const BCH_WRITE_DATA_ENCODED: u16 = 4;
pub const BCH_WRITE_CHECK_ENOSPC: u16 = 256;

pub fn bch2_write_op_init(
    _op: &mut std::ffi::c_void,
    _c: &BchFs,
    _opts: u64,
) {
    todo!()
}

pub fn bch2_write(
    _wq: *mut std::ffi::c_void,
    _op: *mut std::ffi::c_void,
) {
    todo!()
}

pub fn bch2_write_extent(
    _trans: *mut std::ffi::c_void,
    _op: *mut std::ffi::c_void,
    _k: *mut std::ffi::c_void,
    _sectors: u32,
) -> BchResult<()> {
    todo!()
}

pub fn bch2_write_submit(
    _op: *mut std::ffi::c_void,
) -> BchResult<()> {
    todo!()
}

pub fn bch2_writepoint_ec_buf(
    _c: &BchFs,
    _wp: *mut std::ffi::c_void,
) -> *mut std::ffi::c_void {
    todo!()
}

pub fn bch2_submit_wbio_replicas(
    _wbio: *mut std::ffi::c_void,
    _c: &BchFs,
    _data_type: u8,
    _k: &std::ffi::c_void,
    _sync: bool,
    _devs: *mut *mut std::ffi::c_void,
) {
    todo!()
}

pub fn bch2_extent_update(
    _trans: *mut std::ffi::c_void,
    _inum: (u32, u64),
    _iter: *mut std::ffi::c_void,
    _k: *mut std::ffi::c_void,
    _k_buf_u64s: u32,
    _disk_res: *mut std::ffi::c_void,
    _new_i_size: u64,
    _i_sectors_delta_total: *mut i64,
    _check_enospc: bool,
    _change_cookie: u32,
    _flush: *mut std::ffi::c_void,
) -> BchResult<()> {
    todo!()
}

pub fn bch2_bio_free_pages_pool(_c: &BchFs, _bio: *mut std::ffi::c_void) {
    todo!()
}

pub fn bch2_bio_alloc_pages_pool(_c: &BchFs, _bio: *mut std::ffi::c_void, _bs: u32, _size: usize) {
    todo!()
}

pub fn bch2_fs_io_write_init(_c: &BchFs) -> BchResult<()> {
    todo!()
}

pub fn bch2_fs_io_write_exit(_c: &BchFs) {
    todo!()
}
