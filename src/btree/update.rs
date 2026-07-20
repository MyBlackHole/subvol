use crate::bcachefs::*;
use crate::bcachefs_format::*;
use crate::btree::bkey::*;
use crate::btree::bset::*;
use crate::btree::cache::*;
use crate::btree::locking::*;
use crate::btree::types::*;
use crate::errcode::*;

/// Btree commit flags
#[derive(Clone, Copy)]
pub struct BtreeIterUpdateTriggerFlags(u32);

impl BtreeIterUpdateTriggerFlags {
    pub const NOFILL: Self = Self(1 << 0);
    pub const TRIGGER_NORUN: Self = Self(1 << 1);
    pub const INSERT_NOMARK: Self = Self(1 << 2);
}

/// Btree node update type
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BtreeNodeUpdateType {
    BtreeDelete = -1,
    BtreeNoop = 0,
    BtreeInsert = 1,
}

/// Btree path level update flags
#[derive(Clone, Copy)]
pub struct BtreeNodeIterUpdateFlags(u32);

impl BtreeNodeIterUpdateFlags {
    pub const NEW_NODE: Self = Self(1 << 0);
    pub const OLD_NODE: Self = Self(1 << 1);
}

/// Insert key into a btree node (interior or leaf)
pub fn bch2_btree_node_insert(
    trans: &mut BtreeTrans,
    path: &mut BtreePath,
    b: &mut BtreeNode,
    k: &BkeyI,
    key_u64s: usize,
) -> Result<BtreeNodeUpdateType, BchError> {
    let data = b.data.as_ref().unwrap();
    let update_type = if btree_node_is_interior(b) {
        BtreeNodeUpdateType::BtreeInsert
    } else {
        BtreeNodeUpdateType::BtreeInsert
    };

    // Find insertion point
    let mut iter = BtreeNodeIter::new();
    bch2_btree_node_iter_init(&mut iter, b, &k.k.p);

    // Check for overwrite
    let (overwrite, insert_pos) = if let Some(existing) = bch2_btree_node_iter_peek(&iter, b) {
        if bkey_cmp_left_packed(b, existing, &k.k.p) == 0 {
            (true, existing)
        } else {
            (false, bch2_btree_node_iter_prev_all(&iter, b))
        }
    } else {
        let last = bch2_btree_node_iter_prev_all(&iter, b);
        (false, last)
    };

    if overwrite {
        // Whiteout + insert
        if let Some(existing) = insert_pos {
            bch2_btree_node_iter_key_set(&mut iter, b, existing);
            bch2_cut_front(&k.k.p, existing);
            // Mark as whiteout
        }
    }

    // Check if node has space
    let free = btree_node_free_u64s(b);
    if key_u64s > free {
        return Ok(BtreeNodeUpdateType::BtreeNoop);
    }

    // Insert into bset
    let bset_idx = b.nsets as usize - 1;
    let i = bset(b, bset_idx);
    let start = i.start();

    if let Some(pos) = insert_pos {
        // Insert before pos
        let pos_offset = (pos.as_ptr() as usize - start.as_ptr() as usize) / 8;
        let remaining = i.u64s() as usize - pos_offset;
        // shift right
        unsafe {
            let dst = start.as_ptr().add(pos_offset + key_u64s) as *mut u64;
            let src = start.as_ptr().add(pos_offset) as *const u64;
            core::ptr::copy(src, dst, remaining);
        }
        // Copy key
        unsafe {
            core::ptr::copy_nonoverlapping(
                k.as_ptr(),
                start.as_ptr().add(pos_offset) as *mut u64,
                key_u64s,
            );
        }
    } else {
        // Append at end
        unsafe {
            core::ptr::copy_nonoverlapping(
                k.as_ptr(),
                vstruct_end(i).as_ptr() as *mut u64,
                key_u64s,
            );
        }
    }

    i.set_u64s(i.u64s() + key_u64s as u16);
    b.nr.live_u64s += key_u64s as u16;
    b.nr.bset_u64s[bset_idx / 2] += key_u64s as u16;

    update_type
}

/// Delete key from a btree node
pub fn bch2_btree_node_delete(
    trans: &mut BtreeTrans,
    path: &mut BtreePath,
    b: &mut BtreeNode,
    k: &BkeyI,
) -> Result<(), BchError> {
    let mut iter = BtreeNodeIter::new();
    bch2_btree_node_iter_init(&mut iter, b, &k.k.p);

    if let Some(existing) = bch2_btree_node_iter_peek(&iter, b) {
        if bkey_cmp_left_packed(b, existing, &k.k.p) == 0 {
            // Mark as whiteout
            bch2_btree_node_iter_key_set(&mut iter, b, existing);
            let bset_idx = (existing.offset_in_bset() / 2) as usize;
            b.nr.bset_u64s[bset_idx / 2] -= 1;
            b.whiteout_u64s += 1;
        }
    }

    Ok(())
}

/// Trim key to fit within node boundaries
pub fn bch2_cut_front(cut: &Bpos, k: &mut BkeyPacked) {
    // Implementation: adjust key prefix
}

/// Calculate free space in btree node
pub fn btree_node_free_u64s(b: &BtreeNode) -> usize {
    let data = b.data.as_ref().unwrap();
    let used = data.keys.u64s() as usize;
    let cap = (data.keys.len() * 8) / 8; // u64 count
    cap - used
}

/// Check if btree node is interior (non-leaf)
pub fn btree_node_is_interior(b: &BtreeNode) -> bool {
    b.c.level > 0
}

/// Get btree node from path
pub fn btree_path_node<'a>(path: &'a BtreePath, level: usize) -> Option<&'a BtreeNode> {
    if level < BTREE_MAX_DEPTH && !path.l[level].b.is_null() {
        Some(unsafe { &*path.l[level].b.as_ptr() })
    } else {
        None
    }
}

pub fn btree_path_node_mut<'a>(path: &'a mut BtreePath, level: usize) -> Option<&'a mut BtreeNode> {
    if level < BTREE_MAX_DEPTH && !path.l[level].b.is_null() {
        Some(unsafe { &mut *path.l[level].b.as_ptr() })
    } else {
        None
    }
}

/// Compute bkey_cmp for packed keys vs left
pub fn bkey_cmp_left_packed(b: &BtreeNode, k: &BkeyPacked, pos: &Bpos) -> i32 {
    let unpacked = bkey_unpack(b, k);
    bkey_cmp(&unpacked.p, pos)
}

/// Btree node count keys
pub fn bch2_btree_node_count_keys(b: &BtreeNode) -> BtreeNrKeys {
    let mut total_live = 0u16;
    let mut total_bset = [0u16; 8];
    let mut i = 0;
    for (idx, set) in b.set.iter().enumerate() {
        if set.data_offset == 0 {
            continue;
        }
        let bset_ref = bset(b, idx);
        let mut k = bset_ref.start();
        let mut live = 0u16;
        while k < vstruct_last(bset_ref) {
            if !bkey_packed_whiteout(k) {
                live += k.u64s();
            }
            k = bkey_p_next(k);
        }
        total_live += live;
        total_bset[i] = live;
        i += 1;
    }
    BtreeNrKeys {
        live_u64s: total_live,
        bset_u64s: total_bset,
        pad: 0,
    }
}

/// Unpack a key
fn bkey_unpack(b: &BtreeNode, k: &BkeyPacked) -> Bkey {
    let mut unpacked = Bkey::default();
    bch2_bkey_unpack(b, &mut unpacked, k);
    unpacked
}
