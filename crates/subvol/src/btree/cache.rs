use super::iter::btree_trans;
use super::types::{
    __btree_aux_data_bytes, btree, btree_node_cache_state, btree_node_dirty,
    btree_node_write_in_flight,
};
use crate::lock::six::{
    six_lock_wakeup_all, six_trylock_intent, six_trylock_write, six_unlock_intent, six_unlock_write,
};
use std::sync::atomic::{fence, Ordering};

pub unsafe fn bch2_fs_btree_cache_init(c: *mut super::types::bch_fs) -> i32 {
    if c.is_null() {
        return -22;
    }
    if (*c).btree.cache.table_init_done {
        return 0;
    }
    bch2_fs_btree_cache_init_early(&mut (*c).btree.cache);
    let params = crate::util::rhashtable::rhashtable_params {
        nelem_hint: 0,
        key_len: core::mem::size_of::<u64>() as u16,
        key_offset: core::mem::offset_of!(btree, hash_val) as u16,
        head_offset: core::mem::offset_of!(btree, hash) as u16,
        max_size: 0,
        min_size: 0,
        automatic_shrinking: true,
        hashfn: None,
        obj_hashfn: None,
        obj_cmpfn: None,
    };
    let ret = crate::util::rhashtable::rhashtable_init(&mut (*c).btree.cache.table, &params);
    if ret == 0 {
        (*c).btree.cache.table_init_done = true;
        bch2_recalc_btree_reserve(c);
        if !(*c).disk_sb.sb.is_null() {
            for _ in 0..(*c).btree.cache.nr_reserve {
                let node = __bch2_btree_node_mem_alloc(c);
                if node.is_null() {
                    bch2_fs_btree_cache_exit(c);
                    return -12;
                }
                if !six_trylock_intent(&(*node).c.lock) {
                    bch2_btree_node_mem_free(c, node);
                    bch2_fs_btree_cache_exit(c);
                    return -12;
                }
                if !six_trylock_write(&(*node).c.lock) {
                    six_unlock_intent(&(*node).c.lock);
                    bch2_btree_node_mem_free(c, node);
                    bch2_fs_btree_cache_exit(c);
                    return -12;
                }
                let transition = bch2_btree_node_transition_state(
                    &mut (*c).btree.cache,
                    node,
                    btree_node_cache_state::BTREE_NODE_CACHE_FREEABLE,
                );
                six_unlock_write(&(*node).c.lock);
                six_unlock_intent(&(*node).c.lock);
                if transition != 0 {
                    bch2_btree_node_mem_free(c, node);
                    bch2_fs_btree_cache_exit(c);
                    return transition;
                }
            }
        }
    }
    ret
}

pub unsafe fn bch2_fs_btree_cache_init_early(bc: *mut super::types::bch_fs_btree_cache) {
    super::types::INIT_LIST_HEAD(&mut (*bc).freeable);
    super::types::INIT_LIST_HEAD(&mut (*bc).freed_pcpu);
    super::types::INIT_LIST_HEAD(&mut (*bc).freed_nonpcpu);
    for (idx, live) in (*bc).live.iter_mut().enumerate() {
        live.idx = idx;
        super::types::INIT_LIST_HEAD(&mut live.clean);
        super::types::INIT_LIST_HEAD(&mut live.dirty);
        live.nr_clean = 0;
        live.nr_dirty = 0;
    }
}

pub unsafe fn bch2_recalc_btree_reserve(c: *mut super::types::bch_fs) {
    if c.is_null() {
        return;
    }
    let mut reserve = 16usize;
    if (*c).btree.cache.roots_known[0].b.is_null() {
        reserve += 8;
    }
    for id in 0..super::types::BTREE_ID_NR {
        let root = &(*c).btree.cache.roots_known[id];
        if !root.b.is_null() {
            reserve += usize::from((*(*root).b).c.level.min(1)) * 8;
        }
    }
    (*c).btree.cache.nr_reserve = reserve;
}

pub unsafe fn btree_ptr_hash_val(k: *const super::bkey::bkey_i) -> u64 {
    if (*k).k.type_ == super::bset::KEY_TYPE_btree_ptr_v2 {
        (*(k as *const super::bset::bkey_i_btree_ptr_v2)).v.seq
    } else {
        0
    }
}

pub unsafe fn btree_node_hashed(b: *const btree) -> bool {
    (*b).hash_val != 0
}

pub const BTREE_EVICTED_SIZE_HASH_MASK: u64 = (1u64 << 48) - 1;

pub const fn btree_evicted_size_pack(hash: u64, live_u64s: u16) -> u64 {
    ((hash & BTREE_EVICTED_SIZE_HASH_MASK) << 16) | live_u64s as u64
}

pub unsafe fn bch2_btree_evicted_size_record(
    c: *mut super::types::bch_fs,
    hash: u64,
    live_u64s: u16,
) {
    let e = &mut (*c).btree.evicted_size;
    if !e.entries.is_empty() {
        let idx = (hash & e.mask) as usize;
        e.entries[idx] = btree_evicted_size_pack(hash, live_u64s);
    }
}

pub unsafe fn bch2_btree_evicted_size_lookup(
    c: *mut super::types::bch_fs,
    hash: u64,
    out: *mut u16,
) -> bool {
    let e = &(*c).btree.evicted_size;
    if e.entries.is_empty() || out.is_null() {
        return false;
    }
    let entry = e.entries[(hash & e.mask) as usize];
    if (entry >> 16) != (hash & BTREE_EVICTED_SIZE_HASH_MASK) {
        return false;
    }
    *out = entry as u16;
    true
}

pub unsafe fn bch2_fs_btree_evicted_size_init(c: *mut super::types::bch_fs) -> i32 {
    if c.is_null() {
        return -22;
    }
    let entries = 1usize << 17;
    (*c).btree.evicted_size.entries = vec![0; entries];
    (*c).btree.evicted_size.mask = (entries - 1) as u64;
    0
}

pub unsafe fn bch2_fs_btree_evicted_size_exit(c: *mut super::types::bch_fs) {
    if c.is_null() {
        return;
    }
    (*c).btree.evicted_size.entries.clear();
    (*c).btree.evicted_size.mask = 0;
}

pub unsafe fn btree_node_cache_state(b: *const btree) -> btree_node_cache_state {
    (*b).cache_state
}

unsafe fn btree_node_state_hashed(state: btree_node_cache_state) -> bool {
    state == btree_node_cache_state::BTREE_NODE_CACHE_CLEAN
        || state == btree_node_cache_state::BTREE_NODE_CACHE_DIRTY
}

unsafe fn btree_node_state_has_buffer(state: btree_node_cache_state) -> bool {
    btree_node_state_hashed(state) || state == btree_node_cache_state::BTREE_NODE_CACHE_FREEABLE
}

pub const BTREE_NODE_RECLAIM_shrinker: u32 = 1 << 0;
pub const BTREE_NODE_RECLAIM_allow_dirty: u32 = 1 << 1;

pub unsafe fn btree_node_reclaim(c: *mut super::types::bch_fs, b: *mut btree, flags: u32) -> i32 {
    let _bc = &(*c).btree.cache;
    let checks = |node: *const btree| {
        if super::types::btree_node_permanent(node)
            || super::types::btree_node_noevict(node)
            || super::types::btree_node_write_blocked(node)
            || super::types::btree_node_will_make_reachable(node)
        {
            return -12;
        }
        if flags & BTREE_NODE_RECLAIM_allow_dirty == 0
            && (super::types::btree_node_dirty(node)
                || super::types::btree_node_read_in_flight(node)
                || super::types::btree_node_write_in_flight(node))
        {
            return -12;
        }
        0
    };

    let ret = checks(b);
    if ret != 0 {
        return ret;
    }
    if !six_trylock_intent(&(*b).c.lock) {
        return -12;
    }
    if !six_trylock_write(&(*b).c.lock) {
        six_unlock_intent(&(*b).c.lock);
        return -12;
    }
    let ret = checks(b);
    if ret != 0 {
        six_unlock_write(&(*b).c.lock);
        six_unlock_intent(&(*b).c.lock);
    }
    ret
}

pub unsafe fn btree_node_live_state(b: *const btree) -> btree_node_cache_state {
    if btree_node_dirty(b) || btree_node_write_in_flight(b) {
        btree_node_cache_state::BTREE_NODE_CACHE_DIRTY
    } else {
        btree_node_cache_state::BTREE_NODE_CACHE_CLEAN
    }
}

unsafe fn btree_node_is_root(c: *const super::types::bch_fs, b: *const btree) -> bool {
    let id = (*b).c.btree_id as usize;
    id < super::types::BTREE_ID_NR && super::types::bch2_btree_id_root_b(c, id) == b.cast_mut()
}

pub unsafe fn bch2_node_pin(c: *mut super::types::bch_fs, b: *mut btree) {
    let bc = &mut (*c).btree.cache as *mut super::types::bch_fs_btree_cache;
    let _lock = (*bc).lock.lock().unwrap();
    if !btree_node_is_root(c, b) && !super::types::btree_node_pinned(b) {
        super::types::set_btree_node_pinned(b);
        match (*b).cache_state {
            btree_node_cache_state::BTREE_NODE_CACHE_CLEAN => {
                (*bc).live[0].nr_clean -= 1;
                (*bc).live[1].nr_clean += 1;
                super::types::list_move_tail(&mut (*b).list, &mut (*bc).live[1].clean);
            }
            btree_node_cache_state::BTREE_NODE_CACHE_DIRTY => {
                (*bc).live[0].nr_dirty -= 1;
                (*bc).live[1].nr_dirty += 1;
                super::types::list_move_tail(&mut (*b).list, &mut (*bc).live[1].dirty);
            }
            _ => {}
        }
    }
}

pub unsafe fn bch2_btree_cache_unpin(c: *mut super::types::bch_fs) {
    let bc = &mut (*c).btree.cache as *mut super::types::bch_fs_btree_cache;
    let _lock = (*bc).lock.lock().unwrap();
    (*bc).pinned_nodes_mask = [0; 2];
    let list_offset = core::mem::offset_of!(btree, list);
    for head in [
        &mut (*bc).live[1].clean as *mut super::types::list_head,
        &mut (*bc).live[1].dirty as *mut super::types::list_head,
    ] {
        let mut pos = (*head).next;
        while pos != head {
            let next = (*pos).next;
            let b = pos.cast::<u8>().sub(list_offset).cast::<btree>();
            super::types::clear_btree_node_pinned(b);
            pos = next;
        }
    }
    super::types::list_splice_tail_init(&mut (*bc).live[1].clean, &mut (*bc).live[0].clean);
    super::types::list_splice_tail_init(&mut (*bc).live[1].dirty, &mut (*bc).live[0].dirty);
    (*bc).live[0].nr_clean += (*bc).live[1].nr_clean;
    (*bc).live[0].nr_dirty += (*bc).live[1].nr_dirty;
    (*bc).live[1].nr_clean = 0;
    (*bc).live[1].nr_dirty = 0;
}

pub unsafe fn bch2_btree_node_transition_state_locked(
    bc: *mut super::types::bch_fs_btree_cache,
    b: *mut btree,
    mut new: btree_node_cache_state,
) -> i32 {
    let old = (*b).cache_state;
    if old == new {
        return 0;
    }
    assert!(!btree_node_state_has_buffer(old) || !(*b).data.is_null());
    assert!(!btree_node_state_has_buffer(new) || !(*b).data.is_null());
    assert!(btree_node_state_hashed(new) || !btree_node_dirty(b));

    let mut ret = 0;
    let mut hashed_delta =
        btree_node_state_hashed(new) as i32 - btree_node_state_hashed(old) as i32;
    if hashed_delta > 0 {
        (*b).hash_val = btree_ptr_hash_val(&(*b).key);
        ret = crate::util::rhashtable::rhashtable_lookup_insert_fast(
            &mut (*bc).table,
            &mut (*b).hash,
        );
        if ret != 0 {
            (*b).hash_val = 0;
            new = btree_node_cache_state::BTREE_NODE_CACHE_FREEABLE;
            hashed_delta = 0;
        } else if (*b).c.btree_id < super::types::BTREE_ID_NR as u8 {
            (*bc).nr_by_btree[(*b).c.btree_id as usize] += 1;
        }
    }
    if hashed_delta < 0 {
        assert_eq!(
            crate::util::rhashtable::rhashtable_remove_fast(&mut (*bc).table, &mut (*b).hash),
            0
        );
        (*b).hash_val = 0;
        super::types::clear_btree_node_just_written(b);
        fence(Ordering::SeqCst);
        six_lock_wakeup_all(&(*b).c.lock);
        if (*b).c.btree_id < super::types::BTREE_ID_NR as u8 {
            (*bc).nr_by_btree[(*b).c.btree_id as usize] -= 1;
        }
    }

    match old {
        btree_node_cache_state::BTREE_NODE_CACHE_CLEAN => {
            let pinned = super::types::btree_node_pinned(b) as usize;
            (*bc).live[pinned].nr_clean -= 1;
        }
        btree_node_cache_state::BTREE_NODE_CACHE_DIRTY => {
            let pinned = super::types::btree_node_pinned(b) as usize;
            (*bc).live[pinned].nr_dirty -= 1;
        }
        btree_node_cache_state::BTREE_NODE_CACHE_FREEABLE => (*bc).nr_freeable -= 1,
        btree_node_cache_state::BTREE_NODE_CACHE_NONE
        | btree_node_cache_state::BTREE_NODE_CACHE_FREED => {}
    }
    super::types::list_del_init(&mut (*b).list);

    match new {
        btree_node_cache_state::BTREE_NODE_CACHE_NONE => {}
        btree_node_cache_state::BTREE_NODE_CACHE_CLEAN => {
            let pinned = super::types::btree_node_pinned(b) as usize;
            (*bc).live[pinned].nr_clean += 1;
            super::types::list_add_tail(&mut (*b).list, &mut (*bc).live[pinned].clean);
        }
        btree_node_cache_state::BTREE_NODE_CACHE_DIRTY => {
            let pinned = super::types::btree_node_pinned(b) as usize;
            (*bc).live[pinned].nr_dirty += 1;
            super::types::list_add_tail(&mut (*b).list, &mut (*bc).live[pinned].dirty);
        }
        btree_node_cache_state::BTREE_NODE_CACHE_FREEABLE => {
            (*bc).nr_freeable += 1;
            super::types::list_add(&mut (*b).list, &mut (*bc).freeable);
        }
        btree_node_cache_state::BTREE_NODE_CACHE_FREED => {
            bch2_btree_node_data_free(b);
            let list = if (*b).c.lock.readers.is_some() {
                &mut (*bc).freed_pcpu
            } else {
                &mut (*bc).freed_nonpcpu
            };
            super::types::list_add_tail(&mut (*b).list, list);
        }
    }
    (*b).cache_state = new;
    ret
}

pub unsafe fn bch2_btree_node_transition_state(
    bc: *mut super::types::bch_fs_btree_cache,
    b: *mut btree,
    new: btree_node_cache_state,
) -> i32 {
    let _lock = (*bc).lock.lock().unwrap();
    bch2_btree_node_transition_state_locked(bc, b, new)
}

pub unsafe fn bch2_btree_node_set_dirty(c: *mut super::types::bch_fs, b: *mut btree) {
    let bc = &mut (*c).btree.cache as *mut super::types::bch_fs_btree_cache;
    let _lock = (*bc).lock.lock().unwrap();
    if super::types::btree_node_dirty(b) {
        return;
    }
    super::types::set_btree_node_dirty(b);
    if btree_node_state_hashed((*b).cache_state) {
        bch2_btree_node_transition_state_locked(
            bc,
            b,
            btree_node_cache_state::BTREE_NODE_CACHE_DIRTY,
        );
    }
}

pub unsafe fn bch2_btree_node_write_done_clean(c: *mut super::types::bch_fs, b: *mut btree) {
    let bc = &mut (*c).btree.cache as *mut super::types::bch_fs_btree_cache;
    let _lock = (*bc).lock.lock().unwrap();
    if btree_node_state_hashed((*b).cache_state) {
        bch2_btree_node_transition_state_locked(bc, b, btree_node_live_state(b));
    }
}

pub unsafe fn bch2_btree_node_data_free(b: *mut btree) {
    assert!(!super::io::btree_node_write_in_flight(b));
    if (*b).data.is_null() {
        return;
    }

    super::types::clear_btree_node_just_written(b);
    let data = (*b).data;
    let aux_data = (*b).aux_data;
    let byte_order = (*b).byte_order;
    (*b).data = core::ptr::null_mut();
    (*b).aux_data = core::ptr::null_mut();

    let data_layout = std::alloc::Layout::from_size_align_unchecked(
        1usize << byte_order,
        core::mem::align_of::<u64>(),
    );
    std::alloc::dealloc(data.cast(), data_layout);
    if !aux_data.is_null() {
        let aux_layout = std::alloc::Layout::from_size_align_unchecked(
            __btree_aux_data_bytes(byte_order as u32),
            core::mem::align_of::<u64>(),
        );
        std::alloc::dealloc(aux_data.cast(), aux_layout);
    }
}

pub unsafe fn __bch2_btree_node_mem_alloc(c: *mut super::types::bch_fs) -> *mut btree {
    if c.is_null() || (*c).disk_sb.sb.is_null() {
        return core::ptr::null_mut();
    }
    let bytes = crate::sb::io::BCH_SB_BTREE_NODE_SIZE(&*(*c).disk_sb.sb) as usize * 512;
    let byte_order = bytes.trailing_zeros() as u8;
    if !bytes.is_power_of_two() || byte_order < 9 {
        return core::ptr::null_mut();
    }

    let data_layout = std::alloc::Layout::from_size_align_unchecked(
        1usize << byte_order,
        core::mem::align_of::<u64>(),
    );
    let data = std::alloc::alloc_zeroed(data_layout).cast::<u64>();
    if data.is_null() {
        return core::ptr::null_mut();
    }
    let aux_layout = std::alloc::Layout::from_size_align_unchecked(
        __btree_aux_data_bytes(byte_order as u32),
        core::mem::align_of::<u64>(),
    );
    let aux = std::alloc::alloc_zeroed(aux_layout).cast::<u64>();
    if aux.is_null() {
        std::alloc::dealloc(data.cast(), data_layout);
        return core::ptr::null_mut();
    }

    let mut node = Box::new(btree::default());
    node.key.k.u64s = super::bkey::BKEY_U64S;
    node.key.k.format = super::bkey::KEY_FORMAT_CURRENT;
    node.key.k.type_ = super::bset::KEY_TYPE_btree_ptr_v2;
    crate::lock::six::six_lock_init(&mut node.c.lock, 0);
    node.data = data.cast();
    node.aux_data = aux.cast();
    node.byte_order = byte_order;
    super::types::INIT_LIST_HEAD(&mut node.list);
    super::types::INIT_LIST_HEAD(&mut node.write_blocked);
    super::bset_build::bch2_btree_keys_init(&mut *node);
    let node = Box::into_raw(node);
    (*c).btree
        .cache
        .allocations
        .lock()
        .unwrap()
        .push(node as usize);
    node
}

pub unsafe fn bch2_btree_node_mem_free(c: *mut super::types::bch_fs, b: *mut btree) {
    let mut allocations = (*c).btree.cache.allocations.lock().unwrap();
    let index = allocations
        .iter()
        .position(|&node| node == b as usize)
        .unwrap();
    allocations.swap_remove(index);
    drop(allocations);
    crate::lock::six::six_lock_exit(&mut (*b).c.lock);
    drop(Box::from_raw(b));
}

pub unsafe fn bch2_btree_node_mem_alloc(
    trans: *mut btree_trans,
    pcpu_read_locks: bool,
) -> *mut btree {
    if trans.is_null() || (*trans).c.is_null() {
        return core::ptr::null_mut();
    }
    let c = (*trans).c;
    if (*c).disk_sb.sb.is_null() {
        return core::ptr::null_mut();
    }
    let bytes = crate::sb::io::BCH_SB_BTREE_NODE_SIZE(&*(*c).disk_sb.sb) as usize * 512;
    let byte_order = bytes.trailing_zeros() as u8;
    if !bytes.is_power_of_two() || byte_order < 9 {
        return core::ptr::null_mut();
    }

    let bc = &mut (*c).btree.cache as *mut super::types::bch_fs_btree_cache;
    let mut reused_freeable = core::ptr::null_mut();
    if (*bc).table_init_done {
        let _lock = (*bc).lock.lock().unwrap();
        let head = &mut (*bc).freeable as *mut super::types::list_head;
        let list_offset = core::mem::offset_of!(btree, list);
        let mut pos = (*head).next;
        while pos != head {
            let next = (*pos).next;
            let node = pos.cast::<u8>().sub(list_offset).cast::<btree>();
            if pcpu_read_locks == (*node).c.lock.readers.is_some()
                && six_trylock_intent(&(*node).c.lock)
            {
                if six_trylock_write(&(*node).c.lock)
                    && bch2_btree_node_transition_state_locked(
                        bc,
                        node,
                        btree_node_cache_state::BTREE_NODE_CACHE_NONE,
                    ) == 0
                {
                    six_unlock_write(&(*node).c.lock);
                    six_unlock_intent(&(*node).c.lock);
                    reused_freeable = node;
                    break;
                }
                if crate::lock::six::six_lock_counts(&(*node).c.lock).n[2] != 0 {
                    six_unlock_write(&(*node).c.lock);
                }
                six_unlock_intent(&(*node).c.lock);
            }
            pos = next;
        }
    }

    if !reused_freeable.is_null() {
        (*reused_freeable).flags = 0;
        (*reused_freeable).written = 0;
        (*reused_freeable).nsets = 0;
        (*reused_freeable).sib_u64s = [0; 2];
        (*reused_freeable).whiteout_u64s = 0;
        super::types::INIT_LIST_HEAD(&mut (*reused_freeable).write_blocked);
        super::bset_build::bch2_btree_keys_init(reused_freeable);
        return reused_freeable;
    }

    let data_layout = std::alloc::Layout::from_size_align_unchecked(
        1usize << byte_order,
        core::mem::align_of::<u64>(),
    );
    let data_ptr = std::alloc::alloc_zeroed(data_layout).cast::<u64>();
    if data_ptr.is_null() {
        if (*bc).table_init_done {
            let _cache_lock = (*bc).lock.lock().unwrap();
            let list_offset = core::mem::offset_of!(btree, list);
            for live_idx in 0..2 {
                let head = &mut (*bc).live[live_idx].clean as *mut super::types::list_head;
                let mut pos = (*head).next;
                while pos != head {
                    let next = (*pos).next;
                    let node = pos.cast::<u8>().sub(list_offset).cast::<btree>();
                    if pcpu_read_locks == (*node).c.lock.readers.is_some()
                        && btree_node_reclaim(c, node, 0) == 0
                    {
                        let ret = bch2_btree_node_transition_state_locked(
                            bc,
                            node,
                            btree_node_cache_state::BTREE_NODE_CACHE_NONE,
                        );
                        six_unlock_write(&(*node).c.lock);
                        six_unlock_intent(&(*node).c.lock);
                        if ret == 0 {
                            (*node).flags = 0;
                            (*node).written = 0;
                            (*node).nsets = 0;
                            (*node).sib_u64s = [0; 2];
                            (*node).whiteout_u64s = 0;
                            super::types::INIT_LIST_HEAD(&mut (*node).write_blocked);
                            super::bset_build::bch2_btree_keys_init(node);
                            return node;
                        }
                    }
                    pos = next;
                }
            }
        }
        return core::ptr::null_mut();
    }
    let aux_layout = std::alloc::Layout::from_size_align_unchecked(
        __btree_aux_data_bytes(byte_order as u32),
        core::mem::align_of::<u64>(),
    );
    let aux_ptr = std::alloc::alloc_zeroed(aux_layout).cast::<u64>();
    if aux_ptr.is_null() {
        std::alloc::dealloc(data_ptr.cast(), data_layout);
        return core::ptr::null_mut();
    }

    if (*bc).table_init_done {
        let _lock = (*bc).lock.lock().unwrap();
        let head = if pcpu_read_locks {
            &mut (*bc).freed_pcpu as *mut super::types::list_head
        } else {
            &mut (*bc).freed_nonpcpu as *mut super::types::list_head
        };
        let list_offset = core::mem::offset_of!(btree, list);
        let mut pos = (*head).next;
        while pos != head {
            let next = (*pos).next;
            let node = pos.cast::<u8>().sub(list_offset).cast::<btree>();
            if pcpu_read_locks == (*node).c.lock.readers.is_some()
                && bch2_btree_node_transition_state_locked(
                    bc,
                    node,
                    btree_node_cache_state::BTREE_NODE_CACHE_NONE,
                ) == 0
            {
                (*node).data = data_ptr.cast();
                (*node).aux_data = aux_ptr.cast();
                (*node).byte_order = byte_order;
                super::types::INIT_LIST_HEAD(&mut (*node).write_blocked);
                (*node).flags = 0;
                (*node).written = 0;
                (*node).nsets = 0;
                (*node).sib_u64s = [0; 2];
                (*node).whiteout_u64s = 0;
                super::bset_build::bch2_btree_keys_init(node);
                return node;
            }
            pos = next;
        }
    }

    let mut node = Box::new(btree::default());
    (*node).key.k.u64s = super::bkey::BKEY_U64S;
    (*node).key.k.format = super::bkey::KEY_FORMAT_CURRENT;
    (*node).key.k.type_ = super::bset::KEY_TYPE_btree_ptr_v2;
    crate::lock::six::six_lock_init(
        &mut node.c.lock,
        if pcpu_read_locks {
            crate::lock::six::SIX_LOCK_INIT_PCPU
        } else {
            0
        },
    );
    node.data = data_ptr.cast();
    node.aux_data = aux_ptr.cast();
    node.byte_order = byte_order;
    let node = Box::into_raw(node);
    super::types::INIT_LIST_HEAD(&mut (*node).write_blocked);
    super::types::INIT_LIST_HEAD(&mut (*node).list);
    super::bset_build::bch2_btree_keys_init(node);

    (*c).btree
        .cache
        .allocations
        .lock()
        .unwrap()
        .push(node as usize);
    node
}

pub unsafe fn bch2_btree_node_evict(trans: *mut btree_trans, key: *const super::bkey::bkey_i) {
    if trans.is_null() || (*trans).c.is_null() || key.is_null() {
        return;
    }
    let c = (*trans).c;
    let bc = &mut (*c).btree.cache;
    if !bc.table_init_done {
        return;
    }
    let hash_val = btree_ptr_hash_val(key);
    let node = crate::util::rhashtable::rhashtable_lookup_fast(
        &mut bc.table,
        &hash_val as *const u64 as *const _,
    ) as *mut btree;
    if node.is_null() {
        return;
    }
    assert!(!super::types::btree_node_permanent(node));

    loop {
        while super::types::btree_node_read_in_flight(node)
            || super::types::btree_node_write_in_flight(node)
        {
            std::thread::yield_now();
        }
        while crate::lock::six::six_lock_intent(&(*node).c.lock) != 0 {
            std::thread::yield_now();
        }
        while crate::lock::six::six_lock_write(&(*node).c.lock) != 0 {
            std::thread::yield_now();
        }

        if (*node).hash_val != hash_val {
            crate::lock::six::six_unlock_write(&(*node).c.lock);
            crate::lock::six::six_unlock_intent(&(*node).c.lock);
            return;
        }
        if super::types::btree_node_dirty(node) {
            super::io::bch2_btree_node_write_trans(
                trans,
                node,
                crate::lock::six::six_lock_type::SIX_LOCK_write,
                super::io::BTREE_WRITE_cache_reclaim,
            );
            crate::lock::six::six_unlock_write(&(*node).c.lock);
            crate::lock::six::six_unlock_intent(&(*node).c.lock);
            continue;
        }

        bch2_btree_evicted_size_record(c, (*node).hash_val, (*node).nr.live_u64s);
        let _ = bch2_btree_node_transition_state(
            bc,
            node,
            btree_node_cache_state::BTREE_NODE_CACHE_FREED,
        );
        crate::lock::six::six_unlock_write(&(*node).c.lock);
        crate::lock::six::six_unlock_intent(&(*node).c.lock);
        return;
    }
}

pub unsafe fn bch2_fs_btree_cache_exit(c: *mut super::types::bch_fs) {
    if c.is_null() {
        return;
    }
    let bc = &mut (*c).btree.cache as *mut super::types::bch_fs_btree_cache;
    if !(*bc).table_init_done {
        return;
    }
    assert_eq!((*bc).live[0].nr_dirty + (*bc).live[1].nr_dirty, 0);
    {
        let _cache_lock = (*bc).lock.lock().unwrap();
        let list_offset = core::mem::offset_of!(btree, list);
        for head in [
            &mut (*bc).live[0].clean as *mut super::types::list_head,
            &mut (*bc).live[1].clean as *mut super::types::list_head,
            &mut (*bc).freeable as *mut super::types::list_head,
        ] {
            let mut pos = (*head).next;
            while pos != head {
                let next = (*pos).next;
                let node = pos.cast::<u8>().sub(list_offset).cast::<btree>();
                super::types::clear_btree_node_permanent(node);
                assert!(six_trylock_intent(&(*node).c.lock));
                assert!(six_trylock_write(&(*node).c.lock));
                assert_eq!(
                    bch2_btree_node_transition_state_locked(
                        bc,
                        node,
                        btree_node_cache_state::BTREE_NODE_CACHE_FREED,
                    ),
                    0
                );
                six_unlock_write(&(*node).c.lock);
                six_unlock_intent(&(*node).c.lock);
                pos = next;
            }
        }
        super::types::list_splice(&mut (*bc).freed_pcpu, &mut (*bc).freed_nonpcpu);
        let head = &mut (*bc).freed_nonpcpu as *mut super::types::list_head;
        let mut pos = (*head).next;
        while pos != head {
            let next = (*pos).next;
            let node = pos.cast::<u8>().sub(list_offset).cast::<btree>();
            super::types::list_del_init(pos);
            bch2_btree_node_mem_free(c, node);
            pos = next;
        }
    }
    crate::util::rhashtable::rhashtable_destroy(&mut (*bc).table);
    (*bc).table_init_done = false;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btree::bset::{bkey_i_btree_ptr_v2, KEY_TYPE_btree_ptr_v2};
    use crate::btree::types::bch_fs;
    use crate::sb::io::{bch2_free_super, bch2_sb_realloc};

    #[test]
    fn allocates_and_initializes_node_buffers_from_cache_geometry() {
        unsafe {
            let mut c = bch_fs::default();
            assert_eq!(bch2_fs_btree_cache_init(&mut c), 0);
            assert!(c.btree.cache.table_init_done);
            assert_eq!(bch2_sb_realloc(&mut c.disk_sb, 0), 0);
            (*c.disk_sb.sb).flags[0] = 1 << 12;
            let mut trans = crate::btree::iter::btree_trans::default();
            crate::btree::iter::bch2_trans_init(&mut trans, &mut c);

            let node = bch2_btree_node_mem_alloc(&mut trans, false);
            assert!(!node.is_null());
            assert_eq!((*node).byte_order, 9);
            assert!(!(*node).data.is_null());
            assert!(!(*node).aux_data.is_null());
            assert_eq!((*node).nsets, 0);
            assert_eq!((*node).key.k.u64s, super::super::bkey::BKEY_U64S);
            assert_eq!(
                (*node).key.k.type_,
                super::super::bset::KEY_TYPE_btree_ptr_v2
            );
            assert_eq!((*node).nr, Default::default());
            assert_eq!((*node).set[0].data_offset, u16::MAX);
            assert_eq!(
                (*node).write_blocked.next,
                core::ptr::addr_of_mut!((*node).write_blocked)
            );
            assert_eq!(
                (*node).write_blocked.prev,
                core::ptr::addr_of_mut!((*node).write_blocked)
            );
            assert_eq!((*node).list.next, core::ptr::addr_of_mut!((*node).list));
            assert_eq!((*node).list.prev, core::ptr::addr_of_mut!((*node).list));
            assert_eq!(c.btree.cache.allocations.lock().unwrap().len(), 1);
            bch2_free_super(&mut c.disk_sb);
        }
    }

    #[test]
    fn cache_init_preallocates_reserved_freeable_nodes() {
        unsafe {
            let mut c = bch_fs::default();
            assert_eq!(bch2_sb_realloc(&mut c.disk_sb, 0), 0);
            (*c.disk_sb.sb).flags[0] = 1 << 12;
            assert_eq!(bch2_fs_btree_cache_init(&mut c), 0);
            assert_eq!(c.btree.cache.nr_freeable, c.btree.cache.nr_reserve);
            assert!(c.btree.cache.nr_freeable > 0);
            bch2_fs_btree_cache_exit(&mut c);
            bch2_free_super(&mut c.disk_sb);
        }
    }

    #[test]
    fn allocates_standalone_node_shell_and_buffers() {
        unsafe {
            let mut c = bch_fs::default();
            assert_eq!(bch2_sb_realloc(&mut c.disk_sb, 0), 0);
            (*c.disk_sb.sb).flags[0] = 1 << 12;
            let node = __bch2_btree_node_mem_alloc(&mut c);
            assert!(!node.is_null());
            assert_eq!((*node).byte_order, 9);
            assert!(!(*node).data.is_null());
            assert!(!(*node).aux_data.is_null());
            assert_eq!((*node).key.k.type_, KEY_TYPE_btree_ptr_v2);
            assert_eq!(
                (*node).cache_state,
                btree_node_cache_state::BTREE_NODE_CACHE_NONE
            );
            bch2_btree_node_mem_free(&mut c, node);
            bch2_free_super(&mut c.disk_sb);
        }
    }

    #[test]
    fn evicts_clean_node_and_records_live_size() {
        unsafe {
            let mut c = bch_fs::default();
            assert_eq!(bch2_fs_btree_cache_init(&mut c), 0);
            assert_eq!(bch2_fs_btree_evicted_size_init(&mut c), 0);
            assert_eq!(bch2_sb_realloc(&mut c.disk_sb, 0), 0);
            (*c.disk_sb.sb).flags[0] = 1 << 12;
            let mut trans = crate::btree::iter::btree_trans::default();
            crate::btree::iter::bch2_trans_init(&mut trans, &mut c);
            let node = bch2_btree_node_mem_alloc(&mut trans, false);
            assert!(!node.is_null());
            (*node).c.btree_id = 0;
            (*super::super::bset::bkey_i_to_btree_ptr_v2(&mut (*node).key))
                .v
                .seq = 0x1234;
            assert_eq!(
                bch2_btree_node_transition_state(
                    &mut c.btree.cache,
                    node,
                    btree_node_cache_state::BTREE_NODE_CACHE_CLEAN,
                ),
                0
            );
            let hash = (*node).hash_val;
            bch2_btree_node_evict(&mut trans, &(*node).key);
            assert_eq!(
                (*node).cache_state,
                btree_node_cache_state::BTREE_NODE_CACHE_FREED
            );
            let mut live = 0;
            assert!(bch2_btree_evicted_size_lookup(&mut c, hash, &mut live));
            assert_eq!(live, 0);
            bch2_fs_btree_evicted_size_exit(&mut c);
            bch2_fs_btree_cache_exit(&mut c);
            bch2_free_super(&mut c.disk_sb);
        }
    }

    #[test]
    fn noiter_get_returns_read_locked_cached_node() {
        unsafe {
            let mut c = bch_fs::default();
            assert_eq!(bch2_fs_btree_cache_init(&mut c), 0);
            assert_eq!(bch2_sb_realloc(&mut c.disk_sb, 0), 0);
            (*c.disk_sb.sb).flags[0] = 1 << 12;
            let mut trans = crate::btree::iter::btree_trans::default();
            crate::btree::iter::bch2_trans_init(&mut trans, &mut c);
            let node = bch2_btree_node_mem_alloc(&mut trans, false);
            (*node).c.btree_id = 0;
            (*super::super::bset::bkey_i_to_btree_ptr_v2(&mut (*node).key))
                .v
                .seq = 0x5678;
            assert_eq!(
                bch2_btree_node_transition_state(
                    &mut c.btree.cache,
                    node,
                    btree_node_cache_state::BTREE_NODE_CACHE_CLEAN,
                ),
                0
            );
            let got =
                super::super::io::bch2_btree_node_get_noiter(&mut trans, &(*node).key, 0, 0, true);
            assert_eq!(got, node);
            assert_eq!(crate::lock::six::six_lock_counts(&(*node).c.lock).n[0], 1);
            crate::lock::six::six_unlock_read(&(*node).c.lock);
            bch2_btree_node_transition_state(
                &mut c.btree.cache,
                node,
                btree_node_cache_state::BTREE_NODE_CACHE_FREEABLE,
            );
            bch2_fs_btree_cache_exit(&mut c);
            bch2_free_super(&mut c.disk_sb);
        }
    }

    #[test]
    fn allocates_percpu_reader_nodes_with_matching_lock_layout() {
        unsafe {
            let mut c = bch_fs::default();
            assert_eq!(bch2_fs_btree_cache_init(&mut c), 0);
            assert_eq!(bch2_sb_realloc(&mut c.disk_sb, 0), 0);
            (*c.disk_sb.sb).flags[0] = 1 << 12;
            let mut trans = crate::btree::iter::btree_trans::default();
            crate::btree::iter::bch2_trans_init(&mut trans, &mut c);

            let node = bch2_btree_node_mem_alloc(&mut trans, true);
            assert!(!node.is_null());
            assert!((*node).c.lock.readers.is_some());
            bch2_btree_node_data_free(node);
            bch2_btree_node_mem_free(&mut c, node);
            bch2_fs_btree_cache_exit(&mut c);
            bch2_free_super(&mut c.disk_sb);
        }
    }

    #[test]
    fn data_and_node_shell_are_freed_in_separate_stages() {
        unsafe {
            let mut c = bch_fs::default();
            assert_eq!(bch2_fs_btree_cache_init(&mut c), 0);
            assert_eq!(bch2_sb_realloc(&mut c.disk_sb, 0), 0);
            (*c.disk_sb.sb).flags[0] = 1 << 12;
            let mut trans = crate::btree::iter::btree_trans::default();
            crate::btree::iter::bch2_trans_init(&mut trans, &mut c);

            let node = bch2_btree_node_mem_alloc(&mut trans, false);
            assert!(!node.is_null());
            (*node).flags |= 1usize << super::super::io::BTREE_NODE_just_written;
            bch2_btree_node_data_free(node);
            assert!((*node).data.is_null());
            assert!((*node).aux_data.is_null());
            assert_eq!(
                (*node).flags & (1usize << super::super::io::BTREE_NODE_just_written),
                0
            );
            assert_eq!(c.btree.cache.allocations.lock().unwrap().len(), 1);

            bch2_btree_node_mem_free(&mut c, node);
            assert!(c.btree.cache.allocations.lock().unwrap().is_empty());
            bch2_free_super(&mut c.disk_sb);
        }
    }

    #[test]
    fn hash_and_live_state_follow_current_btree_pointer_and_flags() {
        unsafe {
            let mut reserve_fs = bch_fs::default();
            bch2_recalc_btree_reserve(&mut reserve_fs);
            assert_eq!(reserve_fs.btree.cache.nr_reserve, 24);

            let mut key = bkey_i_btree_ptr_v2::default();
            key.k.type_ = KEY_TYPE_btree_ptr_v2;
            key.v.seq = 0x1234_5678_9abc_def0;
            assert_eq!(
                btree_ptr_hash_val((&key as *const bkey_i_btree_ptr_v2).cast()),
                key.v.seq
            );

            let mut node = btree::default();
            assert!(!btree_node_hashed(&node));
            assert_eq!(
                btree_node_live_state(&node),
                btree_node_cache_state::BTREE_NODE_CACHE_CLEAN
            );
            crate::btree::types::set_btree_node_dirty(&mut node);
            assert_eq!(
                btree_node_live_state(&node),
                btree_node_cache_state::BTREE_NODE_CACHE_DIRTY
            );
            crate::btree::types::clear_btree_node_dirty(&mut node);
            crate::btree::types::set_btree_node_write_in_flight(&mut node);
            assert_eq!(
                btree_node_live_state(&node),
                btree_node_cache_state::BTREE_NODE_CACHE_DIRTY
            );
            node.hash_val = key.v.seq;
            node.cache_state = btree_node_cache_state::BTREE_NODE_CACHE_DIRTY;
            assert!(btree_node_hashed(&node));
            assert_eq!(
                btree_node_cache_state(&node),
                btree_node_cache_state::BTREE_NODE_CACHE_DIRTY
            );
        }
    }

    #[test]
    fn cache_state_transition_updates_hash_lists_and_counters() {
        unsafe {
            let mut c = bch_fs::default();
            assert_eq!(bch2_fs_btree_cache_init(&mut c), 0);
            assert_eq!(bch2_sb_realloc(&mut c.disk_sb, 0), 0);
            (*c.disk_sb.sb).flags[0] = 1 << 12;
            let mut trans = crate::btree::iter::btree_trans::default();
            crate::btree::iter::bch2_trans_init(&mut trans, &mut c);
            let node = bch2_btree_node_mem_alloc(&mut trans, false);
            assert!(!node.is_null());
            (*node).c.btree_id = 0;
            (*node).key.k.type_ = super::super::bset::KEY_TYPE_btree_ptr_v2;
            (*node).key.k.u64s = super::super::bkey::BKEY_U64S;
            (*super::super::bset::bkey_i_to_btree_ptr_v2(&mut (*node).key))
                .v
                .seq = 42;

            assert_eq!(
                bch2_btree_node_transition_state(
                    &mut c.btree.cache,
                    node,
                    btree_node_cache_state::BTREE_NODE_CACHE_CLEAN,
                ),
                0
            );
            assert!(btree_node_hashed(node));
            assert_eq!(c.btree.cache.live[0].nr_clean, 1);
            {
                let c_ptr = &mut c as *mut bch_fs;
                let bc_ptr =
                    &mut (*c_ptr).btree.cache as *mut super::super::types::bch_fs_btree_cache;
                let _cache_lock = (*bc_ptr).lock.lock().unwrap();
                assert_eq!(btree_node_reclaim(c_ptr, node, 0), 0);
                assert_eq!(
                    bch2_btree_node_transition_state_locked(
                        bc_ptr,
                        node,
                        btree_node_cache_state::BTREE_NODE_CACHE_NONE,
                    ),
                    0
                );
                super::super::super::lock::six::six_unlock_write(&(*node).c.lock);
                super::super::super::lock::six::six_unlock_intent(&(*node).c.lock);
            }
            assert_eq!(
                bch2_btree_node_transition_state(
                    &mut c.btree.cache,
                    node,
                    btree_node_cache_state::BTREE_NODE_CACHE_CLEAN,
                ),
                0
            );
            bch2_node_pin(&mut c, node);
            assert!(super::super::types::btree_node_pinned(node));
            assert_eq!(c.btree.cache.live[0].nr_clean, 0);
            assert_eq!(c.btree.cache.live[1].nr_clean, 1);
            bch2_btree_cache_unpin(&mut c);
            assert!(!super::super::types::btree_node_pinned(node));
            assert_eq!(c.btree.cache.live[0].nr_clean, 1);
            assert_eq!(c.btree.cache.live[1].nr_clean, 0);
            let key = 42u64;
            assert_eq!(
                crate::util::rhashtable::rhashtable_lookup_fast(
                    &mut c.btree.cache.table,
                    &key as *const u64 as *const _,
                ),
                node.cast()
            );

            bch2_btree_node_set_dirty(&mut c, node);
            assert_eq!(
                (*node).cache_state,
                btree_node_cache_state::BTREE_NODE_CACHE_DIRTY
            );
            assert!(!super::super::io::bch2_btree_flush_all_writes(&mut c));
            super::super::io::bch2_btree_cancel_all_writes(&mut c);
            assert_eq!(
                (*node).cache_state,
                btree_node_cache_state::BTREE_NODE_CACHE_CLEAN
            );
            assert_eq!(c.btree.cache.live[0].nr_dirty, 0);
            super::super::types::clear_btree_node_dirty(node);
            bch2_btree_node_write_done_clean(&mut c, node);
            assert_eq!(
                (*node).cache_state,
                btree_node_cache_state::BTREE_NODE_CACHE_CLEAN
            );

            assert_eq!(
                bch2_btree_node_transition_state(
                    &mut c.btree.cache,
                    node,
                    btree_node_cache_state::BTREE_NODE_CACHE_FREEABLE,
                ),
                0
            );
            assert!(!btree_node_hashed(node));
            assert_eq!(c.btree.cache.nr_freeable, 1);
            let reused = bch2_btree_node_mem_alloc(&mut trans, false);
            assert_eq!(reused, node);
            assert_eq!(c.btree.cache.nr_freeable, 0);
            assert_eq!(
                (*node).cache_state,
                btree_node_cache_state::BTREE_NODE_CACHE_NONE
            );
            assert_eq!(
                bch2_btree_node_transition_state(
                    &mut c.btree.cache,
                    node,
                    btree_node_cache_state::BTREE_NODE_CACHE_FREEABLE,
                ),
                0
            );
            assert_eq!(
                bch2_btree_node_transition_state(
                    &mut c.btree.cache,
                    node,
                    btree_node_cache_state::BTREE_NODE_CACHE_FREED,
                ),
                0
            );
            assert!(node.is_null() || (*node).data.is_null());
            let reused_freed = bch2_btree_node_mem_alloc(&mut trans, false);
            assert_eq!(reused_freed, node);
            assert!(!(*reused_freed).data.is_null());
            bch2_btree_node_data_free(reused_freed);
            bch2_btree_node_mem_free(&mut c, node);
            bch2_fs_btree_cache_exit(&mut c);
            assert!(!c.btree.cache.table_init_done);
            bch2_free_super(&mut c.disk_sb);
        }
    }
}
