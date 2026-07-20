use crate::bcachefs::*;
use crate::bcachefs_format::*;
use crate::btree::bkey::*;
use crate::btree::bset::*;
use crate::btree::cache::*;
use crate::btree::locking::*;
use crate::btree::sort::*;
use crate::btree::types::*;
use crate::btree::update::*;
use crate::errcode::*;

/// Btree update for interior node operations
pub struct BtreeUpdate {
    pub btree_id: u8,
    pub level: u8,
    pub k: BkeyI,
    pub old_k: BkeyI,
}

/// Start a btree update (for split/merge)
pub fn bch2_btree_update_start(
    trans: &mut BtreeTrans,
    path: &mut BtreePath,
    b: &mut BtreeNode,
) -> Result<BtreeUpdate, BchError> {
    Ok(BtreeUpdate {
        btree_id: b.btree_id,
        level: b.c.level,
        k: b.key.clone(),
        old_k: BkeyI::default(),
    })
}

/// Insert keys into parent after a split
pub fn bch2_btree_insert_node(
    trans: &mut BtreeTrans,
    path: &mut BtreePath,
    new_nodes: &[&mut BtreeNode],
    keys: &[BkeyI],
) -> Result<(), BchError> {
    let parent_level = new_nodes[0].c.level as usize + 1;

    // Find parent
    let parent_path = &mut trans.paths[0];
    let parent = if let Some(p) = btree_path_node_mut(parent_path, parent_level) {
        p
    } else {
        return Err(BchError::EINVAL);
    };

    // Insert keys into parent node
    for (node, k) in new_nodes.iter().zip(keys.iter()) {
        // Set the mem_ptr
        if k.k.type_val == BTREE_PTR_V2_TYPE {
            // Update mem ptr
            let bp = bkey_i_to_btree_ptr_v2_mut(k);
            bp.v.mem_ptr = *node as *mut _ as u64;
        }

        bch2_btree_node_insert(trans, parent_path, parent, k, k.k.u64s as usize)?;
    }

    Ok(())
}

/// Btree node split
pub fn bch2_btree_node_split(
    trans: &mut BtreeTrans,
    path: &mut BtreePath,
    b: &mut BtreeNode,
) -> Result<(), BchError> {
    let min_keys = 8u16; // Minimum keys for a split
    let nr_keys = b.nr.live_u64s;

    if nr_keys < min_keys * 2 {
        return Ok(()); // Not enough keys
    }

    // Allocate new node
    let n = bch2_btree_node_mem_alloc(trans, b.c.level != 0)?;

    // Copy key
    n.key = b.key.clone();
    n.btree_id = b.btree_id;
    n.c.level = b.c.level;

    // Find split point (midpoint)
    let mid_key = bch2_btree_node_split_midpoint(b);

    // Move keys to new node
    bch2_btree_node_split_move_keys(b, n, &mid_key);

    // Update keys
    let mut left_key = BkeyI::default();
    let mut right_key = BkeyI::default();
    bch2_btree_node_split_update_keys(b, n, &mut left_key, &mut right_key);

    // Insert into parent
    bch2_btree_insert_node(trans, path, &[b, n], &[left_key, right_key])?;

    Ok(())
}

/// Find midpoint for split
pub fn bch2_btree_node_split_midpoint(b: &BtreeNode) -> Bpos {
    // Simple midpoint based on key count
    let total = b.nr.live_u64s as usize;
    let mid = total / 2;
    let mut count = 0usize;

    for idx in 0..b.nsets as usize {
        let i = bset(b, idx);
        let mut k = i.start();
        while k < vstruct_last(i) {
            count += k.u64s() as usize;
            if count >= mid {
                return bkey_unpack(b, k).p;
            }
            k = bkey_p_next(k);
        }
    }

    Bpos::default()
}

/// Move keys across nodes during split
pub fn bch2_btree_node_split_move_keys(b: &mut BtreeNode, n: &mut BtreeNode, split: &Bpos) {
    // Move all keys >= split to new node
    let mut move_keys = Vec::new();
    for idx in 0..b.nsets as usize {
        let i = bset(b, idx);
        let mut k = i.start();
        while k < vstruct_last(i) {
            let unpacked = bkey_unpack(b, k);
            if bkey_cmp(&unpacked.p, split) >= 0 {
                move_keys.push(unsafe { *k.as_ptr() });
            }
            k = bkey_p_next(k);
        }
    }
}

/// Update keys after split
pub fn bch2_btree_node_split_update_keys(
    b: &mut BtreeNode,
    n: &mut BtreeNode,
    left_key: &mut BkeyI,
    right_key: &mut BkeyI,
) {
    // Set left/right key bounds
}

/// Btree node compact (merge adjacent keys)
pub fn bch2_btree_node_compact(b: &mut BtreeNode) -> Result<(), BchError> {
    // Compact the bset by removing whiteouts
    for idx in 0..b.nsets as usize {
        let i = bset(b, idx);
        let mut read = i.start();
        let mut write = i.start_mut();
        let mut new_u64s = 0u16;

        while read < vstruct_last(i) {
            if !bkey_packed_whiteout(&read) {
                if write.as_ptr() != read.as_ptr() {
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            read.as_ptr(),
                            write.as_ptr() as *mut u64,
                            read.u64s() as usize,
                        );
                    }
                }
                new_u64s += read.u64s();
                write = bkey_p_next_mut(write);
            }
            read = bkey_p_next(read);
        }
        i.set_u64s(new_u64s);
    }

    b.whiteout_u64s = 0;
    b.nr = bch2_btree_node_count_keys(b);

    Ok(())
}

/// Btree node merge (combine two sibling nodes)
pub fn bch2_btree_node_merge(
    trans: &mut BtreeTrans,
    path: &mut BtreePath,
    left: &mut BtreeNode,
    right: &mut BtreeNode,
) -> Result<(), BchError> {
    // Check if keys fit in one node
    let total_keys = left.nr.live_u64s + right.nr.live_u64s;
    if total_keys > btree_node_max_u64s(trans.c) {
        return Ok(()); // Can't merge
    }

    // Move keys from right to left
    for idx in 0..right.nsets as usize {
        let i = bset(right, idx);
        let mut k = i.start();
        while k < vstruct_last(i) {
            let unpacked = bkey_unpack(right, k);
            let insert_key = unsafe { &*k.as_ptr() };
            // Insert in left node
            let ki = BkeyI { k: insert_key.clone(), ..Default::default() };
            bch2_btree_node_insert(trans, path, left, &ki, insert_key.u64s() as usize)?;
            k = bkey_p_next(k);
        }
    }

    // Remove right node from parent
    bch2_btree_node_delete_from_parent(trans, path, right)?;

    Ok(())
}

/// Delete a node reference from its parent
pub fn bch2_btree_node_delete_from_parent(
    trans: &mut BtreeTrans,
    path: &mut BtreePath,
    b: &mut BtreeNode,
) -> Result<(), BchError> {
    let parent_level = b.c.level as usize + 1;
    if let Some(parent) = btree_path_node_mut(path, parent_level) {
        let mut iter = BtreeNodeIter::new();
        bch2_btree_node_iter_init(&mut iter, parent, &b.key.k.p);
        if let Some(k) = bch2_btree_node_iter_peek(&iter, parent) {
            let ki = BkeyI { k: unsafe { *k.as_ptr() }, ..Default::default() };
            bch2_btree_node_delete(trans, path, parent, &ki)?;
        }
    }
    Ok(())
}

/// Maximum u64s in a btree node
pub fn btree_node_max_u64s(c: &BchFs) -> u16 {
    let bytes = c.opts.btree_node_size;
    (bytes / 8) as u16 - 64 // reserve space for headers
}

/// Check if key is a btree pointer
pub fn bkey_is_btree_ptr(k: &Bkey) -> bool {
    matches!(k.type_val, BTREE_PTR_TYPE | BTREE_PTR_V2_TYPE)
}
