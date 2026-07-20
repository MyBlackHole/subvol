use crate::c;
use crate::errcode::BchResult;

pub fn bch2_sb_member_get(
    sb: &c::bch_sb_handle,
    dev: u32,
) -> *const c::bch_member {
    unsafe { c::bch2_sb_member_get(sb as *const _ as *mut _, dev) }
}

pub fn bch2_sb_member_get_mut(
    sb: &mut c::bch_sb_handle,
    dev: u32,
) -> *mut c::bch_member {
    unsafe { c::bch2_sb_member_get_mut(sb as *mut _, dev) }
}

pub fn bch2_sb_member_set(
    sb: &mut c::bch_sb_handle,
    dev: u32,
    m: &c::bch_member,
) -> BchResult<i32> {
    crate::errcode::ret_to_result_void(unsafe {
        c::bch2_sb_member_set(sb as *mut _, dev, m as *const _ as *mut _)
    })
}

pub fn bch2_sb_member_iter(
    sb: &c::bch_sb_handle,
) -> c::bch_sb_member_iter {
    unsafe { c::bch2_sb_member_iter(sb as *const _ as *mut _) }
}

pub fn bch2_sb_member_iter_next(
    iter: &mut c::bch_sb_member_iter,
) -> *const c::bch_member {
    unsafe { c::bch2_sb_member_iter_next(iter as *mut _) }
}

pub fn bch2_sb_member_valid(
    sb: &c::bch_sb_handle,
    dev: u32,
) -> bool {
    unsafe { c::bch2_sb_member_valid(sb as *const _ as *mut _, dev) }
}

pub fn bch2_sb_nr_devices(sb: &c::bch_sb_handle) -> u32 {
    unsafe { c::bch2_sb_nr_devices(sb as *const _ as *mut _) }
}

pub fn bch2_sb_dev_have(sb: &c::bch_sb_handle, dev: u32) -> bool {
    unsafe { c::bch2_sb_dev_have(sb as *const _ as *mut _, dev) }
}

pub fn bch2_sb_dev_is_available(sb: &c::bch_sb_handle, dev: u32) -> bool {
    unsafe { c::bch2_sb_dev_is_available(sb as *const _ as *mut _, dev) }
}

pub fn bch2_sb_member_id_valid(sb: &c::bch_sb_handle, dev: u32) -> bool {
    unsafe { c::bch2_sb_member_id_valid(sb as *const _ as *mut _, dev) }
}
