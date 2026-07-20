use crate::c;
use crate::errcode::BchResult;

pub fn bch2_btree_trans_to_text(
    out: *mut c::printbuf,
    trans: *mut c::btree_trans,
) {
    unsafe { c::bch2_btree_trans_to_text(out, trans) }
}

pub fn bch2_btree_iter_to_text(
    out: *mut c::printbuf,
    iter: *mut c::btree_iter,
) {
    unsafe { c::bch2_btree_iter_to_text(out, iter) }
}

pub fn bch2_fs_btree_debug_to_text(
    out: *mut c::printbuf,
    c: &c::bch_fs,
) {
    unsafe { c::bch2_fs_btree_debug_to_text(out, c as *const _ as *mut _) }
}

pub fn bch2_fs_btree_cache_to_text(
    out: *mut c::printbuf,
    c: &c::bch_fs,
) {
    unsafe { c::bch2_fs_btree_cache_to_text(out, c as *const _ as *mut _) }
}

pub fn bch2_fs_journal_to_text(
    out: *mut c::printbuf,
    c: &c::bch_fs,
) {
    unsafe { c::bch2_fs_journal_to_text(out, c as *const _ as *mut _) }
}

pub fn bch2_fs_io_to_text(
    out: *mut c::printbuf,
    c: &c::bch_fs,
) {
    unsafe { c::bch2_fs_io_to_text(out, c as *const _ as *mut _) }
}

pub fn bch2_fs_alloc_to_text(
    out: *mut c::printbuf,
    c: &c::bch_fs,
) {
    unsafe { c::bch2_fs_alloc_to_text(out, c as *const _ as *mut _) }
}

pub fn bch2_fs_stripes_to_text(
    out: *mut c::printbuf,
    c: &c::bch_fs,
) {
    unsafe { c::bch2_fs_stripes_to_text(out, c as *const _ as *mut _) }
}

pub fn bch2_fs_superblock_to_text(
    out: *mut c::printbuf,
    c: &c::bch_fs,
) {
    unsafe { c::bch2_fs_superblock_to_text(out, c as *const _ as *mut _) }
}

pub fn bch2_fs_superblock_compressed_to_text(
    out: *mut c::printbuf,
    c: &c::bch_fs,
) {
    unsafe { c::bch2_fs_superblock_compressed_to_text(out, c as *const _ as *mut _) }
}

pub fn bch2_fs_accounting_to_text(
    out: *mut c::printbuf,
    c: &c::bch_fs,
) {
    unsafe { c::bch2_fs_accounting_to_text(out, c as *const _ as *mut _) }
}

pub fn bch2_fs_replicas_to_text(
    out: *mut c::printbuf,
    c: &c::bch_fs,
) {
    unsafe { c::bch2_fs_replicas_to_text(out, c as *const _ as *mut _) }
}

pub fn bch2_fs_dev_cache_to_text(
    out: *mut c::printbuf,
    c: &c::bch_fs,
) {
    unsafe { c::bch2_fs_dev_cache_to_text(out, c as *const _ as *mut _) }
}

pub fn bch2_fs_debug_to_text(
    out: *mut c::printbuf,
    c: &c::bch_fs,
) {
    unsafe { c::bch2_fs_debug_to_text(out, c as *const _ as *mut _) }
}

pub fn bch2_fs_ec_debug_to_text(
    out: *mut c::printbuf,
    c: &c::bch_fs,
) {
    unsafe { c::bch2_fs_ec_debug_to_text(out, c as *const _ as *mut _) }
}

pub fn bch2_fs_compression_to_text(
    out: *mut c::printbuf,
    c: &c::bch_fs,
) {
    unsafe { c::bch2_fs_compression_to_text(out, c as *const _ as *mut _) }
}

pub fn bch2_btree_node_debug_to_text(
    out: *mut c::printbuf,
    b: *mut c::btree_node,
) {
    unsafe { c::bch2_btree_node_debug_to_text(out, b) }
}
