use crate::bcachefs::*;
use crate::bcachefs_format::*;
use crate::btree::bkey::*;
use crate::btree::bset::*;
use crate::btree::cache::*;
use crate::btree::locking::*;
use crate::btree::sort::*;
use crate::btree::types::*;
use crate::errcode::*;

/// Wait for read in flight to complete
pub fn bch2_btree_node_wait_on_read(trans: &mut BtreeTrans, b: &mut BtreeNode) {
    bch2_trans_submit_write_bios(trans);
    while btree_node_read_in_flight(b) {
        // yield
    }
}

/// Wait for write in flight to complete
pub fn bch2_btree_node_wait_on_write(trans: &mut BtreeTrans, b: &mut BtreeNode) {
    bch2_trans_submit_write_bios(trans);
    while btree_node_write_in_flight(b) {
        // yield
    }
}

/// IO lock for btree node
pub fn bch2_btree_node_io_lock(b: &mut BtreeNode) {
    while test_and_set_bit(BTREE_NODE_WRITE_IN_FLIGHT as u32, &mut b.flags) {
        // spin
    }
}

/// IO unlock for btree node
pub fn bch2_btree_node_io_unlock(b: &mut BtreeNode) {
    clear_bit(BTREE_NODE_WRITE_IN_FLIGHT as u32, &mut b.flags);
    clear_bit(BTREE_NODE_WRITE_IN_FLIGHT_INNER as u32, &mut b.flags);
}

/// Drop keys outside the node's range
pub fn bch2_btree_node_drop_keys_outside_node(b: &mut BtreeNode) {
    let data = b.data.as_ref().unwrap();
    for t in b.bset_iter() {
        let i = bset(b, t);
        let mut k = i.start();
        while k < vstruct_last(i) {
            if bkey_cmp_left_packed(b, k, &data.min_key) >= 0 {
                break;
            }
            k = bkey_p_next(k);
        }
        if k != i.start() {
            let shift = (k.as_ptr() as usize - i.start().as_ptr() as usize) / 8;
            let remaining = (vstruct_end(i).as_ptr() as usize - k.as_ptr() as usize) / 8;
            // memmove_u64s_down
            unsafe {
                core::ptr::copy(k.as_ptr(), i.start().as_ptr() as *mut u64, remaining);
            }
            i.set_u64s(i.u64s() - shift);
        }

        let mut k = i.start();
        while k < vstruct_last(i) {
            if bkey_cmp_left_packed(b, k, &data.max_key) > 0 {
                break;
            }
            k = bkey_p_next(k);
        }
        if k != vstruct_last(i) {
            let new_u64s = (k.as_ptr() as usize - i.start().as_ptr() as usize) / 8;
            i.set_u64s(new_u64s);
        }
    }
    // Rebuild aux trees
    bch2_bset_set_no_aux_tree(b, b.set[0]);
    bch2_btree_build_aux_trees(b);
    b.nr = bch2_btree_node_count_keys(b);
}

/// Validate a bset
pub fn bch2_validate_bset(
    c: &BchFs,
    b: &BtreeNode,
    i: &Bset,
    write: bool,
) -> Result<(), BchError> {
    let version = i.version();
    // check version compatibility
    if !bch2_version_compatible(version) {
        return Err(BchError::EINVAL);
    }
    Ok(())
}

/// Read btree node from disk (done processing)
pub fn bch2_btree_node_read_done(
    c: &mut BchFs,
    b: &mut BtreeNode,
) -> Result<(), BchError> {
    let mut max_journal_seq = 0u64;

    b.version_ondisk = u16::MAX;
    b.written = 0;

    // Process bsets
    let ptr_written = btree_ptr_sectors_written(bkey_i_to_s_c(&b.key));
    let total_sectors = if ptr_written > 0 { ptr_written } else { btree_sectors(c) };

    loop {
        if b.written >= total_sectors {
            break;
        }
        let first = b.written == 0;
        let i = if first {
            &b.data.as_ref().unwrap().keys
        } else {
            let bne = write_block(b);
            if bne.keys.seq != b.data.as_ref().unwrap().keys.seq {
                break;
            }
            &bne.keys
        };

        // Validate bset
        bch2_validate_bset(c, b, i, false)?;

        if first {
            let sectors = vstruct_sectors(b.data.as_ref().unwrap(), c.block_bits as u8);
            b.written += sectors;
        } else {
            b.written += 1; // 1 sector for subsequent bsets
        }
    }

    // Sort and merge
    let mut sorted: Vec<u64> = Vec::new();
    let mut iter = sort_iter_init(b, (btree_blocks(c) + 1) * 2);
    sort_iter_add(&mut iter, b, &b.data.as_ref().unwrap().keys);

    bch2_sort_keys_into(&mut sorted, &mut iter, None);

    // Build aux trees
    bch2_bset_set_no_aux_tree(b, b.set.iter_mut().next().unwrap());
    bch2_btree_build_aux_trees(b);
    b.nr = bch2_btree_node_count_keys(b);

    Ok(())
}

/// Submit write bios (stub)
pub fn bch2_trans_submit_write_bios(trans: &mut BtreeTrans) {
    // In Rust impl, this is a no-op since we handle I/O synchronously
}

/// Clear a bit
fn clear_bit(bit: u32, flags: &mut u64) {
    *flags &= !(1u64 << bit);
}
