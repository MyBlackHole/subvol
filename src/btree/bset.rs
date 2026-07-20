use crate::bcachefs_format::*;
use crate::bcachefs::*;
use crate::btree::types::*;

pub const BSET_CSUM_TYPE: u32 = 0;
pub const BSET_BIG_ENDIAN: u32 = 1;
pub const BSET_SEPARATE_WHITEOUTS: u32 = 2;
pub const BSET_OFFSET: u32 = 3;

pub fn bset_has_whiteouts(bset: &Bset) -> bool {
    (bset.flags >> BSET_SEPARATE_WHITEOUTS) & 1 != 0
}

pub fn bset_byte_offset(b: &BtreeNode, set: &Bset) -> u32 {
    let base = b.data.as_ptr() as usize;
    let set_addr = set as *const Bset as usize;
    (set_addr - base) as u32
}

pub fn bset_sector_offset(b: &BtreeNode, set: &Bset) -> u32 {
    bset_byte_offset(b, set) >> 9
}

pub fn bset_tree_offset(b: &BtreeNode, t: &BsetTree) -> u32 {
    t.data_offset as u32 * 8
}

pub fn btree_bkey_header(b: &BtreeNode, t: &BsetTree) -> *const BkeyPacked {
    let offset = bset_tree_offset(b, t);
    unsafe { (b.data.as_ptr().add(offset as usize)) as *const BkeyPacked }
}

pub fn btree_bset(b: &BtreeNode, t: &BsetTree) -> &Bset {
    let offset = bset_tree_offset(b, t);
    unsafe { &*(b.data.as_ptr().add(offset as usize) as *const Bset) }
}

pub fn btree_bset_first(b: &BtreeNode) -> &Bset {
    unsafe { &*(b.data.as_ptr() as *const Bset) }
}

pub fn bset_bkey_idx(bset: &Bset, idx: u32) -> *const BkeyPacked {
    unsafe { &bset.start as *const _ as *const BkeyPacked }
}

pub fn bset_bkey_last(bset: &Bset) -> *const BkeyPacked {
    let ptr = unsafe { (bset as *const Bset as *const u8).add(bset.u64s as usize * 8) };
    ptr as *const BkeyPacked
}

pub fn btree_nr_keys(b: &BtreeNode) -> u16 {
    b.nr.live_u64s
}

pub fn btree_keys(b: &BtreeNode) -> u16 {
    b.nr.packed_keys + b.nr.unpacked_keys
}

pub fn btree_node_type(b: &BtreeNode) -> u8 {
    b.c.btree_id
}

pub fn bch2_bkey_prev(b: &BtreeNode, t: &BsetTree, pos: *const BkeyPacked) -> *const BkeyPacked {
    std::ptr::null()
}

pub fn bch2_bset_search(b: &BtreeNode, t: &BsetTree, pos: &Bpos) -> u32 {
    0
}

pub fn bch2_bset_find(b: &BtreeNode, t: &BsetTree, pos: &Bpos) -> u32 {
    bch2_bset_search(b, t, pos)
}

pub fn bch2_bkey_to_bset(b: &BtreeNode, k: &BkeyPacked) -> *const Bset {
    std::ptr::null()
}

pub fn bch2_bset_nr_entries(b: &BtreeNode, t: &BsetTree) -> u32 {
    0
}

pub fn bch2_bset_tree_from_idx(b: &BtreeNode, idx: u32) -> &BsetTree {
    &b.set[idx as usize]
}

pub fn btree_bset_for_pos(b: &BtreeNode, pos: &Bpos) -> u32 {
    for i in (0..b.nsets as usize).rev() {
        if b.set[i].end_offset > 0 {
            return i as u32;
        }
    }
    0
}

pub fn bkey_from_bset(b: &BtreeNode, t: &BsetTree, k: &BkeyPacked) -> Bkey {
    Bkey::init()
}
