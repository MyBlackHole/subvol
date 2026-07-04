use core::cmp::Ordering;

use super::bkey::{
    bch2_key_resize, bch_val, bkey, bkey_and_val_eq, bkey_bytes, bkey_copy, bkey_deleted, bkey_fields_eq, bkey_i,
    bkey_init, set_bkey_val_bytes,
    bkey_packed, bkey_s, bkey_s_c, bkey_val_bytes, bpos_cmp, bpos_eq, bpos_gt, bpos_le, bpos_lt,
    bpos_min,
    SPOS_MAX,
};
use super::bset_update::{bch2_bset_delete, bch2_bset_insert, btree_keys_account_key};
use super::iter::{
    bch2_btree_iter_advance, bch2_btree_iter_peek_max, bch2_btree_iter_peek_prev,
    bch2_btree_iter_peek_slot, bch2_btree_iter_set_pos, bch2_trans_iter_exit,
    bch2_trans_iter_init, btree_insert_entry, btree_iter, btree_path, btree_path_idx_t,
    btree_trans, btree_trans_commit_hook,
    btree_trans_subbuf,
    BTREE_ITER_all_snapshots, BTREE_ITER_cached, BTREE_ITER_intent, BTREE_ITER_nopreserve,
    BTREE_ITER_nofilter_whiteouts, BTREE_ITER_not_extents, BTREE_ITER_INITIAL,
    BTREE_NODE_INTENT_LOCKED,
};
use super::node_iter::{
    bch2_btree_node_iter_bset_pos, bch2_btree_node_iter_peek_all,
    bkey_unpack_pos,
};
use super::types::{
    bset_tree_last, btree, btree_bkey_first_offset, btree_current_write, btree_node_write_idx,
    journal_entry_pin,
};
use crate::lock::six::{
    six_lock_read, six_lock_type, six_lock_write, six_unlock_read,
    six_unlock_write,
};

const UPDATE_KEY_OWNED: usize = usize::MAX;
const BTREE_TRANS_MEM_MAX: u32 = 1 << 16;

#[allow(dead_code)]
unsafe fn need_whiteout_for_snapshot(
    trans: *mut btree_trans,
    btree_id: u8,
    mut pos: super::bkey::bpos,
) -> i32 {
    if trans.is_null() || (*trans).c.is_null() {
        return -22;
    }
    let c = &*(*trans).c;
    let snapshot = pos.snapshot;
    if crate::snapshot::bch2_snapshot_parent_early(c, snapshot) == 0 {
        return 0;
    }

    pos.snapshot = pos.snapshot.wrapping_add(1);
    let mut iter = btree_iter::default();
    bch2_trans_iter_init(
        trans,
        &mut iter,
        btree_id,
        pos,
        BTREE_ITER_all_snapshots | BTREE_ITER_nopreserve,
    );

    let mut ret = 0;
    loop {
        let k = bch2_btree_iter_peek_max(&mut iter, &SPOS_MAX);
        let err = super::bkey::bkey_err(k);
        if err != 0 {
            ret = err;
            break;
        }
        if k.k.is_null() || !bpos_eq((*k.k).p, pos) {
            break;
        }
        if crate::snapshot::bch2_snapshot_is_ancestor(c, snapshot, (*k.k).p.snapshot) {
            ret = if (*k.k).type_ == super::bset::KEY_TYPE_deleted
                || (*k.k).type_ == super::bset::KEY_TYPE_whiteout
            {
                0
            } else {
                1
            };
            break;
        }
        if !bch2_btree_iter_advance(&mut iter) {
            break;
        }
    }
    bch2_trans_iter_exit(&mut iter);
    ret
}

pub unsafe fn __bch2_trans_kmalloc(trans: *mut btree_trans, size: usize, zero: bool) -> *mut u8 {
    if trans.is_null() || size > BTREE_TRANS_MEM_MAX as usize {
        return core::ptr::null_mut();
    }
    let size = (size + 7) & !7;
    let top = (*trans).mem_top as usize;
    if top.saturating_add(size) <= (*trans).mem_bytes as usize {
        let p = (*trans).mem.add(top);
        (*trans).mem_top = (top + size) as u32;
        if zero {
            core::ptr::write_bytes(p, 0, size);
        }
        return p;
    }
    if (*trans).mem_bytes != 0 {
        (*trans).realloc_bytes_required = (top + size) as u32;
        return core::ptr::null_mut();
    }
    let new_bytes = (top + size).next_power_of_two();
    if new_bytes > BTREE_TRANS_MEM_MAX as usize {
        return core::ptr::null_mut();
    }
    let layout = match std::alloc::Layout::from_size_align(new_bytes, 8) {
        Ok(layout) => layout,
        Err(_) => return core::ptr::null_mut(),
    };
    let mem = if zero {
        std::alloc::alloc_zeroed(layout)
    } else {
        std::alloc::alloc(layout)
    };
    if mem.is_null() {
        return core::ptr::null_mut();
    }
    (*trans).mem = mem;
    (*trans).mem_bytes = new_bytes as u32;
    (*trans).mem_top = (top + size) as u32;
    mem.add(top)
}

pub unsafe fn bch2_trans_kmalloc(trans: *mut btree_trans, size: usize) -> *mut u8 {
    __bch2_trans_kmalloc(trans, size, true)
}

pub unsafe fn bch2_trans_kmalloc_nomemzero(trans: *mut btree_trans, size: usize) -> *mut u8 {
    __bch2_trans_kmalloc(trans, size, false)
}

pub unsafe fn bch2_trans_kmalloc_ip(
    trans: *mut btree_trans,
    size: usize,
    _ip: usize,
) -> *mut u8 {
    bch2_trans_kmalloc(trans, size)
}

pub unsafe fn bch2_trans_kmalloc_nomemzero_ip(
    trans: *mut btree_trans,
    size: usize,
    _ip: usize,
) -> *mut u8 {
    bch2_trans_kmalloc_nomemzero(trans, size)
}

unsafe fn __bch2_trans_subbuf_alloc(
    trans: *mut btree_trans,
    buf: *mut btree_trans_subbuf,
    u64s: u16,
) -> *mut u8 {
    let new_top = (*buf).u64s as usize + u64s as usize;
    if new_top > u16::MAX as usize {
        return core::ptr::null_mut();
    }
    let mut new_size = (*buf).size as usize;
    if new_top > new_size {
        new_size = new_top.next_power_of_two();
    }
    if new_size > u16::MAX as usize {
        return core::ptr::null_mut();
    }

    let old_u64s = (*buf).u64s as usize;
    let old_base = (*buf).base as usize;
    let base = if new_size != (*buf).size as usize || old_u64s == 0 {
        let storage = bch2_trans_kmalloc(trans, new_size * core::mem::size_of::<u64>());
        if storage.is_null() {
            return core::ptr::null_mut();
        }
        let base = storage.offset_from((*trans).mem) as usize / core::mem::size_of::<u64>();
        if old_u64s != 0 {
            core::ptr::copy_nonoverlapping(
                (*trans).mem.add(old_base * core::mem::size_of::<u64>()),
                storage,
                old_u64s * core::mem::size_of::<u64>(),
            );
        }
        base
    } else {
        old_base
    };

    (*buf).base = base as u16;
    (*buf).size = new_size as u16;
    let old_top = (*buf).u64s as usize;
    (*buf).u64s = old_top as u16;
    (*trans)
        .mem
        .add((base + old_top) * core::mem::size_of::<u64>())
}

pub unsafe fn bch2_trans_subbuf_alloc(
    trans: *mut btree_trans,
    buf: *mut btree_trans_subbuf,
    u64s: u16,
) -> *mut u8 {
    let p = __bch2_trans_subbuf_alloc(trans, buf, u64s);
    if !p.is_null() {
        (*buf).u64s += u64s;
    }
    p
}

pub unsafe fn bch2_trans_subbuf_alloc_ip(
    trans: *mut btree_trans,
    buf: *mut btree_trans_subbuf,
    u64s: u16,
    _ip: usize,
) -> *mut u8 {
    bch2_trans_subbuf_alloc(trans, buf, u64s)
}

pub unsafe fn bch2_trans_subbuf_reserve(
    trans: *mut btree_trans,
    buf: *mut btree_trans_subbuf,
    u64s: u16,
) -> i32 {
    if (*buf).u64s as usize + u64s as usize > (*buf).size as usize {
        if __bch2_trans_subbuf_alloc(trans, buf, u64s).is_null() {
            return -12;
        }
    }
    0
}

pub unsafe fn bch2_trans_jset_entry_alloc_ip(
    trans: *mut btree_trans,
    u64s: u16,
    _ip: usize,
) -> *mut crate::journal::jset_entry {
    let entry_u64s = crate::journal::jset_u64s(u64s as u32);
    if entry_u64s > u16::MAX as u32 {
        return core::ptr::null_mut();
    }
    let buf = &mut (*trans).journal_entries;
    let offset = buf.u64s as usize;
    let p = bch2_trans_subbuf_alloc(trans, buf, entry_u64s as u16);
    if p.is_null() {
        return core::ptr::null_mut();
    }
    (*trans)
        .mem
        .add((buf.base as usize + offset) * core::mem::size_of::<u64>())
        .cast::<crate::journal::jset_entry>()
}

pub unsafe fn bch2_trans_jset_entry_alloc(
    trans: *mut btree_trans,
    u64s: u16,
) -> *mut crate::journal::jset_entry {
    bch2_trans_jset_entry_alloc_ip(trans, u64s, 0)
}

pub unsafe fn bch2_bkey_make_mut_noupdate(
    trans: *mut btree_trans,
    k: bkey_s_c,
) -> *mut bkey_i {
    __bch2_bkey_make_mut_noupdate(trans, k, 0)
}

pub unsafe fn __bch2_bkey_make_mut_noupdate(
    trans: *mut btree_trans,
    k: bkey_s_c,
    min_bytes: usize,
) -> *mut bkey_i {
    if trans.is_null() || k.k.is_null() {
        return core::ptr::null_mut();
    }
    let old_bytes = bkey_bytes(&*k.k);
    let bytes = old_bytes.max(min_bytes);
    let mut_ = bch2_trans_kmalloc_nomemzero(trans, bytes.saturating_add(8)) as *mut bkey_i;
    if mut_.is_null() {
        return core::ptr::null_mut();
    }
    core::ptr::copy_nonoverlapping(k.k, &mut (*mut_).k, 1);
    let value_bytes = bkey_val_bytes(&*k.k);
    if value_bytes != 0 && !k.v.is_null() {
        core::ptr::copy_nonoverlapping(
            k.v.cast::<u8>(),
            (mut_.cast::<u8>()).add(core::mem::size_of::<bkey>()),
            value_bytes,
        );
    }
    if bytes > old_bytes {
        core::ptr::write_bytes(
            mut_.cast::<u8>().add(old_bytes),
            0,
            bytes - old_bytes,
        );
        (*mut_).k.u64s = bytes.div_ceil(core::mem::size_of::<u64>()) as u8;
    }
    mut_
}

pub unsafe fn bch2_bkey_get_mut_noupdate(iter: *mut btree_iter) -> *mut bkey_i {
    if iter.is_null() || (*iter).trans.is_null() {
        return core::ptr::null_mut();
    }
    let current = super::iter::bch2_btree_iter_peek_slot(iter);
    if super::bkey::bkey_err(current) != 0 || current.k.is_null() {
        return core::ptr::null_mut();
    }
    __bch2_bkey_make_mut_noupdate((*iter).trans, current, 0)
}

pub unsafe fn bch2_bkey_make_mut(
    trans: *mut btree_trans,
    iter: *mut btree_iter,
    current: *mut bkey_s_c,
    flags: u32,
) -> *mut bkey_i {
    if trans.is_null() || iter.is_null() || current.is_null() {
        return core::ptr::null_mut();
    }
    let mut_ = bch2_bkey_make_mut_noupdate(trans, *current);
    if mut_.is_null() || bch2_trans_update(trans, iter, mut_, flags) != 0 {
        return core::ptr::null_mut();
    }
    *current = bkey_s_c {
        k: &(*mut_).k,
        v: &(*mut_).v,
    };
    mut_
}

pub unsafe fn bch2_bkey_get_mut(
    trans: *mut btree_trans,
    btree_id: u8,
    pos: super::bkey::bpos,
    flags: u32,
) -> *mut bkey_i {
    bch2_bkey_get_mut_minsize(trans, btree_id, pos, flags, 0)
}

pub unsafe fn bch2_bkey_get_mut_minsize(
    trans: *mut btree_trans,
    btree_id: u8,
    pos: super::bkey::bpos,
    flags: u32,
    min_bytes: usize,
) -> *mut bkey_i {
    __bch2_bkey_get_mut(trans, btree_id, pos, flags, 0, min_bytes)
}

pub unsafe fn __bch2_bkey_get_mut(
    trans: *mut btree_trans,
    btree_id: u8,
    pos: super::bkey::bpos,
    flags: u32,
    type_: u8,
    min_bytes: usize,
) -> *mut bkey_i {
    if trans.is_null() {
        return core::ptr::null_mut();
    }
    let mut iter = btree_iter::default();
    bch2_trans_iter_init(
        trans,
        &mut iter,
        btree_id,
        pos,
        BTREE_ITER_intent | flags as u16,
    );
    let ret = if super::iter::bch2_btree_iter_traverse(&mut iter) != 0 {
        core::ptr::null_mut()
    } else {
        let current = super::iter::bch2_btree_iter_peek_slot(&mut iter);
        let mut_ = if super::bkey::bkey_err(current) != 0 || current.k.is_null()
            || (type_ != 0 && (*current.k).type_ != type_)
        {
            core::ptr::null_mut()
        } else {
            __bch2_bkey_make_mut_noupdate(trans, current, min_bytes)
        };
        if mut_.is_null() || bch2_trans_update(trans, &mut iter, mut_, flags) != 0 {
            core::ptr::null_mut()
        } else {
            mut_
        }
    };
    bch2_trans_iter_exit(&mut iter);
    ret
}

pub unsafe fn bch2_bkey_alloc(
    trans: *mut btree_trans,
    iter: *mut btree_iter,
    flags: u32,
    type_: u8,
    val_size: usize,
) -> *mut bkey_i {
    if trans.is_null() || iter.is_null() {
        return core::ptr::null_mut();
    }
    let k = bch2_trans_kmalloc(trans, core::mem::size_of::<bkey_i>() + val_size)
        as *mut bkey_i;
    if k.is_null() {
        return core::ptr::null_mut();
    }
    bkey_init(&mut (*k).k);
    (*k).k.p = (*iter).pos;
    (*k).k.type_ = type_;
    set_bkey_val_bytes(&mut (*k).k, val_size as u32);
    if bch2_trans_update(trans, iter, k, flags) != 0 {
        return core::ptr::null_mut();
    }
    k
}

unsafe fn bch2_trans_free_owned_key(i: *mut btree_insert_entry) {
    if (*i).ip_allocated == UPDATE_KEY_OWNED && !(*i).k.is_null() {
        drop(Box::from_raw((*i).k));
        (*i).k = core::ptr::null_mut();
        (*i).ip_allocated = 0;
    }
}

pub const BTREE_UPDATE_none: u32 = 0;
pub const BTREE_UPDATE_internal_snapshot_node: u32 = 1 << 18;
pub const BTREE_UPDATE_nojournal: u32 = 1 << 19;
pub const BTREE_TRIGGER_norun: u32 = 1 << 21;
pub const BTREE_TRIGGER_transactional: u32 = 1 << 22;
pub const BTREE_TRIGGER_atomic: u32 = 1 << 23;
pub const BTREE_TRIGGER_gc: u32 = 1 << 24;
pub const BTREE_TRIGGER_insert: u32 = 1 << 25;
pub const BTREE_TRIGGER_overwrite: u32 = 1 << 26;

pub const BCH_TRANS_COMMIT_no_enospc: u32 = 1 << (crate::journal::BCH_WATERMARK_BITS + 0);
pub const BCH_TRANS_COMMIT_no_check_rw: u32 = 1 << (crate::journal::BCH_WATERMARK_BITS + 1);
pub const BCH_TRANS_COMMIT_no_journal_res: u32 = 1 << (crate::journal::BCH_WATERMARK_BITS + 2);
pub const BCH_TRANS_COMMIT_no_skip_noops: u32 = 1 << (crate::journal::BCH_WATERMARK_BITS + 3);
pub const BCH_TRANS_COMMIT_journal_reclaim: u32 = 1 << (crate::journal::BCH_WATERMARK_BITS + 4);
pub const BCH_TRANS_COMMIT_journal_replay: u32 = 1 << (crate::journal::BCH_WATERMARK_BITS + 5);
pub const BCH_TRANS_COMMIT_skip_accounting_apply: u32 =
    1 << (crate::journal::BCH_WATERMARK_BITS + 6);

unsafe fn extent_whiteout_type(
    c: *mut super::types::bch_fs,
    btree_id: u8,
    k: *const super::bkey::bkey,
) -> u8 {
    if super::types::btree_id_is_extents_snapshots(btree_id)
        && (*k).type_ == super::bset::KEY_TYPE_deleted
        && crate::snapshot::bch2_snapshot_is_leaf(&*c, (*k).p.snapshot) == 1
    {
        super::bset::KEY_TYPE_extent_whiteout
    } else {
        super::bset::KEY_TYPE_whiteout
    }
}

pub unsafe fn bch2_insert_snapshot_whiteouts(
    trans: *mut btree_trans,
    btree_id: u8,
    old_pos: super::bkey::bpos,
    new_pos: super::bkey::bpos,
) -> i32 {
    assert_eq!(old_pos.snapshot, new_pos.snapshot);
    let mut s = crate::snapshot::snapshot_id_list::default();
    let ret = crate::snapshot::__bch2_get_snapshot_overwrites(trans, btree_id, old_pos, &mut s);
    if ret != 0 {
        return ret;
    }
    if s.nr == 0 {
        return 0;
    }
    __bch2_insert_snapshot_whiteouts(trans, btree_id, new_pos, &s)
}

pub unsafe fn bch2_trans_update_extent_overwrite(
    trans: *mut btree_trans,
    iter: *mut btree_iter,
    flags: u32,
    old: bkey_s_c,
    new: bkey_s_c,
) -> i32 {
    let btree_id = (*iter).btree_id;
    let new_start = super::bkey::bkey_start_pos(&*new.k);
    let front_split = bpos_lt(super::bkey::bkey_start_pos(&*old.k), new_start);
    let back_split = bpos_gt((*old.k).p, (*new.k).p);
    let middle_split = (front_split || back_split) && (*old.k).p.snapshot != (*new.k).p.snapshot;
    let nr_splits = front_split as u32 + back_split as u32 + middle_split as u32;
    if nr_splits > 1 {
        let compressed = super::bset::bch2_bkey_sectors_compressed((*trans).c, old);
        (*trans).extra_disk_res += compressed as u64 * (nr_splits - 1) as u64;
    }

    if front_split {
        let update = bch2_bkey_make_mut_noupdate(trans, old);
        if update.is_null() {
            return -12;
        }
        super::bset::bch2_cut_back(new_start, update);
        let ret = bch2_insert_snapshot_whiteouts(trans, btree_id, (*old.k).p, (*update).k.p);
        if ret != 0 {
            return ret;
        }
        let ret = bch2_btree_insert_nonextent(
            trans,
            btree_id,
            update,
            (*update).k.u64s,
            BTREE_UPDATE_internal_snapshot_node | flags,
        );
        if ret != 0 {
            return ret;
        }
    }

    if middle_split {
        let update = bch2_bkey_make_mut_noupdate(trans, old);
        if update.is_null() {
            return -12;
        }
        super::bset::bch2_cut_front((*trans).c, new_start, update);
        super::bset::bch2_cut_back((*new.k).p, update);
        let ret = bch2_insert_snapshot_whiteouts(trans, btree_id, (*old.k).p, (*update).k.p);
        if ret != 0 {
            return ret;
        }
        let ret = bch2_btree_insert_nonextent(
            trans,
            btree_id,
            update,
            (*update).k.u64s,
            BTREE_UPDATE_internal_snapshot_node | flags,
        );
        if ret != 0 {
            return ret;
        }
    }

    if !back_split {
        let update = bch2_trans_kmalloc(trans, core::mem::size_of::<bkey_i>()) as *mut bkey_i;
        if update.is_null() {
            return -12;
        }
        bkey_init(&mut (*update).k);
        (*update).k.p = (*old.k).p;
        (*update).k.p.snapshot = (*new.k).p.snapshot;
        if super::types::btree_type_has_snapshots(btree_id) {
            let ret = if (*new.k).p.snapshot != (*old.k).p.snapshot {
                1
            } else {
                need_whiteout_for_snapshot(trans, btree_id, (*update).k.p)
            };
            if ret < 0 {
                return ret;
            }
            if ret != 0 {
                (*update).k.type_ = extent_whiteout_type((*trans).c, btree_id, new.k);
            }
        }
        return bch2_btree_insert_nonextent(
            trans,
            btree_id,
            update,
            (*update).k.u64s,
            BTREE_UPDATE_internal_snapshot_node | flags,
        );
    }

    let update = bch2_bkey_make_mut_noupdate(trans, old);
    if update.is_null() {
        return -12;
    }
    super::bset::bch2_cut_front((*trans).c, (*new.k).p, update);
    if btree_trans_update_by_path(
        trans,
        (*iter).path,
        update,
        (*update).k.u64s,
        BTREE_UPDATE_internal_snapshot_node | flags,
        0,
    )
    .is_null()
    {
        return -12;
    }
    0
}

pub unsafe fn __bch2_insert_snapshot_whiteouts(
    trans: *mut btree_trans,
    btree_id: u8,
    mut pos: super::bkey::bpos,
    s: *const crate::snapshot::snapshot_id_list,
) -> i32 {
    let mut i = 0usize;
    while i < (*s).nr {
        pos.snapshot = *(*s).data.add(i);
        let mut iter = btree_iter::default();
        bch2_trans_iter_init(
            trans,
            &mut iter,
            btree_id,
            pos,
            BTREE_ITER_not_extents | BTREE_ITER_intent,
        );
        let k = super::iter::bch2_btree_iter_peek_slot(&mut iter);
        let err = super::bkey::bkey_err(k);
        if err != 0 {
            bch2_trans_iter_exit(&mut iter);
            return err;
        }
        if (*k.k).type_ == super::bset::KEY_TYPE_deleted {
            let update = bch2_trans_kmalloc(trans, core::mem::size_of::<bkey_i>())
                as *mut bkey_i;
            if update.is_null() {
                bch2_trans_iter_exit(&mut iter);
                return -12;
            }
            bkey_init(&mut (*update).k);
            (*update).k.p = pos;
            (*update).k.type_ = super::bset::KEY_TYPE_whiteout;
            let ret = bch2_trans_update(
                trans,
                &mut iter,
                update,
                BTREE_UPDATE_internal_snapshot_node,
            );
            bch2_trans_iter_exit(&mut iter);
            if ret != 0 {
                return ret;
            }
        } else {
            bch2_trans_iter_exit(&mut iter);
        }
        i += 1;
    }
    0
}

unsafe fn extent_front_merge(
    trans: *mut btree_trans,
    iter: *mut btree_iter,
    k: bkey_s_c,
    insert: *mut *mut bkey_i,
    k_buf_u64s: *mut u32,
    flags: u32,
) -> i32 {
    if (*trans).journal_replay_not_finished {
        return 0;
    }
    let update = bch2_bkey_make_mut_noupdate(trans, k);
    if update.is_null() {
        return -12;
    }
    if !super::bkey::bch2_bkey_merge(
        (*trans).c,
        bkey_s {
            k: &mut (*update).k,
            v: &mut (*update).v,
        },
        bkey_s_c {
            k: &(**insert).k,
            v: &(**insert).v,
        },
    ) {
        return 0;
    }
    let first = crate::snapshot::__bch2_key_has_snapshot_overwrites(
        trans,
        (*iter).btree_id,
        (*k.k).p,
    );
    let ret = if first != 0 {
        first
    } else {
        crate::snapshot::__bch2_key_has_snapshot_overwrites(
            trans,
            (*iter).btree_id,
            (**insert).k.p,
        )
    };
    if ret < 0 {
        return ret;
    }
    if ret != 0 {
        return 0;
    }
    let ret = bch2_btree_delete_at(trans, iter, flags);
    if ret != 0 {
        return ret;
    }
    *insert = update;
    *k_buf_u64s = (*update).k.u64s as u32;
    0
}

unsafe fn extent_back_merge(
    trans: *mut btree_trans,
    iter: *mut btree_iter,
    insert: *mut bkey_i,
    k: bkey_s_c,
) -> i32 {
    if (*trans).journal_replay_not_finished {
        return 0;
    }
    let first = crate::snapshot::__bch2_key_has_snapshot_overwrites(
        trans,
        (*iter).btree_id,
        (*insert).k.p,
    );
    let ret = if first != 0 {
        first
    } else {
        crate::snapshot::__bch2_key_has_snapshot_overwrites(trans, (*iter).btree_id, (*k.k).p)
    };
    if ret < 0 {
        return ret;
    }
    if ret != 0 {
        return 0;
    }
    super::bkey::bch2_bkey_merge(
        (*trans).c,
        bkey_s {
            k: &mut (*insert).k,
            v: &mut (*insert).v,
        },
        k,
    );
    0
}

unsafe fn bch2_trans_update_extent(
    trans: *mut btree_trans,
    orig_iter: *mut btree_iter,
    mut insert: *mut bkey_i,
    k_buf_u64s: u8,
    flags: u32,
) -> i32 {
    let btree_id = (*orig_iter).btree_id;
    let mut k_buf_u64s = k_buf_u64s as u32;
    let mut iter = btree_iter::default();
    bch2_trans_iter_init(
        trans,
        &mut iter,
        btree_id,
        super::bkey::bkey_start_pos(&(*insert).k),
        BTREE_ITER_intent | BTREE_ITER_not_extents | BTREE_ITER_nofilter_whiteouts,
    );
    let end = super::bkey::POS((*insert).k.p.inode, u64::MAX);
    let mut k = bch2_btree_iter_peek_max(&mut iter, &end);
    let mut ret = super::bkey::bkey_err(k);
    if ret != 0 {
        bch2_trans_iter_exit(&mut iter);
        return ret;
    }
    if k.k.is_null() {
        bch2_trans_iter_exit(&mut iter);
        return if !bkey_deleted(&*(insert.cast::<bkey_packed>())) {
            bch2_btree_insert_nonextent(trans, btree_id, insert, k_buf_u64s as u8, flags)
        } else {
            0
        };
    }

    if bpos_eq((*k.k).p, super::bkey::bkey_start_pos(&(*insert).k)) {
        if super::bkey::bch2_bkey_maybe_mergable(&*k.k, &(*insert).k) {
            ret = extent_front_merge(
                trans,
                &mut iter,
                k,
                &mut insert,
                &mut k_buf_u64s,
                flags,
            );
            if ret != 0 {
                bch2_trans_iter_exit(&mut iter);
                return ret;
            }
        }
        bch2_btree_iter_advance(&mut iter);
        k = bch2_btree_iter_peek_max(&mut iter, &end);
        ret = super::bkey::bkey_err(k);
        if ret != 0 {
            bch2_trans_iter_exit(&mut iter);
            return ret;
        }
    } else {
        loop {
            assert!(!bpos_le((*k.k).p, super::bkey::bkey_start_pos(&(*insert).k)));
            if (*k.k).type_ != super::bset::KEY_TYPE_whiteout
                && bpos_le((*insert).k.p, super::bkey::bkey_start_pos(&*k.k))
            {
                break;
            }
            let done = (*k.k).type_ != super::bset::KEY_TYPE_whiteout
                && bpos_lt((*insert).k.p, (*k.k).p);
            if super::bset::bkey_extent_whiteout(&*(k.k.cast::<bkey_packed>())) {
                let whiteout_type = extent_whiteout_type((*trans).c, btree_id, &(*insert).k);
                if bpos_le((*k.k).p, (*insert).k.p) && (*k.k).type_ != whiteout_type {
                    let update = bch2_bkey_make_mut_noupdate(trans, k);
                    if update.is_null() {
                        bch2_trans_iter_exit(&mut iter);
                        return -12;
                    }
                    (*update).k.p.snapshot = iter.snapshot;
                    (*update).k.type_ = whiteout_type;
                    ret = bch2_trans_update(trans, &mut iter, update, 0);
                    if ret != 0 {
                        bch2_trans_iter_exit(&mut iter);
                        return ret;
                    }
                }
            } else {
                ret = bch2_trans_update_extent_overwrite(
                    trans,
                    &mut iter,
                    flags,
                    k,
                    bkey_s_c {
                        k: &(*insert).k,
                        v: &(*insert).v,
                    },
                );
                if ret != 0 {
                    bch2_trans_iter_exit(&mut iter);
                    return ret;
                }
            }
            if done {
                break;
            }
            bch2_btree_iter_advance(&mut iter);
            k = bch2_btree_iter_peek_max(&mut iter, &end);
            ret = super::bkey::bkey_err(k);
            if ret != 0 {
                bch2_trans_iter_exit(&mut iter);
                return ret;
            }
            if k.k.is_null() {
                break;
            }
        }
    }

    if !k.k.is_null() && super::bkey::bch2_bkey_maybe_mergable(&(*insert).k, &*k.k) {
        ret = extent_back_merge(&mut *trans, &mut iter, insert, k);
        if ret != 0 {
            bch2_trans_iter_exit(&mut iter);
            return ret;
        }
    }
    bch2_trans_iter_exit(&mut iter);
    if !bkey_deleted(&*(insert.cast::<bkey_packed>())) {
        bch2_btree_insert_nonextent(trans, btree_id, insert, k_buf_u64s as u8, flags)
    } else {
        0
    }
}

unsafe fn __btree_node_flush(
    j: *mut crate::journal::journal,
    pin: *mut journal_entry_pin,
    i: usize,
    seq: u64,
) -> i32 {
    let b = (pin.cast::<u8>())
        .sub(
            core::mem::offset_of!(btree, writes)
                + i * core::mem::size_of::<super::types::btree_write>()
                + core::mem::offset_of!(super::types::btree_write, journal),
        )
        .cast::<btree>();
    let c = (j.cast::<u8>())
        .sub(core::mem::offset_of!(super::types::bch_fs, journal))
        .cast::<super::types::bch_fs>();
    if (*b).flags & (1usize << super::io::BTREE_NODE_dirty) == 0
        || btree_node_write_idx(b) != i
        || (*pin).seq != seq
    {
        return if (*pin).seq == seq { -1 } else { 0 };
    }

    let ret = six_lock_read(&(*b).c.lock);
    if ret != 0 {
        return ret;
    }
    (*b).flags |= 1usize << super::io::BTREE_NODE_need_write;
    let mut trans = btree_trans::default();
    super::iter::bch2_trans_init(&mut trans, c);
    super::io::bch2_btree_node_write_trans(
        &mut trans,
        b,
        six_lock_type::SIX_LOCK_read,
        super::io::BTREE_WRITE_only_if_need,
    );
    six_unlock_read(&(*b).c.lock);
    if (*pin).seq == seq {
        -1
    } else {
        0
    }
}

pub unsafe extern "C" fn bch2_btree_node_flush0(
    j: *mut crate::journal::journal,
    pin: *mut journal_entry_pin,
    seq: u64,
) -> i32 {
    __btree_node_flush(j, pin, 0, seq)
}

pub unsafe extern "C" fn bch2_btree_node_flush1(
    j: *mut crate::journal::journal,
    pin: *mut journal_entry_pin,
    seq: u64,
) -> i32 {
    __btree_node_flush(j, pin, 1, seq)
}

pub unsafe fn bch2_btree_add_journal_pin(c: *mut super::types::bch_fs, b: *mut btree, seq: u64) {
    let idx = btree_node_write_idx(b);
    let w = btree_current_write(b);
    crate::journal::bch2_journal_pin_add(
        &(*c).journal,
        seq,
        &mut (*w).journal,
        if idx == 0 {
            bch2_btree_node_flush0
        } else {
            bch2_btree_node_flush1
        },
    );
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct btree_trigger_op {
    pub btree: u8,
    pub level: u32,
    pub old: bkey_s_c,
    pub new: bkey_s,
    pub new_buf_u64s: u32,
    pub flags: u32,
}

/// Grow the in-flight update buffer for a trigger, matching the local
/// `bch2_trigger_get_mutable_new()` slow path.
pub unsafe fn bch2_trigger_get_mutable_new(
    trans: *mut btree_trans,
    op: btree_trigger_op,
    needed_u64s: u32,
    out: *mut bkey_s,
) -> i32 {
    assert!(!trans.is_null());
    assert!(!out.is_null());
    assert_ne!(op.flags & BTREE_TRIGGER_insert, 0);

    if needed_u64s <= op.new_buf_u64s {
        *out = op.new;
        return 0;
    }

    assert_eq!(op.flags & BTREE_TRIGGER_atomic, 0);

    let mut found: *mut btree_insert_entry = core::ptr::null_mut();
    for idx in 0..(*trans).nr_updates as usize {
        let entry = (*trans).updates.add(idx);
        if (*entry).btree_id == op.btree
            && (*entry).level as u32 == op.level
            && bpos_eq((*(*entry).k).k.p, (*op.new.k).p)
        {
            found = entry;
            break;
        }
    }
    assert!(!found.is_null());

    let new_buf = bch2_trans_kmalloc(
        trans,
        (needed_u64s as usize) * core::mem::size_of::<u64>(),
    ) as *mut bkey_i;
    if new_buf.is_null() {
        return -12;
    }
    bkey_copy(new_buf, (*found).k);
    (*found).k = new_buf;
    (*found).k_buf_u64s = needed_u64s as u8;

    *out = bkey_s {
        k: &mut (*new_buf).k,
        v: (new_buf as *mut u64).add(5).cast::<bch_val>(),
    };
    0
}

fn btree_insert_entry_cmp(l: &btree_insert_entry, r: &btree_insert_entry) -> Ordering {
    l.sort_order
        .cmp(&r.sort_order)
        .then(l.cached.cmp(&r.cached))
        .then(r.level.cmp(&l.level))
        .then_with(|| unsafe { bpos_cmp((*l.k).k.p, (*r.k).k.p).cmp(&0) })
}

fn btree_trigger_order(btree: u8) -> u8 {
    match btree {
        4 => u8::MAX,
        6 => u8::MAX - 1,
        _ => btree,
    }
}

pub unsafe fn bch2_btree_path_peek_slot_exact(path: *mut btree_path, u: *mut bkey) -> bkey_s_c {
    super::iter::bch2_btree_path_peek_slot_exact(path, u)
}

pub unsafe fn btree_trans_update_by_path(
    trans: *mut btree_trans,
    path_idx: btree_path_idx_t,
    k: *mut bkey_i,
    k_buf_u64s: u8,
    flags: u32,
    ip: usize,
) -> *mut btree_insert_entry {
    let path = (*trans).paths.add(path_idx as usize);
    assert!((*path).should_be_locked);
    assert!((*trans).nr_updates < (*trans).nr_paths);
    assert!(bpos_eq((*k).k.p, (*path).pos));
    assert!(k_buf_u64s >= (*k).k.u64s);
    (*trans).has_interior_updates |= (*path).level != 0;

    let n = btree_insert_entry {
        flags,
        sort_order: btree_trigger_order((*path).btree_id),
        bkey_type: if (*path).level != 0 {
            0
        } else {
            (*path).btree_id.saturating_add(1)
        },
        btree_id: (*path).btree_id,
        level: (*path).level,
        cached: (*path).cached,
        k_buf_u64s,
        path: path_idx,
        k,
        ip_allocated: ip,
        ..Default::default()
    };

    let updates = (*trans).updates;
    let nr = (*trans).nr_updates as usize;
    let mut idx = 0usize;
    while idx < nr && btree_insert_entry_cmp(&n, &*updates.add(idx)) == Ordering::Greater {
        idx += 1;
    }

    let overwrite = idx < nr && btree_insert_entry_cmp(&n, &*updates.add(idx)) == Ordering::Equal;
    let i = updates.add(idx);
    if overwrite {
        assert!(!(*i).insert_trigger_run && !(*i).overwrite_trigger_run);
        bch2_trans_free_owned_key(i);
        super::iter::bch2_path_put(trans, (*i).path, true);
        (*i).flags = n.flags;
        (*i).cached = n.cached;
        (*i).k_buf_u64s = n.k_buf_u64s;
        (*i).k = n.k;
        (*i).path = n.path;
        (*i).ip_allocated = n.ip_allocated;
    } else {
        core::ptr::copy(i, i.add(1), nr - idx);
        *i = n;
        (*trans).nr_updates += 1;
        let old = bch2_btree_path_peek_slot_exact(path, &mut (*i).old_k);
        (*i).old_v = old.v;
        (*i).old_btree_u64s = if (*i).old_k.type_ != 0 {
            (*i).old_k.u64s
        } else {
            0
        };
        if (*trans).journal_replay_not_finished {
            let journal_k = crate::journal::bch2_journal_keys_peek_slot(
                (*trans).c,
                n.btree_id,
                n.level,
                (*k).k.p,
            );
            if !journal_k.is_null() {
                (*i).old_k = (*journal_k).k;
                (*i).old_v = &(*journal_k).v;
            }
        }
    }
    let path_ref = (*trans).paths.add((*i).path as usize);
    assert!((*path_ref).ref_ != u8::MAX);
    (*path_ref).ref_ += 1;
    (*path_ref).intent_ref += 1;
    i
}

pub unsafe fn bch2_btree_delete_at(
    trans: *mut btree_trans,
    iter: *mut btree_iter,
    flags: u32,
) -> i32 {
    if trans.is_null() || iter.is_null() {
        return -22;
    }
    let k = bch2_trans_kmalloc(trans, core::mem::size_of::<bkey_i>()) as *mut bkey_i;
    if k.is_null() {
        return -12;
    }
    bkey_init(&mut (*k).k);
    (*k).k.p = (*iter).pos;
    bch2_trans_update(trans, iter, k, flags)
}

pub unsafe fn bch2_btree_delete(
    trans: *mut btree_trans,
    btree_id: u8,
    pos: super::bkey::bpos,
    flags: u32,
) -> i32 {
    if trans.is_null() {
        return -22;
    }
    let mut iter = btree_iter::default();
    bch2_trans_iter_init(
        trans,
        &mut iter,
        btree_id,
        pos,
        BTREE_ITER_cached | BTREE_ITER_intent,
    );
    let ret = super::iter::bch2_btree_iter_traverse(&mut iter);
    if ret != 0 {
        super::iter::bch2_trans_iter_exit(&mut iter);
        return ret;
    }
    let ret = bch2_btree_delete_at(trans, &mut iter, flags);
    super::iter::bch2_trans_iter_exit(&mut iter);
    ret
}

pub unsafe fn bch2_btree_insert_nonextent(
    trans: *mut btree_trans,
    btree_id: u8,
    k: *mut bkey_i,
    k_buf_u64s: u8,
    flags: u32,
) -> i32 {
    if trans.is_null() || k.is_null() {
        return -22;
    }
    let mut iter = btree_iter::default();
    bch2_trans_iter_init(
        trans,
        &mut iter,
        btree_id,
        (*k).k.p,
        BTREE_ITER_cached | BTREE_ITER_intent | BTREE_ITER_not_extents,
    );
    let mut ret = super::iter::bch2_btree_iter_traverse(&mut iter);
    if ret == 0 {
        ret = bch2_trans_update_ip(trans, &mut iter, k, k_buf_u64s, flags, 0);
    }
    super::iter::bch2_trans_iter_exit(&mut iter);
    ret
}

pub unsafe fn bch2_btree_insert_trans(
    trans: *mut btree_trans,
    btree_id: u8,
    k: *mut bkey_i,
    flags: u32,
) -> i32 {
    if trans.is_null() || k.is_null() {
        return -22;
    }
    let mut iter = btree_iter::default();
    bch2_trans_iter_init(
        trans,
        &mut iter,
        btree_id,
        (*k).k.p,
        BTREE_ITER_intent | flags as u16,
    );
    let mut ret = super::iter::bch2_btree_iter_traverse(&mut iter);
    if ret == 0 {
        ret = bch2_trans_update_ip(trans, &mut iter, k, (*k).k.u64s, flags, 0);
    }
    super::iter::bch2_trans_iter_exit(&mut iter);
    ret
}

pub unsafe fn bch2_btree_insert(
    c: *mut super::types::bch_fs,
    btree_id: u8,
    k: *mut bkey_i,
    _disk_res: *mut super::types::disk_reservation,
    _commit_flags: u32,
    iter_flags: u32,
) -> i32 {
    if c.is_null() || k.is_null() {
        return -22;
    }
    let mut trans = btree_trans::default();
    super::iter::bch2_trans_init(&mut trans, c);
    let ret = bch2_btree_insert_trans(&mut trans, btree_id, k, iter_flags);
    if ret != 0 {
        return ret;
    }
    bch2_trans_commit(&mut trans)
}

pub unsafe fn bch2_btree_insert_clone_trans(
    trans: *mut btree_trans,
    btree_id: u8,
    k: *mut bkey_i,
) -> i32 {
    if trans.is_null() || k.is_null() {
        return -22;
    }
    let clone = bch2_trans_kmalloc(trans, bkey_bytes(&(*k).k)) as *mut bkey_i;
    if clone.is_null() {
        return -12;
    }
    bkey_copy(clone, k);
    bch2_btree_insert_trans(trans, btree_id, clone, 0)
}

pub unsafe fn bch2_bkey_get_empty_slot(
    trans: *mut btree_trans,
    iter: *mut btree_iter,
    btree_id: u8,
    start: super::bkey::bpos,
    end: super::bkey::bpos,
) -> i32 {
    if trans.is_null() || iter.is_null() {
        return -22;
    }
    bch2_trans_iter_init(trans, iter, btree_id, end, BTREE_ITER_intent);
    let k = bch2_btree_iter_peek_prev(iter);
    let ret = super::bkey::bkey_err(k);
    if ret != 0 {
        return ret;
    }
    if super::bkey::bpos_lt((*iter).pos, start) {
        bch2_btree_iter_set_pos(iter, start);
    } else {
        bch2_btree_iter_advance(iter);
    }
    let k = bch2_btree_iter_peek_slot(iter);
    let ret = super::bkey::bkey_err(k);
    if ret != 0 {
        return ret;
    }
    assert!(!k.k.is_null());
    assert_eq!((*k.k).type_, super::bset::KEY_TYPE_deleted);
    if super::bkey::bpos_gt((*k.k).p, end) {
        return -28;
    }
    0
}

pub unsafe fn bch2_btree_delete_range_trans(
    trans: *mut btree_trans,
    btree_id: u8,
    start: super::bkey::bpos,
    end: super::bkey::bpos,
    flags: u32,
) -> i32 {
    if trans.is_null() {
        return -22;
    }
    let restart_count = (*trans).restart_count;
    let mut iter = btree_iter::default();
    bch2_trans_iter_init(
        trans,
        &mut iter,
        btree_id,
        start,
        BTREE_ITER_intent | flags as u16,
    );

    unsafe fn delete_range_one(
        trans: *mut btree_trans,
        iter: *mut btree_iter,
        end: super::bkey::bpos,
        flags: u32,
    ) -> i32 {
        let k = bch2_btree_iter_peek_max(iter, &end);
        let err = super::bkey::bkey_err(k);
        if err != 0 {
            return err;
        }
        if k.k.is_null() {
            return 1;
        }

        let mut delete = bkey_i::default();
        bkey_init(&mut delete.k);
        delete.k.p = (*iter).pos;
        if (*iter).flags & super::iter::BTREE_ITER_is_extents != 0 {
            bch2_key_resize(
                &mut delete.k,
                bpos_min(end, (*k.k).p)
                    .offset
                    .saturating_sub((*iter).pos.offset) as u32,
            );
        }

        let ret = bch2_trans_update(trans, iter, &mut delete, flags);
        if ret != 0 {
            return ret;
        }
        bch2_trans_commit(trans)
    }

    let mut ret;
    loop {
        ret = delete_range_one(trans, &mut iter, end, flags);
        super::iter::bch2_trans_begin(trans);
        if ret == -4 {
            ret = 0;
        }
        if ret != 0 {
            break;
        }
    }
    super::iter::bch2_trans_iter_exit(&mut iter);
    if ret < 0 {
        ret
    } else if (*trans).restart_count != restart_count {
        -4
    } else {
        0
    }
}

pub unsafe fn bch2_btree_delete_range(
    c: *mut super::types::bch_fs,
    btree_id: u8,
    start: super::bkey::bpos,
    end: super::bkey::bpos,
    flags: u32,
) -> i32 {
    if c.is_null() {
        return -22;
    }
    let mut trans = btree_trans::default();
    super::iter::bch2_trans_init(&mut trans, c);
    let ret = bch2_btree_delete_range_trans(&mut trans, btree_id, start, end, flags);
    if ret == -4 {
        0
    } else {
        ret
    }
}

pub unsafe fn bch2_btree_bit_mod_iter(
    trans: *mut btree_trans,
    iter: *mut btree_iter,
    set: bool,
) -> i32 {
    if trans.is_null() || iter.is_null() {
        return -22;
    }
    let k = bch2_trans_kmalloc(trans, core::mem::size_of::<bkey_i>()) as *mut bkey_i;
    if k.is_null() {
        return -12;
    }
    bkey_init(&mut (*k).k);
    (*k).k.type_ = if set {
        super::bset::KEY_TYPE_set
    } else {
        super::bset::KEY_TYPE_deleted
    };
    (*k).k.p = (*iter).pos;
    if (*iter).flags & super::iter::BTREE_ITER_is_extents != 0 {
        bch2_key_resize(&mut (*k).k, 1);
    }
    bch2_trans_update(trans, iter, k, 0)
}

pub unsafe fn bch2_btree_bit_mod(
    trans: *mut btree_trans,
    btree_id: u8,
    pos: super::bkey::bpos,
    set: bool,
) -> i32 {
    if trans.is_null() {
        return -22;
    }
    let mut iter = btree_iter::default();
    bch2_trans_iter_init(trans, &mut iter, btree_id, pos, BTREE_ITER_intent);
    let ret = super::iter::bch2_btree_iter_traverse(&mut iter);
    if ret != 0 {
        bch2_trans_iter_exit(&mut iter);
        return ret;
    }
    let ret = bch2_btree_bit_mod_iter(trans, &mut iter, set);
    bch2_trans_iter_exit(&mut iter);
    ret
}

pub unsafe fn bch2_trans_update_buffered(
    trans: *mut btree_trans,
    btree_id: u8,
    k: *const bkey_i,
) -> i32 {
    if trans.is_null() || k.is_null() {
        return -22;
    }
    if (*trans).journal_replay_not_finished
        && (*k).k.type_ != super::bset::KEY_TYPE_accounting
    {
        return bch2_btree_insert_clone_trans(trans, btree_id, k.cast_mut());
    }
    let u64s = (*k).k.u64s as u16;
    let entry = bch2_trans_jset_entry_alloc(trans, u64s);
    if entry.is_null() {
        return -12;
    }
    crate::journal::journal_entry_init(
        entry,
        crate::journal::BCH_JSET_ENTRY_write_buffer_keys,
        btree_id,
        0,
        u64s,
    );
    core::ptr::copy_nonoverlapping(
        k.cast::<u64>(),
        entry.add(1).cast::<u64>(),
        u64s as usize,
    );
    0
}

pub unsafe fn bch2_btree_bit_mod_buffered(
    trans: *mut btree_trans,
    btree_id: u8,
    pos: super::bkey::bpos,
    set: bool,
) -> i32 {
    if trans.is_null() {
        return -22;
    }
    let mut key = bkey_i::default();
    bkey_init(&mut key.k);
    key.k.type_ = if set {
        super::bset::KEY_TYPE_set
    } else {
        super::bset::KEY_TYPE_deleted
    };
    key.k.p = pos;
    bch2_trans_update_buffered(trans, btree_id, &key)
}

/* Matches the local update.h inline wrapper: buffered deletion is the
 * buffered bit modification with the value cleared. */
pub unsafe fn bch2_btree_delete_at_buffered(
    trans: *mut btree_trans,
    btree_id: u8,
    pos: super::bkey::bpos,
) -> i32 {
    bch2_btree_bit_mod_buffered(trans, btree_id, pos, false)
}

pub unsafe fn bch2_trans_log_bkey(
    trans: *mut btree_trans,
    btree_id: u8,
    level: u8,
    k: *const bkey_i,
) -> i32 {
    if trans.is_null() || k.is_null() {
        return -22;
    }
    let u64s = (*k).k.u64s as u16;
    let entry = bch2_trans_jset_entry_alloc(trans, u64s);
    if entry.is_null() {
        return -12;
    }
    crate::journal::journal_entry_init(
        entry,
        crate::journal::BCH_JSET_ENTRY_log_bkey,
        btree_id,
        level,
        u64s,
    );
    core::ptr::copy_nonoverlapping(k.cast::<u64>(), entry.add(1).cast::<u64>(), u64s as usize);
    0
}

pub unsafe fn bch2_trans_log_str(trans: *mut btree_trans, str_: *const u8) -> i32 {
    if trans.is_null() || str_.is_null() {
        return -22;
    }
    let mut len = 0usize;
    while *str_.add(len) != 0 {
        len += 1;
    }
    let u64s = len.div_ceil(core::mem::size_of::<u64>());
    if u64s > u16::MAX as usize {
        return -12;
    }
    let entry = bch2_trans_jset_entry_alloc(trans, u64s as u16);
    if entry.is_null() {
        return -12;
    }
    crate::journal::journal_entry_init(
        entry,
        crate::journal::BCH_JSET_ENTRY_log,
        0,
        1,
        u64s as u16,
    );
    let payload = entry.add(1).cast::<u8>();
    core::ptr::write_bytes(payload, 0, u64s * core::mem::size_of::<u64>());
    core::ptr::copy_nonoverlapping(str_, payload, len);
    0
}

pub unsafe fn bch2_trans_update_ip(
    trans: *mut btree_trans,
    iter: *mut btree_iter,
    k: *mut bkey_i,
    k_buf_u64s: u8,
    flags: u32,
    ip: usize,
) -> i32 {
    assert!((*iter).flags & super::iter::BTREE_ITER_intent != 0);
    if (*iter).flags & super::iter::BTREE_ITER_is_extents != 0 {
        return bch2_trans_update_extent(trans, iter, k, k_buf_u64s, flags);
    }
    let path = (*trans).paths.add((*iter).path as usize);
    assert_eq!((*path).nodes_locked & 3, BTREE_NODE_INTENT_LOCKED);
    if btree_trans_update_by_path(trans, (*iter).path, k, k_buf_u64s, flags, ip).is_null() {
        -12
    } else {
        0
    }
}

pub unsafe fn bch2_trans_update(
    trans: *mut btree_trans,
    iter: *mut btree_iter,
    k: *mut bkey_i,
    flags: u32,
) -> i32 {
    bch2_trans_update_ip(trans, iter, k, (*k).k.u64s, flags, 0)
}

pub unsafe fn bch2_trans_update_buf(
    trans: *mut btree_trans,
    iter: *mut btree_iter,
    k: *mut bkey_i,
    k_buf_u64s: u8,
    flags: u32,
) -> i32 {
    assert!((*iter).flags & super::iter::BTREE_ITER_intent != 0);
    let path = (*trans).paths.add((*iter).path as usize);
    assert_eq!((*path).nodes_locked & 3, BTREE_NODE_INTENT_LOCKED);
    if btree_trans_update_by_path(trans, (*iter).path, k, k_buf_u64s, flags, 0).is_null() {
        -12
    } else {
        0
    }
}

pub unsafe fn bch2_trans_commit_hook(
    trans: *mut btree_trans,
    hook: *mut btree_trans_commit_hook,
) {
    (*hook).next = (*trans).hooks;
    (*trans).hooks = hook;
}

pub unsafe fn bch2_trans_reset_updates(trans: *mut btree_trans) {
    if trans.is_null() {
        return;
    }
    for idx in 0..(*trans).nr_updates as usize {
        bch2_trans_free_owned_key((*trans).updates.add(idx));
        let path = (*trans).updates.add(idx).read().path;
        super::iter::bch2_path_put(trans, path, true);
    }
    (*trans).nr_updates = 0;
    (*trans).journal_entries.u64s = 0;
    (*trans).journal_entries.size = 0;
    (*trans).accounting.u64s = 0;
    (*trans).accounting.size = 0;
    (*trans).hooks = core::ptr::null_mut();
    (*trans).journal_u64s = 0;
    (*trans).extra_journal_u64s = 0;
    (*trans).extra_disk_res = 0;
    (*trans).has_interior_updates = false;
}

pub unsafe fn bch2_trans_node_reinit_iter(trans: *mut btree_trans, b: *mut btree) {
    if trans.is_null() || b.is_null() {
        return;
    }
    let level = (*b).c.level;
    for idx in 1..BTREE_ITER_INITIAL {
        if (*trans).paths_allocated & (1u64 << idx) == 0 {
            continue;
        }
        let path = (*trans).paths.add(idx);
        if (*path).l[level as usize].b != b {
            continue;
        }
        super::iter::bch2_btree_path_level_init(trans, path, level, b);
    }
    super::iter::bch2_trans_revalidate_updates_in_node(trans, b);
}

pub unsafe fn bch2_btree_node_prep_for_write(
    trans: *mut btree_trans,
    _path: *mut btree_path,
    b: *mut btree,
) {
    if super::types::btree_node_just_written(b)
        && super::io::bch2_btree_post_write_cleanup((*trans).c, b)
    {
        bch2_trans_node_reinit_iter(trans, b);
    }
    if !super::interior::want_new_bset((*trans).c, b).is_null() {
        super::bset_build::bch2_btree_init_next(trans, b);
    }
}

unsafe fn bch2_btree_bset_insert_key_inlined(
    trans: *mut btree_trans,
    path: *mut btree_path,
    b: *mut btree,
    insert: *mut bkey_i,
) -> bool {
    let node_iter = &mut (*path).l[(*path).level as usize].iter;
    let mut k = bch2_btree_node_iter_peek_all(node_iter, b);
    if !k.is_null() && !bpos_eq(bkey_unpack_pos(b, k), (*insert).k.p) {
        k = core::ptr::null_mut();
    }
    assert!(k.is_null() || !bkey_deleted(&*k));

    if bkey_deleted(&*(insert.cast::<bkey_packed>())) && k.is_null() {
        return false;
    }

    let last = bset_tree_last(b);
    let last_start = btree_bkey_first_offset(last);
    let k_writeable = !k.is_null() && super::types::__btree_node_key_to_offset(b, k) >= last_start;
    if !k.is_null() {
        let bset_idx =
            super::types::bch2_bkey_to_bset_inlined(b, k).offset_from((*b).set.as_ptr()) as usize;
        btree_keys_account_key(&mut (*b).nr, bset_idx, k, -1);

        /* bkey::needs_whiteout is the high bit of the packed format byte.
         * Preserve the local commit.c ordering: consume the old marker,
         * push a whiteout for deletion, or propagate it to the replacement.
         */
        if (*k).format & 0x80 != 0 {
            if bkey_deleted(&*(insert.cast::<bkey_packed>())) {
                super::interior::bch2_push_whiteout(b, (*insert).k.p);
            } else {
                (*insert).k.format |= 0x80;
            }
            (*k).format &= 0x7f;
        }
        (*k).type_ = 0;
        if !k_writeable {
            super::iter::bch2_btree_path_fix_key_modified(trans, b, k);
        }
    }

    let clobber_u64s = if k_writeable { (*k).u64s as u32 } else { 0 };
    if bkey_deleted(&*(insert.cast::<bkey_packed>())) {
        if k_writeable {
            bch2_bset_delete(b, k, clobber_u64s);
        }
    } else {
        if !k_writeable {
            k = bch2_btree_node_iter_bset_pos(node_iter, b, last);
        }
        bch2_bset_insert(b, k, insert, clobber_u64s);
    }

    let new_u64s = if !bkey_deleted(&*(insert.cast::<bkey_packed>())) {
        (*k).u64s as u32
    } else {
        0
    };
    if clobber_u64s != new_u64s {
        super::node_iter::bch2_btree_node_iter_fix(
            trans,
            path,
            b,
            node_iter,
            k,
            clobber_u64s,
            new_u64s,
        );
    }
    true
}

pub unsafe fn bch2_btree_insert_key_leaf(
    trans: *mut btree_trans,
    path: *mut btree_path,
    insert: *mut bkey_i,
    journal_seq: u64,
) {
    let b = (*path).l[(*path).level as usize].b;
    let last = bset_tree_last(b);
    let old_u64s = super::types::bset_u64s(last) as i32;
    let old_live_u64s = (*b).nr.live_u64s as i32;
    if !bch2_btree_bset_insert_key_inlined(trans, path, b, insert) {
        return;
    }
    if (*b).c.level != 0
        && super::interior::bch2_btree_node_check_topology(trans, b) != 0
    {
        return;
    }
    let set = super::types::bset(b, last);
    (*set).journal_seq = (*set).journal_seq.max(journal_seq);
    bch2_btree_add_journal_pin((*trans).c, b, journal_seq);
    super::cache::bch2_btree_node_set_dirty((*trans).c, b);

    let live_u64s_added = (*b).nr.live_u64s as i32 - old_live_u64s;
    let u64s_added = super::types::bset_u64s(last) as i32 - old_u64s;
    if (*b).sib_u64s[0] != u16::MAX && live_u64s_added < 0 {
        (*b).sib_u64s[0] =
            ((*b).sib_u64s[0] as i32 + live_u64s_added).max((*b).nr.live_u64s as i32) as u16;
    }
    if (*b).sib_u64s[1] != u16::MAX && live_u64s_added < 0 {
        (*b).sib_u64s[1] =
            ((*b).sib_u64s[1] as i32 + live_u64s_added).max((*b).nr.live_u64s as i32) as u16;
    }

    if u64s_added > live_u64s_added {
        let mut compact = false;
        for idx in 0..(*b).nsets as usize {
            if super::bset_build::should_compact_bset_lazy(b, (*b).set.as_mut_ptr().add(idx)) {
                compact = true;
                break;
            }
        }
        if compact
            && super::bset_build::bch2_compact_whiteouts(
                (*trans).c,
                b,
                super::bset_build::compact_mode::COMPACT_LAZY,
            )
        {
            bch2_trans_node_reinit_iter(trans, b);
        }
    }
}

unsafe fn verify_update_old_key(trans: *mut btree_trans, i: *mut btree_insert_entry) -> bool {
    let path = (*trans).paths.add((*i).path as usize);
    let mut u = bkey::default();
    let mut k = bch2_btree_path_peek_slot_exact(path, &mut u);
    if (*trans).journal_replay_not_finished {
        let journal_k = crate::journal::bch2_journal_keys_peek_slot(
            (*trans).c,
            (*i).btree_id,
            (*i).level,
            (*i).old_k.p,
        );
        if !journal_k.is_null() {
            u = (*journal_k).k;
            k.k = &u;
            k.v = &(*journal_k).v;
        }
    }
    bkey_fields_eq(&*k.k, &(*i).old_k) && k.v == (*i).old_v
}

unsafe fn bch2_key_trigger(trans: *mut btree_trans, op: btree_trigger_op) -> i32 {
    let type_ = if (*op.old.k).type_ != 0 {
        (*op.old.k).type_
    } else {
        (*op.new.k).type_
    };
    if type_ == crate::snapshot::KEY_TYPE_snapshot {
        crate::snapshot::bch2_mark_snapshot(trans, op)
    } else {
        0
    }
}

unsafe fn run_one_mem_trigger(
    trans: *mut btree_trans,
    i: *mut btree_insert_entry,
    flags: u32,
) -> i32 {
    if !verify_update_old_key(trans, i) {
        return -1;
    }
    let mut deleted = bkey_i::default();
    bkey_init(&mut deleted.k);
    deleted.k.p = (*(*i).k).k.p;
    let old_k = (*i).old_k;
    let new_v = ((*i).k as *mut u64).add(5).cast::<bch_val>();
    let mut op = btree_trigger_op {
        btree: (*i).btree_id,
        level: (*i).level as u32,
        old: bkey_s_c {
            k: &old_k,
            v: (*i).old_v,
        },
        new: bkey_s {
            k: &mut (*(*i).k).k,
            v: new_v,
        },
        new_buf_u64s: (*i).k_buf_u64s as u32,
        flags: 0,
    };
    let old_has_trigger = old_k.type_ == crate::snapshot::KEY_TYPE_snapshot;
    let new_has_trigger = (*(*i).k).k.type_ == crate::snapshot::KEY_TYPE_snapshot;
    if old_has_trigger && new_has_trigger {
        op.flags = flags | BTREE_TRIGGER_insert | BTREE_TRIGGER_overwrite;
        return bch2_key_trigger(trans, op);
    }
    if new_has_trigger {
        op.flags = flags | BTREE_TRIGGER_insert;
        op.old = bkey_s_c {
            k: &deleted.k,
            v: core::ptr::null(),
        };
        let ret = bch2_key_trigger(trans, op);
        if ret != 0 {
            return ret;
        }
    }
    if old_has_trigger {
        op.flags = flags | BTREE_TRIGGER_overwrite;
        op.old = bkey_s_c {
            k: &old_k,
            v: (*i).old_v,
        };
        op.new = bkey_s {
            k: &mut deleted.k,
            v: core::ptr::null_mut(),
        };
        op.new_buf_u64s = 0;
        return bch2_key_trigger(trans, op);
    }
    0
}

pub unsafe fn bch2_trans_commit(trans: *mut btree_trans) -> i32 {
    if !super::iter::bch2_trans_has_updates(trans) {
        return 0;
    }
    crate::rewrite_log_debug!("transaction commit begin updates={}", (*trans).nr_updates);

    if !(*trans).journal_replay_not_finished {
        /* Local commit.c skips non-inode no-op updates unless the caller
         * explicitly requests no-skip-noops.  This commit entry point has no
         * flags parameter; journal replay is the corresponding no-skip path.
         */
        let mut dst = 0usize;
        let nr_updates = (*trans).nr_updates as usize;
        for idx in 0..nr_updates {
            let i = (*trans).updates.add(idx);
            let old = bkey_s_c {
                k: &(*i).old_k,
                v: (*i).old_v,
            };
            let new = bkey_s_c {
                k: &(*(*i).k).k,
                v: &(*(*i).k).v,
            };
            if bkey_and_val_eq(old, new) {
                bch2_trans_free_owned_key(i);
                super::iter::bch2_path_put(trans, (*i).path, true);
                continue;
            }
            if dst != idx {
                *(*trans).updates.add(dst) = *i;
            }
            dst += 1;
        }
        (*trans).nr_updates = dst as btree_path_idx_t;
        if (*trans).nr_updates == 0
            && (*trans).journal_entries.u64s == 0
            && (*trans).accounting.u64s == 0
        {
            bch2_trans_reset_updates(trans);
            return 0;
        }
    }

    for idx in 0..(*trans).nr_updates as usize {
        let i = (*trans).updates.add(idx);
        let path = (*trans).paths.add((*i).path as usize);
        let b = (*path).l[(*i).level as usize].b;
        #[cfg(debug_assertions)]
        {
            assert!(bpos_eq((*(*i).k).k.p, (*path).pos));
            assert_eq!((*i).cached, (*path).cached);
            assert!(!((*i).cached
                && !(*i).key_cache_already_flushed
                && (*(*i).k).k.type_ == super::bset::KEY_TYPE_deleted));
            assert_eq!((*i).level, (*path).level);
            assert_eq!((*i).btree_id, (*path).btree_id);
            let expected_bkey_type = if (*path).level != 0 {
                0
            } else {
                (*path).btree_id.saturating_add(1)
            };
            assert_eq!((*i).bkey_type, expected_bkey_type);
        }
        if !super::interior::bch2_btree_node_insert_fits(b, (*(*i).k).k.u64s as u32)
            && (*(*i).k).k.type_ == super::bset::KEY_TYPE_deleted
            && super::bset_build::bch2_compact_whiteouts(
                (*trans).c,
                b,
                super::bset_build::compact_mode::COMPACT_ALL,
            )
        {
            bch2_trans_node_reinit_iter(trans, b);
        }
        let mut old_key = bkey::default();
        let old = bch2_btree_path_peek_slot_exact(path, &mut old_key);
        let required_u64s = if (*(*i).k).k.type_ == super::bset::KEY_TYPE_deleted
            && !old.v.is_null()
        {
            0
        } else {
            (*(*i).k).k.u64s as u32
        };
        if !super::interior::bch2_btree_node_insert_fits(b, required_u64s)
            && super::interior::want_new_bset((*trans).c, b).is_null()
        {
            let ret = super::interior::bch2_btree_split_leaf(
                trans,
                (*i).path,
                (*(*i).k).k.u64s as u32,
                0,
            );
            if ret != 0 {
                crate::rewrite_log_error!("transaction split failed ret={ret}");
                return ret;
            }
            crate::rewrite_log_debug!("transaction requested restart after split");
            return -4;
        }
    }

    let mut journal_u64s = 0u32;
    for idx in 0..(*trans).nr_updates as usize {
        let i = (*trans).updates.add(idx);
        if (*i).flags & BTREE_UPDATE_nojournal == 0 {
            journal_u64s += crate::journal::jset_u64s((*(*i).k).k.u64s as u32);
        }
    }
    journal_u64s += (*trans).journal_entries.u64s as u32;
    journal_u64s += (*trans).extra_journal_u64s;
    (*trans).journal_u64s = journal_u64s;
    if journal_u64s > u16::MAX as u32 {
        crate::rewrite_log_error!("transaction journal entry too large u64s={journal_u64s}");
        return -2;
    }
    let journal = &(*(*trans).c).journal;
    if journal_u64s != 0 {
        let ret = crate::journal::bch2_journal_res_get(
            journal,
            &mut (*trans).journal_res,
            journal_u64s as u16,
            0,
        );
        if ret != 0 {
            return ret;
        }
    }

    let mut locked: [*mut btree; BTREE_ITER_INITIAL] = [core::ptr::null_mut(); BTREE_ITER_INITIAL];
    let mut nr_locked = 0usize;
    for idx in 0..(*trans).nr_updates as usize {
        let i = (*trans).updates.add(idx);
        let path = (*trans).paths.add((*i).path as usize);
        let b = (*path).l[(*i).level as usize].b;
        if idx != 0 {
            let prev = (*trans).updates.add(idx - 1);
            let prev_path = (*trans).paths.add((*prev).path as usize);
            let prev_b = (*prev_path).l[(*prev).level as usize].b;
            if prev_b == b {
                continue;
            }
        }
        let ret = six_lock_write(&(*b).c.lock);
        if ret != 0 {
            for held in locked[..nr_locked].iter().rev() {
                six_unlock_write(&(**held).c.lock);
            }
            crate::journal::bch2_journal_res_put(journal, &mut (*trans).journal_res);
            return ret;
        }
        locked[nr_locked] = b;
        nr_locked += 1;
        if !(*i).cached {
            bch2_btree_node_prep_for_write(trans, path, b);
        }
    }
    (*trans).write_locked = true;

    for idx in 0..(*trans).nr_updates as usize {
        if !verify_update_old_key(trans, (*trans).updates.add(idx)) {
            for held in locked[..nr_locked].iter().rev() {
                six_unlock_write(&(**held).c.lock);
            }
            (*trans).write_locked = false;
            crate::journal::bch2_journal_res_put(journal, &mut (*trans).journal_res);
            return -1;
        }
    }

    let mut hook = (*trans).hooks;
    while !hook.is_null() {
        let ret = ((*hook).fn_)(trans, hook);
        if ret != 0 {
            for held in locked[..nr_locked].iter().rev() {
                six_unlock_write(&(**held).c.lock);
            }
            (*trans).write_locked = false;
            crate::journal::bch2_journal_res_put(journal, &mut (*trans).journal_res);
            return ret;
        }
        hook = (*hook).next;
    }

    for idx in 0..(*trans).nr_updates as usize {
        let i = (*trans).updates.add(idx);
        if (*i).flags & BTREE_TRIGGER_norun == 0
            && ((*i).old_k.type_ == crate::snapshot::KEY_TYPE_snapshot
                || (*(*i).k).k.type_ == crate::snapshot::KEY_TYPE_snapshot)
        {
            let ret = run_one_mem_trigger(trans, i, BTREE_TRIGGER_atomic | (*i).flags);
            if ret != 0 {
                for held in locked[..nr_locked].iter().rev() {
                    six_unlock_write(&(**held).c.lock);
                }
                (*trans).write_locked = false;
                crate::journal::bch2_journal_res_put(journal, &mut (*trans).journal_res);
                return ret;
            }
        }
    }

    for idx in 0..(*trans).nr_updates as usize {
        let i = (*trans).updates.add(idx);
        if (*i).flags & BTREE_UPDATE_nojournal != 0 {
            continue;
        }
        let key_u64s = (*(*i).k).k.u64s as usize;
        let entry = crate::journal::bch2_journal_add_entry(
            journal,
            &mut (*trans).journal_res,
            crate::journal::BCH_JSET_ENTRY_btree_keys,
            (*i).btree_id,
            (*i).level,
            key_u64s as u16,
        );
        core::ptr::copy_nonoverlapping((*i).k.cast::<u64>(), (entry as *mut u64).add(1), key_u64s);
    }
    let mut entry_offset = 0usize;
    while entry_offset < (*trans).journal_entries.u64s as usize {
        let source = (*trans)
            .mem
            .add(((*trans).journal_entries.base as usize + entry_offset) * core::mem::size_of::<u64>())
            .cast::<crate::journal::jset_entry>();
        let entry = crate::journal::bch2_journal_add_entry(
            journal,
            &mut (*trans).journal_res,
            (*source).type_,
            (*source).btree_id,
            (*source).level,
            (*source).u64s,
        );
        let entry_u64s = crate::journal::jset_u64s((*source).u64s as u32) as usize;
        core::ptr::copy_nonoverlapping(
            source.cast::<u64>(),
            entry.cast::<u64>(),
            entry_u64s,
        );
        entry_offset += entry_u64s;
    }
    let journal_seq = (*trans).journal_res.seq;
    crate::journal::bch2_journal_res_put(journal, &mut (*trans).journal_res);

    for idx in 0..(*trans).nr_updates as usize {
        let i = (*trans).updates.add(idx);
        let path = (*trans).paths.add((*i).path as usize);
        bch2_btree_insert_key_leaf(trans, path, (*i).k, journal_seq);
    }

    for held in locked[..nr_locked].iter().rev() {
        six_unlock_write(&(**held).c.lock);
    }
    (*trans).write_locked = false;
    for idx in 0..(*trans).nr_updates as usize {
        bch2_trans_free_owned_key((*trans).updates.add(idx));
        let path_idx = (*trans).updates.add(idx).read().path;
        super::iter::bch2_path_put(trans, path_idx, true);
    }
    (*trans).nr_updates = 0;
    (*trans).journal_entries.u64s = 0;
    (*trans).journal_entries.size = 0;
    (*trans).accounting.u64s = 0;
    (*trans).accounting.size = 0;
    (*trans).journal_u64s = 0;
    (*trans).extra_journal_u64s = 0;
    (*trans).extra_disk_res = 0;
    crate::rewrite_log_debug!("transaction commit complete journal_seq={journal_seq}");
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btree::bkey::{
        bkey_bytes, BKEY_FORMAT_CURRENT, BKEY_U64S, KEY_FORMAT_CURRENT, POS_MIN, SPOS,
        SPOS_MAX,
    };
    use crate::btree::bset::{
        bch2_bkey_append_ptr, bch_extent_ptr, bkey_i_to_btree_ptr_v2, bset as disk_bset,
        btree_node as disk_btree_node, SET_BCH_EXTENT_PTR_DEV, SET_BCH_EXTENT_PTR_OFFSET,
    };
    use crate::btree::iter::{
        bch2_btree_iter_peek, bch2_trans_begin, bch2_trans_init, bch2_trans_iter_exit,
        bch2_trans_iter_init,
        BTREE_ITER_intent,
    };
    use crate::btree::node_iter::{
        bch2_btree_node_iter_advance, bch2_btree_node_iter_init_from_start,
        bch2_btree_node_iter_peek, bkey_unpack_pos,
    };
    use crate::btree::types::{
        bch2_btree_id_root_set, bch_fs, bset_tree, btree_node_iter, BSET_NO_AUX_TREE_VAL,
    };
    use crate::sb::io::{bch2_free_super, bch2_sb_realloc};

    #[test]
    fn transaction_update_order_keeps_alloc_after_stripes() {
        unsafe {
            let mut trans = btree_trans::default();
            bch2_trans_init(&mut trans, core::ptr::null_mut());
            trans.paths_allocated |= (1u64 << 1) | (1u64 << 2);
            for (idx, btree_id) in [(1usize, 4u8), (2usize, 6u8)] {
                let path = trans.paths.add(idx);
                (*path).btree_id = btree_id;
                (*path).pos = SPOS(1, 1, 0);
                (*path).should_be_locked = true;
            }
            let mut alloc = Box::new(bkey_i::default());
            alloc.k.u64s = BKEY_U64S;
            alloc.k.type_ = crate::btree::bset::KEY_TYPE_alloc;
            alloc.k.p = SPOS(1, 1, 0);
            let mut stripes = Box::new(bkey_i::default());
            stripes.k.u64s = BKEY_U64S;
            stripes.k.type_ = crate::btree::bset::KEY_TYPE_stripe;
            stripes.k.p = SPOS(1, 1, 0);

            assert!(!btree_trans_update_by_path(
                &mut trans,
                1,
                &mut *alloc,
                BKEY_U64S,
                0,
                0,
            )
            .is_null());
            assert!(!btree_trans_update_by_path(
                &mut trans,
                2,
                &mut *stripes,
                BKEY_U64S,
                0,
                0,
            )
            .is_null());
            assert_eq!(trans.nr_updates, 2);
            assert_eq!((*trans.updates).btree_id, 6);
            assert_eq!((*trans.updates.add(1)).btree_id, 4);
            bch2_trans_reset_updates(&mut trans);
        }
    }

    #[test]
    fn bit_mod_allocates_key_from_transaction_memory() {
        unsafe {
            let mut trans = btree_trans::default();
            bch2_trans_init(&mut trans, core::ptr::null_mut());
            trans.paths_allocated |= 1u64 << 1;
            let path = trans.paths.add(1);
            (*path).btree_id = 0;
            (*path).pos = SPOS(2, 3, 0);
            (*path).should_be_locked = true;
            (*path).nodes_locked = BTREE_NODE_INTENT_LOCKED;

            let mut iter = btree_iter::default();
            iter.trans = &mut trans;
            iter.path = 1;
            iter.btree_id = 0;
            iter.pos = (*path).pos;
            iter.flags = BTREE_ITER_intent;

            assert_eq!(bch2_btree_bit_mod_iter(&mut trans, &mut iter, true), 0);
            let entry = &*trans.updates;
            let key_addr = entry.k as usize;
            let mem_start = trans.mem as usize;
            assert!(key_addr >= mem_start);
            assert!(key_addr < mem_start + trans.mem_bytes as usize);
            assert_ne!(entry.ip_allocated, UPDATE_KEY_OWNED);
            bch2_trans_reset_updates(&mut trans);
        }
    }

    #[test]
    fn transaction_subbuf_reserve_tracks_used_u64s() {
        unsafe {
            let mut trans = btree_trans::default();
            bch2_trans_init(&mut trans, core::ptr::null_mut());
            let mut buf = btree_trans_subbuf::default();
            assert_eq!(bch2_trans_subbuf_reserve(&mut trans, &mut buf, 2), 0);
            assert_eq!(buf.u64s, 0);
            assert!(buf.size >= 2);
            let first = trans.mem.add(buf.base as usize * core::mem::size_of::<u64>());
            core::ptr::write_bytes(first, 0x5a, 2 * core::mem::size_of::<u64>());
            assert_eq!(bch2_trans_subbuf_reserve(&mut trans, &mut buf, 1), 0);
            assert_eq!(buf.u64s, 0);
            assert_eq!(*(first.cast::<u64>()), 0x5a5a5a5a5a5a5a5a);
        }
    }

    #[test]
    fn transaction_jset_entry_alloc_is_u64_aligned() {
        unsafe {
            let mut trans = btree_trans::default();
            bch2_trans_init(&mut trans, core::ptr::null_mut());
            let entry = bch2_trans_jset_entry_alloc(&mut trans, 2);
            assert!(!entry.is_null());
            assert_eq!((entry as usize) % core::mem::size_of::<u64>(), 0);
            assert_eq!(trans.journal_entries.u64s, crate::journal::jset_u64s(2) as u16);
            crate::journal::journal_entry_init(
                entry,
                crate::journal::BCH_JSET_ENTRY_write_buffer_keys,
                3,
                0,
                2,
            );
            assert_eq!((*entry).type_, crate::journal::BCH_JSET_ENTRY_write_buffer_keys);
            assert_eq!((*entry).btree_id, 3);
            assert_eq!((*entry).u64s, 2);
        }
    }

    #[test]
    fn buffered_update_queues_write_buffer_journal_entry() {
        unsafe {
            let mut trans = btree_trans::default();
            bch2_trans_init(&mut trans, core::ptr::null_mut());
            let mut key = bkey_i::default();
            bkey_init(&mut key.k);
            key.k.u64s = BKEY_U64S;
            key.k.p = SPOS(4, 5, 0);
            assert_eq!(bch2_trans_update_buffered(&mut trans, 7, &key), 0);
            let entry = trans
                .mem
                .add(trans.journal_entries.base as usize * core::mem::size_of::<u64>())
                .cast::<crate::journal::jset_entry>();
            assert_eq!((*entry).type_, crate::journal::BCH_JSET_ENTRY_write_buffer_keys);
            assert_eq!((*entry).btree_id, 7);
            assert_eq!((*entry).u64s, BKEY_U64S as u16);
            assert_eq!((*(entry.add(1).cast::<bkey>())).p, key.k.p);
            assert!(crate::btree::iter::bch2_trans_has_updates(&trans));
            let base = trans.journal_entries.base;
            trans.extra_journal_u64s = 3;
            trans.journal_u64s = 4;
            bch2_trans_reset_updates(&mut trans);
            assert_eq!(trans.journal_entries.u64s, 0);
            assert_eq!(trans.journal_entries.size, 0);
            assert_eq!(trans.journal_entries.base, base);
            assert_eq!(trans.extra_journal_u64s, 0);
            assert_eq!(trans.journal_u64s, 0);
            assert!(!crate::btree::iter::bch2_trans_has_updates(&trans));
        }
    }

    #[test]
    fn buffered_bit_mod_builds_set_key() {
        unsafe {
            let mut trans = btree_trans::default();
            bch2_trans_init(&mut trans, core::ptr::null_mut());
            assert_eq!(
                bch2_btree_bit_mod_buffered(&mut trans, 2, SPOS(8, 9, 0), true),
                0
            );
            let entry = trans
                .mem
                .add(trans.journal_entries.base as usize * core::mem::size_of::<u64>())
                .cast::<crate::journal::jset_entry>();
            let key = &*(entry.add(1).cast::<bkey>());
            assert_eq!(key.type_, crate::btree::bset::KEY_TYPE_set);
            assert_eq!(key.p, SPOS(8, 9, 0));
        }
    }

    #[test]
    fn buffered_delete_wrapper_builds_deleted_key() {
        unsafe {
            let mut trans = btree_trans::default();
            bch2_trans_init(&mut trans, core::ptr::null_mut());
            assert_eq!(
                bch2_btree_delete_at_buffered(&mut trans, 2, SPOS(8, 9, 0)),
                0
            );
            let entry = trans
                .mem
                .add(trans.journal_entries.base as usize * core::mem::size_of::<u64>())
                .cast::<crate::journal::jset_entry>();
            let key = &*(entry.add(1).cast::<bkey>());
            assert_eq!(key.type_, crate::btree::bset::KEY_TYPE_deleted);
            assert_eq!(key.p, SPOS(8, 9, 0));
        }
    }

    #[test]
    fn transaction_log_bkey_queues_structured_entry() {
        unsafe {
            let mut trans = btree_trans::default();
            bch2_trans_init(&mut trans, core::ptr::null_mut());
            let mut key = bkey_i::default();
            bkey_init(&mut key.k);
            key.k.u64s = BKEY_U64S;
            key.k.p = SPOS(10, 11, 0);
            assert_eq!(bch2_trans_log_bkey(&mut trans, 3, 2, &key), 0);
            let entry = trans
                .mem
                .add(trans.journal_entries.base as usize * core::mem::size_of::<u64>())
                .cast::<crate::journal::jset_entry>();
            assert_eq!((*entry).type_, crate::journal::BCH_JSET_ENTRY_log_bkey);
            assert_eq!((*entry).btree_id, 3);
            assert_eq!((*entry).level, 2);
            assert_eq!((*(entry.add(1).cast::<bkey>())).p, key.k.p);
        }
    }

    #[test]
    fn transaction_log_string_pads_to_u64_boundary() {
        unsafe {
            let mut trans = btree_trans::default();
            bch2_trans_init(&mut trans, core::ptr::null_mut());
            let message = b"btree log\0";
            assert_eq!(bch2_trans_log_str(&mut trans, message.as_ptr()), 0);
            let entry = trans
                .mem
                .add(trans.journal_entries.base as usize * core::mem::size_of::<u64>())
                .cast::<crate::journal::jset_entry>();
            assert_eq!((*entry).type_, crate::journal::BCH_JSET_ENTRY_log);
            assert_eq!((*entry).level, 1);
            assert_eq!((*entry).u64s, 2);
            let payload = entry.add(1).cast::<u8>();
            assert_eq!(&core::slice::from_raw_parts(payload, 9)[..], b"btree log");
            assert_eq!(*payload.add(9), 0);
        }
    }

    #[test]
    fn extent_whiteout_type_matches_current_snapshot_leaf_rules() {
        unsafe {
            let mut c = bch_fs::default();
            {
                let mut table = c.snapshots.table.write().unwrap();
                table.s.resize(2, crate::snapshot::snapshot_t::default());
                table.nr = 2;
                table.s[1].state = crate::snapshot::snapshot_id_state::SNAPSHOT_ID_live;
            }
            let leaf = u32::MAX - 1;
            let mut deleted = bkey::default();
            deleted.type_ = crate::btree::bset::KEY_TYPE_deleted;
            deleted.p = SPOS(1, 1, leaf);
            assert!(crate::btree::types::btree_id_is_extents_snapshots(0));
            assert_eq!(crate::snapshot::bch2_snapshot_is_leaf(&c, leaf), 1);
            assert_eq!(
                extent_whiteout_type(&mut c, 0, &deleted),
                crate::btree::bset::KEY_TYPE_extent_whiteout
            );
            assert_eq!(
                extent_whiteout_type(&mut c, 1, &deleted),
                crate::btree::bset::KEY_TYPE_whiteout
            );
        }
    }

    #[test]
    fn transaction_bump_allocator_reuses_memory_after_begin() {
        unsafe {
            let mut fs = bch_fs::default();
            let mut trans = btree_trans::default();
            bch2_trans_init(&mut trans, &mut fs);
            let first = bch2_trans_kmalloc(&mut trans, 100);
            assert!(!first.is_null());
            assert_eq!(trans.mem_top, 104);
            let second = bch2_trans_kmalloc_nomemzero(&mut trans, 8);
            assert_eq!(second, first.add(104));
            bch2_trans_begin(&mut trans);
            assert_eq!(trans.mem_top, 0);
            let reused = bch2_trans_kmalloc(&mut trans, 8);
            assert_eq!(reused, first);
        }
    }

    #[test]
    fn need_whiteout_for_snapshot_matches_parent_short_circuit() {
        let pos = SPOS(1, 1, 1);
        assert_eq!(unsafe { need_whiteout_for_snapshot(core::ptr::null_mut(), 0, pos) }, -22);
        unsafe {
            let mut fs = bch_fs::default();
            let mut trans = btree_trans::default();
            bch2_trans_init(&mut trans, &mut fs);
            assert_eq!(need_whiteout_for_snapshot(&mut trans, 0, pos), 0);
        }
    }

    #[test]
    fn empty_snapshot_whiteout_list_is_a_noop() {
        let list = crate::snapshot::snapshot_id_list::default();
        assert_eq!(
            unsafe {
                __bch2_insert_snapshot_whiteouts(core::ptr::null_mut(), 0, SPOS(1, 1, 1), &list)
            },
            0
        );
    }

    #[test]
    fn mutable_key_copy_uses_transaction_memory() {
        unsafe {
            let mut fs = bch_fs::default();
            let mut trans = btree_trans::default();
            bch2_trans_init(&mut trans, &mut fs);
            let mut source = bkey_i::default();
            bkey_init(&mut source.k);
            source.k.p = SPOS(7, 11, 0);
            source.k.type_ = crate::btree::bset::KEY_TYPE_extent;
            let copy = bch2_bkey_make_mut_noupdate(
                &mut trans,
                bkey_s_c {
                    k: &source.k,
                    v: core::ptr::null(),
                },
            );
            assert!(!copy.is_null());
            assert_eq!((*copy).k.p, source.k.p);
            assert_eq!((*copy).k.type_, source.k.type_);
            assert_ne!(
                copy.cast::<u8>(),
                (&source as *const bkey_i).cast::<u8>() as *mut u8
            );
        }
    }

    #[test]
    fn mutable_key_copy_honors_type_and_minimum_size() {
        unsafe {
            let mut fs = bch_fs::default();
            let mut trans = btree_trans::default();
            bch2_trans_init(&mut trans, &mut fs);
            let mut source = bkey_i::default();
            bkey_init(&mut source.k);
            source.k.p = SPOS(9, 13, 0);
            source.k.type_ = crate::btree::bset::KEY_TYPE_extent;

            let min_bytes = 64usize;
            let copy = __bch2_bkey_make_mut_noupdate(
                &mut trans,
                bkey_s_c {
                    k: &source.k,
                    v: core::ptr::null(),
                },
                min_bytes,
            );
            assert!(!copy.is_null());
            assert_eq!((*copy).k.p, source.k.p);
            assert_eq!((*copy).k.type_, source.k.type_);
            assert!(bkey_bytes(&(*copy).k) >= min_bytes);
            assert_ne!(
                copy.cast::<u8>(),
                (&source as *const bkey_i).cast::<u8>() as *mut u8
            );
        }
    }

    #[test]
    fn trigger_mutable_new_rewires_matching_update() {
        unsafe {
            let mut fs = bch_fs::default();
            let mut trans = btree_trans::default();
            bch2_trans_init(&mut trans, &mut fs);
            let mut source = Box::new(bkey_i::default());
            bkey_init(&mut source.k);
            source.k.p = SPOS(3, 17, 0);

            let entry = trans.updates;
            *entry = btree_insert_entry {
                btree_id: 2,
                level: 1,
                k: &mut *source,
                k_buf_u64s: source.k.u64s,
                ..Default::default()
            };
            trans.nr_updates = 1;
            let mut out = bkey_s::default();
            let op = btree_trigger_op {
                btree: 2,
                level: 1,
                old: bkey_s_c::default(),
                new: bkey_s {
                    k: &mut source.k,
                    v: core::ptr::null_mut(),
                },
                new_buf_u64s: source.k.u64s as u32,
                flags: BTREE_TRIGGER_insert,
            };
            let mut fast = bkey_s::default();
            assert_eq!(
                bch2_trigger_get_mutable_new(&mut trans, op, source.k.u64s as u32, &mut fast),
                0
            );
            assert_eq!(fast.k, &mut source.k as *mut bkey);

            let needed = source.k.u64s as u32 + 2;
            assert_eq!(
                bch2_trigger_get_mutable_new(&mut trans, op, needed, &mut out),
                0
            );
            assert_eq!((*entry).k_buf_u64s, needed as u8);
            assert_eq!(out.k, &(*(*entry).k).k as *const bkey as *mut bkey);
            assert_eq!((*out.k).p, source.k.p);
            bch2_trans_reset_updates(&mut trans);
        }
    }

    #[test]
    fn commit_hook_registration_matches_bcachefs_chain_order() {
        unsafe extern "C" fn hook(
            _trans: *mut btree_trans,
            _hook: *mut btree_trans_commit_hook,
        ) -> i32 {
            0
        }

        let mut trans = btree_trans::default();
        let mut first = btree_trans_commit_hook {
            fn_: hook,
            next: core::ptr::null_mut(),
        };
        let mut second = btree_trans_commit_hook {
            fn_: hook,
            next: core::ptr::null_mut(),
        };

        unsafe {
            bch2_trans_commit_hook(&mut trans, &mut first);
            bch2_trans_commit_hook(&mut trans, &mut second);
        }
        assert!(core::ptr::eq(trans.hooks, &mut second));
        assert!(core::ptr::eq(second.next, &mut first));
        unsafe { bch2_trans_reset_updates(&mut trans) };
        assert!(trans.hooks.is_null());
    }

    #[test]
    fn empty_slot_rejects_missing_transaction_or_iterator() {
        let mut iter = btree_iter::default();
        let pos = SPOS(1, 1, 0);
        assert_eq!(unsafe {
            bch2_bkey_get_empty_slot(core::ptr::null_mut(), &mut iter, 0, pos, pos)
        }, -22);
        assert_eq!(unsafe {
            bch2_bkey_get_empty_slot(
                core::ptr::null_mut(),
                core::ptr::null_mut(),
                0,
                pos,
                pos,
            )
        }, -22);
    }

    #[test]
    fn commit_flag_bits_follow_bcachefs_watermark_prefix() {
        assert_eq!(BCH_TRANS_COMMIT_no_enospc, 1 << 3);
        assert_eq!(BCH_TRANS_COMMIT_no_check_rw, 1 << 4);
        assert_eq!(BCH_TRANS_COMMIT_no_journal_res, 1 << 5);
        assert_eq!(BCH_TRANS_COMMIT_no_skip_noops, 1 << 6);
        assert_eq!(BCH_TRANS_COMMIT_journal_reclaim, 1 << 7);
        assert_eq!(BCH_TRANS_COMMIT_journal_replay, 1 << 8);
        assert_eq!(BCH_TRANS_COMMIT_skip_accounting_apply, 1 << 9);
    }

    #[test]
    fn transaction_writes_large_middle_bset_before_compacting_all() {
        unsafe {
            let path = std::env::temp_dir().join(format!(
                "subvol-core-rewrite-init-next-write-{}",
                std::process::id()
            ));
            let file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .read(true)
                .write(true)
                .open(&path)
                .unwrap();
            file.set_len(512 * 512).unwrap();

            let mut words = vec![0u64; 2048];
            let mut aux = vec![0u64; crate::btree::types::__btree_aux_data_bytes(14) / 8];
            let mut b = Box::new(btree::default());
            b.data = words.as_mut_ptr().cast::<disk_btree_node>();
            b.aux_data = aux.as_mut_ptr().cast();
            b.format = BKEY_FORMAT_CURRENT;
            b.nr_key_bits = crate::btree::bkey::bkey_format_key_bits(&b.format) as u8;
            b.byte_order = 14;
            b.c.btree_id = 0;
            b.c.level = 0;
            crate::btree::bset_build::bch2_btree_keys_init(&mut *b);
            b.nsets = 1;
            (*b.data).min_key = POS_MIN;
            (*b.data).max_key = SPOS_MAX;
            (*b.data).keys.seq = 777;
            (*b.data).keys.journal_seq = 1;
            (*b.data).keys.u64s = 5;
            *words.as_mut_ptr().add(20).cast::<bkey>() = bkey {
                u64s: BKEY_U64S,
                format: KEY_FORMAT_CURRENT,
                type_: 1,
                p: SPOS(1, 1, 0),
                ..Default::default()
            };
            b.set[0] = bset_tree {
                size: 0,
                extra: BSET_NO_AUX_TREE_VAL,
                data_offset: 17,
                aux_data_offset: u16::MAX,
                end_offset: 25,
            };
            b.nr.live_u64s = 5;
            b.nr.bset_u64s[0] = 5;
            b.nr.unpacked_keys = 1;

            b.key.k = bkey {
                u64s: 10,
                format: KEY_FORMAT_CURRENT,
                type_: crate::btree::bset::KEY_TYPE_btree_ptr_v2,
                p: SPOS_MAX,
                ..Default::default()
            };
            let node_ptr = bkey_i_to_btree_ptr_v2(&mut b.key);
            (*node_ptr).v.mem_ptr = (&mut *b as *mut btree) as usize as u64;
            (*node_ptr).v.seq = 777;
            (*node_ptr).v.min_key = POS_MIN;
            let mut extent = bch_extent_ptr::default();
            SET_BCH_EXTENT_PTR_OFFSET(&mut extent, 64);
            SET_BCH_EXTENT_PTR_DEV(&mut extent, 0);
            bch2_bkey_append_ptr(core::ptr::null(), &mut b.key, extent);

            let mut c = bch_fs::default();
            c.disk_sb.s_bdev_file = Box::into_raw(Box::new(file.try_clone().unwrap())).cast();
            assert_eq!(bch2_sb_realloc(&mut c.disk_sb, 0), 0);
            (*c.disk_sb.sb).uuid = [0x71; 16];
            (*c.disk_sb.sb).dev_idx = 0;
            (*c.disk_sb.sb).block_size = 1;
            assert_eq!(
                crate::btree::io::__bch2_btree_node_write(&mut c.disk_sb, &mut *b),
                0
            );
            assert_eq!(b.written, 1);

            let second_entry = words
                .as_mut_ptr()
                .add(64)
                .cast::<crate::btree::bset::btree_node_entry>();
            crate::btree::bset_build::bch2_bset_init_next(&mut *b, second_entry);
            (*second_entry).keys.journal_seq = 2;
            (*second_entry).keys.u64s = 515;
            for idx in 0..103usize {
                *words.as_mut_ptr().add(69 + idx * 5).cast::<bkey>() = bkey {
                    u64s: BKEY_U64S,
                    format: KEY_FORMAT_CURRENT,
                    type_: 1,
                    p: SPOS(1, idx as u64 + 2, 0),
                    ..Default::default()
                };
            }
            crate::btree::types::set_btree_bset_end(&mut *b, b.set.as_mut_ptr().add(1));

            let third_entry = words
                .as_mut_ptr()
                .add(584)
                .cast::<crate::btree::bset::btree_node_entry>();
            crate::btree::bset_build::bch2_bset_init_next(&mut *b, third_entry);
            (*third_entry).keys.journal_seq = 3;
            (*third_entry).keys.u64s = 515;
            for idx in 0..103usize {
                *words.as_mut_ptr().add(589 + idx * 5).cast::<bkey>() = bkey {
                    u64s: BKEY_U64S,
                    format: KEY_FORMAT_CURRENT,
                    type_: 1,
                    p: SPOS(1, idx as u64 + 105, 0),
                    ..Default::default()
                };
            }
            crate::btree::types::set_btree_bset_end(&mut *b, b.set.as_mut_ptr().add(2));
            b.nr.live_u64s = 1035;
            b.nr.bset_u64s = [5, 515, 515];
            b.nr.unpacked_keys = 207;
            b.flags |= 1 << crate::btree::io::BTREE_NODE_dirty;
            bch2_btree_id_root_set(&mut c, 0, &mut *b);

            let mut trans = btree_trans::default();
            bch2_trans_init(&mut trans, &mut c);
            let mut iter = btree_iter::default();
            bch2_trans_iter_init(&mut trans, &mut iter, 0, SPOS(1, 208, 0), BTREE_ITER_intent);
            let _ = bch2_btree_iter_peek(&mut iter);
            let mut insertion = bkey_i {
                k: bkey {
                    u64s: BKEY_U64S,
                    format: KEY_FORMAT_CURRENT,
                    type_: 8,
                    p: SPOS(1, 208, 0),
                    ..Default::default()
                },
                ..Default::default()
            };
            assert_eq!(
                bch2_trans_update(&mut trans, &mut iter, &mut insertion, 0),
                0
            );
            assert_eq!(bch2_trans_commit(&mut trans), 0);
            bch2_trans_iter_exit(&mut iter);

            assert_eq!(b.written, 18);
            assert_eq!((*node_ptr).v.sectors_written, 18);
            assert_eq!(b.nsets, 2);
            assert_eq!(b.set[0].end_offset, 1055);
            assert_eq!(b.set[1].data_offset, 1154);
            assert_eq!(b.set[1].end_offset, 1162);
            assert_eq!(b.nr.bset_u64s, [1035, 5, 0]);
            assert_eq!(b.nr.live_u64s, 1040);
            assert!(!crate::btree::io::btree_node_write_in_flight(&*b));
            assert!(!crate::btree::io::btree_node_just_written(&*b));

            let mut recovered_words = vec![0u64; 2048];
            let mut recovered_aux =
                vec![0u64; crate::btree::types::__btree_aux_data_bytes(14) / 8];
            let mut recovered = btree::default();
            recovered.data = recovered_words.as_mut_ptr().cast();
            recovered.aux_data = recovered_aux.as_mut_ptr().cast();
            recovered.byte_order = 14;
            recovered.c.btree_id = 0;
            recovered.c.level = 0;
            core::ptr::copy_nonoverlapping(
                (&b.key as *const bkey_i).cast::<u64>(),
                (&mut recovered.key as *mut bkey_i).cast::<u64>(),
                b.key.k.u64s as usize,
            );
            (*bkey_i_to_btree_ptr_v2(&mut recovered.key)).v.mem_ptr = 0;
            assert_eq!(
                crate::btree::io::bch2_btree_node_read(&mut c.disk_sb, &mut recovered),
                0
            );
            assert_eq!(recovered.nr.unpacked_keys, 207);
            assert_eq!((*recovered.data).keys.journal_seq, 3);

            let current_write = crate::btree::types::btree_current_write(&mut *b);
            assert_eq!((*current_write).journal.seq, 1);
            assert_eq!(crate::journal::bch2_journal_flush(&c.journal), 0);
            assert_eq!(
                c.journal
                    .last_seq
                    .load(std::sync::atomic::Ordering::Acquire),
                1
            );
            {
                let _reclaim = c.journal.reclaim_lock.lock().unwrap();
                assert_eq!(
                    crate::journal::__bch2_journal_reclaim(&c.journal, false, true),
                    0
                );
            }
            assert_eq!(
                c.journal
                    .nr_background_reclaim
                    .load(std::sync::atomic::Ordering::Acquire),
                1
            );
            assert_eq!((*current_write).journal.seq, 0);
            assert_eq!(
                c.journal
                    .last_seq
                    .load(std::sync::atomic::Ordering::Acquire),
                2
            );
            assert_eq!(crate::journal::bch2_journal_flush(&c.journal), 0);
            assert_eq!(
                c.journal
                    .last_seq_ondisk
                    .load(std::sync::atomic::Ordering::Acquire),
                3
            );

            bch2_free_super(&mut c.disk_sb);
            drop(file);
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn transaction_compacts_max_bsets_before_starting_next() {
        unsafe {
            let mut words = vec![0u64; 2048];
            let mut aux = vec![0u64; crate::btree::types::__btree_aux_data_bytes(14) / 8];
            let mut b = Box::new(btree::default());
            b.data = words.as_mut_ptr().cast::<disk_btree_node>();
            b.aux_data = aux.as_mut_ptr().cast();
            b.format = BKEY_FORMAT_CURRENT;
            b.nr_key_bits = crate::btree::bkey::bkey_format_key_bits(&b.format) as u8;
            b.byte_order = 14;
            b.c.level = 0;
            b.written = 1;
            crate::btree::bset_build::bch2_btree_keys_init(&mut *b);
            b.nsets = 3;
            (*b.data).min_key = POS_MIN;
            (*b.data).max_key = SPOS_MAX;

            let first = words.as_mut_ptr().add(17).cast::<disk_bset>();
            (*first).seq = 99;
            (*first).journal_seq = 1;
            (*first).u64s = 5;
            *words.as_mut_ptr().add(20).cast::<bkey>() = bkey {
                u64s: BKEY_U64S,
                format: KEY_FORMAT_CURRENT,
                type_: 1,
                p: SPOS(1, 1, 0),
                ..Default::default()
            };

            let second = words.as_mut_ptr().add(66).cast::<disk_bset>();
            (*second).seq = 99;
            (*second).journal_seq = 2;
            (*second).u64s = 5;
            *words.as_mut_ptr().add(69).cast::<bkey>() = bkey {
                u64s: BKEY_U64S,
                format: KEY_FORMAT_CURRENT,
                type_: 1,
                p: SPOS(1, 2, 0),
                ..Default::default()
            };

            let third = words.as_mut_ptr().add(76).cast::<disk_bset>();
            (*third).seq = 99;
            (*third).journal_seq = 3;
            (*third).u64s = 515;
            for idx in 0..103usize {
                *words.as_mut_ptr().add(79 + idx * 5).cast::<bkey>() = bkey {
                    u64s: BKEY_U64S,
                    format: KEY_FORMAT_CURRENT,
                    type_: 1,
                    p: SPOS(1, idx as u64 + 3, 0),
                    ..Default::default()
                };
            }

            b.set[0] = bset_tree {
                size: 0,
                extra: BSET_NO_AUX_TREE_VAL,
                data_offset: 17,
                aux_data_offset: u16::MAX,
                end_offset: 25,
            };
            b.set[1] = bset_tree {
                size: 0,
                extra: BSET_NO_AUX_TREE_VAL,
                data_offset: 66,
                aux_data_offset: u16::MAX,
                end_offset: 74,
            };
            b.set[2] = bset_tree {
                size: 0,
                extra: BSET_NO_AUX_TREE_VAL,
                data_offset: 76,
                aux_data_offset: u16::MAX,
                end_offset: 594,
            };
            b.nr.live_u64s = 525;
            b.nr.bset_u64s = [5, 5, 515];
            b.nr.unpacked_keys = 105;

            let mut c = bch_fs::default();
            assert_eq!(bch2_sb_realloc(&mut c.disk_sb, 0), 0);
            (*c.disk_sb.sb).block_size = 1;
            bch2_btree_id_root_set(&mut c, 0, &mut *b);

            let mut trans = btree_trans::default();
            bch2_trans_init(&mut trans, &mut c);
            let mut iter = btree_iter::default();
            bch2_trans_iter_init(&mut trans, &mut iter, 0, SPOS(1, 106, 0), BTREE_ITER_intent);
            let _ = bch2_btree_iter_peek(&mut iter);
            let mut insertion = bkey_i {
                k: bkey {
                    u64s: BKEY_U64S,
                    format: KEY_FORMAT_CURRENT,
                    type_: 8,
                    p: SPOS(1, 106, 0),
                    ..Default::default()
                },
                ..Default::default()
            };
            assert_eq!(
                bch2_trans_update(&mut trans, &mut iter, &mut insertion, 0),
                0
            );
            assert_eq!(bch2_trans_commit(&mut trans), 0);
            bch2_trans_iter_exit(&mut iter);

            assert_eq!(b.nsets, 3);
            assert_eq!(b.set[1].data_offset, 66);
            assert_eq!(b.set[1].end_offset, 589);
            assert_eq!((*second).journal_seq, 3);
            assert_eq!(b.set[2].data_offset, 591);
            assert_eq!(b.set[2].end_offset, 599);
            assert_eq!(b.nr.bset_u64s, [5, 520, 5]);
            assert_eq!(b.nr.live_u64s, 530);

            let mut node_iter = btree_node_iter::default();
            bch2_btree_node_iter_init_from_start(&mut node_iter, &mut *b);
            let mut seen = Vec::new();
            loop {
                let k = bch2_btree_node_iter_peek(&mut node_iter, &mut *b);
                if k.is_null() {
                    break;
                }
                seen.push(bkey_unpack_pos(&*b, k).offset);
                bch2_btree_node_iter_advance(&mut node_iter, &mut *b);
            }
            assert_eq!(seen, (1..=106).collect::<Vec<_>>());
            bch2_free_super(&mut c.disk_sb);
        }
    }

    #[test]
    fn transaction_starts_block_aligned_bset_after_written_set() {
        unsafe {
            let mut words = vec![0u64; 256];
            let mut b = Box::new(btree::default());
            b.data = words.as_mut_ptr().cast::<disk_btree_node>();
            b.format = BKEY_FORMAT_CURRENT;
            b.nr_key_bits = crate::btree::bkey::bkey_format_key_bits(&b.format) as u8;
            crate::btree::bset_build::bch2_btree_keys_init(&mut *b);
            b.nsets = 1;
            b.byte_order = 11;
            let mut aux =
                vec![0u64; crate::btree::types::__btree_aux_data_bytes(b.byte_order as u32) / 8];
            b.aux_data = aux.as_mut_ptr().cast();
            b.c.level = 0;
            b.written = 1;
            (*b.data).min_key = POS_MIN;
            (*b.data).max_key = SPOS_MAX;

            let first = words.as_mut_ptr().add(17).cast::<disk_bset>();
            (*first).seq = 42;
            (*first).u64s = 5;
            *words.as_mut_ptr().add(20).cast::<bkey>() = bkey {
                u64s: BKEY_U64S,
                format: KEY_FORMAT_CURRENT,
                type_: 1,
                p: SPOS(1, 1, 0),
                ..Default::default()
            };
            b.set[0] = bset_tree {
                size: 0,
                extra: BSET_NO_AUX_TREE_VAL,
                data_offset: 17,
                aux_data_offset: u16::MAX,
                end_offset: 25,
            };
            b.nr.live_u64s = 5;
            b.nr.bset_u64s[0] = 5;
            b.nr.unpacked_keys = 1;

            let mut c = bch_fs::default();
            assert_eq!(bch2_sb_realloc(&mut c.disk_sb, 0), 0);
            (*c.disk_sb.sb).block_size = 1;
            bch2_btree_id_root_set(&mut c, 0, &mut *b);

            let mut trans = btree_trans::default();
            bch2_trans_init(&mut trans, &mut c);
            let mut iter = btree_iter::default();
            bch2_trans_iter_init(&mut trans, &mut iter, 0, SPOS(1, 2, 0), BTREE_ITER_intent);
            let _ = bch2_btree_iter_peek(&mut iter);
            let mut insertion = bkey_i {
                k: bkey {
                    u64s: BKEY_U64S,
                    format: KEY_FORMAT_CURRENT,
                    type_: 8,
                    p: SPOS(1, 2, 0),
                    ..Default::default()
                },
                ..Default::default()
            };
            assert_eq!(
                bch2_trans_update(&mut trans, &mut iter, &mut insertion, 0),
                0
            );
            assert_eq!(bch2_trans_commit(&mut trans), 0);
            bch2_trans_iter_exit(&mut iter);

            assert_eq!(b.nsets, 2);
            assert_eq!(b.set[1].data_offset, 66);
            assert_eq!(b.set[1].end_offset, 74);
            let second = words.as_mut_ptr().add(66).cast::<disk_bset>();
            assert_eq!((*second).seq, 42);
            assert_eq!((*second).u64s, 5);
            assert_eq!((*words.as_ptr().add(69).cast::<bkey>()).p, SPOS(1, 2, 0));

            let mut node_iter = btree_node_iter::default();
            bch2_btree_node_iter_init_from_start(&mut node_iter, &mut *b);
            let mut seen = Vec::new();
            loop {
                let k = bch2_btree_node_iter_peek(&mut node_iter, &mut *b);
                if k.is_null() {
                    break;
                }
                seen.push((bkey_unpack_pos(&*b, k).offset, (*k).type_));
                bch2_btree_node_iter_advance(&mut node_iter, &mut *b);
            }
            assert_eq!(seen, [(1, 1), (2, 8)]);
            bch2_free_super(&mut c.disk_sb);
        }
    }

    #[test]
    fn transaction_replaces_and_inserts_under_write_lock() {
        unsafe {
            let mut words = vec![0u64; 80];
            let mut b = Box::new(btree::default());
            b.data = words.as_mut_ptr().cast::<disk_btree_node>();
            b.format = BKEY_FORMAT_CURRENT;
            b.nr_key_bits = crate::btree::bkey::bkey_format_key_bits(&b.format) as u8;
            b.nsets = 1;
            b.byte_order = 9;
            b.c.level = 0;
            (*b.data).min_key = POS_MIN;
            (*b.data).max_key = SPOS_MAX;
            let set = words.as_mut_ptr().add(17).cast::<disk_bset>();
            (*set).u64s = 10;
            for (idx, offset) in [2, 4].into_iter().enumerate() {
                *words.as_mut_ptr().add(20 + idx * 5).cast::<bkey>() = bkey {
                    u64s: BKEY_U64S,
                    format: KEY_FORMAT_CURRENT,
                    type_: 1,
                    p: SPOS(1, offset, 0),
                    ..Default::default()
                };
            }
            b.set[0] = bset_tree {
                size: 0,
                extra: BSET_NO_AUX_TREE_VAL,
                data_offset: 17,
                aux_data_offset: u16::MAX,
                end_offset: 30,
            };
            b.nr.live_u64s = 10;
            b.nr.bset_u64s[0] = 10;
            b.nr.unpacked_keys = 2;

            let mut c = bch_fs::default();
            bch2_btree_id_root_set(&mut c, 0, &mut *b);
            let mut trans = btree_trans::default();
            bch2_trans_init(&mut trans, &mut c);

            let mut replace_iter = btree_iter::default();
            bch2_trans_iter_init(
                &mut trans,
                &mut replace_iter,
                0,
                SPOS(1, 2, 0),
                BTREE_ITER_intent,
            );
            assert_eq!(
                (*bch2_btree_iter_peek(&mut replace_iter).k).p,
                SPOS(1, 2, 0)
            );
            let mut replacement = bkey_i {
                k: bkey {
                    u64s: BKEY_U64S,
                    format: KEY_FORMAT_CURRENT,
                    type_: 7,
                    p: SPOS(1, 2, 0),
                    ..Default::default()
                },
                ..Default::default()
            };
            assert_eq!(
                bch2_trans_update(&mut trans, &mut replace_iter, &mut replacement, 0),
                0
            );

            let mut insert_iter = btree_iter::default();
            bch2_trans_iter_init(
                &mut trans,
                &mut insert_iter,
                0,
                SPOS(1, 3, 0),
                BTREE_ITER_intent,
            );
            assert_eq!((*bch2_btree_iter_peek(&mut insert_iter).k).p, SPOS(1, 4, 0));
            let mut insertion = bkey_i {
                k: bkey {
                    u64s: BKEY_U64S,
                    format: KEY_FORMAT_CURRENT,
                    type_: 8,
                    p: SPOS(1, 3, 0),
                    ..Default::default()
                },
                ..Default::default()
            };
            assert_eq!(
                bch2_trans_update(&mut trans, &mut insert_iter, &mut insertion, 0),
                0
            );
            assert_eq!(trans.nr_updates, 2);
            assert_eq!(bch2_trans_commit(&mut trans), 0);
            assert_eq!(trans.nr_updates, 0);
            assert_eq!(trans.journal_u64s, 0);
            assert_eq!(trans.extra_journal_u64s, 0);
            assert_eq!(trans.extra_disk_res, 0);
            bch2_trans_iter_exit(&mut insert_iter);
            bch2_trans_iter_exit(&mut replace_iter);

            let mut node_iter = btree_node_iter::default();
            bch2_btree_node_iter_init_from_start(&mut node_iter, &mut *b);
            let mut seen = Vec::new();
            loop {
                let k = bch2_btree_node_iter_peek(&mut node_iter, &mut *b);
                if k.is_null() {
                    break;
                }
                seen.push((bkey_unpack_pos(&*b, k).offset, (*k).type_));
                bch2_btree_node_iter_advance(&mut node_iter, &mut *b);
            }
            assert_eq!(seen, [(2, 7), (3, 8), (4, 1)]);
            assert_eq!(b.nr.live_u64s, 15);
            assert_eq!((*set).journal_seq, 1);
            assert_ne!(b.flags & (1 << 5), 0);

            assert_eq!(crate::journal::bch2_journal_flush(&c.journal), 0);
            let records = c.journal.closed.lock().unwrap();
            assert_eq!(records.len(), 1);
            assert_eq!(records[0][5], 12);
            let first = &*(records[0]
                .as_ptr()
                .add(crate::journal::JSET_HEADER_U64S)
                .cast::<crate::journal::jset_entry>());
            let second = &*(records[0]
                .as_ptr()
                .add(crate::journal::JSET_HEADER_U64S + 6)
                .cast::<crate::journal::jset_entry>());
            assert_eq!((first.type_, first.btree_id, first.u64s), (0, 0, 5));
            assert_eq!((second.type_, second.btree_id, second.u64s), (0, 0, 5));
            drop(records);

            (*set).u64s = 10;
            (*set).journal_seq = 0;
            b.set[0].end_offset = 30;
            b.nr.live_u64s = 10;
            b.nr.bset_u64s = [10, 0, 0];
            b.nr.unpacked_keys = 2;
            b.flags = 0;
            for (idx, offset) in [2, 4].into_iter().enumerate() {
                *words.as_mut_ptr().add(20 + idx * 5).cast::<bkey>() = bkey {
                    u64s: BKEY_U64S,
                    format: KEY_FORMAT_CURRENT,
                    type_: 1,
                    p: SPOS(1, offset, 0),
                    ..Default::default()
                };
            }

            assert_eq!(crate::journal::bch2_journal_replay(&mut c), 0);
            let mut replay_iter = btree_node_iter::default();
            bch2_btree_node_iter_init_from_start(&mut replay_iter, &mut *b);
            let mut replayed = Vec::new();
            loop {
                let k = bch2_btree_node_iter_peek(&mut replay_iter, &mut *b);
                if k.is_null() {
                    break;
                }
                replayed.push((bkey_unpack_pos(&*b, k).offset, (*k).type_));
                bch2_btree_node_iter_advance(&mut replay_iter, &mut *b);
            }
            assert_eq!(replayed, [(2, 7), (3, 8), (4, 1)]);
            assert_eq!((*set).journal_seq, 1);
            assert_eq!(c.journal.closed.lock().unwrap().len(), 1);
        }
    }
}
