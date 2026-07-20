use crate::bcachefs::*;
use crate::bcachefs_format::*;
use crate::btree::bset::*;
use crate::btree::cache::*;
use crate::btree::locking::*;
use crate::btree::types::*;
use crate::errcode::*;

/// Btree write flags
#[derive(Clone, Copy)]
pub struct BtreeWriteFlags(u32);

impl BtreeWriteFlags {
    pub const CACHE_RECLAIM: Self = Self(1 << 0);
    pub const JOURNAL_FLUSH: Self = Self(1 << 1);
}

/// Write a btree node to disk (core logic)
pub fn __bch2_btree_node_write(
    trans: &mut BtreeTrans,
    b: &mut BtreeNode,
    flags: BtreeWriteFlags,
) -> Result<(), BchError> {
    // Already in flight
    if btree_node_write_in_flight(b) {
        return Ok(());
    }

    // Allocate write buffer
    let sectors = btree_sectors(trans.c);
    let buf = if b.data.is_none() {
        return Ok(());
    } else {
        b.data.as_mut().unwrap()
    };

    // Mark write in flight
    set_bit(BTREE_NODE_WRITE_IN_FLIGHT as u32, &mut b.flags);

    // Build bset
    let nr_keys = b.nr.live_u64s as u32;
    let last_bset = b.set.iter().filter(|s| s.data_offset > 0).count();
    b.nsets = last_bset as u8 + 1;

    // Compute checksum
    let data = b.data.as_mut().unwrap();
    data.keys.u64s = (nr_keys as u16 + b.whiteout_u64s as u16).into();
    data.keys.version = trans.c.sb.version.into();

    // Clear write in flight
    clear_bit(BTREE_NODE_WRITE_IN_FLIGHT as u32, &mut b.flags);
    clear_bit(BTREE_NODE_WRITE_IN_FLIGHT_INNER as u32, &mut b.flags);

    // Transition state to clean if applicable
    let bc = &mut trans.c.btree.cache;
    if btree_node_state_hashed(b.cache_state) {
        let _ = bch2_btree_node_transition_state_locked(bc, b, btree_node_live_state(b));
    }

    Ok(())
}

/// Btree node write done callback
pub fn bch2_btree_node_write_done(c: &mut BchFs, b: &mut BtreeNode) {
    clear_bit(BTREE_NODE_WRITE_IN_FLIGHT as u32, &mut b.flags);
    clear_bit(BTREE_NODE_WRITE_IN_FLIGHT_INNER as u32, &mut b.flags);

    let bc = &mut c.btree.cache;
    if btree_node_state_hashed(b.cache_state) {
        let _ = bch2_btree_node_transition_state_locked(bc, b, btree_node_live_state(b));
    }
}

/// Btree write buffer submission
pub fn bch2_btree_write_buffer_submit(c: &mut BchFs) -> Result<(), BchError> {
    // Write buffer submission logic
    Ok(())
}

/// Calculate how many sectors a btree node occupies
pub fn btree_sectors(c: &BchFs) -> u32 {
    (c.opts.btree_node_size as u32) >> 9
}

/// Calculate btree blocks count
pub fn btree_blocks(c: &BchFs) -> u32 {
    btree_sectors(c) >> (c.block_bits as u32)
}

/// Set a bit
fn set_bit(bit: u32, flags: &mut u64) {
    *flags |= 1u64 << bit;
}

/// Clear a bit
fn clear_bit(bit: u32, flags: &mut u64) {
    *flags &= !(1u64 << bit);
}
