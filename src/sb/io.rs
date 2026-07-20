use crate::c;
use crate::errcode::BchResult;

pub fn bch2_sb_read(c: &c::bch_fs, dev: u32) -> *mut c::bch_sb_handle {
    unsafe {
        c::bch2_sb_read(c as *const _ as *mut _, dev)
    }
}

pub fn bch2_sb_read_only(c: &c::bch_fs, sb: *mut c::bch_sb_handle) -> BchResult<i32> {
    crate::errcode::ret_to_result_void(unsafe {
        c::bch2_sb_read_only(c as *const _ as *mut _, sb)
    })
}

pub fn bch2_sb_write(c: &c::bch_fs, sb: *mut c::bch_sb_handle) -> BchResult<i32> {
    crate::errcode::ret_to_result_void(unsafe {
        c::bch2_sb_write(c as *const _ as *mut _, sb)
    })
}

pub fn bch2_sb_realloc(sb: *mut c::bch_sb_handle, new_len: usize) -> BchResult<i32> {
    crate::errcode::ret_to_result_void(unsafe {
        c::bch2_sb_realloc(sb, new_len)
    })
}

pub fn bch2_sb_field_resize(sb: *mut c::bch_sb_handle, field_type: u32, new_len: usize) -> *mut core::ffi::c_void {
    unsafe { c::bch2_sb_field_resize(sb, field_type, new_len) }
}

pub fn bch2_sb_field_get(sb: &c::bch_sb_handle, field_type: u32) -> *mut core::ffi::c_void {
    unsafe { c::bch2_sb_field_get(sb as *const _ as *mut _, field_type) }
}

pub fn bch2_sb_set_read_only(sb: &mut c::bch_sb_handle) {
    unsafe { c::bch2_sb_set_read_only(sb as *mut _) }
}

pub fn bch2_sb_set_read_write(sb: &mut c::bch_sb_handle) {
    unsafe { c::bch2_sb_set_read_write(sb as *mut _) }
}

pub fn bch2_sb_validate(c: &c::bch_fs, sb: &c::bch_sb_handle) -> BchResult<i32> {
    crate::errcode::ret_to_result_void(unsafe {
        c::bch2_sb_validate(c as *const _ as *mut _, sb as *const _ as *mut _)
    })
}

pub fn bch2_sb_clean_write(c: &c::bch_fs, sb: &mut c::bch_sb_handle) -> BchResult<i32> {
    crate::errcode::ret_to_result_void(unsafe {
        c::bch2_sb_clean_write(c as *const _ as *mut _, sb as *mut _)
    })
}

pub fn bch2_sb_clean_read(c: &c::bch_fs, sb: &mut c::bch_sb_handle) -> BchResult<i32> {
    crate::errcode::ret_to_result_void(unsafe {
        c::bch2_sb_clean_read(c as *const _ as *mut _, sb as *mut _)
    })
}

pub fn bch2_fs_sb_init(c: &c::bch_fs) -> BchResult<i32> {
    crate::errcode::ret_to_result_void(unsafe {
        c::bch2_fs_sb_init(c as *const _ as *mut _)
    })
}

pub fn bch2_fs_sb_exit(c: &c::bch_fs) {
    unsafe { c::bch2_fs_sb_exit(c as *const _ as *mut _) }
}
