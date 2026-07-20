use crate::c;
use crate::errcode::BchResult;

pub fn bch2_snapshot_create(
    trans: *mut c::btree_trans,
    inum: c::subvol_inum,
    snapshot: *mut u32,
) -> BchResult<i32> {
    crate::errcode::ret_to_result_void(unsafe {
        c::bch2_snapshot_create(trans, inum, snapshot)
    })
}

pub fn bch2_snapshot_delete(
    trans: *mut c::btree_trans,
    id: u32,
    delete_children: bool,
) -> BchResult<i32> {
    crate::errcode::ret_to_result_void(unsafe {
        c::bch2_snapshot_delete(trans, id, delete_children)
    })
}

pub fn bch2_snapshot_equiv(
    c: &c::bch_fs,
    id: u32,
) -> u32 {
    unsafe { c::bch2_snapshot_equiv(c as *const _ as *mut _, id) }
}

pub fn bch2_snapshot_parent(
    c: &c::bch_fs,
    id: u32,
) -> u32 {
    unsafe { c::bch2_snapshot_parent(c as *const _ as *mut _, id) }
}

pub fn bch2_snapshot_is_ancestor(
    c: &c::bch_fs,
    id: u32,
    ancestor: u32,
) -> bool {
    unsafe { c::bch2_snapshot_is_ancestor(c as *const _ as *mut _, id, ancestor) }
}

pub fn bch2_snapshot_is_ancestor_equiv(
    c: &c::bch_fs,
    id: u32,
    ancestor: u32,
) -> bool {
    unsafe { c::bch2_snapshot_is_ancestor_equiv(c as *const _ as *mut _, id, ancestor) }
}

pub fn bch2_snapshot_tree_create(
    trans: *mut c::btree_trans,
    subvol_id: u32,
    snapshot: *mut u32,
) -> BchResult<i32> {
    crate::errcode::ret_to_result_void(unsafe {
        c::bch2_snapshot_tree_create(trans, subvol_id, snapshot)
    })
}

pub fn bch2_subvolume_activate(
    trans: *mut c::btree_trans,
    subvol: u32,
) -> BchResult<i32> {
    crate::errcode::ret_to_result_void(unsafe {
        c::bch2_subvolume_activate(trans, subvol)
    })
}

pub fn bch2_subvolume_deactivate(
    trans: *mut c::btree_trans,
    subvol: u32,
) -> BchResult<i32> {
    crate::errcode::ret_to_result_void(unsafe {
        c::bch2_subvolume_deactivate(trans, subvol)
    })
}

pub fn bch2_subvolume_is_ro(c: &c::bch_fs, subvol: u32) -> bool {
    unsafe { c::bch2_subvolume_is_ro(c as *const _ as *mut _, subvol) }
}

pub fn bch2_snapshot_set_equiv(
    trans: *mut c::btree_trans,
    id: u32,
    equiv: u32,
) -> BchResult<i32> {
    crate::errcode::ret_to_result_void(unsafe {
        c::bch2_snapshot_set_equiv(trans, id, equiv)
    })
}

pub fn bch2_snapshot_tree_to_text(
    out: *mut c::printbuf,
    c: &c::bch_fs,
) {
    unsafe { c::bch2_snapshot_tree_to_text(out, c as *const _ as *mut _) }
}

pub fn bch2_snapshot_to_text(
    out: *mut c::printbuf,
    c: &c::bch_fs,
    id: u32,
) {
    unsafe { c::bch2_snapshot_to_text(out, c as *const _ as *mut _, id) }
}

pub fn bch2_snapshot_tree_id(
    c: &c::bch_fs,
    id: u32,
) -> u32 {
    unsafe { c::bch2_snapshot_tree_id(c as *const _ as *mut _, id) }
}

pub fn bch2_snapshot_tree_parent(
    c: &c::bch_fs,
    id: u32,
) -> u32 {
    unsafe { c::bch2_snapshot_tree_parent(c as *const _ as *mut _, id) }
}
