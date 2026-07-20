use crate::c;
use crate::errcode::BchResult;

pub fn bch2_sb_clean_read(
    c: &c::bch_fs,
    sb: &mut c::bch_sb_handle,
) -> BchResult<i32> {
    crate::errcode::ret_to_result_void(unsafe {
        c::bch2_sb_clean_read(c as *const _ as *mut _, sb as *mut _)
    })
}

pub fn bch2_sb_clean_write(
    c: &c::bch_fs,
    sb: &mut c::bch_sb_handle,
) -> BchResult<i32> {
    crate::errcode::ret_to_result_void(unsafe {
        c::bch2_sb_clean_write(c as *const _ as *mut _, sb as *mut _)
    })
}

pub fn bch2_sb_clean_read_btree_roots(
    sb: &c::bch_sb_handle,
    roots: &mut [c::bch_root; c::BTREE_ID_NR as usize],
) -> BchResult<i32> {
    crate::errcode::ret_to_result_void(unsafe {
        c::bch2_sb_clean_read_btree_roots(
            sb as *const _ as *mut _,
            roots.as_mut_ptr() as *mut _,
        )
    })
}

pub fn bch2_sb_clean_write_btree_roots(
    sb: &mut c::bch_sb_handle,
    roots: &[c::bch_root; c::BTREE_ID_NR as usize],
) -> BchResult<i32> {
    crate::errcode::ret_to_result_void(unsafe {
        c::bch2_sb_clean_write_btree_roots(
            sb as *mut _,
            roots.as_ptr() as *const _ as *mut _,
        )
    })
}

pub fn bch2_sb_clean_read_journal_seq(
    sb: &c::bch_sb_handle,
    journal_seq: &mut u64,
) -> BchResult<i32> {
    crate::errcode::ret_to_result_void(unsafe {
        c::bch2_sb_clean_read_journal_seq(
            sb as *const _ as *mut _,
            journal_seq as *mut _,
        )
    })
}

pub fn bch2_sb_clean_write_journal_seq(
    sb: &mut c::bch_sb_handle,
    journal_seq: u64,
) -> BchResult<i32> {
    crate::errcode::ret_to_result_void(unsafe {
        c::bch2_sb_clean_write_journal_seq(sb as *mut _, journal_seq)
    })
}

pub fn bch2_fs_sb_clean_init(c: &c::bch_fs) {
    unsafe { c::bch2_fs_sb_clean_init(c as *const _ as *mut _) }
}

pub fn bch2_fs_sb_clean_exit(c: &c::bch_fs) {
    unsafe { c::bch2_fs_sb_clean_exit(c as *const _ as *mut _) }
}
