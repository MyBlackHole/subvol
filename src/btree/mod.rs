pub mod types;
pub mod bkey;
pub mod bset;
pub mod iter;
pub mod cache;
pub mod locking;
pub mod read;
pub mod write;
pub mod update;
pub mod interior;
pub mod commit;
pub mod sort;
pub mod init;
pub mod node_scan;
pub mod key_cache;
pub mod write_buffer;
pub mod check;
pub mod journal_overlay;
pub mod bbpos;

pub use types::*;
pub use bkey::*;
pub use iter::BtreeIter;

use crate::bcachefs::*;
use crate::bcachefs_format::*;
use crate::btree::types::*;
use crate::btree::iter::*;
use crate::errcode::*;
use std::ptr;

pub const BKEY_NR_FIELDS: usize = 6;

pub const BTREE_NODE_DIRTY: u64 = 0x0001;
pub const BTREE_NODE_WRITE_IN_FLIGHT: u64 = 0x0002;
pub const BTREE_NODE_FORCE_WRITE: u64 = 0x0004;
pub const BTREE_NODE_NEEDS_REWRITE: u64 = 0x0008;
pub const BTREE_NODE_IN_GC: u64 = 0x0010;
pub const BTREE_NODE_NEEDS_GC: u64 = 0x0020;
pub const BTREE_NODE_WRITE_LOCK_HELD: u64 = 0x0040;
pub const BTREE_NODE_NEEDS_COMPACT: u64 = 0x0080;
pub const BTREE_NODE_PERMANENT: u64 = 0x0100;
pub const BTREE_NODE_READ_IN_FLIGHT: u64 = 0x0200;
pub const BTREE_NODE_IO_COMPLETE: u64 = 0x0400;
pub const BTREE_NODE_BIG_ENDIAN: u64 = 0x0800;
pub const BTREE_NODE_NEW_EXTENT_OVERWRITE: u64 = 0x1000;

pub const BTREE_TRIGGER_INSERT: u32 = 0;
pub const BTREE_TRIGGER_OVERWRITE: u32 = 1;

pub const BTREE_ITER_TYPE: u8 = 0;
pub const BTREE_ITER_CACHED: u8 = 1;

pub fn btree_node_dirty(b: &BtreeNode) -> bool {
    b.flags & BTREE_NODE_DIRTY != 0
}

pub fn btree_node_write_in_flight(b: &BtreeNode) -> bool {
    b.flags & BTREE_NODE_WRITE_IN_FLIGHT != 0
}

pub fn btree_node_read_in_flight(b: &BtreeNode) -> bool {
    b.flags & BTREE_NODE_READ_IN_FLIGHT != 0
}

pub fn btree_node_pinned(b: &BtreeNode) -> bool {
    b.flags & BTREE_NODE_PERMANENT != 0
}

pub fn btree_node_set_dirty(b: &mut BtreeNode) {
    b.flags |= BTREE_NODE_DIRTY;
}

pub fn btree_node_clear_dirty(b: &mut BtreeNode) {
    b.flags &= !BTREE_NODE_DIRTY;
}

pub fn btree_node_set_write_in_flight(b: &mut BtreeNode) {
    b.flags |= BTREE_NODE_WRITE_IN_FLIGHT;
}

pub fn btree_node_clear_write_in_flight(b: &mut BtreeNode) {
    b.flags &= !BTREE_NODE_WRITE_IN_FLIGHT;
}

pub fn btree_node_set_read_in_flight(b: &mut BtreeNode) {
    b.flags |= BTREE_NODE_READ_IN_FLIGHT;
}

pub fn btree_node_clear_read_in_flight(b: &mut BtreeNode) {
    b.flags &= !BTREE_NODE_READ_IN_FLIGHT;
}

pub fn bch2_btree_iter_peek(iter: &mut BtreeIter) -> BchResult<bool> {
    iter_next(iter)
}

pub fn bch2_btree_iter_next(iter: &mut BtreeIter) -> BchResult<bool> {
    iter_next(iter)
}

pub fn bch2_btree_iter_peek_prev(iter: &mut BtreeIter) -> BchResult<bool> {
    iter_prev(iter)
}

pub fn bch2_btree_iter_prev(iter: &mut BtreeIter) -> BchResult<bool> {
    iter_prev(iter)
}

pub fn bch2_btree_iter_peek_slot(iter: &mut BtreeIter) -> BchResult<bool> {
    iter_peek_slot(iter)
}

pub fn bch2_trans_get_iter(trans: &mut BtreeTrans, id: u8, pos: &Bpos) -> BtreeIter {
    BtreeIter::new_with(trans, id, pos)
}

pub fn bch2_trans_copy_iter(dst: &mut BtreeIter, src: &BtreeIter) {
    dst.copy_from(src);
}

pub fn bch2_trans_iter_init(trans: &mut BtreeTrans, id: u8, pos: &Bpos) -> BtreeIter {
    BtreeIter::new_with(trans, id, pos)
}

pub fn bch2_trans_iter_put(iter: &mut BtreeIter) {}

pub fn bch2_btree_iter_set_pos(iter: &mut BtreeIter, pos: &Bpos) {
    iter.set_pos(pos);
}

pub fn bch2_btree_iter_rewind(iter: &mut BtreeIter) {
    iter.set_pos(&Bpos::MIN);
}

pub fn bch2_btree_iter_advance_pos(iter: &mut BtreeIter) {}

pub fn bch2_btree_iter_key_uptodate(iter: &BtreeIter) -> bool {
    iter.uptodate
}

/* Key packing / unpacking utility functions */

pub fn bkey_p_next(k: &BkeyPacked) -> *const BkeyPacked {
    unsafe {
        (k as *const BkeyPacked as *const u8)
            .add(k.u64s as usize * 8) as *const BkeyPacked
    }
}

pub fn bkey_p_next_mut(k: &mut BkeyPacked) -> *mut BkeyPacked {
    unsafe {
        (k as *mut BkeyPacked as *mut u8)
            .add(k.u64s as usize * 8) as *mut BkeyPacked
    }
}

pub fn vstruct_last(s: &Bset) -> *const BkeyPacked {
    unsafe {
        let base = s as *const Bset as *const u8;
        base.add(std::mem::size_of::<Bset>() + s.u64s as usize * 8) as *const BkeyPacked
    }
}

pub fn vstruct_last_mut(s: &mut Bset) -> *mut BkeyPacked {
    unsafe {
        let base = s as *mut Bset as *mut u8;
        base.add(std::mem::size_of::<Bset>() + s.u64s as usize * 8) as *mut BkeyPacked
    }
}

pub fn bkey_next(k: &BkeyI) -> *const BkeyI {
    unsafe {
        (k as *const BkeyI as *const u8)
            .add(k.k.u64s as usize * 8) as *const BkeyI
    }
}

pub fn bkey_next_mut(k: &mut BkeyI) -> *mut BkeyI {
    unsafe {
        (k as *mut BkeyI as *mut u8)
            .add(k.k.u64s as usize * 8) as *mut BkeyI
    }
}

pub fn bset<'a>(b: &'a BtreeNode, t: &BsetTree) -> &'a Bset {
    unsafe {
        let ptr = b.data.as_ptr() as *const u8;
        &*(ptr.add(t.data_offset as usize * 8) as *const Bset)
    }
}

pub fn bset_mut<'a>(b: &'a mut BtreeNode, t: &BsetTree) -> &'a mut Bset {
    unsafe {
        let ptr = b.data.as_mut_ptr() as *mut u8;
        &mut *(ptr.add(t.data_offset as usize * 8) as *mut Bset)
    }
}

pub fn btree_node_offset_to_ptr(b: &BtreeNode, offset: u16) -> *const u8 {
    unsafe {
        (b.data.as_ptr() as *const u8).add(offset as usize * 8)
    }
}

pub fn btree_node_offset_to_ptr_mut(b: &mut BtreeNode, offset: u16) -> *mut u8 {
    unsafe {
        (b.data.as_mut_ptr() as *mut u8).add(offset as usize * 8)
    }
}

pub fn btree_bset(b: &BtreeNode, t: &BsetTree) -> &Bset {
    bset(b, t)
}

pub fn btree_bset_mut(b: &mut BtreeNode, t: &BsetTree) -> &mut Bset {
    bset_mut(b, t)
}

pub fn bch2_bkey_invalid(_k: &Bkey, _type: u32) -> bool {
    false
}

pub unsafe fn memcpy_u64s_small(dst: *mut u64, src: *const u64, nr: usize) {
    for i in 0..nr {
        *dst.add(i) = *src.add(i);
    }
}
