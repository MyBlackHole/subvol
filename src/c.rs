#![allow(non_camel_case_types, non_snake_case, dead_code)]

pub use crate::bcachefs::*;
pub use crate::bcachefs_format::*;
pub use crate::opts::*;

pub use crate::errcode::*;

// workqueue
pub type workqueue_struct = core::ffi::c_void;

// bio
pub type bio = core::ffi::c_void;
pub type bvec_iter = core::ffi::c_void;

// printbuf - raw C struct
#[repr(C)]
pub struct printbuf {
    pub buf: *mut core::ffi::c_char,
    pub pos: usize,
    pub size: usize,
    pub heap: bool,
    pub atomic: u32,
    pub tab_stop: u16,
    pub indent: u16,
    pub nr_tabstops: u8,
}

// closure
pub type closure = core::ffi::c_void;

// Extent entry helpers
pub type bch_extent_entry = crate::bcachefs_format::BchExtentEntry;

// EC stripe new
pub type ec_stripe_new = core::ffi::c_void;
pub type ec_stripe_head = core::ffi::c_void;
pub type ec_bucket = core::ffi::c_void;

// write_point
pub type write_point = core::ffi::c_void;
pub type write_point_specifier = u64;

// Open bucket / disk reservation
pub type disk_reservation = core::ffi::c_void;

// Bkey validate context
pub type bkey_validate_context = core::ffi::c_void;
pub type journal_replay_list = core::ffi::c_void;

// bch_dev
pub type bch_dev = core::ffi::c_void;

// bch_read_bio
pub type bch_read_bio = core::ffi::c_void;
pub type bch_write_bio = core::ffi::c_void;
pub type bch_io_failures = core::ffi::c_void;
pub type bch_io_failure = core::ffi::c_void;
pub type bucket = core::ffi::c_void;

// Subvolume / snapshot internal
pub type bch_sb_member_iter = core::ffi::c_void;

// btree_node
pub type btree_node = core::ffi::c_void;

// nonce
pub type nonce = u64;

// key
pub type bch_key = core::ffi::c_void;

// bch_sb_handle
pub type bch_sb_handle = core::ffi::c_void;

// bch_fs
pub type bch_fs = core::ffi::c_void;

// btree_trans, btree_iter
pub type btree_trans = core::ffi::c_void;
pub type btree_iter = core::ffi::c_void;
pub type btree_id = u32;
pub type bpos = crate::bcachefs_format::Bpos;
pub type bkey_s_c = core::ffi::c_void;
pub type bkey_s = core::ffi::c_void;
pub type bkey_i = core::ffi::c_void;
pub type bkey_buf = core::ffi::c_void;
pub type bch_extent_ptr = core::ffi::c_void;
pub type bch_extent_crc_unpacked = core::ffi::c_void;
pub type bch_csum = core::ffi::c_void;
pub type bversion = core::ffi::c_void;
pub type bch_inode_opts = u64;
pub type bch_write_op = core::ffi::c_void;
pub type bch_alloc_v4 = core::ffi::c_void;
pub type bch_member = core::ffi::c_void;
pub type bch_root = core::ffi::c_void;
pub type subvol_inum = core::ffi::c_void;
pub type bkey_s_c_reflink_p = core::ffi::c_void;
