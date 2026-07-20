use crate::bcachefs::*;
use crate::bcachefs_format::*;
use crate::btree::types::*;
use crate::errcode::*;
use core::sync::atomic::{AtomicU64, Ordering};

/// btree node locked type — mirrors C enum btree_node_locked_type
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i8)]
pub enum BtreeNodeLockedType {
    Unlocked = -1,
    ReadLocked = 0,
    IntentLocked = 1,
    WriteLocked = 2,
}

#[derive(Clone, Copy, Debug)]
pub struct SixLock {
    state: AtomicU64,
    readers: u32,
    seq: u32,
    write_lock_recurse: u32,
}

impl SixLock {
    pub const fn new() -> Self {
        SixLock {
            state: AtomicU64::new(0),
            readers: 0,
            seq: 0,
            write_lock_recurse: 0,
        }
    }

    pub fn try_read(&self) -> bool {
        self.state.fetch_add(1, Ordering::Acquire) & (1 << 31) == 0
    }

    pub fn try_intent(&self) -> bool {
        self.state
            .fetch_update(Ordering::Acquire, Ordering::Relaxed, |s| {
                if s & (3 << 30) == 0 {
                    Some(s + (1 << 30))
                } else {
                    None
                }
            })
            .is_ok()
    }

    pub fn try_write(&self) -> bool {
        self.state
            .fetch_update(Ordering::Acquire, Ordering::Relaxed, |s| {
                if s == 0 {
                    Some(s | (3 << 30))
                } else {
                    None
                }
            })
            .is_ok()
    }

    pub fn read_lock(&self) {
        loop {
            if self.state.fetch_add(1, Ordering::Acquire) & (1 << 31) == 0 {
                return;
            }
            self.state.fetch_sub(1, Ordering::Relaxed);
            while self.state.load(Ordering::Relaxed) & (1 << 31) != 0 {
                core::hint::spin_loop();
            }
        }
    }

    pub fn unlock_read(&self) {
        self.state.fetch_sub(1, Ordering::Release);
    }

    pub fn unlock_intent(&self) {
        self.state.fetch_sub(1 << 30, Ordering::Release);
    }

    pub fn unlock_write(&self) {
        self.state.fetch_and(!(3 << 30), Ordering::Release);
    }

    pub fn seq(&self) -> u32 {
        self.state.load(Ordering::Relaxed) as u32
    }

    pub fn increment(&self, ty: SixLockType) {
        match ty {
            SixLockType::Read => {
                self.state.fetch_add(1, Ordering::Acquire);
            }
            SixLockType::Intent => {
                self.state.fetch_add(1 << 30, Ordering::Acquire);
            }
            SixLockType::Write => {
                self.state.fetch_add(3 << 30, Ordering::Acquire);
            }
        }
    }

    pub fn try_upgrade(&self) -> bool {
        self.state
            .fetch_update(Ordering::Acquire, Ordering::Relaxed, |s| {
                if s & (1 << 30) == 0 && s & (1 << 31) == 0 {
                    Some(s | (2 << 30))
                } else {
                    None
                }
            })
            .is_ok()
    }

    pub fn downgrade(&self) {
        self.state.fetch_sub(2 << 30, Ordering::Release);
    }
}

/// Check if path level has a locked btree node
pub fn is_btree_node(path: &BtreePath, l: usize) -> bool {
    l < BTREE_MAX_DEPTH && !path.l[l].b.is_null()
}

/// Get the lock type held by path at a given level
pub fn btree_node_locked_type(path: &BtreePath, level: usize) -> BtreeNodeLockedType {
    let v = (path.nodes_locked >> (level << 1)) & 3;
    match v {
        0 => BtreeNodeLockedType::Unlocked,
        1 => BtreeNodeLockedType::ReadLocked,
        2 => BtreeNodeLockedType::IntentLocked,
        3 => BtreeNodeLockedType::WriteLocked,
        _ => BtreeNodeLockedType::Unlocked,
    }
}

/// Check if lock is held (any type)
pub fn btree_node_locked(path: &BtreePath, level: usize) -> bool {
    btree_node_locked_type(path, level) != BtreeNodeLockedType::Unlocked
}

pub fn btree_node_write_locked(path: &BtreePath, level: usize) -> bool {
    btree_node_locked_type(path, level) == BtreeNodeLockedType::WriteLocked
}

pub fn btree_node_intent_locked(path: &BtreePath, level: usize) -> bool {
    btree_node_locked_type(path, level) == BtreeNodeLockedType::IntentLocked
}

pub fn btree_node_read_locked(path: &BtreePath, level: usize) -> bool {
    btree_node_locked_type(path, level) == BtreeNodeLockedType::ReadLocked
}

/// Compute desired lock type for a path at a given level
pub fn __btree_lock_want(path: &BtreePath, level: usize) -> SixLockType {
    if level < path.locks_want as usize {
        SixLockType::Intent
    } else {
        SixLockType::Read
    }
}

pub fn btree_lock_want(path: &BtreePath, level: usize) -> BtreeNodeLockedType {
    let level_u = level as u8;
    if level_u < path.level {
        return BtreeNodeLockedType::Unlocked;
    }
    if level_u < path.locks_want {
        return BtreeNodeLockedType::IntentLocked;
    }
    if level_u == path.level {
        return BtreeNodeLockedType::ReadLocked;
    }
    BtreeNodeLockedType::Unlocked
}

/// Mark a path as having a node locked (without resetting timestamp)
pub fn mark_btree_node_locked_noreset(path: &mut BtreePath, level: usize, ty: BtreeNodeLockedType) {
    let mask = !(3u8 << (level << 1));
    let val = (ty as u8 + 1) << (level << 1);
    path.nodes_locked = (path.nodes_locked & mask) | val;
}

pub fn mark_btree_node_locked(path: &mut BtreePath, level: usize, ty: BtreeNodeLockedType) {
    mark_btree_node_locked_noreset(path, level, ty);
}

/// Get the lowest level locked in a path
pub fn btree_path_lowest_level_locked(path: &BtreePath) -> usize {
    (path.nodes_locked.trailing_zeros() >> 1) as usize
}

/// Get the highest level locked in a path
pub fn btree_path_highest_level_locked(path: &BtreePath) -> usize {
    let fls = 7usize.wrapping_sub(path.nodes_locked.leading_zeros() as usize);
    fls >> 1
}

/// Unlock a specific level
pub fn btree_node_unlock(trans: &mut BtreeTrans, path: &mut BtreePath, level: usize) {
    let lock_type = btree_node_locked_type(path, level);
    if lock_type != BtreeNodeLockedType::Unlocked {
        let ty = if lock_type == BtreeNodeLockedType::WriteLocked {
            SixLockType::Intent
        } else {
            match lock_type {
                BtreeNodeLockedType::ReadLocked => SixLockType::Read,
                BtreeNodeLockedType::IntentLocked => SixLockType::Intent,
                _ => SixLockType::Read,
            }
        };
        if let Some(b) = path.l[level].b.as_mut() {
            b.c.lock.unlock(ty);
        }
        mark_btree_node_locked_noreset(path, level, BtreeNodeLockedType::Unlocked);
    }
}

/// Unlock all levels
pub fn __bch2_btree_path_unlock(trans: &mut BtreeTrans, path: &mut BtreePath) {
    while path.nodes_locked != 0 {
        let l = btree_path_lowest_level_locked(path);
        btree_node_unlock(trans, path, l);
    }
}

pub fn btree_node_lock_seq_matches(path: &BtreePath, b: &BtreeNode, level: usize) -> bool {
    path.l[level].lock_seq == b.c.lock.seq()
}

fn lock_type_conflicts(t1: SixLockType, t2: SixLockType) -> bool {
    (t1 as i32 + t2 as i32) > 1
}

/// Try to lock a btree node (fast path)
pub fn btree_node_lock(
    trans: &mut BtreeTrans,
    path: &mut BtreePath,
    b: &mut BtreeNode,
    level: usize,
    ty: SixLockType,
) -> Result<(), BchError> {
    if b.c.lock.try_lock(ty) {
        return Ok(());
    }
    btree_node_lock_slowpath(trans, path, b, level, ty)
}

/// Slow path lock acquisition with deadlock detection
fn btree_node_lock_slowpath(
    trans: &mut BtreeTrans,
    path: &mut BtreePath,
    b: &mut BtreeNode,
    level: usize,
    ty: SixLockType,
) -> Result<(), BchError> {
    if btree_node_lock_increment(trans, b, level, ty) {
        return Ok(());
    }
    b.c.lock.lock(ty);
    Ok(())
}

/// Check if another path in the same trans already has the lock — increment it
fn btree_node_lock_increment(
    trans: &BtreeTrans,
    b: &BtreeNode,
    level: usize,
    want: SixLockType,
) -> bool {
    for path in &trans.paths {
        if !path.l[level].b.is_null()
            && path.l[level].b.as_ptr() as *const _ == b as *const _
            && btree_node_locked_type(path, level) as i8 >= want as i8
        {
            unsafe {
                (*path.l[level].b.as_ptr()).c.lock.increment(want);
            }
            return true;
        }
    }
    false
}

/// Write lock acquisition — must hold intent lock first
pub fn __btree_node_lock_write(
    trans: &mut BtreeTrans,
    path: &mut BtreePath,
    b: &mut BtreeNode,
) -> Result<(), BchError> {
    mark_btree_node_locked_noreset(path, b.c.level as usize, BtreeNodeLockedType::WriteLocked);
    if b.c.lock.try_write() {
        Ok(())
    } else {
        bch2_btree_node_lock_write_contended(trans, path, b, false)
    }
}

pub fn bch2_btree_node_lock_write(
    trans: &mut BtreeTrans,
    path: &mut BtreePath,
    b: &mut BtreeNode,
) -> Result<(), BchError> {
    __btree_node_lock_write(trans, path, b)
}

pub fn bch2_btree_node_lock_write_nofail(
    trans: &mut BtreeTrans,
    path: &mut BtreePath,
    b: &mut BtreeNode,
) {
    let _ = __btree_node_lock_write(trans, path, b);
}

/// Release a write lock back to intent
pub fn __bch2_btree_node_unlock_write(trans: &mut BtreeTrans, b: &mut BtreeNode) {
    for path in &mut trans.paths {
        if !path.l[b.c.level as usize].b.is_null()
            && path.l[b.c.level as usize].b.as_ptr() as *const _ == b as *const _
        {
            path.l[b.c.level as usize].lock_seq += 1;
        }
    }
    b.c.lock.unlock_write();
}

pub fn bch2_btree_node_unlock_write_inlined(
    trans: &mut BtreeTrans,
    path: &mut BtreePath,
    b: &mut BtreeNode,
) {
    let level = b.c.level as usize;
    mark_btree_node_locked_noreset(path, level, BtreeNodeLockedType::IntentLocked);
    __bch2_btree_node_unlock_write(trans, b);
}

/// Write lock contended path
pub fn bch2_btree_node_lock_write_contended(
    trans: &mut BtreeTrans,
    path: &mut BtreePath,
    b: &mut BtreeNode,
    lock_may_not_fail: bool,
) -> Result<(), BchError> {
    b.c.lock.lock_write();
    Ok(())
}

/// Re-acquire a lock after restart (relock)
pub fn __bch2_btree_node_relock(
    trans: &mut BtreeTrans,
    path: &mut BtreePath,
    level: usize,
) -> bool {
    let b_ptr = path.l[level].b;
    if b_ptr.is_null() {
        return false;
    }
    let want = __btree_lock_want(path, level);
    let lock_seq = path.l[level].lock_seq;
    let b = unsafe { &mut *b_ptr.as_ptr() };
    if b.c.lock.relock(want, lock_seq) {
        mark_btree_node_locked(path, level, match want {
            SixLockType::Read => BtreeNodeLockedType::ReadLocked,
            SixLockType::Intent => BtreeNodeLockedType::IntentLocked,
            SixLockType::Write => BtreeNodeLockedType::WriteLocked,
        });
        return true;
    }
    if btree_node_lock_seq_matches(path, b, level)
        && btree_node_lock_increment(trans, b, level, want)
    {
        mark_btree_node_locked(path, level, match want {
            SixLockType::Read => BtreeNodeLockedType::ReadLocked,
            SixLockType::Intent => BtreeNodeLockedType::IntentLocked,
            SixLockType::Write => BtreeNodeLockedType::WriteLocked,
        });
        return true;
    }
    false
}

pub fn bch2_btree_node_relock(
    trans: &mut BtreeTrans,
    path: &mut BtreePath,
    level: usize,
) -> bool {
    if btree_node_locked(path, level) {
        return true;
    }
    if path.l[level].b.is_null() {
        return false;
    }
    __bch2_btree_node_relock(trans, path, level)
}

/// Upgrade a path's lock from read to intent
pub fn bch2_btree_node_upgrade(
    trans: &mut BtreeTrans,
    path: &mut BtreePath,
    level: usize,
) -> bool {
    let b_ptr = path.l[level].b;
    if b_ptr.is_null() {
        return false;
    }
    let b = unsafe { &mut *b_ptr.as_ptr() };
    if btree_node_intent_locked(path, level) {
        return true;
    }
    if !btree_node_locked(path, level) {
        if !__bch2_btree_node_relock(trans, path, level) {
            return false;
        }
        if btree_node_intent_locked(path, level) {
            return true;
        }
    }
    if b.c.lock.try_upgrade() {
        mark_btree_node_locked_noreset(path, level, BtreeNodeLockedType::IntentLocked);
        return true;
    }
    if btree_node_lock_seq_matches(path, b, level)
        && btree_node_lock_increment(trans, b, level, SixLockType::Intent)
    {
        btree_node_unlock(trans, path, level);
        mark_btree_node_locked_noreset(path, level, BtreeNodeLockedType::IntentLocked);
        return true;
    }
    false
}

/// Relock all levels of a path
pub fn bch2_btree_path_relock_norestart(
    trans: &mut BtreeTrans,
    path: &mut BtreePath,
) -> bool {
    let mut l = path.level as usize;
    loop {
        if !btree_path_node(path, l) {
            break;
        }
        if !bch2_btree_node_relock(trans, path, l) {
            break;
        }
        l += 1;
        if l >= path.locks_want as usize || l >= BTREE_MAX_DEPTH {
            break;
        }
    }
    // Check all levels up to locks_want
    let mut l = path.level as usize;
    loop {
        if !btree_path_node(path, l) {
            break;
        }
        if !btree_node_locked(path, l) {
            return false;
        }
        l += 1;
        if l >= path.locks_want as usize || l >= BTREE_MAX_DEPTH {
            break;
        }
    }
    true
}

/// Downgrade locks: release locks above new_locks_want
pub fn __bch2_btree_path_downgrade(
    trans: &mut BtreeTrans,
    path: &mut BtreePath,
    new_locks_want: u8,
) {
    let old = path.locks_want;
    path.locks_want = new_locks_want;
    for l in (path.level as usize..BTREE_MAX_DEPTH).rev() {
        if btree_node_locked(path, l) && !btree_node_write_locked(path, l) {
            let want = __btree_lock_want(path, l);
            if btree_node_locked_type(path, l) as u8 != want as u8 + 1 {
                btree_node_unlock(trans, path, l);
            }
        }
    }
}

/// Initialize six lock for a btree node
pub fn bch2_btree_lock_init(b: &mut BtreeBkeyCachedCommon) {
    // SixLock is always ready (no alloc needed)
}

/// Check if path node at level is valid
fn btree_path_node(path: &BtreePath, level: usize) -> bool {
    level < BTREE_MAX_DEPTH && !path.l[level].b.is_null()
}

impl SixLock {
    fn try_lock(&self, ty: SixLockType) -> bool {
        match ty {
            SixLockType::Read => self.try_read(),
            SixLockType::Intent => self.try_intent(),
            SixLockType::Write => self.try_write(),
        }
    }

    fn lock(&self, ty: SixLockType) {
        match ty {
            SixLockType::Read => self.read_lock(),
            SixLockType::Intent => {
                // intent: spin until acquired
                while !self.try_intent() {
                    core::hint::spin_loop();
                }
            }
            SixLockType::Write => {
                // write: spin until acquired
                while !self.try_write() {
                    core::hint::spin_loop();
                }
            }
        }
    }

    fn unlock(&self, ty: SixLockType) {
        match ty {
            SixLockType::Read => self.unlock_read(),
            SixLockType::Intent => self.unlock_intent(),
            SixLockType::Write => self.unlock_write(),
        }
    }

    fn lock_write(&self) {
        while !self.try_write() {
            core::hint::spin_loop();
        }
    }

    fn relock(&self, want: SixLockType, seq: u32) -> bool {
        if self.seq() != seq {
            return false;
        }
        self.try_lock(want)
    }
}

impl BtreeNodeLockedType {
    pub fn from_six(ty: SixLockType) -> Self {
        match ty {
            SixLockType::Read => BtreeNodeLockedType::ReadLocked,
            SixLockType::Intent => BtreeNodeLockedType::IntentLocked,
            SixLockType::Write => BtreeNodeLockedType::WriteLocked,
        }
    }
}
