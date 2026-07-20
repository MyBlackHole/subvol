use crate::bcachefs::*;
use crate::bcachefs_format::*;
use crate::btree::bkey::*;
use crate::btree::bset::*;
use crate::btree::cache::*;
use crate::btree::locking::*;
use crate::btree::types::*;
use crate::btree::update::*;
use crate::errcode::*;

/// Btree iterator for traversing btree key-value pairs
pub struct BtreeIter {
    pub trans: *mut BtreeTrans,
    pub path: BtreePath,
    pub btree_id: u8,
    pub level: u8,
    pub flags: BtreeIterFlags,
    pub key: BkeyI,
}

#[derive(Clone, Copy)]
pub struct BtreeIterFlags(u32);

impl BtreeIterFlags {
    pub const SLOTS: Self = Self(1 << 0);
    pub const CACHED: Self = Self(1 << 1);
    pub const NOFILL: Self = Self(1 << 2);
    pub const ALL_SNAPSHOTS: Self = Self(1 << 3);
    pub const KEY_CACHE: Self = Self(1 << 4);
}

/// Initialize a btree iterator
pub fn bch2_btree_iter_init(
    trans: &mut BtreeTrans,
    btree_id: u8,
    pos: &Bpos,
    flags: BtreeIterFlags,
) -> BtreeIter {
    let mut iter = BtreeIter {
        trans: trans as *mut BtreeTrans,
        path: BtreePath {
            btree_id,
            level: 0,
            locks_want: 1,
            nodes_locked: 0,
            l: [BtreePathLevel::default(); BTREE_MAX_DEPTH],
            pos: *pos,
            ..Default::default()
        },
        btree_id,
        level: 0,
        flags,
        key: BkeyI::default(),
    };

    // Initialize root-level path
    iter.path.l[BTREE_MAX_DEPTH - 1].b = core::ptr::null_mut();
    iter.path.pos = *pos;

    iter
}

/// Traverse to the target position
pub fn bch2_btree_iter_traverse(iter: &mut BtreeIter) -> Result<(), BchError> {
    let trans = unsafe { &mut *iter.trans };

    // Start from root, walk down to leaf
    let mut level = iter.path.level;
    loop {
        if level >= BTREE_MAX_DEPTH as u8 - 1 {
            break;
        }

        let b = match btree_path_node_mut(&mut iter.path, level as usize) {
            Some(b) => b,
            None => break,
        };

        // Find child for our target position
        let child_k = bch2_btree_node_iter_peek_for_child(b, &iter.path.pos);
        if child_k.is_none() {
            break;
        }

        // Move down
        level += 1;
        iter.path.level = level;
    }
    Ok(())
}

/// Peek at current key
pub fn bch2_btree_iter_peek(iter: &mut BtreeIter) -> Result<Option<&BkeyI>, BchError> {
    let trans = unsafe { &mut *iter.trans };

    // Traverse to position
    bch2_btree_iter_traverse(iter)?;

    // Get leaf node
    let b = match btree_path_node_mut(&mut iter.path, 0) {
        Some(b) => b,
        None => return Ok(None),
    };

    // Find key at position
    let key = bch2_btree_node_iter_peek_for_pos(b, &iter.path.pos);

    match key {
        Some(k) => {
            iter.key = unsafe { *k };
            Ok(Some(&iter.key))
        }
        None => Ok(None),
    }
}

/// Peek at next key
pub fn bch2_btree_iter_peek_next(iter: &mut BtreeIter) -> Result<Option<&BkeyI>, BchError> {
    let trans = unsafe { &mut *iter.trans };

    // Advance position
    let next_pos = bpos_successor(&iter.path.pos);
    iter.path.pos = next_pos;
    bch2_btree_iter_peek(iter)
}

/// Peek at previous key
pub fn bch2_btree_iter_peek_prev(iter: &mut BtreeIter) -> Result<Option<&BkeyI>, BchError> {
    let trans = unsafe { &mut *iter.trans };

    let prev_pos = bpos_predecessor(&iter.path.pos);
    iter.path.pos = prev_pos;
    bch2_btree_iter_peek(iter)
}

/// Advance the iterator to the next key
pub fn bch2_btree_iter_advance(iter: &mut BtreeIter) -> Result<Option<&BkeyI>, BchError> {
    bch2_btree_iter_peek_next(iter)
}

/// Get helper: find key in btree node for child iteration
fn bch2_btree_node_iter_peek_for_child<'a>(
    b: &'a BtreeNode,
    pos: &Bpos,
) -> Option<*const BkeyI> {
    let mut iter = BtreeNodeIter::new();
    bch2_btree_node_iter_init(&mut iter, b, pos);

    let k = bch2_btree_node_iter_peek_all(&iter, b)?;
    let ki: &BkeyI = unsafe { &*(k as *const BkeyPacked as *const BkeyI) };
    Some(ki as *const BkeyI)
}

/// Get helper: find key at or after position
fn bch2_btree_node_iter_peek_for_pos<'a>(
    b: &'a BtreeNode,
    pos: &Bpos,
) -> Option<*const BkeyI> {
    let mut iter = BtreeNodeIter::new();
    bch2_btree_node_iter_init(&mut iter, b, pos);

    let k = bch2_btree_node_iter_peek(&iter, b)?;
    let ki: &BkeyI = unsafe { &*(k as *const BkeyPacked as *const BkeyI) };
    Some(ki as *const BkeyI)
}

/// Bpos successor
fn bpos_successor(pos: &Bpos) -> Bpos {
    let mut next = *pos;
    next.offset = next.offset.wrapping_add(1);
    next
}

/// Bpos predecessor
fn bpos_predecessor(pos: &Bpos) -> Bpos {
    let mut prev = *pos;
    prev.offset = prev.offset.wrapping_sub(1);
    prev
}

impl Default for BtreeIterFlags {
    fn default() -> Self {
        BtreeIterFlags(0)
    }
}
