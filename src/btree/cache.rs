use crate::bcachefs::*;
use crate::bcachefs_format::*;
use crate::btree::locking::*;
use crate::btree::types::*;
use crate::errcode::*;
use core::sync::atomic::{AtomicU64, Ordering};

/// Flags for btree node reclaim
#[derive(Clone, Copy)]
pub struct BtreeNodeReclaimFlags(u32);

impl BtreeNodeReclaimFlags {
    pub const SHRINKER: Self = Self(1 << 0);
    pub const ALLOW_DIRTY: Self = Self(1 << 1);
}

/// Hash function for btree pointer
pub fn btree_ptr_hash_val(k: &BkeyI) -> u64 {
    // hash of btree pointer
    k.k.p.offset ^ k.k.p.snapshot
}

/// Check if btree node is hashed
pub fn btree_node_state_hashed(state: BtreeNodeCacheState) -> bool {
    matches!(state, BtreeNodeCacheState::Clean | BtreeNodeCacheState::Dirty)
}

/// Check if btree node state has a data buffer
pub fn btree_node_state_has_buffer(state: BtreeNodeCacheState) -> bool {
    btree_node_state_hashed(state) || state == BtreeNodeCacheState::Freeable
}

/// Get the live state from flags
pub fn btree_node_live_state(b: &BtreeNode) -> BtreeNodeCacheState {
    if btree_node_dirty(b) {
        BtreeNodeCacheState::Dirty
    } else {
        BtreeNodeCacheState::Clean
    }
}

/// Transition cache state (locked variant)
pub fn bch2_btree_node_transition_state_locked(
    bc: &mut BchFsBtreeCache,
    b: &mut BtreeNode,
    new: BtreeNodeCacheState,
) -> Result<(), BchError> {
    let old = b.cache_state;
    let pinned = btree_node_pinned(b);
    let hashed_delta = btree_node_state_hashed(new) as i32 - btree_node_state_hashed(old) as i32;
    let hashed_new = btree_node_state_hashed(new);
    let hashed_old = btree_node_state_hashed(old);

    if old == new {
        return Ok(());
    }

    // hash table transition
    if hashed_delta > 0 {
        b.hash_val = btree_ptr_hash_val(&b.key);
        // Simple hash insert
        bc.table.insert(b.hash_val, b as *mut _ as usize);
        if b.btree_id < BTREE_ID_NR as u8 {
            bc.nr_by_btree[b.btree_id as usize] += 1;
        }
    }
    if hashed_delta < 0 {
        bc.table.remove(&b.hash_val);
        b.hash_val = 0;
        clear_btree_node_just_written(b);
        if b.btree_id < BTREE_ID_NR as u8 {
            bc.nr_by_btree[b.btree_id as usize] -= 1;
        }
        // wake up all waiters
    }

    // Remove from old list
    match old {
        BtreeNodeCacheState::Clean => {
            bc.live[pinned as usize].nr_clean -= 1;
        }
        BtreeNodeCacheState::Dirty => {
            bc.live[pinned as usize].nr_dirty -= 1;
        }
        BtreeNodeCacheState::Freeable => {
            bc.nr_freeable -= 1;
        }
        BtreeNodeCacheState::Freed | BtreeNodeCacheState::None => {}
    }

    // Add to new list
    match new {
        BtreeNodeCacheState::None => {}
        BtreeNodeCacheState::Clean => {
            bc.live[pinned as usize].nr_clean += 1;
            bc.live[pinned as usize].clean.push(b);
        }
        BtreeNodeCacheState::Dirty => {
            bc.live[pinned as usize].nr_dirty += 1;
            bc.live[pinned as usize].dirty.push(b);
        }
        BtreeNodeCacheState::Freeable => {
            bc.nr_freeable += 1;
            bc.freeable.push(b);
        }
        BtreeNodeCacheState::Freed => {
            // data is already freed
            if b.lock.readers {
                bc.freed_pcpu.push(b);
            } else {
                bc.freed_nonpcpu.push(b);
            }
        }
    }

    b.clear_btree_node_accessed();
    b.cache_state = new;
    Ok(())
}

/// Transition cache state
pub fn bch2_btree_node_transition_state(
    bc: &mut BchFsBtreeCache,
    b: &mut BtreeNode,
    new: BtreeNodeCacheState,
) -> Result<(), BchError> {
    bch2_btree_node_transition_state_locked(bc, b, new)
}

/// Set btree node dirty
pub fn bch2_btree_node_set_dirty(c: &mut BchFs, b: &mut BtreeNode) {
    let bc = &mut c.btree.cache;
    if test_and_set_bit(BTREE_NODE_DIRTY, &mut b.flags) {
        return;
    }
    if btree_node_state_hashed(b.cache_state) {
        let _ = bch2_btree_node_transition_state_locked(bc, b, BtreeNodeCacheState::Dirty);
    }
}

/// Re-acquire locks after transaction restart
pub fn bch2_trans_relock(trans: &mut BtreeTrans) -> Result<(), BchError> {
    for path in &mut trans.paths {
        if !bch2_btree_path_relock_norestart(trans, path) {
            return Err(BchError::EINVAL);
        }
    }
    Ok(())
}

/// Unlock all paths in a transaction
pub fn bch2_trans_unlock(trans: &mut BtreeTrans) {
    for path in &mut trans.paths {
        __bch2_btree_path_unlock(trans, path);
    }
}

/// Allocate btree node memory
pub fn bch2_btree_node_mem_alloc(
    trans: &mut BtreeTrans,
    pcpu_read_locks: bool,
) -> Result<&mut BtreeNode, BchError> {
    let c = &mut trans.c;
    let bc = &mut c.btree.cache;

    // Try freeable list first
    if let Some(b) = bc.freeable.pop() {
        // Reclaim it
        if b.lock.try_intent() && b.lock.try_write() {
            return init_node(trans, bc, b);
        }
        bc.freeable.push(b);
    }

    // Try freed pool
    let freed = if pcpu_read_locks {
        &mut bc.freed_pcpu
    } else {
        &mut bc.freed_nonpcpu
    };

    if let Some(b) = freed.pop() {
        if b.lock.try_intent() && b.lock.try_write() {
            return init_node(trans, bc, b);
        }
        freed.push(b);
    }

    // Allocate new
    let alloc_size = if pcpu_read_locks {
        core::mem::size_of::<BtreeNode>() + 64
    } else {
        core::mem::size_of::<BtreeNode>()
    };
    let b = Box::into_raw(Box::new(BtreeNode::new(alloc_size)));
    if b.is_null() {
        return Err(BchError::ENOMEM);
    }
    let b = unsafe { &mut *b };

    b.lock.try_intent();
    b.lock.try_write();
    init_node(trans, bc, b)
}

fn init_node(
    trans: &mut BtreeTrans,
    bc: &mut BchFsBtreeCache,
    b: &mut BtreeNode,
) -> Result<&mut BtreeNode, BchError> {
    b.flags = 0;
    b.written = 0;
    b.nsets = 0;
    b.sib_u64s = [0, 0];
    b.whiteout_u64s = 0;
    let _ = bch2_btree_node_transition_state_locked(bc, b, BtreeNodeCacheState::Clean);
    Ok(b)
}

/// Wait for read to complete
pub fn bch2_btree_node_wait_on_read(trans: &mut BtreeTrans, b: &mut BtreeNode) {
    while btree_node_read_in_flight(b) {
        // yield
    }
}

/// Wait for write to complete
pub fn bch2_btree_node_wait_on_write(trans: &mut BtreeTrans, b: &mut BtreeNode) {
    while btree_node_write_in_flight(b) {
        // yield
    }
}

/// Find a btree node in the cache
pub fn btree_cache_find(bc: &BchFsBtreeCache, k: &BkeyI) -> Option<&mut BtreeNode> {
    let hash_val = btree_ptr_hash_val(k);
    bc.table.get(&hash_val).map(|ptr| unsafe { &mut *(ptr as *mut BtreeNode) })
}

/// Get a btree node from cache (high-level)
pub fn bch2_btree_node_get(
    trans: &mut BtreeTrans,
    path: &mut BtreePath,
    k: &BkeyI,
    level: usize,
    lock_type: SixLockType,
    flags: BtreeIterUpdateTriggerFlags,
) -> Result<&mut BtreeNode, BchError> {
    if level >= BTREE_MAX_DEPTH {
        return Err(BchError::EINVAL);
    }

    let b = bch2_btree_node_get_noiter(trans, k, path.btree_id, level, flags.contains(BtreeIterUpdateTriggerFlags::NOFILL))?;
    Ok(b)
}

/// Get btree node without iterator
pub fn bch2_btree_node_get_noiter(
    trans: &mut BtreeTrans,
    k: &BkeyI,
    btree_id: u8,
    level: usize,
    nofill: bool,
) -> Result<&mut BtreeNode, BchError> {
    let bc = &mut trans.c.btree.cache;

    loop {
        if let Some(b) = btree_cache_find(bc, k) {
            trans.locking_hash_val = btree_ptr_hash_val(k);
            trans.locking_root_id = -1i8 as u8;

            if b.lock.try_read() {
                if b.hash_val == btree_ptr_hash_val(k)
                    && b.btree_id == btree_id
                    && b.c.level == level as u8
                {
                    set_btree_node_accessed(b);
                    bch2_btree_node_wait_on_read(trans, b);
                    bch2_btree_node_wait_on_write(trans, b);
                    return Ok(b);
                }
                b.lock.unlock_read();
            }
        } else if nofill {
            return Err(BchError::EINVAL);
        } else {
            let b = bch2_btree_node_fill(trans, k, btree_id, level)?;
            return Ok(b);
        }
    }
}

/// Fill (read) a btree node from disk
fn bch2_btree_node_fill(
    trans: &mut BtreeTrans,
    k: &BkeyI,
    btree_id: u8,
    level: usize,
) -> Result<&mut BtreeNode, BchError> {
    let b = bch2_btree_node_mem_alloc(trans, level != 0)?;

    b.key = *k;
    b.btree_id = btree_id;
    b.c.level = level as u8;

    bch2_btree_node_read_done(trans, b)
}

/// Check if btree node is pinned
fn btree_node_pinned(b: &BtreeNode) -> bool {
    b.flags & (1 << BTREE_NODE_PINNED) != 0
}

/// Test and set bit
fn test_and_set_bit(bit: u32, flags: &mut u64) -> bool {
    let mask = 1u64 << bit;
    let old = *flags;
    *flags |= mask;
    old & mask != 0
}

/// Set btree node accessed
fn set_btree_node_accessed(b: &mut BtreeNode) {
    b.flags |= 1 << BTREE_NODE_ACCESSED;
}

/// Clear btree node accessed
impl BtreeNode {
    fn clear_btree_node_accessed(&mut self) {
        self.flags &= !(1 << BTREE_NODE_ACCESSED);
    }
}

/// Hash table for btree cache
pub struct BtreeCacheHashTable {
    buckets: Vec<Option<(u64, usize)>>,
    len: usize,
}

impl BtreeCacheHashTable {
    pub fn new() -> Self {
        BtreeCacheHashTable {
            buckets: vec![None; 256],
            len: 0,
        }
    }

    fn hash(key: &u64) -> usize {
        (*key as usize) % 256
    }

    pub fn insert(&mut self, key: u64, val: usize) {
        let mut idx = Self::hash(&key);
        loop {
            if self.buckets[idx].is_none() || self.buckets[idx].unwrap().0 == key {
                if self.buckets[idx].is_none() {
                    self.len += 1;
                }
                self.buckets[idx] = Some((key, val));
                return;
            }
            idx = (idx + 1) % self.buckets.len();
        }
    }

    pub fn get(&self, key: &u64) -> Option<usize> {
        let mut idx = Self::hash(key);
        loop {
            match &self.buckets[idx] {
                Some((k, v)) if k == key => return Some(*v),
                None => return None,
                _ => {
                    idx = (idx + 1) % self.buckets.len();
                    if idx == Self::hash(key) {
                        return None;
                    }
                }
            }
        }
    }

    pub fn remove(&mut self, key: &u64) {
        let mut idx = Self::hash(key);
        loop {
            match &self.buckets[idx] {
                Some((k, _)) if k == key => {
                    self.buckets[idx] = None;
                    self.len -= 1;
                    return;
                }
                None => return,
                _ => {
                    idx = (idx + 1) % self.buckets.len();
                    if idx == Self::hash(key) {
                        return;
                    }
                }
            }
        }
    }
}
