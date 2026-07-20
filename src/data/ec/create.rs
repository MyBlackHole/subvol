use crate::bcachefs::*;
use crate::errcode::BchResult;

pub fn bch2_ec_bucket_written(
    _c: &BchFs,
    _bucket: u64,
    _dev: u32,
    _sectors: u32,
) -> BchResult<i32> {
    todo!()
}

pub fn bch2_ec_bucket_written_size(
    _c: &BchFs,
    _bucket: u64,
    _dev: u32,
    _sectors: u32,
    _size: u32,
) -> BchResult<i32> {
    todo!()
}

pub fn bch2_ec_do_stripe(
    _trans: *mut std::ffi::c_void,
    _ec: *mut std::ffi::c_void,
) -> BchResult<i32> {
    todo!()
}

pub fn bch2_ec_write_stripe(
    _trans: *mut std::ffi::c_void,
    _ec: *mut std::ffi::c_void,
) -> BchResult<i32> {
    todo!()
}

pub fn ec_stripe_key_init_stripe(_k: *mut std::ffi::c_void, _ec: &std::ffi::c_void) {
    todo!()
}

pub fn ec_stripe_key_init_data(_k: *mut std::ffi::c_void, _ec: &std::ffi::c_void) {
    todo!()
}
