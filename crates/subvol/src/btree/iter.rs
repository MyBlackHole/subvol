use super::bkey::{
    bch2_key_resize, bkey, bkey_deleted, bkey_eq, bkey_ge, bkey_init, bkey_lt, bkey_packed,
    bkey_s_c, bkey_start_pos, bkeyp_key_u64s, bkeyp_val_u64s, bpos, bpos_cmp, bpos_eq, bpos_gt,
    bpos_lt, bpos_max, bpos_nosnap_predecessor, bpos_nosnap_successor, bpos_predecessor,
    bpos_successor, bpos_with_snapshot, KEY_INODE_MAX, KEY_OFFSET_MAX, KEY_SIZE_MAX, POS_MAX,
    POS_MIN, SPOS_MAX,
};
use super::bset::{bkey_i_btree_ptr_v2, btree_node_mem_ptr, BTREE_MAX_DEPTH};
use super::node_iter::{
    bch2_btree_node_iter_advance, bch2_btree_node_iter_init, bch2_btree_node_iter_peek,
    bch2_btree_node_iter_peek_all, bch2_btree_node_iter_prev, bch2_btree_node_iter_sort,
    bkey_iter_pos_cmp,
};
use super::types::{
    bch2_btree_id_root_packed, bch2_btree_root_unpack_b, bch2_btree_root_unpack_level, bch_fs,
    btree, btree_node_iter,
};
use crate::lock::six::{
    six_lock_counts, six_lock_downgrade, six_lock_increment, six_lock_intent, six_lock_read,
    six_lock_seq, six_lock_tryupgrade, six_lock_type, six_lock_write, six_relock_type,
    six_unlock_intent, six_unlock_read, six_unlock_write,
};
use core::sync::atomic::Ordering;

pub const BTREE_ITER_INITIAL: usize = 64;
pub type btree_path_idx_t = u16;

pub type btree_trans_commit_hook_fn =
    unsafe extern "C" fn(*mut btree_trans, *mut btree_trans_commit_hook) -> i32;

#[repr(C)]
pub struct btree_trans_commit_hook {
    pub fn_: btree_trans_commit_hook_fn,
    pub next: *mut btree_trans_commit_hook,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct btree_trans_subbuf {
    pub base: u16,
    pub u64s: u16,
    pub size: u16,
}

pub const BTREE_ITER_slots: u16 = 1 << 0;
pub const BTREE_ITER_intent: u16 = 1 << 1;
pub const BTREE_ITER_prefetch: u16 = 1 << 2;
pub const BTREE_ITER_is_extents: u16 = 1 << 3;
pub const BTREE_ITER_not_extents: u16 = 1 << 4;
pub const BTREE_ITER_cached: u16 = 1 << 5;
pub const BTREE_ITER_with_key_cache: u16 = 1 << 6;
pub const BTREE_ITER_with_journal: u16 = 1 << 7;
pub const BTREE_ITER_snapshot_field: u16 = 1 << 8;
pub const BTREE_ITER_all_snapshots: u16 = 1 << 9;
pub const BTREE_ITER_filter_snapshots: u16 = 1 << 10;
pub const BTREE_ITER_nofilter_whiteouts: u16 = 1 << 11;
pub const BTREE_ITER_nopreserve: u16 = 1 << 12;
pub const BTREE_ITER_nofill: u16 = 1 << 13;
pub const BTREE_ITER_cached_nofill: u16 = 1 << 14;
pub const BTREE_ITER_key_cache_fill: u16 = 1 << 15;

fn btree_id_cached(btree_id: u8) -> bool {
    matches!(btree_id, 1 | 4)
}

fn btree_type_has_snapshot_field(_btree_id: u8) -> bool {
    false
}

/// Matches the local `bch2_btree_iter_flags()` property normalization.
pub unsafe fn bch2_btree_iter_flags(
    trans: *const btree_trans,
    btree_id: u8,
    level: u8,
    mut flags: u16,
) -> u16 {
    if level != 0 || !btree_id_cached(btree_id) {
        flags &= !BTREE_ITER_cached;
        flags &= !BTREE_ITER_with_key_cache;
    } else if flags & BTREE_ITER_cached == 0 {
        flags |= BTREE_ITER_with_key_cache;
    }
    if flags & (BTREE_ITER_all_snapshots | BTREE_ITER_not_extents) == 0
        && super::types::btree_id_is_extents(btree_id)
    {
        flags |= BTREE_ITER_is_extents;
    }
    if flags & BTREE_ITER_snapshot_field == 0 && !btree_type_has_snapshot_field(btree_id) {
        flags &= !BTREE_ITER_all_snapshots;
    }
    if flags & BTREE_ITER_all_snapshots == 0 && super::types::btree_type_has_snapshots(btree_id) {
        flags |= BTREE_ITER_filter_snapshots;
    }
    if !trans.is_null() && (*trans).journal_replay_not_finished {
        flags |= BTREE_ITER_with_journal;
    }
    flags
}

pub const BTREE_NODE_UNLOCKED: u8 = 0;
pub const BTREE_NODE_READ_LOCKED: u8 = 1;
pub const BTREE_NODE_INTENT_LOCKED: u8 = 2;
pub const BTREE_NODE_WRITE_LOCKED: u8 = 3;

pub unsafe fn btree_path_node(path: *mut btree_path, level: usize) -> *mut btree {
    if path.is_null() || level >= BTREE_MAX_DEPTH as usize {
        return core::ptr::null_mut();
    }
    (*path).l[level].b
}

pub unsafe fn btree_node_parent(path: *mut btree_path, b: *mut btree) -> *mut btree {
    if path.is_null() || b.is_null() {
        return core::ptr::null_mut();
    }
    btree_path_node(path, (*b).c.level as usize + 1)
}

pub unsafe fn btree_node_lock_seq_matches(
    path: *mut btree_path,
    b: *const btree,
    level: usize,
) -> bool {
    if path.is_null() || b.is_null() || level >= BTREE_MAX_DEPTH as usize {
        return false;
    }
    (*path).l[level].lock_seq == six_lock_seq(&(*b).c.lock)
}

pub unsafe fn btree_path_pos_before_node(path: *const btree_path, b: *const btree) -> bool {
    if path.is_null() || b.is_null() || (*b).data.is_null() {
        return false;
    }
    bpos_lt((*path).pos, (*(*b).data).min_key)
}

pub unsafe fn btree_path_pos_after_node(path: *const btree_path, b: *const btree) -> bool {
    if path.is_null() || b.is_null() {
        return false;
    }
    bpos_gt((*path).pos, (*b).key.k.p)
}

pub unsafe fn btree_path_pos_in_node(path: *const btree_path, b: *const btree) -> bool {
    if path.is_null() || b.is_null() {
        return false;
    }
    (*path).btree_id == (*b).c.btree_id
        && !btree_path_pos_before_node(path, b)
        && !btree_path_pos_after_node(path, b)
}

pub unsafe fn btree_path_advance_to_pos(
    path: *mut btree_path,
    level: usize,
    max_advance: usize,
) -> bool {
    if path.is_null() || level >= BTREE_MAX_DEPTH as usize {
        return false;
    }
    let b = (*path).l[level].b;
    if b.is_null() {
        return false;
    }
    let iter = &mut (*path).l[level].iter;
    let mut advanced = 0;
    while {
        let k = bch2_btree_node_iter_peek_all(iter, b);
        !k.is_null() && bkey_iter_pos_cmp(b, k, &(*path).pos) < 0
    } {
        if advanced >= max_advance {
            return false;
        }
        bch2_btree_node_iter_advance(iter, b);
        advanced += 1;
    }
    true
}

pub unsafe fn btree_path_set_should_be_locked(_trans: *mut btree_trans, path: *mut btree_path) {
    if path.is_null() {
        return;
    }
    assert!(btree_node_locked(path, (*path).level as usize));
    (*path).should_be_locked = true;
}

pub unsafe fn btree_node_locked_type(path: *const btree_path, level: usize) -> u8 {
    if path.is_null() || level >= BTREE_MAX_DEPTH as usize {
        return BTREE_NODE_UNLOCKED;
    }
    path_locked_type(&*path, level)
}

pub unsafe fn btree_node_locked_type_nowrite(path: *const btree_path, level: usize) -> u8 {
    let lock_type = btree_node_locked_type(path, level);
    if lock_type == BTREE_NODE_WRITE_LOCKED {
        BTREE_NODE_INTENT_LOCKED
    } else {
        lock_type
    }
}

pub unsafe fn btree_node_write_locked(path: *const btree_path, level: usize) -> bool {
    btree_node_locked_type(path, level) == BTREE_NODE_WRITE_LOCKED
}

pub unsafe fn btree_node_intent_locked(path: *const btree_path, level: usize) -> bool {
    btree_node_locked_type(path, level) == BTREE_NODE_INTENT_LOCKED
}

pub unsafe fn btree_node_read_locked(path: *const btree_path, level: usize) -> bool {
    btree_node_locked_type(path, level) == BTREE_NODE_READ_LOCKED
}

pub unsafe fn btree_node_locked(path: *const btree_path, level: usize) -> bool {
    btree_node_locked_type(path, level) != BTREE_NODE_UNLOCKED
}

pub unsafe fn btree_node_lock_increment(
    trans: *mut btree_trans,
    b: *mut super::types::btree_bkey_cached_common,
    level: usize,
    want: u8,
) -> bool {
    if trans.is_null() || b.is_null() || level >= BTREE_MAX_DEPTH as usize {
        return false;
    }
    for idx in 1..BTREE_ITER_INITIAL {
        if (*trans).paths_allocated & (1u64 << idx) == 0 {
            continue;
        }
        let path = (*trans).paths.add(idx);
        let node = (*path).l[level].b;
        if node.is_null()
            || !core::ptr::eq(&mut (*node).c, b)
            || btree_node_locked_type(path, level) < want
        {
            continue;
        }
        let six_type = match want {
            BTREE_NODE_READ_LOCKED => six_lock_type::SIX_LOCK_read,
            BTREE_NODE_INTENT_LOCKED => six_lock_type::SIX_LOCK_intent,
            BTREE_NODE_WRITE_LOCKED => six_lock_type::SIX_LOCK_write,
            _ => return false,
        };
        six_lock_increment(&(*b).lock, six_type);
        return true;
    }
    false
}

pub unsafe fn btree_path_lowest_level_locked(path: *const btree_path) -> Option<usize> {
    if path.is_null() || (*path).nodes_locked == 0 {
        return None;
    }
    Some(((*path).nodes_locked.trailing_zeros() as usize) >> 1)
}

pub unsafe fn btree_path_highest_level_locked(path: *const btree_path) -> Option<usize> {
    if path.is_null() || (*path).nodes_locked == 0 {
        return None;
    }
    Some((usize::BITS as usize - 1 - (*path).nodes_locked.leading_zeros() as usize) >> 1)
}

pub unsafe fn bch2_btree_node_relock(
    trans: *mut btree_trans,
    path: *mut btree_path,
    level: usize,
) -> bool {
    if trans.is_null() || path.is_null() || level >= BTREE_MAX_DEPTH as usize {
        return false;
    }
    if btree_node_locked(path, level) {
        return true;
    }
    let b = (*path).l[level].b;
    if b.is_null() {
        return false;
    }
    let want = path_lock_type(&*path, level);
    let six_type = match want {
        BTREE_NODE_READ_LOCKED => six_lock_type::SIX_LOCK_read,
        BTREE_NODE_INTENT_LOCKED => six_lock_type::SIX_LOCK_intent,
        BTREE_NODE_WRITE_LOCKED => six_lock_type::SIX_LOCK_write,
        _ => return false,
    };
    if six_relock_type(&(*b).c.lock, six_type, (*path).l[level].lock_seq)
        || (btree_node_lock_seq_matches(path, b, level)
            && btree_node_lock_increment(trans, &mut (*b).c, level, want))
    {
        path_mark_locked(&mut *path, level, want);
        return true;
    }
    false
}

pub unsafe fn bch2_btree_path_relock_norestart(
    trans: *mut btree_trans,
    path: *mut btree_path,
) -> bool {
    if trans.is_null() || path.is_null() {
        return false;
    }
    let mut level = (*path).level as usize;
    while level < (*path).locks_want as usize {
        if (*path).l[level].b.is_null() {
            break;
        }
        if !bch2_btree_node_relock(trans, path, level) {
            return false;
        }
        level += 1;
    }
    true
}

pub unsafe fn bch2_btree_path_relock(trans: *mut btree_trans, path: *mut btree_path) -> i32 {
    if trans.is_null() || path.is_null() {
        return -22;
    }
    if bch2_btree_path_relock_norestart(trans, path) {
        0
    } else {
        -11
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct btree_insert_entry {
    pub flags: u32,
    pub sort_order: u8,
    pub bkey_type: u8,
    pub btree_id: u8,
    pub level: u8,
    pub cached: bool,
    pub insert_trigger_run: bool,
    pub overwrite_trigger_run: bool,
    pub key_cache_already_flushed: bool,
    pub key_cache_flushing: bool,
    pub old_btree_u64s: u8,
    pub k_buf_u64s: u8,
    pub path: btree_path_idx_t,
    pub k: *mut super::bkey::bkey_i,
    pub old_k: bkey,
    pub old_v: *const super::bkey::bch_val,
    pub ip_allocated: usize,
}

impl Default for btree_insert_entry {
    fn default() -> Self {
        Self {
            flags: 0,
            sort_order: 0,
            bkey_type: 0,
            btree_id: 0,
            level: 0,
            cached: false,
            insert_trigger_run: false,
            overwrite_trigger_run: false,
            key_cache_already_flushed: false,
            key_cache_flushing: false,
            old_btree_u64s: 0,
            k_buf_u64s: 0,
            path: 0,
            k: core::ptr::null_mut(),
            old_k: bkey::default(),
            old_v: core::ptr::null(),
            ip_allocated: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct btree_path_level {
    pub b: *mut btree,
    pub iter: btree_node_iter,
    pub lock_seq: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct btree_path {
    pub sorted_idx: btree_path_idx_t,
    pub ref_: u8,
    pub intent_ref: u8,
    pub pos: bpos,
    pub btree_id: u8,
    pub cached: bool,
    pub preserve: bool,
    pub should_be_locked: bool,
    pub level: u8,
    pub locks_want: u8,
    pub nodes_locked: u8,
    pub l: [btree_path_level; BTREE_MAX_DEPTH as usize],
}

impl Default for btree_path {
    fn default() -> Self {
        Self {
            sorted_idx: 0,
            ref_: 0,
            intent_ref: 0,
            pos: Default::default(),
            btree_id: 0,
            cached: false,
            preserve: false,
            should_be_locked: false,
            level: 0,
            locks_want: 0,
            nodes_locked: 0,
            l: [btree_path_level::default(); BTREE_MAX_DEPTH as usize],
        }
    }
}

#[repr(C)]
pub struct btree_trans {
    pub c: *mut bch_fs,
    pub paths_allocated: u64,
    pub paths: *mut btree_path,
    pub updates: *mut btree_insert_entry,
    pub mem: *mut u8,
    pub mem_top: u32,
    pub mem_bytes: u32,
    pub realloc_bytes_required: u32,
    pub nr_paths: btree_path_idx_t,
    pub nr_paths_max: btree_path_idx_t,
    pub nr_sorted: btree_path_idx_t,
    pub nr_updates: btree_path_idx_t,
    pub restarted: i16,
    pub restart_count: u32,
    pub locked: bool,
    pub write_locked: bool,
    pub journal_replay_not_finished: bool,
    pub has_interior_updates: bool,
    pub hooks: *mut btree_trans_commit_hook,
    pub journal_res: crate::journal::journal_res,
    pub journal_entries: btree_trans_subbuf,
    pub accounting: btree_trans_subbuf,
    pub journal_u64s: u32,
    pub extra_journal_u64s: u32,
    pub extra_disk_res: u64,
    pub _paths: [btree_path; BTREE_ITER_INITIAL],
    pub _updates: [btree_insert_entry; BTREE_ITER_INITIAL],
}

impl Default for btree_trans {
    fn default() -> Self {
        Self {
            c: core::ptr::null_mut(),
            paths_allocated: 0,
            paths: core::ptr::null_mut(),
            updates: core::ptr::null_mut(),
            mem: core::ptr::null_mut(),
            mem_top: 0,
            mem_bytes: 0,
            realloc_bytes_required: 0,
            nr_paths: BTREE_ITER_INITIAL as u16,
            nr_paths_max: 0,
            nr_sorted: 0,
            nr_updates: 0,
            restarted: 0,
            restart_count: 0,
            locked: false,
            write_locked: false,
            journal_replay_not_finished: false,
            has_interior_updates: false,
            hooks: core::ptr::null_mut(),
            journal_res: Default::default(),
            journal_entries: btree_trans_subbuf::default(),
            accounting: btree_trans_subbuf::default(),
            journal_u64s: 0,
            extra_journal_u64s: 0,
            extra_disk_res: 0,
            _paths: core::array::from_fn(|_| btree_path::default()),
            _updates: [btree_insert_entry::default(); BTREE_ITER_INITIAL],
        }
    }
}

#[repr(C)]
pub struct btree_iter {
    pub trans: *mut btree_trans,
    pub path: btree_path_idx_t,
    pub update_path: btree_path_idx_t,
    pub key_cache_path: btree_path_idx_t,
    pub btree_id: u8,
    pub min_depth: u8,
    pub flags: u16,
    pub snapshot: u32,
    pub pos: bpos,
    pub k: bkey,
    pub journal_idx: usize,
}

impl Default for btree_iter {
    fn default() -> Self {
        Self {
            trans: core::ptr::null_mut(),
            path: 0,
            update_path: 0,
            key_cache_path: 0,
            btree_id: 0,
            min_depth: 0,
            flags: 0,
            snapshot: 0,
            pos: Default::default(),
            k: Default::default(),
            journal_idx: 0,
        }
    }
}

pub unsafe fn bch2_trans_init(trans: *mut btree_trans, c: *mut bch_fs) {
    *trans = btree_trans::default();
    (*trans).c = c;
    (*trans).paths = (*trans)._paths.as_mut_ptr();
    (*trans).updates = (*trans)._updates.as_mut_ptr();
    (*trans).paths_allocated = 1;
    (*trans).locked = true;
}

pub unsafe fn bch2_trans_begin(trans: *mut btree_trans) -> u32 {
    if trans.is_null() {
        return 0;
    }
    super::update::bch2_trans_reset_updates(trans);
    (*trans).mem_top = 0;
    (*trans).realloc_bytes_required = 0;
    if (*trans).journal_replay_not_finished
        && (*trans).c != core::ptr::null_mut()
        && (*(*trans).c)
            .journal
            .flags
            .load(core::sync::atomic::Ordering::Acquire)
            & (1usize << crate::journal::JOURNAL_replay_done)
            != 0
    {
        (*trans).journal_replay_not_finished = false;
    }
    let was_restarted = (*trans).restarted != 0;
    (*trans).restart_count = (*trans).restart_count.wrapping_add(1);
    (*trans).nr_sorted = 0;
    (*trans).locked = false;
    (*trans).write_locked = false;
    for idx in 1..BTREE_ITER_INITIAL {
        if (*trans).paths_allocated & (1u64 << idx) == 0 {
            continue;
        }
        let path = (*trans).paths.add(idx);
        (*path).should_be_locked = false;
        if !was_restarted {
            (*path).preserve = false;
        }
        if (*path).ref_ == 0 && !(*path).preserve {
            btree_path_unlock(path);
            *path = btree_path::default();
            (*trans).paths_allocated &= !(1u64 << idx);
            continue;
        }
        (*path).preserve = false;
    }
    (*trans).restarted = 0;
    (*trans).restart_count
}

/*
 * Stack-owned counterpart of the local bch2_trans_put().  The C version
 * unlocks every path, releases outstanding update references and then frees
 * per-transaction allocation before returning the transaction object to its
 * pool.  This port keeps btree_trans on the caller's stack, so only the
 * latter pool-return step is omitted.
 */
pub(crate) unsafe fn bch2_trans_put(trans: *mut btree_trans) {
    if trans.is_null() {
        return;
    }

    bch2_trans_unlock(trans);
    if !(*trans).c.is_null() {
        crate::journal::bch2_journal_res_put(&(*(*trans).c).journal, &mut (*trans).journal_res);
    }
    super::update::bch2_trans_reset_updates(trans);

    for idx in 1..BTREE_ITER_INITIAL {
        while (*trans).paths_allocated & (1u64 << idx) != 0 {
            let path = (*trans).paths.add(idx);
            let intent = (*path).intent_ref != 0;
            bch2_path_put(trans, idx as btree_path_idx_t, intent);
        }
    }

    if !(*trans).mem.is_null() && (*trans).mem_bytes != 0 {
        if let Ok(layout) = std::alloc::Layout::from_size_align(
            (*trans).mem_bytes as usize,
            core::mem::align_of::<u64>(),
        ) {
            std::alloc::dealloc((*trans).mem, layout);
        }
    }
    (*trans).mem = core::ptr::null_mut();
    (*trans).mem_top = 0;
    (*trans).mem_bytes = 0;
    (*trans).realloc_bytes_required = 0;
}

/*
 * Port of trans_maybe_inject_restart() in the local fs/btree/iter.h.  The
 * source helper records the restart error in trans->restarted and returns its
 * negative value; bch2_trans_begin() is responsible for resetting the
 * transaction before the caller retraverses its iterator paths.
 */
pub(crate) unsafe fn bch2_trans_maybe_inject_restart(trans: *mut btree_trans) -> i32 {
    if trans.is_null() || (*trans).c.is_null() {
        return 0;
    }

    let restarts = &(*(*trans).c).fault_inject_transaction_restarts;
    if restarts
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
            count.checked_sub(1)
        })
        .is_ok()
    {
        /* BCH_ERR_transaction_restart_fault_inject in the port's error
         * convention is represented by the restart return value -4. */
        (*trans).restarted = 4;
        return -4;
    }

    0
}

fn path_lock_type(path: &btree_path, level: usize) -> u8 {
    if level < path.locks_want as usize {
        BTREE_NODE_INTENT_LOCKED
    } else {
        BTREE_NODE_READ_LOCKED
    }
}

pub unsafe fn btree_lock_want(path: *const btree_path, level: usize) -> u8 {
    if path.is_null() {
        return BTREE_NODE_UNLOCKED;
    }
    if level < (*path).level as usize {
        BTREE_NODE_UNLOCKED
    } else if level < (*path).locks_want as usize {
        BTREE_NODE_INTENT_LOCKED
    } else if level == (*path).level as usize {
        BTREE_NODE_READ_LOCKED
    } else {
        BTREE_NODE_UNLOCKED
    }
}

fn path_locked_type(path: &btree_path, level: usize) -> u8 {
    (path.nodes_locked >> (level * 2)) & 3
}

fn path_mark_locked(path: &mut btree_path, level: usize, lock_type: u8) {
    path.nodes_locked &= !(3 << (level * 2));
    path.nodes_locked |= lock_type << (level * 2);
}

pub unsafe fn bch2_btree_path_upgrade(
    trans: *mut btree_trans,
    path: *mut btree_path,
    new_locks_want: u8,
) -> i32 {
    if trans.is_null() || path.is_null() {
        return -22;
    }
    let new_locks_want = new_locks_want.min(BTREE_MAX_DEPTH);
    if (*path).locks_want >= new_locks_want && (*path).nodes_locked != 0 {
        return 0;
    }
    for level in (*path).locks_want as usize..new_locks_want as usize {
        let b = (*path).l[level].b;
        if b.is_null() {
            continue;
        }
        match path_locked_type(&*path, level) {
            BTREE_NODE_INTENT_LOCKED => {}
            BTREE_NODE_READ_LOCKED => {
                if !six_lock_tryupgrade(&(*b).c.lock) {
                    let mut reentrant = false;
                    for idx in 1..BTREE_ITER_INITIAL {
                        if (*trans).paths_allocated & (1u64 << idx) == 0 {
                            continue;
                        }
                        let other = (*trans).paths.add(idx);
                        if other == path
                            || (*other).l[level].b != b
                            || path_locked_type(&*other, level) < BTREE_NODE_INTENT_LOCKED
                        {
                            continue;
                        }
                        six_lock_increment(&(*b).c.lock, six_lock_type::SIX_LOCK_intent);
                        six_unlock_read(&(*b).c.lock);
                        reentrant = true;
                        break;
                    }
                    if !reentrant {
                        let readers = six_lock_counts(&(*b).c.lock).n[0];
                        if readers > 0 && readers < (*path).ref_ as u32 {
                            six_lock_increment(&(*b).c.lock, six_lock_type::SIX_LOCK_intent);
                            six_unlock_read(&(*b).c.lock);
                            reentrant = true;
                        } else if readers == (*path).ref_ as u32 && readers > 1 {
                            for _ in 1..readers {
                                six_unlock_read(&(*b).c.lock);
                            }
                            reentrant = six_lock_tryupgrade(&(*b).c.lock);
                        }
                    }
                    if !reentrant {
                        let mut other_read = false;
                        for idx in 1..BTREE_ITER_INITIAL {
                            if (*trans).paths_allocated & (1u64 << idx) == 0 {
                                continue;
                            }
                            let other = (*trans).paths.add(idx);
                            if other != path
                                && (*other).l[level].b == b
                                && path_locked_type(&*other, level) == BTREE_NODE_READ_LOCKED
                            {
                                other_read = true;
                                break;
                            }
                        }
                        if !other_read {
                            six_lock_increment(&(*b).c.lock, six_lock_type::SIX_LOCK_intent);
                            six_unlock_read(&(*b).c.lock);
                            reentrant = true;
                        }
                    }
                    if !reentrant {
                        return -1;
                    }
                }
                path_mark_locked(&mut *path, level, BTREE_NODE_INTENT_LOCKED);
            }
            _ => return -1,
        }
    }
    (*path).locks_want = new_locks_want;
    0
}

pub unsafe fn bch2_btree_path_upgrade_norestart(
    trans: *mut btree_trans,
    path: *mut btree_path,
    new_locks_want: u8,
) -> bool {
    if trans.is_null() || path.is_null() {
        return false;
    }
    let new_locks_want = new_locks_want.min(BTREE_MAX_DEPTH);
    if new_locks_want <= (*path).locks_want {
        true
    } else {
        bch2_btree_path_upgrade(trans, path, new_locks_want) == 0
    }
}

unsafe fn btree_path_unlock(path: *mut btree_path) {
    for level in 0..BTREE_MAX_DEPTH as usize {
        let b = (*path).l[level].b;
        if b.is_null() {
            continue;
        }
        match path_locked_type(&*path, level) {
            BTREE_NODE_READ_LOCKED => six_unlock_read(&(*b).c.lock),
            BTREE_NODE_INTENT_LOCKED => six_unlock_intent(&(*b).c.lock),
            /* A SIX write lock is held on top of its intent lock.  Keep
             * this paired with btree_node_unlock(), as in
             * fs/btree/locking.h's btree_node_unlock(). */
            BTREE_NODE_WRITE_LOCKED => {
                six_unlock_write(&(*b).c.lock);
                six_unlock_intent(&(*b).c.lock);
            }
            _ => {}
        }
        (*path).l[level].b = core::ptr::null_mut();
    }
    (*path).nodes_locked = 0;
}

pub(crate) unsafe fn btree_node_lock(
    trans: *mut btree_trans,
    path: *mut btree_path,
    b: *mut btree,
    level: usize,
) -> i32 {
    let lock_type = path_lock_type(&*path, level);
    btree_node_lock_type(trans, path, b, level, lock_type)
}

pub(crate) unsafe fn btree_node_lock_type(
    trans: *mut btree_trans,
    path: *mut btree_path,
    b: *mut btree,
    level: usize,
    lock_type: u8,
) -> i32 {
    let mut incremented = false;
    for idx in 1..BTREE_ITER_INITIAL {
        if (*trans).paths_allocated & (1u64 << idx) == 0 {
            continue;
        }
        let other = (*trans).paths.add(idx);
        if other != path
            && (*other).l[level].b == b
            && path_locked_type(&*other, level) >= lock_type
        {
            let six_type = match lock_type {
                BTREE_NODE_WRITE_LOCKED => six_lock_type::SIX_LOCK_write,
                BTREE_NODE_INTENT_LOCKED => six_lock_type::SIX_LOCK_intent,
                _ => six_lock_type::SIX_LOCK_read,
            };
            six_lock_increment(&(*b).c.lock, six_type);
            incremented = true;
            break;
        }
    }
    let ret = if incremented {
        0
    } else {
        match lock_type {
            BTREE_NODE_WRITE_LOCKED => six_lock_write(&(*b).c.lock),
            BTREE_NODE_INTENT_LOCKED => six_lock_intent(&(*b).c.lock),
            _ => six_lock_read(&(*b).c.lock),
        }
    };
    if ret == 0 {
        path_mark_locked(&mut *path, level, lock_type);
        (*path).l[level].lock_seq = six_lock_seq(&(*b).c.lock);
        (*path).l[level].b = b;
    }
    ret
}

pub unsafe fn bch2_btree_node_upgrade(
    trans: *mut btree_trans,
    path: *mut btree_path,
    level: usize,
) -> bool {
    if trans.is_null()
        || path.is_null()
        || level >= BTREE_MAX_DEPTH as usize
        || (*path).l[level].b.is_null()
    {
        return false;
    }
    let b = (*path).l[level].b;
    match btree_lock_want(path, level) {
        BTREE_NODE_UNLOCKED => true,
        BTREE_NODE_READ_LOCKED => bch2_btree_node_relock(trans, path, level),
        BTREE_NODE_INTENT_LOCKED => {
            if path_locked_type(&*path, level) == BTREE_NODE_INTENT_LOCKED {
                true
            } else {
                let acquired = if btree_node_locked(path, level) {
                    six_lock_tryupgrade(&(*b).c.lock)
                } else {
                    six_relock_type(
                        &(*b).c.lock,
                        six_lock_type::SIX_LOCK_intent,
                        (*path).l[level].lock_seq,
                    )
                };
                if acquired
                    || (btree_node_lock_seq_matches(path, b, level)
                        && btree_node_lock_increment(
                            trans,
                            &mut (*b).c,
                            level,
                            BTREE_NODE_INTENT_LOCKED,
                        ))
                {
                    if !acquired {
                        btree_node_unlock(path, level);
                    }
                    path_mark_locked(&mut *path, level, BTREE_NODE_INTENT_LOCKED);
                    true
                } else {
                    false
                }
            }
        }
        BTREE_NODE_WRITE_LOCKED => false,
        _ => false,
    }
}

pub(crate) unsafe fn btree_node_unlock(path: *mut btree_path, level: usize) {
    if path.is_null() || level >= BTREE_MAX_DEPTH as usize {
        return;
    }
    let b = (*path).l[level].b;
    let lock_type = path_locked_type(&*path, level);
    match lock_type {
        BTREE_NODE_READ_LOCKED => six_unlock_read(&(*b).c.lock),
        BTREE_NODE_INTENT_LOCKED => six_unlock_intent(&(*b).c.lock),
        BTREE_NODE_WRITE_LOCKED => {
            six_unlock_write(&(*b).c.lock);
            if (*b)
                .c
                .lock
                .write_lock_recurse
                .load(core::sync::atomic::Ordering::Relaxed)
                == 0
            {
                (*path).l[level].lock_seq = six_lock_seq(&(*b).c.lock);
            }
            six_unlock_intent(&(*b).c.lock);
        }
        _ => return,
    }
    path_mark_locked(&mut *path, level, BTREE_NODE_UNLOCKED);
}

pub unsafe fn bch2_btree_path_downgrade(
    trans: *mut btree_trans,
    path: *mut btree_path,
    new_locks_want: u8,
) {
    if trans.is_null() || path.is_null() || (*trans).restarted != 0 {
        return;
    }
    if new_locks_want >= (*path).locks_want {
        return;
    }
    (*path).locks_want = new_locks_want;
    loop {
        let Some(level) = btree_path_highest_level_locked(path) else {
            break;
        };
        if level < new_locks_want as usize {
            break;
        }
        if level > (*path).level as usize {
            btree_node_unlock(path, level);
        } else if path_locked_type(&*path, level) == BTREE_NODE_INTENT_LOCKED {
            let b = (*path).l[level].b;
            six_lock_downgrade(&(*b).c.lock);
            path_mark_locked(&mut *path, level, BTREE_NODE_READ_LOCKED);
            break;
        } else {
            break;
        }
    }
}

pub unsafe fn bch2_path_get(
    trans: *mut btree_trans,
    btree_id: u8,
    pos: *const bpos,
    mut locks_want: u8,
    level: u8,
    flags: u16,
) -> btree_path_idx_t {
    if flags & BTREE_ITER_intent != 0 {
        locks_want = locks_want.max(level + 1);
    }
    locks_want = locks_want.min(BTREE_MAX_DEPTH);

    let free = (!(*trans).paths_allocated).trailing_zeros() as usize;
    assert!(free < BTREE_ITER_INITIAL);
    (*trans).paths_allocated |= 1u64 << free;
    let path = (*trans).paths.add(free);
    *path = btree_path {
        ref_: 1,
        intent_ref: (flags & BTREE_ITER_intent != 0) as u8,
        pos: *pos,
        btree_id,
        cached: flags & BTREE_ITER_cached != 0,
        preserve: flags & BTREE_ITER_nopreserve == 0,
        level,
        locks_want,
        ..Default::default()
    };
    (*trans).nr_paths_max = (*trans).nr_paths_max.max(free as u16);
    free as u16
}

pub unsafe fn bch2_path_get_unlocked_mut(
    trans: *mut btree_trans,
    btree_id: u8,
    level: u8,
    pos: bpos,
    cached: bool,
) -> btree_path_idx_t {
    let flags =
        BTREE_ITER_nopreserve | BTREE_ITER_intent | if cached { BTREE_ITER_cached } else { 0 };
    let path_idx = bch2_path_get(trans, btree_id, &pos, level + 1, level, flags);
    let path_idx = bch2_btree_path_make_mut(trans, path_idx, true, 0);
    let path = (*trans).paths.add(path_idx as usize);
    let new_locks_want = (*path).level + u8::from((*path).intent_ref != 0);
    bch2_btree_path_downgrade(trans, path, new_locks_want);
    btree_path_unlock(path);
    path_idx
}

/*
 * Port of fs/btree/interior.c's btree_path_take_new_node().  A consumed
 * preallocated node arrives with the update owner's primary intent/write
 * references; the temporary transaction path takes a matching recursive
 * write reference so every construction and publication mutation remains
 * represented in the iterator path graph.
 */
pub(crate) unsafe fn btree_path_take_new_node(
    trans: *mut btree_trans,
    path: *mut btree_path,
    b: *mut btree,
) {
    assert!(!trans.is_null());
    assert!(!path.is_null());
    assert!(!b.is_null());

    let level = (*b).c.level;
    assert!((level as usize) < BTREE_MAX_DEPTH as usize);
    six_lock_increment(&(*b).c.lock, six_lock_type::SIX_LOCK_write);
    path_mark_locked(&mut *path, level as usize, BTREE_NODE_WRITE_LOCKED);
    bch2_btree_path_level_init(trans, path, level, b);
}

pub unsafe fn bch2_btree_path_level_init(
    trans: *mut btree_trans,
    path: *mut btree_path,
    level: u8,
    b: *mut btree,
) {
    assert!(!trans.is_null());
    assert!(!path.is_null());
    assert!(!b.is_null());
    assert!((level as usize) < BTREE_MAX_DEPTH as usize);
    (*path).l[level as usize].lock_seq = six_lock_seq(&(*b).c.lock);
    (*path).l[level as usize].b = b;
    bch2_btree_node_iter_init(
        (*trans).c,
        b,
        &mut (*path).l[level as usize].iter,
        &(*path).pos,
    );
    if level != 0 {
        bch2_btree_node_iter_peek(&mut (*path).l[level as usize].iter, b);
    }
}

pub unsafe fn bch2_trans_revalidate_updates_in_node(trans: *mut btree_trans, b: *mut btree) {
    if trans.is_null() || b.is_null() {
        return;
    }
    for idx in 0..(*trans).nr_updates as usize {
        let update = &mut *(*trans).updates.add(idx);
        if update.cached
            || update.key_cache_flushing
            || update.level != (*b).c.level
            || update.btree_id != (*b).c.btree_id
            || bpos_cmp((*update.k).k.p, (*(*b).data).min_key) < 0
            || bpos_cmp((*update.k).k.p, (*(*b).data).max_key) > 0
        {
            continue;
        }
        let path = (*trans).paths.add(update.path as usize);
        update.old_v = bch2_btree_path_peek_slot(path, &mut update.old_k).v;
        if (*trans).journal_replay_not_finished {
            let journal_k = crate::journal::bch2_journal_keys_peek_slot(
                (*trans).c,
                update.btree_id,
                update.level,
                (*update.k).k.p,
            );
            if !journal_k.is_null() {
                update.old_k = (*journal_k).k;
                update.old_v = &(*journal_k).v;
            }
        }
    }
}

pub unsafe fn bch2_trans_node_add(trans: *mut btree_trans, b: *mut btree) {
    if trans.is_null() || b.is_null() {
        return;
    }
    let level = (*b).c.level as usize;
    for idx in 1..BTREE_ITER_INITIAL {
        if (*trans).paths_allocated & (1u64 << idx) == 0 {
            continue;
        }
        let path = (*trans).paths.add(idx);
        if (*path).cached || level < (*path).level as usize || !btree_path_pos_in_node(path, b) {
            continue;
        }
        let lock_type = if (*path).nodes_locked != 0 {
            btree_lock_want(path, level)
        } else {
            BTREE_NODE_UNLOCKED
        };
        btree_node_unlock(path, level);
        if lock_type != BTREE_NODE_UNLOCKED {
            let six_type = match lock_type {
                BTREE_NODE_WRITE_LOCKED => six_lock_type::SIX_LOCK_write,
                BTREE_NODE_INTENT_LOCKED => six_lock_type::SIX_LOCK_intent,
                BTREE_NODE_READ_LOCKED => six_lock_type::SIX_LOCK_read,
                _ => unreachable!(),
            };
            six_lock_increment(&(*b).c.lock, six_type);
            path_mark_locked(&mut *path, level, lock_type);
        }
        bch2_btree_path_level_init(trans, path, level as u8, b);
    }
    bch2_trans_revalidate_updates_in_node(trans, b);
}

pub unsafe fn bch2_trans_node_verify_not_in_iters(trans: *mut btree_trans, b: *mut btree) {
    if trans.is_null() || b.is_null() {
        return;
    }
    let level = (*b).c.level as usize;
    for idx in 1..BTREE_ITER_INITIAL {
        if (*trans).paths_allocated & (1u64 << idx) == 0 {
            continue;
        }
        let path = (*trans).paths.add(idx);
        if (*path).l[level].b == b && path_locked_type(&*path, level) != BTREE_NODE_UNLOCKED {
            panic!("btree node is still referenced by a locked transaction path");
        }
    }
}

pub unsafe fn bch2_btree_path_fix_key_modified(
    trans: *mut btree_trans,
    b: *mut btree,
    where_: *mut super::bkey::bkey_packed,
) {
    if trans.is_null() || b.is_null() || where_.is_null() {
        return;
    }
    let level = (*b).c.level as usize;
    if level >= BTREE_MAX_DEPTH as usize {
        return;
    }
    for idx in 1..BTREE_ITER_INITIAL {
        if (*trans).paths_allocated & (1u64 << idx) == 0 {
            continue;
        }
        let path = (*trans).paths.add(idx);
        if (*path).l[level].b != b || (*path).l[level].lock_seq != six_lock_seq(&(*b).c.lock) {
            continue;
        }
        let peeked = bch2_btree_node_iter_peek_all(&mut (*path).l[level].iter, b);
        if peeked == where_ && bkey_iter_pos_cmp(b, where_, &(*path).pos) < 0 {
            bch2_btree_node_iter_advance(&mut (*path).l[level].iter, b);
        } else {
            bch2_btree_node_iter_sort(&mut (*path).l[level].iter, b);
        }
    }
}

pub unsafe fn bch2_btree_path_make_mut(
    trans: *mut btree_trans,
    path_idx: btree_path_idx_t,
    intent: bool,
    _ip: usize,
) -> btree_path_idx_t {
    assert!(!trans.is_null());
    assert!((*trans).paths_allocated & (1u64 << path_idx) != 0);
    let old = (*trans).paths.add(path_idx as usize);
    assert!((*old).ref_ != 0);

    if (*old).ref_ <= 1 && !(*old).preserve {
        (*old).should_be_locked = false;
        return path_idx;
    }

    let free = btree_path_clone(trans, path_idx, intent, _ip);
    let new = (*trans).paths.add(free as usize);
    (*new).preserve = false;

    if intent {
        assert!((*old).intent_ref != 0);
        (*old).intent_ref -= 1;
    }
    (*old).ref_ -= 1;
    if (*old).ref_ == 0 {
        btree_path_unlock(old);
        *old = btree_path::default();
        (*trans).paths_allocated &= !(1u64 << path_idx);
    }
    free
}

unsafe fn btree_path_clone(
    trans: *mut btree_trans,
    src: btree_path_idx_t,
    intent: bool,
    _ip: usize,
) -> btree_path_idx_t {
    assert!(!trans.is_null());
    assert!((*trans).paths_allocated & (1u64 << src) != 0);
    let free = (!(*trans).paths_allocated).trailing_zeros() as usize;
    assert!(free < BTREE_ITER_INITIAL);
    let old = (*trans).paths.add(src as usize);
    let new = (*trans).paths.add(free);
    *new = *old;
    (*new).ref_ = 1;
    (*new).intent_ref = u8::from(intent);
    (*new).should_be_locked = false;
    (*new).sorted_idx = 0;
    (*trans).paths_allocated |= 1u64 << free;
    (*trans).nr_paths_max = (*trans).nr_paths_max.max(free as u16);

    for level in 0..BTREE_MAX_DEPTH as usize {
        let b = (*new).l[level].b;
        if b.is_null() {
            continue;
        }
        let lock_type = path_locked_type(&*new, level);
        let six_type = match lock_type {
            BTREE_NODE_WRITE_LOCKED => six_lock_type::SIX_LOCK_write,
            BTREE_NODE_INTENT_LOCKED => six_lock_type::SIX_LOCK_intent,
            BTREE_NODE_READ_LOCKED => six_lock_type::SIX_LOCK_read,
            _ => continue,
        };
        six_lock_increment(&(*b).c.lock, six_type);
    }
    free as u16
}

pub unsafe fn bch2_btree_path_set_pos(
    trans: *mut btree_trans,
    path_idx: btree_path_idx_t,
    new_pos: *const bpos,
    _intent: bool,
    _ip: usize,
) -> btree_path_idx_t {
    assert!(!trans.is_null());
    assert!(!new_pos.is_null());
    assert!((*trans).paths_allocated & (1u64 << path_idx) != 0);
    if bpos_eq((*(*trans).paths.add(path_idx as usize)).pos, *new_pos) {
        return path_idx;
    }
    let path_idx = bch2_btree_path_make_mut(trans, path_idx, _intent, _ip);
    let path = (*trans).paths.add(path_idx as usize);
    let old_pos = (*path).pos;
    (*path).pos = *new_pos;
    if (*path).cached {
        btree_node_unlock(path, 0);
        (*path).l[0].b = core::ptr::null_mut();
        (*path).level = 0;
        (*path).should_be_locked = false;
        return path_idx;
    }
    if ((*path).level as usize) < BTREE_MAX_DEPTH as usize {
        let level = (*path).level as usize;
        let b = (*path).l[level].b;
        if !b.is_null()
            && btree_node_locked(path, level)
            && btree_node_lock_seq_matches(path, b, level)
            && btree_path_pos_in_node(path, b)
        {
            let cmp = bpos_cmp(*new_pos, old_pos);
            if cmp < 0 || !btree_path_advance_to_pos(path, level, 8) {
                bch2_btree_node_iter_init((*trans).c, b, &mut (*path).l[level].iter, &(*path).pos);
            }
            if level != 0 {
                bch2_btree_node_iter_peek(&mut (*path).l[level].iter, b);
            }
            (*path).should_be_locked = false;
            return path_idx;
        }
    }
    btree_path_unlock(path);
    (*path).level = 0;
    (*path).should_be_locked = false;
    path_idx
}

pub unsafe fn bch2_btree_path_can_relock(_trans: *mut btree_trans, path: *mut btree_path) -> bool {
    if path.is_null() {
        return false;
    }
    let mut level = (*path).level as usize;
    loop {
        if level >= BTREE_MAX_DEPTH as usize {
            break;
        }
        let b = (*path).l[level].b;
        if b.is_null() {
            break;
        }
        if !btree_node_lock_seq_matches(path, b, level) {
            return false;
        }
        level += 1;
        if level >= (*path).locks_want as usize {
            break;
        }
    }
    true
}

pub unsafe fn bch2_path_put(trans: *mut btree_trans, path_idx: btree_path_idx_t, _intent: bool) {
    if trans.is_null()
        || path_idx == 0
        || path_idx as usize >= BTREE_ITER_INITIAL
        || (*trans).paths_allocated & (1u64 << path_idx) == 0
    {
        return;
    }
    let path = (*trans).paths.add(path_idx as usize);
    if _intent {
        assert!((*path).intent_ref != 0);
        (*path).intent_ref -= 1;
    }
    assert!((*path).ref_ != 0);
    (*path).ref_ -= 1;
    if (*path).ref_ == 0 {
        btree_path_unlock(path);
        *path = btree_path::default();
        (*trans).paths_allocated &= !(1u64 << path_idx);
    }
}

pub unsafe fn bch2_btree_path_peek_slot(path: *mut btree_path, u: *mut bkey) -> bkey_s_c {
    if path.is_null() || u.is_null() {
        return bkey_s_c::default();
    }
    let level = (*path).level as usize;
    if level >= BTREE_MAX_DEPTH as usize {
        return bkey_s_c::default();
    }
    let b = (*path).l[level].b;
    if b.is_null() {
        return bkey_s_c::default();
    }
    if (*path).cached {
        let cached = b.cast::<super::types::bkey_cached>();
        let cached_btree_id = core::ptr::addr_of!((*cached).key.btree_id).read_unaligned();
        let cached_pos = core::ptr::addr_of!((*cached).key.pos).read_unaligned();
        if (*cached).k.is_null()
            || (*path).btree_id as u32 != cached_btree_id
            || !bpos_eq((*path).pos, cached_pos)
        {
            return bkey_s_c::default();
        }
        *u = (*(*cached).k).k;
        return bkey_s_c {
            k: u,
            v: core::ptr::addr_of!((*(*cached).k).v),
        };
    }
    let packed = bch2_btree_node_iter_peek_all(&mut (*path).l[level].iter, b);
    if !packed.is_null() {
        if super::bkey::bkey_packed(&*packed) {
            super::bkey::__bch2_bkey_unpack_key(&(*b).format, &mut *u, &*packed);
        } else {
            *u = *(packed as *const bkey);
        }
        if bpos_eq((*u).p, (*path).pos) {
            let value = (packed as *const u64).add(bkeyp_key_u64s(&(*b).format, &*packed) as usize);
            return bkey_s_c {
                k: u,
                v: value.cast(),
            };
        }
    }
    *u = bkey::default();
    (*u).p = (*path).pos;
    bkey_s_c {
        k: u,
        v: core::ptr::null(),
    }
}

pub unsafe fn bch2_btree_path_peek_slot_exact(path: *mut btree_path, u: *mut bkey) -> bkey_s_c {
    if path.is_null() || u.is_null() {
        return bkey_s_c::default();
    }
    let k = bch2_btree_path_peek_slot(path, u);
    if super::bkey::bkey_err(k) == 0 && !k.k.is_null() && bpos_eq((*path).pos, (*k.k).p) {
        return k;
    }
    *u = bkey::default();
    (*u).p = (*path).pos;
    bkey_s_c {
        k: u,
        v: core::ptr::null(),
    }
}

unsafe fn btree_path_level_init(trans: *mut btree_trans, path: *mut btree_path, level: usize) {
    let l = &mut (*path).l[level];
    bch2_btree_node_iter_init((*trans).c, l.b, &mut l.iter, &(*path).pos);
}

unsafe fn unpack_btree_ptr(
    b: *const btree,
    src: *const bkey_packed,
    dst: *mut bkey_i_btree_ptr_v2,
) {
    if super::bkey::bkey_packed(&*src) {
        super::bkey::__bch2_bkey_unpack_key(&(*b).format, &mut (*dst).k, &*src);
    } else {
        (*dst).k = *(src as *const bkey);
    }
    let key_u64s = bkeyp_key_u64s(&(*b).format, &*src) as usize;
    let val_u64s = bkeyp_val_u64s(&(*b).format, &*src) as usize;
    assert!(val_u64s <= super::types::BKEY_BTREE_PTR_VAL_U64S_MAX);
    core::ptr::copy_nonoverlapping(
        (src as *const u64).add(key_u64s),
        core::ptr::addr_of_mut!((*dst).v).cast::<u64>(),
        val_u64s,
    );
}

#[inline]
unsafe fn btree_path_cached_set(
    path: *mut btree_path,
    cached: *mut super::types::bkey_cached,
    lock_held: u8,
) {
    (*path).l[0].lock_seq = six_lock_seq(&(*cached).c.lock);
    (*path).l[0].b = cached.cast();
    path_mark_locked(&mut *path, 0, lock_held);
}

unsafe fn btree_path_traverse_cached_fast(
    trans: *mut btree_trans,
    path_idx: btree_path_idx_t,
) -> i32 {
    let path = (*trans).paths.add(path_idx as usize);
    let cached = (*path).l[0].b.cast::<super::types::bkey_cached>();
    if cached.is_null() {
        return -2;
    }

    let lock_want = btree_lock_want(path, 0);
    let six_type = match lock_want {
        BTREE_NODE_READ_LOCKED => six_lock_type::SIX_LOCK_read,
        BTREE_NODE_INTENT_LOCKED => six_lock_type::SIX_LOCK_intent,
        BTREE_NODE_WRITE_LOCKED => six_lock_type::SIX_LOCK_write,
        _ => return 0,
    };
    let ret = match six_type {
        six_lock_type::SIX_LOCK_read => six_lock_read(&(*cached).c.lock),
        six_lock_type::SIX_LOCK_intent => six_lock_intent(&(*cached).c.lock),
        six_lock_type::SIX_LOCK_write => six_lock_write(&(*cached).c.lock),
    };
    if ret != 0 {
        return ret;
    }
    if (*cached).key.btree_id != (*path).btree_id as u32 || !bpos_eq((*cached).key.pos, (*path).pos)
    {
        match six_type {
            six_lock_type::SIX_LOCK_read => six_unlock_read(&(*cached).c.lock),
            six_lock_type::SIX_LOCK_intent => six_unlock_intent(&(*cached).c.lock),
            six_lock_type::SIX_LOCK_write => six_unlock_write(&(*cached).c.lock),
        }
        return -2;
    }
    btree_path_cached_set(path, cached, lock_want);
    0
}

pub unsafe fn bch2_btree_path_traverse_cached(
    trans: *mut btree_trans,
    path_idx: btree_path_idx_t,
    flags: u16,
) -> i32 {
    let path = (*trans).paths.add(path_idx as usize);
    let ret = btree_path_traverse_cached_fast(trans, path_idx);
    if ret == -2 && flags & BTREE_ITER_cached_nofill != 0 {
        (*path).l[0].b = core::ptr::null_mut();
        return 0;
    }
    if ret != 0 {
        btree_path_unlock(path);
    }
    ret
}

pub unsafe fn bch2_btree_path_traverse_one(
    trans: *mut btree_trans,
    path_idx: btree_path_idx_t,
    flags: u16,
) -> i32 {
    if trans.is_null()
        || path_idx == 0
        || path_idx as usize >= BTREE_ITER_INITIAL
        || (*trans).paths_allocated & (1u64 << path_idx) == 0
    {
        return -22;
    }
    if (*trans).restarted != 0 {
        return -((*trans).restarted as i32);
    }
    let path = (*trans).paths.add(path_idx as usize);
    let depth_want = (*path).level as usize;
    if (*path).cached {
        let ret = bch2_btree_path_traverse_cached(trans, path_idx, flags);
        if ret == 0 || flags & BTREE_ITER_cached_nofill != 0 {
            return ret;
        }
        (*path).cached = false;
    }
    btree_path_unlock(path);

    let root_packed = bch2_btree_id_root_packed((*trans).c, (*path).btree_id as usize);
    let root = bch2_btree_root_unpack_b(root_packed);
    if root.is_null() {
        return -1;
    }
    let mut level = bch2_btree_root_unpack_level(root_packed) as usize;
    if level < depth_want {
        (*path).level = depth_want as u8;
        return 0;
    }

    (*path).level = level as u8;
    let ret = btree_node_lock(trans, path, root, level);
    if ret != 0 {
        return ret;
    }
    btree_path_level_init(trans, path, level);

    while level > depth_want {
        let parent = (*path).l[level].b;
        let mut packed = bch2_btree_node_iter_peek(&mut (*path).l[level].iter, parent);
        if packed.is_null() && !(*trans).journal_replay_not_finished {
            btree_path_unlock(path);
            return -2;
        }
        let mut ptr_words =
            [0u64; super::bkey::BKEY_U64S as usize + super::types::BKEY_BTREE_PTR_VAL_U64S_MAX];
        let ptr = ptr_words.as_mut_ptr().cast::<bkey_i_btree_ptr_v2>();
        let mut from_journal = false;
        if (*trans).journal_replay_not_finished {
            let mut journal_pos = (*path).pos;
            loop {
                let mut journal_idx = 0;
                let journal_k = crate::journal::bch2_journal_keys_peek_max(
                    (*trans).c,
                    (*path).btree_id,
                    level as u8,
                    journal_pos,
                    (*(*parent).data).max_key,
                    &mut journal_idx,
                );
                if journal_k.is_null() {
                    break;
                }
                if (*journal_k).k.type_ == super::bset::KEY_TYPE_deleted {
                    if !packed.is_null()
                        && bpos_eq(
                            super::node_iter::bkey_unpack_pos(parent, packed),
                            (*journal_k).k.p,
                        )
                    {
                        super::node_iter::bch2_btree_node_iter_advance(
                            &mut (*path).l[level].iter,
                            parent,
                        );
                        packed = bch2_btree_node_iter_peek(&mut (*path).l[level].iter, parent);
                    }
                    if bpos_eq((*journal_k).k.p, SPOS_MAX) {
                        break;
                    }
                    journal_pos = bpos_successor((*journal_k).k.p);
                    continue;
                }
                if (*journal_k).k.type_ == super::bset::KEY_TYPE_btree_ptr_v2
                    && (packed.is_null()
                        || bpos_cmp(
                            (*journal_k).k.p,
                            super::node_iter::bkey_unpack_pos(parent, packed),
                        ) <= 0)
                {
                    *ptr = *(journal_k.cast::<bkey_i_btree_ptr_v2>());
                    from_journal = true;
                }
                break;
            }
        }
        if !from_journal {
            if packed.is_null() {
                btree_path_unlock(path);
                return -2;
            }
            unpack_btree_ptr(parent, packed, ptr);
        }
        if bpos_cmp((*path).pos, (*ptr).k.p) > 0 || bpos_cmp((*path).pos, (*ptr).v.min_key) < 0 {
            btree_path_unlock(path);
            return -3;
        }
        let mut child = btree_node_mem_ptr(ptr.cast());
        if child.is_null() {
            child = super::io::bch2_btree_node_get_noiter_unlocked(
                trans,
                ptr.cast(),
                (*path).btree_id,
                level as u8 - 1,
                flags & BTREE_ITER_nofill != 0,
            );
            if child.is_null() {
                btree_path_unlock(path);
                return -4;
            }
            if !from_journal {
                let key_u64s = bkeyp_key_u64s(&(*parent).format, &*packed) as usize;
                *((packed as *mut u64).add(key_u64s)) = child as usize as u64;
            }
        }
        level -= 1;
        let ret = btree_node_lock(trans, path, child, level);
        if ret != 0 {
            btree_path_unlock(path);
            return ret;
        }
        (*path).level = level as u8;
        btree_path_level_init(trans, path, level);
    }
    0
}

pub unsafe fn bch2_btree_path_traverse(
    trans: *mut btree_trans,
    path_idx: btree_path_idx_t,
    flags: u16,
) -> i32 {
    if trans.is_null()
        || path_idx == 0
        || path_idx as usize >= BTREE_ITER_INITIAL
        || (*trans).paths_allocated & (1u64 << path_idx) == 0
    {
        return -22;
    }
    if (*(*trans).paths.add(path_idx as usize)).nodes_locked == 0 {
        bch2_btree_path_traverse_one(trans, path_idx, flags)
    } else {
        0
    }
}

pub unsafe fn bch2_trans_iter_init_common(
    trans: *mut btree_trans,
    iter: *mut btree_iter,
    btree_id: u8,
    pos: bpos,
    locks_want: u8,
    depth: u8,
    flags: u16,
) {
    let flags = if (*trans).journal_replay_not_finished {
        flags | BTREE_ITER_with_journal
    } else {
        flags
    };
    *iter = btree_iter {
        trans,
        btree_id,
        flags,
        snapshot: pos.snapshot,
        pos,
        k: {
            let mut k = bkey::default();
            bkey_init(&mut k);
            k.p = pos;
            k
        },
        ..Default::default()
    };
    (*iter).path = bch2_path_get(trans, btree_id, &(*iter).pos, locks_want, depth, flags);
}

pub unsafe fn bch2_trans_copy_iter(dst: *mut btree_iter, src: *mut btree_iter) {
    if dst.is_null() || src.is_null() {
        return;
    }
    core::ptr::copy_nonoverlapping(src, dst, 1);
    let trans = (*src).trans;
    if trans.is_null() {
        return;
    }
    let intent = (*src).flags & BTREE_ITER_intent != 0;
    if (*src).path != 0 {
        let path = (*trans).paths.add((*src).path as usize);
        (*path).ref_ += 1;
        if intent {
            (*path).intent_ref += 1;
        }
    }
    if (*src).update_path != 0 {
        let path = (*trans).paths.add((*src).update_path as usize);
        (*path).ref_ += 1;
        if intent {
            (*path).intent_ref += 1;
        }
    }
    (*dst).key_cache_path = 0;
}

pub unsafe fn bch2_trans_iter_init(
    trans: *mut btree_trans,
    iter: *mut btree_iter,
    btree_id: u8,
    pos: bpos,
    flags: u16,
) {
    bch2_trans_iter_exit(iter);
    bch2_trans_iter_init_common(trans, iter, btree_id, pos, 0, 0, flags);
}

pub unsafe fn bch2_trans_iter_init_outlined(
    trans: *mut btree_trans,
    iter: *mut btree_iter,
    btree_id: u8,
    pos: bpos,
    flags: u16,
) {
    bch2_trans_iter_exit(iter);
    bch2_trans_iter_init_common(trans, iter, btree_id, pos, 0, 0, flags);
}

pub unsafe fn bch2_trans_node_iter_init(
    trans: *mut btree_trans,
    iter: *mut btree_iter,
    btree_id: u8,
    pos: bpos,
    locks_want: u8,
    depth: u8,
    flags: u16,
) {
    bch2_trans_iter_exit(iter);
    let flags =
        flags | BTREE_ITER_not_extents | BTREE_ITER_snapshot_field | BTREE_ITER_all_snapshots;
    bch2_trans_iter_init_common(trans, iter, btree_id, pos, locks_want, depth, flags);
    (*iter).min_depth = depth;
}

pub unsafe fn bch2_trans_iter_exit(iter: *mut btree_iter) {
    if iter.is_null() {
        return;
    }
    let trans = (*iter).trans;
    if trans.is_null() {
        return;
    }
    if (*iter).update_path != 0 {
        bch2_path_put(
            trans,
            (*iter).update_path,
            (*iter).flags & BTREE_ITER_intent != 0,
        );
    }
    if (*iter).path != 0 {
        bch2_path_put(trans, (*iter).path, (*iter).flags & BTREE_ITER_intent != 0);
    }
    if (*iter).key_cache_path != 0 {
        bch2_path_put(
            trans,
            (*iter).key_cache_path,
            (*iter).flags & BTREE_ITER_intent != 0,
        );
    }
    (*iter).path = 0;
    (*iter).update_path = 0;
    (*iter).key_cache_path = 0;
    (*iter).trans = core::ptr::null_mut();
}

pub unsafe fn bch2_btree_iter_set_pos(iter: *mut btree_iter, mut new_pos: bpos) {
    if iter.is_null() {
        return;
    }
    if (*iter).flags & BTREE_ITER_all_snapshots == 0 {
        new_pos.snapshot = (*iter).snapshot;
    }
    if (*iter).update_path != 0 {
        bch2_path_put(
            (*iter).trans,
            (*iter).update_path,
            (*iter).flags & BTREE_ITER_intent != 0,
        );
        (*iter).update_path = 0;
    }
    (*iter).pos = new_pos;
    bkey_init(&mut (*iter).k);
    (*iter).k.p = new_pos;
}

pub unsafe fn bch2_btree_iter_set_snapshot(iter: *mut btree_iter, snapshot: u32) {
    if iter.is_null() {
        return;
    }
    (*iter).snapshot = snapshot;
    let mut pos = (*iter).pos;
    pos.snapshot = snapshot;
    bch2_btree_iter_set_pos(iter, pos);
}

pub unsafe fn bch2_btree_iter_set_pos_to_extent_start(iter: *mut btree_iter) {
    if iter.is_null() || (*iter).flags & BTREE_ITER_is_extents == 0 {
        return;
    }
    (*iter).pos = super::bkey::bkey_start_pos(&(*iter).k);
}

pub unsafe fn bch2_set_btree_iter_dontneed(iter: *mut btree_iter) {
    if iter.is_null() || (*iter).trans.is_null() || (*iter).path == 0 {
        return;
    }
    let trans = (*iter).trans;
    if (*trans).restarted != 0 {
        return;
    }
    let path = (*trans).paths.add((*iter).path as usize);
    (*path).preserve = false;
    if (*path).ref_ == 1 {
        (*path).should_be_locked = false;
    }
}

pub unsafe fn bch2_btree_iter_traverse(iter: *mut btree_iter) -> i32 {
    if iter.is_null() || (*iter).trans.is_null() || (*iter).path == 0 {
        return -22;
    }
    let trans = (*iter).trans;
    let mut search_key = (*iter).pos;
    if (*iter).flags & BTREE_ITER_is_extents != 0 && !bkey_eq(search_key, POS_MAX) {
        search_key = if (*iter).flags & BTREE_ITER_all_snapshots != 0 {
            super::bkey::bpos_successor(search_key)
        } else {
            bpos_with_snapshot(bpos_nosnap_successor(search_key), (*iter).snapshot)
        };
    }
    (*iter).path = bch2_btree_path_set_pos(
        trans,
        (*iter).path,
        &search_key,
        (*iter).flags & BTREE_ITER_intent != 0,
        0,
    );
    let ret = bch2_btree_path_traverse(trans, (*iter).path, (*iter).flags);
    if ret == 0 {
        btree_path_set_should_be_locked(trans, (*trans).paths.add((*iter).path as usize));
    }
    ret
}

pub unsafe fn bch2_btree_node_get_iter(
    trans: *mut btree_trans,
    iter: *mut btree_iter,
    b: *mut btree,
) -> i32 {
    if trans.is_null() || iter.is_null() || b.is_null() {
        return -22;
    }
    bch2_trans_node_iter_init(
        trans,
        iter,
        (*b).c.btree_id,
        (*b).key.k.p,
        BTREE_MAX_DEPTH as u8,
        (*b).c.level,
        BTREE_ITER_intent,
    );
    let ret = bch2_btree_iter_traverse(iter);
    if ret != 0 {
        return ret;
    }
    let path = (*trans).paths.add((*iter).path as usize);
    if (*path).l[(*b).c.level as usize].b != b {
        return -2;
    }
    if !super::cache::btree_node_hashed(b) {
        return -2;
    }
    0
}

pub unsafe fn bch2_btree_iter_peek_type(iter: *mut btree_iter, flags: u16) -> bkey_s_c {
    if flags & BTREE_ITER_slots != 0 {
        bch2_btree_iter_peek_slot(iter)
    } else {
        bch2_btree_iter_peek(iter)
    }
}

pub unsafe fn bch2_btree_iter_peek_prev_type(iter: *mut btree_iter, flags: u16) -> bkey_s_c {
    if flags & BTREE_ITER_slots != 0 {
        bch2_btree_iter_peek_slot(iter)
    } else {
        bch2_btree_iter_peek_prev(iter)
    }
}

pub unsafe fn bch2_btree_iter_peek_max_type(
    iter: *mut btree_iter,
    end: *const bpos,
    flags: u16,
) -> bkey_s_c {
    if end.is_null() {
        return bkey_s_c::default();
    }
    if flags & BTREE_ITER_slots == 0 {
        bch2_btree_iter_peek_max(iter, end)
    } else if bpos_cmp((*iter).pos, *end) > 0 {
        bkey_s_c::default()
    } else {
        bch2_btree_iter_peek_slot(iter)
    }
}

pub unsafe fn bch2_btree_iter_peek_and_restart_outlined(iter: *mut btree_iter) -> bkey_s_c {
    bch2_btree_iter_peek_type(iter, (*iter).flags)
}

pub unsafe fn bch2_trans_has_updates(trans: *const btree_trans) -> bool {
    !trans.is_null()
        && ((*trans).nr_updates != 0
            || (*trans).journal_entries.u64s != 0
            || (*trans).accounting.u64s != 0)
}

unsafe fn btree_iter_set_pos(iter: *mut btree_iter, pos: bpos) {
    let trans = (*iter).trans;
    if (*iter).update_path != 0 {
        bch2_path_put(
            trans,
            (*iter).update_path,
            (*iter).flags & BTREE_ITER_intent != 0,
        );
        (*iter).update_path = 0;
    }
    let mut new_pos = pos;
    if (*iter).flags & BTREE_ITER_all_snapshots == 0 {
        new_pos.snapshot = (*iter).snapshot;
    }
    (*iter).k.type_ = super::bset::KEY_TYPE_deleted;
    (*iter).k.p = new_pos;
    (*iter).k.size = 0;
    (*iter).pos = new_pos;
    let path = (*trans).paths.add((*iter).path as usize);
    btree_path_unlock(path);
    (*path).pos = new_pos;
    (*path).level = 0;
}

pub unsafe fn bch2_btree_iter_peek_max(iter: *mut btree_iter, end: *const bpos) -> bkey_s_c {
    if (*iter).flags & BTREE_ITER_filter_snapshots != 0 {
        let trans = (*iter).trans;
        let saved_flags = (*iter).flags;
        (*iter).flags &= !BTREE_ITER_filter_snapshots;
        loop {
            let ret = bch2_btree_iter_peek_max(iter, end);
            if super::bkey::bkey_err(ret) != 0 || ret.k.is_null() {
                (*iter).flags = saved_flags;
                return ret;
            }
            let out_of_range = if saved_flags & BTREE_ITER_is_extents != 0 {
                (*ret.k).p.inode > (*end).inode
            } else {
                bkey_lt(*end, (*ret.k).p)
            };
            if out_of_range {
                btree_iter_set_pos(iter, *end);
                (*iter).flags = saved_flags;
                return bkey_s_c::default();
            }
            if (*ret.k).p.snapshot < (*iter).snapshot {
                (*iter).pos = bpos_with_snapshot((*ret.k).p, (*iter).snapshot);
                continue;
            }
            if (*iter).update_path != 0 {
                let update_path = (*trans).paths.add((*iter).update_path as usize);
                if !bkey_eq((*update_path).pos, (*ret.k).p) {
                    bch2_path_put(
                        trans,
                        (*iter).update_path,
                        saved_flags & BTREE_ITER_intent != 0,
                    );
                    (*iter).update_path = 0;
                }
            }
            if saved_flags & BTREE_ITER_intent != 0
                && saved_flags & BTREE_ITER_is_extents == 0
                && (*iter).update_path == 0
            {
                (*iter).update_path =
                    btree_path_clone(trans, (*iter).path, saved_flags & BTREE_ITER_intent != 0, 0);
                let with_snapshot = bpos_with_snapshot((*ret.k).p, (*iter).snapshot);
                (*iter).update_path = bch2_btree_path_set_pos(
                    trans,
                    (*iter).update_path,
                    &with_snapshot,
                    saved_flags & BTREE_ITER_intent != 0,
                    0,
                );
                let ret = bch2_btree_path_traverse(trans, (*iter).update_path, saved_flags);
                if ret != 0 {
                    (*iter).flags = saved_flags;
                    return super::bkey::bkey_s_c_err(ret);
                }
            }
            if (*trans).c.is_null()
                || !crate::snapshot::bch2_snapshot_is_ancestor(
                    &*(*trans).c,
                    (*iter).snapshot,
                    (*ret.k).p.snapshot,
                )
            {
                (*iter).pos = bpos_successor((*ret.k).p);
                continue;
            }
            let is_whiteout = (*ret.k).type_ == super::bset::KEY_TYPE_deleted
                || (*ret.k).type_ == super::bset::KEY_TYPE_whiteout
                || (*ret.k).type_ == super::bset::KEY_TYPE_extent_whiteout;
            if saved_flags & BTREE_ITER_nofilter_whiteouts == 0 && is_whiteout {
                if (*ret.k).type_ == super::bset::KEY_TYPE_extent_whiteout
                    && bkey_ge((*ret.k).p, *end)
                {
                    btree_iter_set_pos(iter, *end);
                    (*iter).flags = saved_flags;
                    return bkey_s_c::default();
                }
                if (*ret.k).type_ == super::bset::KEY_TYPE_extent_whiteout {
                    (*iter).pos = bpos_with_snapshot((*ret.k).p, (*iter).snapshot);
                } else {
                    (*iter).pos = if saved_flags & BTREE_ITER_all_snapshots != 0 {
                        bpos_successor((*ret.k).p)
                    } else {
                        bpos_with_snapshot(bpos_nosnap_successor((*ret.k).p), (*iter).snapshot)
                    };
                }
                continue;
            }
            (*iter).flags = saved_flags;
            return ret;
        }
    }
    let trans = (*iter).trans;
    if (*iter).update_path != 0 {
        bch2_path_put(
            trans,
            (*iter).update_path,
            (*iter).flags & BTREE_ITER_intent != 0,
        );
        (*iter).update_path = 0;
    }
    loop {
        let mut search_key = (*iter).pos;
        if (*iter).flags & BTREE_ITER_is_extents != 0 && !bkey_eq(search_key, POS_MAX) {
            search_key = if (*iter).flags & BTREE_ITER_all_snapshots != 0 {
                super::bkey::bpos_successor(search_key)
            } else {
                bpos_with_snapshot(bpos_nosnap_successor(search_key), (*iter).snapshot)
            };
        }
        (*iter).path = bch2_btree_path_set_pos(
            trans,
            (*iter).path,
            &search_key,
            (*iter).flags & BTREE_ITER_intent != 0,
            0,
        );
        let path = (*trans).paths.add((*iter).path as usize);
        (*path).level = 0;
        let ret = bch2_btree_path_traverse_one(trans, (*iter).path, (*iter).flags);
        if ret != 0 {
            bch2_btree_iter_set_pos(iter, (*iter).pos);
            return super::bkey::bkey_s_c_err(ret);
        }
        (*path).should_be_locked = true;
        let leaf = (*path).l[0].b;
        if leaf.is_null() {
            return bkey_s_c::default();
        }
        let packed = bch2_btree_node_iter_peek_all(&mut (*path).l[0].iter, leaf);
        if packed.is_null() {
            let mut update = bkey_s_c::default();
            let leaf_end = if bpos_cmp((*(*leaf).data).max_key, *end) < 0 {
                (*(*leaf).data).max_key
            } else {
                *end
            };
            if (*trans).journal_replay_not_finished {
                let journal_k = crate::journal::bch2_journal_keys_peek_max(
                    (*trans).c,
                    (*iter).btree_id,
                    (*iter).min_depth,
                    search_key,
                    leaf_end,
                    &mut (*iter).journal_idx,
                );
                if !journal_k.is_null() {
                    if (*journal_k).k.type_ == super::bset::KEY_TYPE_deleted {
                        if bpos_eq((*journal_k).k.p, SPOS_MAX) {
                            return bkey_s_c::default();
                        }
                        btree_iter_set_pos(iter, bpos_successor((*journal_k).k.p));
                        continue;
                    }
                    (*iter).k = (*journal_k).k;
                    (*iter).pos = (*iter).k.p;
                    return bkey_s_c {
                        k: &(*iter).k,
                        v: &(*journal_k).v,
                    };
                }
            }
            bch2_btree_trans_peek_updates(trans, iter, &mut update, leaf_end);
            if !update.k.is_null() {
                (*iter).k = *update.k;
                (*iter).pos = (*iter).k.p;
                return bkey_s_c {
                    k: &(*iter).k,
                    v: update.v,
                };
            }
            let max = (*(*leaf).data).max_key;
            if bpos_eq(max, SPOS_MAX) {
                return bkey_s_c::default();
            }
            btree_iter_set_pos(iter, bpos_nosnap_successor(max));
            continue;
        }

        if super::bkey::bkey_packed(&*packed) {
            super::bkey::__bch2_bkey_unpack_key(&(*leaf).format, &mut (*iter).k, &*packed);
        } else {
            (*iter).k = *(packed as *const bkey);
        }
        let next_pos = if (*iter).flags & BTREE_ITER_is_extents != 0 {
            bpos_max((*iter).pos, bkey_start_pos(&(*iter).k))
        } else {
            (*iter).k.p
        };
        if ((*iter).flags & BTREE_ITER_all_snapshots != 0 && bpos_cmp(next_pos, *end) > 0)
            || ((*iter).flags & BTREE_ITER_all_snapshots == 0
                && ((*iter).flags & BTREE_ITER_is_extents != 0 && !bkey_lt(next_pos, *end)
                    || (*iter).flags & BTREE_ITER_is_extents == 0 && bkey_lt(*end, next_pos)))
        {
            btree_iter_set_pos(iter, *end);
            return bkey_s_c::default();
        }
        (*iter).pos = next_pos;
        if (*iter).flags & BTREE_ITER_all_snapshots == 0 {
            (*iter).pos.snapshot = (*iter).snapshot;
        }
        let value = (packed as *const u64).add(bkeyp_key_u64s(&(*leaf).format, &*packed) as usize);
        let mut ret = bkey_s_c {
            k: &(*iter).k,
            v: value.cast(),
        };
        if (*iter).flags & BTREE_ITER_with_key_cache != 0
            && btree_trans_peek_key_cache(iter, &mut ret) != 0
        {
            return bkey_s_c::default();
        }
        if (*trans).journal_replay_not_finished {
            let journal_k = crate::journal::bch2_journal_keys_peek_max(
                (*trans).c,
                (*iter).btree_id,
                (*iter).min_depth,
                search_key,
                *end,
                &mut (*iter).journal_idx,
            );
            if !journal_k.is_null()
                && (ret.k.is_null() || bpos_cmp((*journal_k).k.p, (*ret.k).p) <= 0)
            {
                if (*journal_k).k.type_ == super::bset::KEY_TYPE_deleted {
                    if bpos_eq((*journal_k).k.p, SPOS_MAX) {
                        return bkey_s_c::default();
                    }
                    btree_iter_set_pos(iter, bpos_successor((*journal_k).k.p));
                    continue;
                }
                (*iter).k = (*journal_k).k;
                let candidate = if (*iter).flags & BTREE_ITER_is_extents != 0 {
                    super::bkey::bpos_max((*iter).pos, super::bkey::bkey_start_pos(&(*iter).k))
                } else {
                    (*iter).k.p
                };
                let out_of_range = if (*iter).flags & BTREE_ITER_all_snapshots != 0 {
                    bpos_gt(candidate, *end)
                } else if (*iter).flags & BTREE_ITER_is_extents != 0 {
                    super::bkey::bkey_ge(candidate, *end)
                } else {
                    bpos_gt(candidate, *end)
                };
                if out_of_range {
                    btree_iter_set_pos(iter, *end);
                    return bkey_s_c::default();
                }
                (*iter).pos = candidate;
                return bkey_s_c {
                    k: &(*iter).k,
                    v: &(*journal_k).v,
                };
            }
        }
        bch2_btree_trans_peek_updates(trans, iter, &mut ret, *end);
        if !ret.k.is_null() && (*ret.k).type_ == super::bset::KEY_TYPE_deleted {
            let next = if bpos_eq((*iter).pos, (*ret.k).p) {
                bpos_successor((*ret.k).p)
            } else {
                (*ret.k).p
            };
            if bpos_eq(next, SPOS_MAX) {
                return bkey_s_c::default();
            }
            btree_iter_set_pos(iter, next);
            continue;
        }
        if !ret.k.is_null() {
            let candidate = if (*iter).flags & BTREE_ITER_is_extents != 0 {
                super::bkey::bpos_max((*iter).pos, super::bkey::bkey_start_pos(&*ret.k))
            } else {
                (*ret.k).p
            };
            let out_of_range = if (*iter).flags & BTREE_ITER_all_snapshots != 0 {
                bpos_gt(candidate, *end)
            } else if (*iter).flags & BTREE_ITER_is_extents != 0 {
                super::bkey::bkey_ge(candidate, *end)
            } else {
                bpos_gt(candidate, *end)
            };
            if out_of_range {
                btree_iter_set_pos(iter, *end);
                return bkey_s_c::default();
            }
            (*iter).k = *ret.k;
            (*iter).pos = if (*iter).flags & BTREE_ITER_is_extents != 0 {
                candidate
            } else {
                (*iter).k.p
            };
            ret.k = &(*iter).k;
        }
        return ret;
    }
}

unsafe fn bch2_btree_trans_peek_updates(
    trans: *mut btree_trans,
    iter: *mut btree_iter,
    current: &mut bkey_s_c,
    end: bpos,
) {
    let level = (*iter).min_depth;
    for idx in 0..(*trans).nr_updates as usize {
        let update = &*(*trans).updates.add(idx);
        if update.key_cache_already_flushed
            || update.btree_id != (*iter).btree_id
            || update.level != level
            || ((*iter).flags & BTREE_ITER_slots == 0
                && bkey_deleted(&*(update.k as *const bkey_packed)))
            || bpos_cmp((*update.k).k.p, (*iter).pos) < 0
            || bpos_cmp((*update.k).k.p, end) > 0
        {
            continue;
        }
        if current.k.is_null() || bpos_cmp((*update.k).k.p, (*current.k).p) <= 0 {
            (*iter).k = (*update.k).k;
            *current = bkey_s_c {
                k: &(*iter).k,
                v: &(*update.k).v,
            };
        }
    }
}

unsafe fn bch2_btree_trans_peek_prev_updates(
    trans: *mut btree_trans,
    iter: *mut btree_iter,
    search_key: bpos,
    current: &mut bkey_s_c,
) {
    let path = (*trans).paths.add((*iter).path as usize);
    let end = (*(*path).l[0].b).data.as_ref().unwrap().min_key;
    for idx in 0..(*trans).nr_updates as usize {
        let update = &*(*trans).updates.add(idx);
        if update.key_cache_already_flushed
            || update.btree_id != (*iter).btree_id
            || bpos_cmp((*update.k).k.p, search_key) > 0
            || bpos_cmp(
                (*update.k).k.p,
                if current.k.is_null() {
                    end
                } else {
                    (*current.k).p
                },
            ) < 0
        {
            continue;
        }
        (*iter).k = (*update.k).k;
        *current = bkey_s_c {
            k: &(*iter).k,
            v: &(*update.k).v,
        };
    }
}

unsafe fn btree_trans_peek_key_cache(iter: *mut btree_iter, current: &mut bkey_s_c) -> i32 {
    if current.k.is_null()
        || (*current.k).type_ == super::bset::KEY_TYPE_deleted
        || (*iter).key_cache_path == 0
    {
        return 0;
    }
    if (*iter).flags & BTREE_ITER_key_cache_fill != 0 && bpos_eq((*iter).pos, (*current.k).p) {
        return 0;
    }

    let trans = (*iter).trans;
    (*iter).key_cache_path = bch2_btree_path_set_pos(
        trans,
        (*iter).key_cache_path,
        &(*current.k).p,
        (*iter).flags & BTREE_ITER_intent != 0,
        0,
    );
    let ret = bch2_btree_path_traverse(
        trans,
        (*iter).key_cache_path,
        (*iter).flags | BTREE_ITER_cached,
    );
    let ret = if ret == 0 {
        bch2_btree_path_relock(trans, (*trans).paths.add((*iter).path as usize))
    } else {
        ret
    };
    if ret != 0 {
        bch2_btree_iter_set_pos(iter, (*iter).pos);
        return ret;
    }

    let mut u = bkey::default();
    let cached =
        bch2_btree_path_peek_slot((*trans).paths.add((*iter).key_cache_path as usize), &mut u);
    if cached.k.is_null()
        || ((*iter).flags & BTREE_ITER_all_snapshots != 0
            && !bpos_eq((*current.k).p, (*cached.k).p))
    {
        return 0;
    }
    (*iter).k = u;
    current.v = cached.v;
    btree_path_set_should_be_locked(trans, (*trans).paths.add((*iter).key_cache_path as usize));
    0
}

pub unsafe fn bch2_btree_iter_peek(iter: *mut btree_iter) -> bkey_s_c {
    bch2_btree_iter_peek_max(iter, &SPOS_MAX)
}

pub unsafe fn bch2_btree_iter_peek_slot(iter: *mut btree_iter) -> bkey_s_c {
    if (*iter).flags & BTREE_ITER_is_extents != 0 && (*iter).pos.offset == KEY_OFFSET_MAX {
        if (*iter).pos.inode == KEY_INODE_MAX {
            return bkey_s_c::default();
        }
        btree_iter_set_pos(iter, bpos_nosnap_successor((*iter).pos));
    }
    let trans = (*iter).trans;
    let mut search_key = (*iter).pos;
    if (*iter).flags & BTREE_ITER_is_extents != 0 && !bkey_eq(search_key, POS_MAX) {
        search_key = if (*iter).flags & BTREE_ITER_all_snapshots != 0 {
            bpos_successor(search_key)
        } else {
            bpos_with_snapshot(bpos_nosnap_successor(search_key), (*iter).snapshot)
        };
    }
    (*iter).path = bch2_btree_path_set_pos(
        trans,
        (*iter).path,
        &search_key,
        (*iter).flags & BTREE_ITER_intent != 0,
        0,
    );
    let path = (*trans).paths.add((*iter).path as usize);
    (*path).level = 0;
    let ret = bch2_btree_path_traverse_one(trans, (*iter).path, (*iter).flags);
    if ret != 0 {
        return super::bkey::bkey_s_c_err(ret);
    }
    (*path).should_be_locked = true;
    let leaf = (*path).l[0].b;
    if leaf.is_null() {
        return bkey_s_c::default();
    }
    if (*iter).flags & BTREE_ITER_cached == 0
        && (*iter).flags & (BTREE_ITER_is_extents | BTREE_ITER_filter_snapshots) != 0
    {
        let mut iter2 = btree_iter::default();
        bch2_trans_copy_iter(&mut iter2, iter);
        iter2.flags |= BTREE_ITER_nofilter_whiteouts;
        let mut extent_end = (*iter).pos;
        if iter2.flags & BTREE_ITER_is_extents != 0 {
            extent_end.offset = KEY_OFFSET_MAX;
        }
        let mut ret;
        loop {
            ret = bch2_btree_iter_peek_max(&mut iter2, &extent_end);
            if !(iter2.flags & BTREE_ITER_is_extents != 0
                && !ret.k.is_null()
                && (*ret.k).type_ == super::bset::KEY_TYPE_deleted)
            {
                break;
            }
            btree_iter_set_pos(&mut iter2, (*ret.k).p);
        }
        if !ret.k.is_null() {
            core::mem::swap(&mut (*iter).key_cache_path, &mut iter2.key_cache_path);
            (*iter).k = iter2.k;
            ret.k = &(*iter).k;
        }
        bch2_trans_iter_exit(&mut iter2);
        if !ret.k.is_null() {
            let next = bkey_start_pos(&*ret.k);
            if bpos_lt((*iter).pos, next) {
                bkey_init(&mut (*iter).k);
                (*iter).k.p = (*iter).pos;
                if (*iter).flags & BTREE_ITER_is_extents != 0 {
                    let size = core::cmp::min(
                        KEY_SIZE_MAX as u64,
                        if next.inode == (*iter).pos.inode {
                            next.offset.wrapping_sub((*iter).pos.offset)
                        } else {
                            KEY_OFFSET_MAX.wrapping_sub((*iter).pos.offset)
                        },
                    ) as u32;
                    bch2_key_resize(&mut (*iter).k, size);
                }
                return bkey_s_c {
                    k: &(*iter).k,
                    v: core::ptr::null(),
                };
            }
        }
        if !ret.k.is_null()
            && (*iter).flags & BTREE_ITER_filter_snapshots != 0
            && (*iter).flags & BTREE_ITER_nofilter_whiteouts == 0
            && ((*ret.k).type_ == super::bset::KEY_TYPE_deleted
                || (*ret.k).type_ == super::bset::KEY_TYPE_whiteout
                || (*ret.k).type_ == super::bset::KEY_TYPE_extent_whiteout)
        {
            (*iter).k.type_ = super::bset::KEY_TYPE_deleted;
        }
        return ret;
    }
    for idx in 0..(*trans).nr_updates as usize {
        let update = &*(*trans).updates.add(idx);
        if update.key_cache_already_flushed
            || update.btree_id != (*iter).btree_id
            || update.level != (*iter).min_depth
            || !bpos_eq((*update.k).k.p, (*iter).pos)
        {
            continue;
        }
        (*iter).k = (*update.k).k;
        return bkey_s_c {
            k: &(*iter).k,
            v: &(*update.k).v,
        };
    }
    if (*trans).journal_replay_not_finished {
        let journal_k = crate::journal::bch2_journal_keys_peek_slot(
            (*trans).c,
            (*iter).btree_id,
            (*iter).min_depth,
            (*iter).pos,
        );
        if !journal_k.is_null() {
            if (*journal_k).k.type_ == super::bset::KEY_TYPE_deleted
                && (*iter).flags & BTREE_ITER_slots == 0
            {
                return bkey_s_c::default();
            }
            (*iter).k = (*journal_k).k;
            return bkey_s_c {
                k: &(*iter).k,
                v: &(*journal_k).v,
            };
        }
    }
    let mut ret = bch2_btree_path_peek_slot(path, &mut (*iter).k);
    if (*iter).flags & BTREE_ITER_with_key_cache != 0
        && btree_trans_peek_key_cache(iter, &mut ret) != 0
    {
        return bkey_s_c::default();
    }
    if !ret.k.is_null()
        && (*iter).flags & BTREE_ITER_filter_snapshots != 0
        && (*iter).flags & BTREE_ITER_nofilter_whiteouts == 0
        && ((*ret.k).type_ == super::bset::KEY_TYPE_deleted
            || (*ret.k).type_ == super::bset::KEY_TYPE_whiteout
            || (*ret.k).type_ == super::bset::KEY_TYPE_extent_whiteout)
    {
        (*iter).k.type_ = super::bset::KEY_TYPE_deleted;
    }
    ret
}

pub unsafe fn bch2_btree_iter_next_slot(iter: *mut btree_iter) -> bkey_s_c {
    if !bch2_btree_iter_advance(iter) {
        bkey_s_c::default()
    } else {
        bch2_btree_iter_peek_slot(iter)
    }
}

pub unsafe fn bch2_btree_iter_peek_node(iter: *mut btree_iter) -> *mut btree {
    if iter.is_null() || (*iter).trans.is_null() || (*iter).path == 0 {
        return core::ptr::null_mut();
    }
    let trans = (*iter).trans;
    if bch2_btree_iter_traverse(iter) != 0 {
        return core::ptr::null_mut();
    }
    let path = (*trans).paths.add((*iter).path as usize);
    let level = (*path).level as usize;
    let node = (*path).l[level].b;
    if !node.is_null() && !(*node).data.is_null() {
        bkey_init(&mut (*iter).k);
        (*iter).k.p = (*(*node).data).min_key;
        (*iter).pos = (*iter).k.p;
        (*iter).path = bch2_btree_path_set_pos(
            trans,
            (*iter).path,
            &(*node).key.k.p,
            (*iter).flags & BTREE_ITER_intent != 0,
            0,
        );
        (*(*trans).paths.add((*iter).path as usize)).should_be_locked = true;
    }
    node
}

pub unsafe fn bch2_btree_iter_peek_root(
    trans: *mut btree_trans,
    iter: *mut btree_iter,
    btree_id: u8,
    level: u8,
) -> bkey_s_c {
    if trans.is_null() || (*trans).c.is_null() || btree_id as usize >= super::types::BTREE_ID_NR {
        return bkey_s_c::default();
    }
    if iter.is_null() {
        return bkey_s_c::default();
    }

    let c = (*trans).c;
    while level
        == bch2_btree_root_unpack_level(bch2_btree_id_root_packed(c, btree_id as usize))
            .saturating_add(1)
    {
        bch2_trans_node_iter_init(
            trans,
            iter,
            btree_id,
            POS_MIN,
            0,
            level.saturating_sub(1),
            BTREE_ITER_not_extents | BTREE_ITER_all_snapshots,
        );
        let b = bch2_btree_iter_peek_node(iter);
        if b.is_null() {
            return bkey_s_c::default();
        }
        let root = super::types::bch2_btree_id_root_b(c, btree_id as usize);
        if b != root {
            continue;
        }
        if super::types::btree_node_fake(b) {
            break;
        }
        return bkey_s_c {
            k: &(*b).key.k,
            v: &(*b).key.v,
        };
    }
    bkey_s_c::default()
}

pub unsafe fn bch2_btree_iter_rewind(iter: *mut btree_iter) -> bool {
    let pos = if (*iter).flags & BTREE_ITER_is_extents != 0 {
        bkey_start_pos(&(*iter).k)
    } else {
        (*iter).k.p
    };
    let ret = if (*iter).flags & BTREE_ITER_all_snapshots != 0 {
        !bpos_eq(pos, POS_MIN)
    } else {
        !bkey_eq(pos, POS_MIN)
    };
    let mut next = pos;
    if ret && (*iter).flags & BTREE_ITER_is_extents == 0 {
        next = if (*iter).flags & BTREE_ITER_all_snapshots != 0 {
            bpos_predecessor(pos)
        } else {
            bpos_with_snapshot(bpos_nosnap_predecessor(pos), (*iter).snapshot)
        };
    }
    btree_iter_set_pos(iter, next);
    ret
}

pub unsafe fn bch2_btree_iter_prev(iter: *mut btree_iter) -> bkey_s_c {
    if !bch2_btree_iter_rewind(iter) {
        bkey_s_c::default()
    } else {
        bch2_btree_iter_peek_prev(iter)
    }
}

pub unsafe fn bch2_btree_iter_prev_slot(iter: *mut btree_iter) -> bkey_s_c {
    if !bch2_btree_iter_rewind(iter) {
        bkey_s_c::default()
    } else {
        bch2_btree_iter_peek_slot(iter)
    }
}

pub unsafe fn bch2_btree_iter_peek_prev(iter: *mut btree_iter) -> bkey_s_c {
    bch2_btree_iter_peek_prev_min(iter, POS_MIN)
}

pub unsafe fn bch2_btree_iter_peek_prev_min(iter: *mut btree_iter, end: bpos) -> bkey_s_c {
    if (*iter).flags & BTREE_ITER_filter_snapshots != 0 {
        let trans = (*iter).trans;
        let saved_flags = (*iter).flags;
        let saved_pos = (*iter).pos;
        let mut saved_path = 0;
        (*iter).flags &= !BTREE_ITER_filter_snapshots;
        loop {
            let k = bch2_btree_iter_peek_prev_min(iter, end);
            if super::bkey::bkey_err(k) != 0 || k.k.is_null() {
                if saved_path != 0 {
                    bch2_path_put(trans, saved_path, saved_flags & BTREE_ITER_intent != 0);
                }
                (*iter).flags = saved_flags;
                return k;
            }
            let below_end = if saved_flags & BTREE_ITER_all_snapshots != 0 {
                bpos_cmp((*k.k).p, end) < 0
            } else {
                bkey_lt((*k.k).p, end)
            };
            if below_end {
                if saved_path != 0 {
                    bch2_path_put(trans, saved_path, saved_flags & BTREE_ITER_intent != 0);
                }
                btree_iter_set_pos(iter, end);
                (*iter).flags = saved_flags;
                return bkey_s_c::default();
            }

            let visible = crate::snapshot::bch2_snapshot_is_ancestor(
                &*(*trans).c,
                saved_pos.snapshot,
                (*k.k).p.snapshot,
            );
            if !visible {
                (*iter).pos = if saved_flags & BTREE_ITER_all_snapshots != 0 {
                    bpos_predecessor((*k.k).p)
                } else {
                    bpos_with_snapshot(bpos_nosnap_predecessor((*k.k).p), saved_pos.snapshot)
                };
                continue;
            }

            if (*k.k).p.snapshot != saved_pos.snapshot {
                if saved_path != 0 {
                    bch2_path_put(trans, saved_path, saved_flags & BTREE_ITER_intent != 0);
                    saved_path = 0;
                }
                if (*k.k).type_ != super::bset::KEY_TYPE_deleted
                    && (*k.k).type_ != super::bset::KEY_TYPE_whiteout
                    && (*k.k).type_ != super::bset::KEY_TYPE_extent_whiteout
                {
                    saved_path = btree_path_clone(
                        trans,
                        (*iter).path,
                        saved_flags & BTREE_ITER_intent != 0,
                        0,
                    );
                }
                (*iter).pos = if saved_flags & BTREE_ITER_all_snapshots != 0 {
                    bpos_predecessor((*k.k).p)
                } else {
                    bpos_with_snapshot(bpos_nosnap_predecessor((*k.k).p), saved_pos.snapshot)
                };
                continue;
            }

            if (*k.k).type_ == super::bset::KEY_TYPE_deleted
                || (*k.k).type_ == super::bset::KEY_TYPE_whiteout
                || (*k.k).type_ == super::bset::KEY_TYPE_extent_whiteout
            {
                let mut previous = if saved_flags & BTREE_ITER_all_snapshots != 0 {
                    bpos_predecessor((*k.k).p)
                } else {
                    bpos_nosnap_predecessor((*k.k).p)
                };
                previous.snapshot = u32::MAX;
                (*iter).pos = previous;
                continue;
            }

            let snapshot_limit = bpos {
                inode: saved_pos.inode,
                offset: saved_pos.offset,
                snapshot: saved_pos.snapshot,
            };
            if saved_path != 0 && bpos_cmp((*k.k).p, snapshot_limit) < 0 {
                bch2_path_put(trans, (*iter).path, saved_flags & BTREE_ITER_intent != 0);
                (*iter).path = saved_path;
                let path = (*trans).paths.add((*iter).path as usize);
                (*path).should_be_locked = true;
                let ret = bch2_btree_path_peek_slot(path, &mut (*iter).k);
                (*iter).flags = saved_flags;
                return ret;
            }
            (*iter).flags = saved_flags;
            if saved_path != 0 {
                bch2_path_put(trans, saved_path, saved_flags & BTREE_ITER_intent != 0);
            }
            return k;
        }
    }
    if (*iter).flags & (BTREE_ITER_is_extents | BTREE_ITER_filter_snapshots) != 0
        && !bkey_eq((*iter).pos, POS_MAX)
        && !((*iter).flags & BTREE_ITER_is_extents != 0 && (*iter).pos.offset == KEY_OFFSET_MAX)
    {
        let k = bch2_btree_iter_peek_slot(iter);
        if super::bkey::bkey_err(k) != 0 || k.k.is_null() {
            return k;
        }
        if (*k.k).type_ != super::bset::KEY_TYPE_deleted
            && ((*iter).flags & BTREE_ITER_is_extents == 0
                || bkey_lt(bkey_start_pos(&*k.k), (*iter).pos))
        {
            return k;
        }
    }
    let trans = (*iter).trans;
    let mut search_key = (*iter).pos;
    loop {
        (*iter).path = bch2_btree_path_set_pos(
            trans,
            (*iter).path,
            &search_key,
            (*iter).flags & BTREE_ITER_intent != 0,
            0,
        );
        let path = (*trans).paths.add((*iter).path as usize);
        (*path).level = 0;
        let ret = bch2_btree_path_traverse_one(trans, (*iter).path, (*iter).flags);
        if ret != 0 {
            bch2_btree_iter_set_pos(iter, (*iter).pos);
            return super::bkey::bkey_s_c_err(ret);
        }
        if (*path).l[0].b.is_null() {
            bch2_btree_iter_set_pos(iter, SPOS_MAX);
            return bkey_s_c::default();
        }
        (*iter).path = bch2_btree_path_make_mut(
            trans,
            (*iter).path,
            (*iter).flags & BTREE_ITER_intent != 0,
            0,
        );
        let path = (*trans).paths.add((*iter).path as usize);
        (*path).should_be_locked = true;
        let leaf = (*path).l[0].b;
        if leaf.is_null() {
            return bkey_s_c::default();
        }
        let mut packed = bch2_btree_node_iter_peek_all(&mut (*path).l[0].iter, leaf);
        if packed.is_null()
            || bpos_cmp(super::node_iter::bkey_unpack_pos(leaf, packed), search_key) > 0
        {
            packed = bch2_btree_node_iter_prev(&mut (*path).l[0].iter, leaf);
            if !packed.is_null() {
                (*path).pos = super::node_iter::bkey_unpack_pos(leaf, packed);
            }
        }
        if !packed.is_null() {
            if super::bkey::bkey_packed(&*packed) {
                super::bkey::__bch2_bkey_unpack_key(&(*leaf).format, &mut (*iter).k, &*packed);
            } else {
                (*iter).k = *(packed as *const bkey);
            }
            let mut ret = bkey_s_c {
                k: &(*iter).k,
                v: (packed as *const u64)
                    .add(bkeyp_key_u64s(&(*leaf).format, &*packed) as usize)
                    .cast(),
            };
            if (*iter).flags & BTREE_ITER_with_key_cache != 0
                && btree_trans_peek_key_cache(iter, &mut ret) != 0
            {
                return bkey_s_c::default();
            }
            if (*trans).journal_replay_not_finished {
                let journal_k = crate::journal::bch2_journal_keys_peek_prev_min(
                    (*trans).c,
                    (*iter).btree_id,
                    (*iter).min_depth,
                    (*iter).pos,
                    end,
                    &mut (*iter).journal_idx,
                );
                if !journal_k.is_null()
                    && (ret.k.is_null() || bpos_cmp((*journal_k).k.p, (*ret.k).p) >= 0)
                {
                    (*iter).k = (*journal_k).k;
                    ret = bkey_s_c {
                        k: &(*iter).k,
                        v: &(*journal_k).v,
                    };
                }
            }
            bch2_btree_trans_peek_prev_updates(trans, iter, search_key, &mut ret);
            let within_end = if (*iter).flags & BTREE_ITER_is_extents != 0 {
                bkey_lt(end, (*iter).k.p)
            } else {
                !bkey_lt((*iter).k.p, end)
            };
            if within_end {
                if (*iter).k.type_ == super::bset::KEY_TYPE_deleted {
                    if bpos_eq((*iter).k.p, POS_MIN) {
                        return bkey_s_c::default();
                    }
                    search_key = bpos_predecessor((*iter).k.p);
                    btree_iter_set_pos(iter, search_key);
                    continue;
                }
                (*iter).pos = if (*iter).flags & BTREE_ITER_is_extents != 0 {
                    super::bkey::bpos_min((*iter).pos, (*iter).k.p)
                } else {
                    (*iter).k.p
                };
                if (*iter).flags & BTREE_ITER_all_snapshots == 0 {
                    (*iter).pos.snapshot = (*iter).snapshot;
                }
                return ret;
            }
            btree_iter_set_pos(iter, end);
            return bkey_s_c::default();
        }
        let mut ret = bkey_s_c::default();
        if (*trans).journal_replay_not_finished {
            let journal_k = crate::journal::bch2_journal_keys_peek_prev_min(
                (*trans).c,
                (*iter).btree_id,
                (*iter).min_depth,
                (*iter).pos,
                end,
                &mut (*iter).journal_idx,
            );
            if !journal_k.is_null() {
                (*iter).k = (*journal_k).k;
                ret = bkey_s_c {
                    k: &(*iter).k,
                    v: &(*journal_k).v,
                };
            }
        }
        bch2_btree_trans_peek_prev_updates(trans, iter, search_key, &mut ret);
        if !ret.k.is_null() {
            if bpos_eq((*ret.k).p, POS_MIN) {
                return bkey_s_c::default();
            }
            if (*ret.k).type_ == super::bset::KEY_TYPE_deleted {
                search_key = bpos_predecessor((*ret.k).p);
                btree_iter_set_pos(iter, search_key);
                continue;
            }
            let within_end = if (*iter).flags & BTREE_ITER_is_extents != 0 {
                bkey_lt(end, (*ret.k).p)
            } else {
                !bkey_lt((*ret.k).p, end)
            };
            if within_end {
                (*iter).pos = if (*iter).flags & BTREE_ITER_is_extents != 0 {
                    super::bkey::bpos_min((*iter).pos, (*ret.k).p)
                } else {
                    (*ret.k).p
                };
                if (*iter).flags & BTREE_ITER_all_snapshots == 0 {
                    (*iter).pos.snapshot = (*iter).snapshot;
                }
                return ret;
            }
            btree_iter_set_pos(iter, end);
            return bkey_s_c::default();
        }
        let min = (*(*leaf).data).min_key;
        if bpos_eq(min, POS_MIN) {
            return bkey_s_c::default();
        }
        search_key = bpos_predecessor(min);
        btree_iter_set_pos(iter, search_key);
    }
}

pub unsafe fn bch2_btree_iter_advance(iter: *mut btree_iter) -> bool {
    let pos = (*iter).k.p;
    let ret = if (*iter).flags & BTREE_ITER_all_snapshots != 0 {
        !bpos_eq(pos, SPOS_MAX)
    } else {
        !bkey_eq(pos, SPOS_MAX)
    };
    let mut next = pos;
    if ret && (*iter).flags & BTREE_ITER_is_extents == 0 {
        next = if (*iter).flags & BTREE_ITER_all_snapshots != 0 {
            super::bkey::bpos_successor(pos)
        } else {
            bpos_with_snapshot(bpos_nosnap_successor(pos), (*iter).snapshot)
        };
    }
    btree_iter_set_pos(iter, next);
    ret
}

pub unsafe fn bch2_btree_iter_next(iter: *mut btree_iter) -> bkey_s_c {
    if !bch2_btree_iter_advance(iter) {
        bkey_s_c::default()
    } else {
        bch2_btree_iter_peek(iter)
    }
}

pub unsafe fn bch2_trans_unlock(trans: *mut btree_trans) {
    if trans.is_null() {
        return;
    }
    (*trans).locked = false;
    for idx in 1..BTREE_ITER_INITIAL {
        if (*trans).paths_allocated & (1u64 << idx) == 0 {
            continue;
        }
        let path = (*trans).paths.add(idx);
        let mut write_nodes = [core::ptr::null_mut(); BTREE_MAX_DEPTH as usize];
        for level in 0..BTREE_MAX_DEPTH as usize {
            if path_locked_type(&*path, level) == BTREE_NODE_WRITE_LOCKED {
                write_nodes[level] = (*path).l[level].b;
            }
        }
        btree_path_unlock(path);
        for level in 0..BTREE_MAX_DEPTH as usize {
            let b = write_nodes[level];
            if b.is_null() {
                continue;
            }
            let lock_seq = six_lock_seq(&(*b).c.lock);
            for linked_idx in 1..BTREE_ITER_INITIAL {
                if (*trans).paths_allocated & (1u64 << linked_idx) == 0 {
                    continue;
                }
                let linked = (*trans).paths.add(linked_idx);
                if (*linked).l[level].b == b {
                    (*linked).l[level].lock_seq = lock_seq;
                }
            }
        }
    }
}

pub unsafe fn bch2_trans_unlock_write(trans: *mut btree_trans) {
    if trans.is_null() {
        return;
    }
    for idx in 1..BTREE_ITER_INITIAL {
        if (*trans).paths_allocated & (1u64 << idx) == 0 {
            continue;
        }
        let path = (*trans).paths.add(idx);
        for level in 0..BTREE_MAX_DEPTH as usize {
            if path_locked_type(&*path, level) != BTREE_NODE_WRITE_LOCKED {
                continue;
            }
            let b = (*path).l[level].b;
            six_unlock_write(&(*b).c.lock);
            path_mark_locked(&mut *path, level, BTREE_NODE_INTENT_LOCKED);
            let lock_seq = six_lock_seq(&(*b).c.lock);
            for linked_idx in 1..BTREE_ITER_INITIAL {
                if (*trans).paths_allocated & (1u64 << linked_idx) == 0 {
                    continue;
                }
                let linked = (*trans).paths.add(linked_idx);
                if (*linked).l[level].b == b {
                    (*linked).l[level].lock_seq = lock_seq;
                }
            }
        }
    }
}

pub unsafe fn bch2_trans_relock(trans: *mut btree_trans) -> i32 {
    bch2_trans_relock_notrace(trans)
}

pub unsafe fn bch2_trans_relock_notrace(trans: *mut btree_trans) -> i32 {
    if trans.is_null() {
        return -22;
    }
    if (*trans).restarted != 0 {
        return -((*trans).restarted as i32);
    }
    for idx in 1..BTREE_ITER_INITIAL {
        if (*trans).paths_allocated & (1u64 << idx) == 0 {
            continue;
        }
        let path = (*trans).paths.add(idx);
        if !(*path).should_be_locked {
            continue;
        }
        if !bch2_btree_path_relock_norestart(trans, path) {
            bch2_trans_unlock(trans);
            return -11;
        }
    }
    (*trans).locked = true;
    0
}

pub unsafe fn bch2_trans_unlock_long(trans: *mut btree_trans) {
    bch2_trans_unlock(trans);
}

pub unsafe fn bch2_trans_downgrade(trans: *mut btree_trans) {
    if trans.is_null() || (*trans).restarted != 0 {
        return;
    }
    for idx in 1..BTREE_ITER_INITIAL {
        if (*trans).paths_allocated & (1u64 << idx) == 0 {
            continue;
        }
        let path = (*trans).paths.add(idx);
        if (*path).ref_ == 0 {
            continue;
        }
        let wanted = (*path).level + u8::from((*path).intent_ref != 0);
        bch2_btree_path_downgrade(trans, path, wanted);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btree::bkey::{BKEY_FORMAT_CURRENT, BKEY_U64S, KEY_FORMAT_CURRENT, POS_MIN, SPOS};
    use crate::btree::bset::{
        bkey_i_btree_ptr_v2, bset as disk_bset, btree_node as disk_btree_node,
        KEY_TYPE_btree_ptr_v2,
    };
    use crate::btree::types::{bch2_btree_id_root_set, bset_tree, BSET_NO_AUX_TREE_VAL};

    #[test]
    fn path_get_preserves_cached_iterator_flag() {
        unsafe {
            let mut c = crate::btree::types::bch_fs::default();
            let mut trans = btree_trans::default();
            bch2_trans_init(&mut trans, &mut c);
            let pos = SPOS(1, 1, 0);
            let path = bch2_path_get(
                &mut trans,
                0,
                &pos,
                0,
                0,
                BTREE_ITER_cached | BTREE_ITER_intent,
            );
            assert!((*trans.paths.add(path as usize)).cached);
            assert_eq!((*trans.paths.add(path as usize)).intent_ref, 1);
            bch2_path_put(&mut trans, path, true);
        }
    }

    #[test]
    fn path_set_pos_reuses_path_for_equal_position() {
        unsafe {
            let mut c = crate::btree::types::bch_fs::default();
            let mut trans = btree_trans::default();
            bch2_trans_init(&mut trans, &mut c);
            let pos = SPOS(1, 1, 0);
            let path = bch2_path_get(&mut trans, 0, &pos, 0, 0, 0);
            let path_ref = trans.paths.add(path as usize);
            (*path_ref).should_be_locked = true;
            let refs = (*path_ref).ref_;
            let got = bch2_btree_path_set_pos(&mut trans, path, &pos, false, 0);
            assert_eq!(got, path);
            assert_eq!((*path_ref).ref_, refs);
            assert!((*path_ref).should_be_locked);
            bch2_path_put(&mut trans, path, false);
        }
    }

    #[test]
    fn cached_path_reposition_invalidates_old_node() {
        unsafe {
            let mut c = crate::btree::types::bch_fs::default();
            let mut trans = btree_trans::default();
            bch2_trans_init(&mut trans, &mut c);
            let pos = SPOS(1, 1, 0);
            let path = bch2_path_get(&mut trans, 0, &pos, 0, 0, BTREE_ITER_cached);
            let path_ref = trans.paths.add(path as usize);
            let mut cached = crate::btree::types::bkey_cached::default();
            (*path_ref).l[0].b = (&mut cached as *mut crate::btree::types::bkey_cached).cast();
            let new_pos = SPOS(2, 1, 0);
            let got = bch2_btree_path_set_pos(&mut trans, path, &new_pos, false, 0);
            assert!((*trans.paths.add(got as usize)).l[0].b.is_null());
            bch2_path_put(&mut trans, got, false);
        }
    }

    #[test]
    fn path_peek_slot_reads_cached_key() {
        unsafe {
            let pos = SPOS(7, 11, 3);
            let mut key = crate::btree::bkey::bkey_i::default();
            key.k.p = pos;
            let mut cached = crate::btree::types::bkey_cached::default();
            core::ptr::addr_of_mut!(cached.key.btree_id).write_unaligned(2);
            core::ptr::addr_of_mut!(cached.key.pos).write_unaligned(pos);
            cached.k = &mut key;
            let mut path = btree_path::default();
            path.cached = true;
            path.btree_id = 2;
            path.pos = pos;
            path.l[0].b = (&mut cached as *mut crate::btree::types::bkey_cached).cast();
            let mut unpacked = bkey::default();
            let got = bch2_btree_path_peek_slot(&mut path, &mut unpacked);
            assert_eq!(got.k, &unpacked);
            assert_eq!(got.v, &key.v);
            assert_eq!(unpacked.p, pos);
            assert_eq!(crate::btree::types::btree_node_pos(&mut cached.c), pos);
        }
    }

    #[test]
    fn path_peek_slot_exact_returns_position_on_miss() {
        unsafe {
            let pos = SPOS(9, 4, 1);
            let mut path = btree_path::default();
            path.pos = pos;
            let mut key = bkey::default();
            let got = bch2_btree_path_peek_slot_exact(&mut path, &mut key);
            assert_eq!(got.k, &key);
            assert!(got.v.is_null());
            assert_eq!(key.p, pos);
        }
    }

    #[test]
    fn path_make_mut_reuses_unique_nonpreserved_path() {
        unsafe {
            let mut c = crate::btree::types::bch_fs::default();
            let mut trans = btree_trans::default();
            bch2_trans_init(&mut trans, &mut c);
            let path = bch2_path_get(&mut trans, 0, &SPOS(1, 1, 0), 0, 0, BTREE_ITER_nopreserve);
            let before = trans.paths_allocated;
            let got = bch2_btree_path_make_mut(&mut trans, path, false, 0);
            assert_eq!(got, path);
            assert_eq!(trans.paths_allocated, before);
            bch2_path_put(&mut trans, path, false);

            let preserved = bch2_path_get(&mut trans, 0, &SPOS(1, 2, 0), 0, 0, 0);
            (*trans.paths.add(preserved as usize)).should_be_locked = true;
            let cloned = bch2_btree_path_make_mut(&mut trans, preserved, false, 0);
            assert_ne!(cloned, preserved);
            assert!(!(*trans.paths.add(cloned as usize)).should_be_locked);
            bch2_path_put(&mut trans, cloned, false);
        }
    }

    #[test]
    fn path_node_helpers_follow_node_level() {
        unsafe {
            let mut path = btree_path::default();
            let mut node = crate::btree::types::btree::default();
            node.c.level = 1;
            path.l[1].b = &mut node;
            path.l[2].b = &mut node;
            assert!(core::ptr::eq(btree_path_node(&mut path, 1), &mut node));
            assert!(core::ptr::eq(
                btree_node_parent(&mut path, &mut node),
                &mut node
            ));
            assert!(btree_path_node(&mut path, BTREE_MAX_DEPTH as usize).is_null());
        }
    }

    #[test]
    fn path_relock_stops_at_unfilled_level() {
        unsafe {
            let mut c = crate::btree::types::bch_fs::default();
            let mut trans = btree_trans::default();
            bch2_trans_init(&mut trans, &mut c);
            let path = trans.paths.add(1);
            (*path).level = 0;
            (*path).locks_want = 2;
            assert!(bch2_btree_path_relock_norestart(&mut trans, path));
        }
    }

    #[test]
    fn path_node_unlock_keeps_node_for_reuse() {
        unsafe {
            let mut node = crate::btree::types::btree::default();
            let mut path = btree_path::default();
            path.l[0].b = &mut node;
            assert_eq!(six_lock_read(&node.c.lock), 0);
            path_mark_locked(&mut path, 0, BTREE_NODE_READ_LOCKED);
            btree_node_unlock(&mut path, 0);
            assert!(path.l[0].b == &mut node);
            assert_eq!(path_locked_type(&path, 0), BTREE_NODE_UNLOCKED);
        }
    }

    #[test]
    fn transaction_unlock_write_keeps_intent_lock() {
        unsafe {
            let mut c = crate::btree::types::bch_fs::default();
            let mut trans = btree_trans::default();
            bch2_trans_init(&mut trans, &mut c);
            let mut node = crate::btree::types::btree::default();
            let path = trans.paths.add(1);
            (*path).l[0].b = &mut node;
            (*trans.paths.add(1)).ref_ = 1;
            trans.paths_allocated |= 1u64 << 1;
            assert_eq!(six_lock_read(&node.c.lock), 0);
            assert!(crate::lock::six::six_trylock_intent(&node.c.lock));
            six_unlock_read(&node.c.lock);
            assert!(crate::lock::six::six_trylock_write(&node.c.lock));
            path_mark_locked(&mut *path, 0, BTREE_NODE_WRITE_LOCKED);
            bch2_trans_unlock_write(&mut trans);
            assert_eq!(path_locked_type(&*path, 0), BTREE_NODE_INTENT_LOCKED);
            six_unlock_intent(&node.c.lock);
        }
    }

    #[test]
    fn transaction_unlock_write_updates_linked_path_sequences() {
        unsafe {
            let mut c = crate::btree::types::bch_fs::default();
            let mut trans = btree_trans::default();
            bch2_trans_init(&mut trans, &mut c);
            let mut node = crate::btree::types::btree::default();
            for idx in 1..=2usize {
                let path = trans.paths.add(idx);
                *path = btree_path {
                    ref_: 1,
                    level: 0,
                    locks_want: 1,
                    ..Default::default()
                };
                (*path).l[0].b = &mut node;
                path_mark_locked(&mut *path, 0, BTREE_NODE_WRITE_LOCKED);
                trans.paths_allocated |= 1u64 << idx;
            }
            assert!(crate::lock::six::six_trylock_read(&node.c.lock));
            assert!(crate::lock::six::six_trylock_intent(&node.c.lock));
            crate::lock::six::six_unlock_read(&node.c.lock);
            assert!(crate::lock::six::six_trylock_write(&node.c.lock));
            six_lock_increment(&node.c.lock, six_lock_type::SIX_LOCK_write);

            bch2_trans_unlock_write(&mut trans);

            let seq = six_lock_seq(&node.c.lock);
            for idx in 1..=2usize {
                let path = trans.paths.add(idx);
                assert_eq!(path_locked_type(&*path, 0), BTREE_NODE_INTENT_LOCKED);
                assert_eq!((*path).l[0].lock_seq, seq);
            }
            bch2_trans_unlock(&mut trans);
        }
    }

    #[test]
    fn transaction_unlock_releases_unreferenced_allocated_path() {
        unsafe {
            let mut c = crate::btree::types::bch_fs::default();
            let mut trans = btree_trans::default();
            bch2_trans_init(&mut trans, &mut c);
            let mut node = crate::btree::types::btree::default();
            let path = trans.paths.add(1);
            (*path).l[0].b = &mut node;
            (*trans.paths.add(1)).ref_ = 0;
            trans.paths_allocated |= 1u64 << 1;
            assert_eq!(six_lock_read(&node.c.lock), 0);
            path_mark_locked(&mut *path, 0, BTREE_NODE_READ_LOCKED);

            bch2_trans_unlock(&mut trans);

            assert_eq!(path_locked_type(&*path, 0), BTREE_NODE_UNLOCKED);
            assert!((*path).l[0].b.is_null());
        }
    }

    #[test]
    fn transaction_relock_restores_unreferenced_preserved_path() {
        unsafe {
            let mut c = crate::btree::types::bch_fs::default();
            let mut trans = btree_trans::default();
            bch2_trans_init(&mut trans, &mut c);
            let mut node = crate::btree::types::btree::default();
            let path = trans.paths.add(1);
            *path = btree_path {
                ref_: 0,
                should_be_locked: true,
                level: 0,
                locks_want: 1,
                ..Default::default()
            };
            (*path).l[0].b = &mut node;
            trans.paths_allocated |= 1u64 << 1;

            assert_eq!(bch2_trans_relock_notrace(&mut trans), 0);
            assert_eq!(path_locked_type(&*path, 0), BTREE_NODE_INTENT_LOCKED);
            btree_path_unlock(path);
        }
    }

    #[test]
    fn node_upgrade_relocks_read_path() {
        unsafe {
            let mut c = crate::btree::types::bch_fs::default();
            let mut trans = btree_trans::default();
            bch2_trans_init(&mut trans, &mut c);
            let mut node = crate::btree::types::btree::default();
            let path = trans.paths.add(1);
            *path = btree_path {
                ref_: 1,
                level: 0,
                locks_want: 0,
                ..Default::default()
            };
            (*path).l[0].b = &mut node;
            trans.paths_allocated |= 1u64 << 1;

            assert!(bch2_btree_node_upgrade(&mut trans, path, 0));
            assert_eq!(path_locked_type(&*path, 0), BTREE_NODE_READ_LOCKED);
            btree_path_unlock(path);
        }
    }

    #[test]
    fn path_upgrade_norestart_raises_lock_demand() {
        unsafe {
            let mut c = crate::btree::types::bch_fs::default();
            let mut trans = btree_trans::default();
            bch2_trans_init(&mut trans, &mut c);
            let mut node = crate::btree::types::btree::default();
            let path = trans.paths.add(1);
            *path = btree_path {
                ref_: 1,
                level: 0,
                locks_want: 0,
                ..Default::default()
            };
            (*path).l[0].b = &mut node;
            trans.paths_allocated |= 1u64 << 1;
            assert!(six_lock_read(&node.c.lock) == 0);
            path_mark_locked(&mut *path, 0, BTREE_NODE_READ_LOCKED);

            assert!(bch2_btree_path_upgrade_norestart(&mut trans, path, 1));
            assert_eq!((*path).locks_want, 1);
            assert_eq!(path_locked_type(&*path, 0), BTREE_NODE_INTENT_LOCKED);
            btree_path_unlock(path);
        }
    }

    #[test]
    fn transaction_begin_preserves_unused_path_during_restart() {
        unsafe {
            let mut c = crate::btree::types::bch_fs::default();
            let mut trans = btree_trans::default();
            bch2_trans_init(&mut trans, &mut c);
            let path = trans.paths.add(1);
            *path = btree_path {
                btree_id: 0,
                preserve: true,
                ..Default::default()
            };
            trans.paths_allocated |= 1u64 << 1;
            trans.restarted = 1;

            bch2_trans_begin(&mut trans);

            assert_ne!(trans.paths_allocated & (1u64 << 1), 0);
            assert_eq!((*trans.paths.add(1)).preserve, false);
        }
    }

    #[test]
    fn copy_iter_keeps_path_reference_until_both_exit() {
        unsafe {
            let mut c = crate::btree::types::bch_fs::default();
            let mut trans = btree_trans::default();
            bch2_trans_init(&mut trans, &mut c);
            let mut src = btree_iter::default();
            bch2_trans_iter_init(&mut trans, &mut src, 0, SPOS(1, 1, 0), BTREE_ITER_intent);
            let path = src.path;
            let refs_before = (*trans.paths.add(path as usize)).ref_;
            let mut dst = btree_iter::default();
            bch2_trans_copy_iter(&mut dst, &mut src);
            assert_eq!((*trans.paths.add(path as usize)).ref_, refs_before + 1);
            assert_eq!(dst.key_cache_path, 0);
            bch2_trans_iter_exit(&mut src);
            assert_eq!((*trans.paths.add(path as usize)).ref_, refs_before);
            bch2_trans_iter_exit(&mut dst);
            assert_eq!(trans.paths_allocated & (1u64 << path), 0);
        }
    }

    struct owned_node {
        node: Box<btree>,
        words: Vec<u64>,
    }

    impl owned_node {
        unsafe fn leaf(min: bpos, max: bpos, offsets: &[u64]) -> Self {
            let mut words = vec![0u64; 80];
            let mut node = Box::new(btree::default());
            node.data = words.as_mut_ptr().cast::<disk_btree_node>();
            node.format = BKEY_FORMAT_CURRENT;
            node.nsets = 1;
            node.c.level = 0;
            (*node.data).min_key = min;
            (*node.data).max_key = max;
            let set = words.as_mut_ptr().add(17).cast::<disk_bset>();
            (*set).u64s = (offsets.len() * 5) as u16;
            for (i, offset) in offsets.iter().enumerate() {
                *words.as_mut_ptr().add(20 + i * 5).cast::<bkey>() = bkey {
                    u64s: BKEY_U64S,
                    format: KEY_FORMAT_CURRENT,
                    type_: 6,
                    p: SPOS(1, *offset, 0),
                    ..Default::default()
                };
            }
            node.set[0] = bset_tree {
                size: 0,
                extra: BSET_NO_AUX_TREE_VAL,
                data_offset: 17,
                aux_data_offset: u16::MAX,
                end_offset: (20 + offsets.len() * 5) as u16,
            };
            Self { node, words }
        }

        unsafe fn interior(children: &[(*mut btree, bpos, bpos)]) -> Self {
            let mut words = vec![0u64; 100];
            let mut node = Box::new(btree::default());
            node.data = words.as_mut_ptr().cast::<disk_btree_node>();
            node.format = BKEY_FORMAT_CURRENT;
            node.nsets = 1;
            node.c.level = 1;
            node.key.k.p = SPOS_MAX;
            (*node.data).min_key = POS_MIN;
            (*node.data).max_key = SPOS_MAX;
            let set = words.as_mut_ptr().add(17).cast::<disk_bset>();
            (*set).u64s = (children.len() * 10) as u16;
            for (i, (child, min, max)) in children.iter().enumerate() {
                *words
                    .as_mut_ptr()
                    .add(20 + i * 10)
                    .cast::<bkey_i_btree_ptr_v2>() = bkey_i_btree_ptr_v2 {
                    k: bkey {
                        u64s: 10,
                        format: KEY_FORMAT_CURRENT,
                        type_: KEY_TYPE_btree_ptr_v2,
                        p: *max,
                        ..Default::default()
                    },
                    v: crate::btree::bset::bch_btree_ptr_v2 {
                        mem_ptr: *child as usize as u64,
                        min_key: *min,
                        ..Default::default()
                    },
                };
            }
            node.set[0] = bset_tree {
                size: 0,
                extra: BSET_NO_AUX_TREE_VAL,
                data_offset: 17,
                aux_data_offset: u16::MAX,
                end_offset: (20 + children.len() * 10) as u16,
            };
            Self { node, words }
        }
    }

    #[test]
    fn iter_flags_match_local_btree_property_normalization() {
        unsafe {
            let extent_flags = bch2_btree_iter_flags(core::ptr::null(), 0, 0, 0);
            assert_ne!(extent_flags & BTREE_ITER_is_extents, 0);
            assert_ne!(extent_flags & BTREE_ITER_filter_snapshots, 0);
            assert_eq!(
                bch2_btree_iter_flags(core::ptr::null(), 0, 0, BTREE_ITER_not_extents)
                    & BTREE_ITER_is_extents,
                0
            );
            let cached_flags = bch2_btree_iter_flags(core::ptr::null(), 1, 0, 0);
            assert_ne!(cached_flags & BTREE_ITER_with_key_cache, 0);
            assert_eq!(
                bch2_btree_iter_flags(core::ptr::null(), 4, 1, BTREE_ITER_cached)
                    & BTREE_ITER_cached,
                0
            );
        }
    }

    #[test]
    fn traverses_root_and_advances_across_leaf_nodes() {
        unsafe {
            let mut left = owned_node::leaf(POS_MIN, SPOS(1, 50, 0), &[10, 20]);
            let mut right = owned_node::leaf(SPOS(1, 51, 0), SPOS_MAX, &[60, 70]);
            let mut root = owned_node::interior(&[
                (&mut *left.node, POS_MIN, SPOS(1, 50, 0)),
                (&mut *right.node, SPOS(1, 51, 0), SPOS_MAX),
            ]);
            let mut c = bch_fs::default();
            bch2_btree_id_root_set(&mut c, 0, &mut *root.node);

            let mut trans = btree_trans::default();
            bch2_trans_init(&mut trans, &mut c);
            let mut iter = btree_iter::default();
            bch2_trans_iter_init(&mut trans, &mut iter, 0, SPOS(1, 0, 0), 0);

            let mut seen = Vec::new();
            let mut k = bch2_btree_iter_peek(&mut iter);
            while !k.k.is_null() {
                let p = (*k.k).p;
                seen.push(p.offset);
                k = bch2_btree_iter_next(&mut iter);
            }
            assert_eq!(seen, [10, 20, 60, 70]);
            bch2_trans_iter_exit(&mut iter);
            let _keep_buffers_alive = (&left.words, &right.words, &root.words);
        }
    }
}
