use crate::bcachefs::*;
use crate::errcode::BchResult;

pub fn bch2_ec_read_stripe(
    _trans: *mut std::ffi::c_void,
    _ec: *mut std::ffi::c_void,
) -> BchResult<i32> {
    todo!()
}

pub fn bch2_ec_stripe_submit(_ec: *mut std::ffi::c_void) {
    todo!()
}

pub fn bch2_ec_stripe_read_endio(_ec: *mut std::ffi::c_void, _err: i32) {
    todo!()
}

pub fn bch2_ec_stripe_write_endio(_ec: *mut std::ffi::c_void, _err: i32) {
    todo!()
}

pub fn ec_stripe_set_pending(_ec: *mut std::ffi::c_void) {
    todo!()
}

pub fn ec_stripe_clear_pending(_ec: *mut std::ffi::c_void) {
    todo!()
}

pub fn ec_stripe_pending(_ec: &std::ffi::c_void) -> bool {
    todo!()
}
