use crate::bcachefs::*;
use crate::errcode::BchResult;

pub fn bch2_ec_stripe_new_alloc(
    _c: &BchFs,
    _wp: *mut std::ffi::c_void,
) -> *mut std::ffi::c_void {
    todo!()
}

pub fn bch2_ec_stripe_head_recalc(_c: &BchFs) {
    todo!()
}

pub fn bch2_ec_stripe_head_get(
    _trans: *mut std::ffi::c_void,
    _wp: *mut std::ffi::c_void,
    _ec_idx: u32,
    _min_blocks: u32,
    _max_blocks: u32,
    _target: i64,
    _algo: u32,
    _redundancy: u32,
) -> *mut std::ffi::c_void {
    todo!()
}

pub fn bch2_ec_stripe_head_put(_h: *mut std::ffi::c_void) {
    todo!()
}

pub fn bch2_ec_bucket_alloc(
    _trans: *mut std::ffi::c_void,
    _ca: *mut BchDev,
    _ec_idx: u32,
    _blocks: u32,
) -> *mut std::ffi::c_void {
    todo!()
}

pub fn bch2_fs_ec_init(_c: &BchFs) -> BchResult<i32> {
    todo!()
}

pub fn bch2_fs_ec_exit(_c: &BchFs) {
    todo!()
}
