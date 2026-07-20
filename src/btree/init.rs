use crate::bcachefs::*;
use crate::bcachefs_format::*;
use crate::btree::bkey::*;
use crate::btree::bset::*;
use crate::btree::cache::*;
use crate::btree::locking::*;
use crate::btree::types::*;
use crate::btree::update::*;
use crate::errcode::*;

/// Initialize btree cache subsystem
pub fn bch2_fs_btree_cache_init_early(c: &mut BchFs) {
    let bc = &mut c.btree.cache;
    bc.table = BtreeCacheHashTable::new();
    bc.freeable = Vec::new();
    bc.freed_pcpu = Vec::new();
    bc.freed_nonpcpu = Vec::new();
    for i in 0..2 {
        bc.live[i].clean = Vec::new();
        bc.live[i].dirty = Vec::new();
        bc.live[i].nr_clean = 0;
        bc.live[i].nr_dirty = 0;
    }
    bc.nr_freeable = 0;
    bc.nr_reserve = 16;
}

/// Initialize btree cache (allocate reserve nodes)
pub fn bch2_fs_btree_cache_init(c: &mut BchFs) -> Result<(), BchError> {
    bch2_fs_btree_cache_init_early(c);
    let bc = &mut c.btree.cache;

    // Allocate reserve nodes
    for _ in 0..bc.nr_reserve {
        let b = Box::into_raw(Box::new(BtreeNode::new(c.opts.btree_node_size as u32)));
        if b.is_null() {
            return Err(BchError::ENOMEM);
        }
        let b = unsafe { &mut *b };

        // Mark as intent+write locked
        // Add to freeable list
        bc.freeable.push(unsafe { &mut *b });
        bc.nr_freeable += 1;
    }

    Ok(())
}

/// Initialize a btree root (create empty btree node)
pub fn bch2_btree_root_init(
    trans: &mut BtreeTrans,
    btree_id: u8,
    level: u8,
) -> Result<*mut BtreeNode, BchError> {
    let c = &mut trans.c;
    let b = bch2_btree_node_mem_alloc(trans, level != 0)?;

    b.btree_id = btree_id;
    b.c.level = level;
    b.key = BkeyI::default();
    b.key.k.p = Bpos::default();
    b.key.k.type_val = BTREE_PTR_V2_TYPE;

    // Initialize bset
    let data = &mut b.data;
    if data.is_some() {
        let d = data.as_mut().unwrap();
        d.btree_id = btree_id;
        d.level = level;
        d.keys.u64s = 0.into();
    }

    // Cache transition
    let bc = &mut c.btree.cache;
    let _ = bch2_btree_node_transition_state_locked(bc, b, BtreeNodeCacheState::Dirty);

    // Set as root
    bc.roots_known[btree_id as usize].b = b;
    set_btree_node_permanent(b);

    Ok(b as *mut BtreeNode)
}

/// Set a node as root
pub fn bch2_btree_set_root_inmem(c: &mut BchFs, b: &mut BtreeNode) {
    let bc = &mut c.btree.cache;
    bc.roots_known[b.btree_id as usize].b = b;
    set_btree_node_permanent(b);
}

pub fn set_btree_node_permanent(b: &mut BtreeNode) {
    b.flags |= 1 << BTREE_NODE_PERMANENT;
}

pub fn btree_node_permanent(b: &BtreeNode) -> bool {
    b.flags & (1 << BTREE_NODE_PERMANENT) != 0
}
