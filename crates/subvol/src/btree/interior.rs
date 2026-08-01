use super::types::{
    bset, bset_tree_last, bset_u64s, btree, btree_bkey_last, BTREE_NODE_need_rewrite,
};

pub const fn btree_buf_bytes(b: &btree) -> usize {
    1usize << b.byte_order
}

pub const fn btree_buf_max_u64s(b: &btree) -> usize {
    (btree_buf_bytes(b) - core::mem::size_of::<super::bset::btree_node>())
        / core::mem::size_of::<u64>()
}

pub unsafe fn btree_data_end(b: *mut btree) -> *mut core::ffi::c_void {
    (*b).data.cast::<u8>().add(btree_buf_bytes(&*b)).cast()
}

pub unsafe fn unwritten_whiteouts_start(b: *mut btree) -> *mut super::bkey::bkey_packed {
    btree_data_end(b)
        .cast::<u64>()
        .sub((*b).whiteout_u64s as usize)
        .cast()
}

pub unsafe fn unwritten_whiteouts_end(b: *mut btree) -> *mut super::bkey::bkey_packed {
    btree_data_end(b).cast()
}

pub unsafe fn bch2_push_whiteout(b: *mut btree, pos: super::bkey::bpos) {
    let mut words = [0u64; super::bkey::BKEY_U64S as usize];
    let k = words.as_mut_ptr().cast::<super::bkey::bkey_packed>();

    assert!(bch2_btree_keys_u64s_remaining(b) >= super::bkey::BKEY_U64S as usize);
    assert!(!super::io::btree_node_just_written(b));

    if !super::bkey::bch2_bkey_pack_pos(&mut *k, pos, &*b) {
        let unpacked = k.cast::<super::bkey::bkey>();
        super::bkey::bkey_init(&mut *unpacked);
        (*unpacked).p = pos;
    }

    (*k).format |= 0x80;
    (*b).whiteout_u64s += (*k).u64s as u16;
    core::ptr::copy_nonoverlapping(
        k.cast::<u64>(),
        unwritten_whiteouts_start(b).cast::<u64>(),
        (*k).u64s as usize,
    );
}

pub unsafe fn __bch2_btree_u64s_remaining(b: *mut btree, end: *const u64) -> isize {
    let used = end.offset_from((*b).data.cast::<u64>()) + (*b).whiteout_u64s as isize + 1;
    let total = (btree_buf_bytes(&*b) / core::mem::size_of::<u64>()) as isize;
    total - used
}

pub unsafe fn bch2_btree_keys_u64s_remaining(b: *mut btree) -> usize {
    let remaining =
        __bch2_btree_u64s_remaining(b, btree_bkey_last(b, bset_tree_last(b)).cast::<u64>());
    assert!(remaining >= 0);
    if ((*b).written as usize) << 9
        > super::types::bset(b, bset_tree_last(b)) as usize - (*b).data as usize
    {
        0
    } else {
        remaining as usize
    }
}

pub unsafe fn bch2_btree_node_insert_fits(b: *mut btree, u64s: u32) -> bool {
    if (*b).flags & (1usize << BTREE_NODE_need_rewrite) != 0 {
        return false;
    }
    u64s as usize <= bch2_btree_keys_u64s_remaining(b)
}

pub unsafe fn bset_written(b: *mut btree, i: *mut super::bset::bset) -> bool {
    i.cast::<u8>() < (*b).data.cast::<u8>().add((*b).written as usize * 512)
}

pub unsafe fn want_new_bset(
    c: *mut super::types::bch_fs,
    b: *mut btree,
) -> *mut super::bset::btree_node_entry {
    let last = bset_tree_last(b);
    let write_block = (*b).data.cast::<u8>().add((*b).written as usize * 512);
    let last_end = btree_bkey_last(b, last).cast::<u8>();
    let bne = core::cmp::max(write_block, last_end).cast::<super::bset::btree_node_entry>();
    let remaining =
        __bch2_btree_u64s_remaining(b, core::ptr::addr_of!((*bne).keys).cast::<u64>().add(3));
    let last_written = bset_written(b, bset(b, last));
    let block_sectors = if c.is_null() || (*c).disk_sb.sb.is_null() {
        1
    } else {
        (*(*c).disk_sb.sb).block_size.max(1) as usize
    };
    let node_sectors = btree_buf_bytes(&*b) / 512;
    if last_written {
        if (*b).written as usize + block_sectors <= node_sectors {
            return bne;
        }
    } else if bset_u64s(last) as usize * 8 > 4096 && remaining > 512 {
        return bne;
    }
    core::ptr::null_mut()
}

pub unsafe fn bch2_journal_entry_to_btree_root(
    c: *mut super::types::bch_fs,
    entry: *mut crate::journal::jset_entry,
) {
    let _guard = (*c).btree.cache.root_lock.lock().unwrap();
    let root = super::types::bch2_btree_id_root(c, (*entry).btree_id as usize);
    assert!(!root.is_null());
    (*root).level = (*entry).level;
    (*root).alive = 1;
    let key = entry.cast::<u64>().add(1).cast::<super::bkey::bkey_i>();
    super::bkey::bkey_copy(&mut (*root).key, key);
    if (*root).key.k.type_ == super::bset::KEY_TYPE_btree_ptr_v2 {
        super::bset::bkey_i_to_btree_ptr_v2(&mut (*root).key)
            .as_mut()
            .unwrap()
            .v
            .mem_ptr = 0;
    }
}

pub unsafe fn bch2_btree_roots_to_journal_entries(
    c: *mut super::types::bch_fs,
    mut end: *mut crate::journal::jset_entry,
    skip: usize,
) -> *mut crate::journal::jset_entry {
    let _guard = (*c).btree.cache.root_lock.lock().unwrap();
    for id in 0..super::types::BTREE_ID_NR {
        let root = (*c).btree.cache.roots_known.as_ptr().add(id);
        if (*root).alive != 0 && skip & (1usize << id) == 0 {
            let actual = crate::journal::journal_entry_set(
                end,
                crate::journal::BCH_JSET_ENTRY_btree_root,
                id as u8,
                (*root).level,
                (&(*root).key as *const super::bkey::bkey_i).cast::<u64>(),
                (*root).key.k.u64s as u16,
            );
            end = end.cast::<u64>().add(actual as usize).cast();
        }
    }
    end
}

pub unsafe fn bch2_btree_root_alloc_fake_trans(
    trans: *mut super::iter::btree_trans,
    id: u8,
    level: u8,
) -> i32 {
    if trans.is_null() || (*trans).c.is_null() || id as usize >= super::types::BTREE_ID_NR {
        return -22;
    }
    let c = (*trans).c;
    let node = super::cache::bch2_btree_node_mem_alloc(trans, false);
    if node.is_null() {
        return -12;
    }
    if crate::lock::six::six_lock_intent(&(*node).c.lock) != 0 {
        super::cache::bch2_btree_node_data_free(node);
        super::cache::bch2_btree_node_mem_free(c, node);
        return -12;
    }
    if crate::lock::six::six_lock_write(&(*node).c.lock) != 0 {
        crate::lock::six::six_unlock_intent(&(*node).c.lock);
        super::cache::bch2_btree_node_data_free(node);
        super::cache::bch2_btree_node_mem_free(c, node);
        return -12;
    }

    super::types::set_btree_node_fake(node);
    super::types::set_btree_node_need_rewrite(node);
    (*node).c.level = level;
    (*node).c.btree_id = id;
    let ptr = super::bset::bkey_i_btree_ptr_v2 {
        k: super::bkey::bkey {
            u64s: 10,
            format: super::bkey::KEY_FORMAT_CURRENT,
            type_: super::bset::KEY_TYPE_btree_ptr_v2,
            p: super::bkey::SPOS_MAX,
            ..Default::default()
        },
        v: super::bset::bch_btree_ptr_v2 {
            seq: u64::MAX - id as u64,
            min_key: super::bkey::POS_MIN,
            ..Default::default()
        },
    };
    super::bkey::bkey_copy(
        &mut (*node).key,
        (&ptr as *const super::bset::bkey_i_btree_ptr_v2).cast(),
    );
    super::bset_build::bch2_bset_init_first(node, &mut (*(*node).data).keys);
    super::bset_build::bch2_btree_build_aux_trees(node);
    (*(*node).data).flags = 0;
    btree_set_min(node, super::bkey::POS_MIN);
    btree_set_max(node, super::bkey::SPOS_MAX);
    (*node).format = super::bkey::BKEY_FORMAT_CURRENT;
    (*node).nr_key_bits = super::bkey::bkey_format_key_bits(&(*node).format) as u8;
    super::bkey::bch2_compute_bkey_unpack_consts(node);
    (*(*node).data).format = (*node).format;
    if super::cache::bch2_btree_node_transition_state(
        &mut (*c).btree.cache,
        node,
        super::cache::btree_node_live_state(node),
    ) != 0
    {
        crate::lock::six::six_unlock_write(&(*node).c.lock);
        crate::lock::six::six_unlock_intent(&(*node).c.lock);
        return -12;
    }
    bch2_btree_set_root_for_read(c, node);
    crate::lock::six::six_unlock_write(&(*node).c.lock);
    crate::lock::six::six_unlock_intent(&(*node).c.lock);
    0
}

pub unsafe fn bch2_btree_set_root_for_read(c: *mut super::types::bch_fs, b: *mut btree) {
    if c.is_null() || b.is_null() {
        return;
    }
    assert_ne!(
        super::types::bch2_btree_id_root_b(c, (*b).c.btree_id as usize),
        b
    );
    {
        let _cache_guard = (*c).btree.cache.lock.lock().unwrap();
        super::types::set_btree_node_permanent(b);
    }
    {
        let _root_guard = (*c).btree.cache.root_lock.lock().unwrap();
        let id = (*b).c.btree_id as usize;
        super::types::bch2_btree_id_root_set(c, id, b);
        let slot = super::types::bch2_btree_id_root(c, id);
        super::bkey::bkey_copy(&mut (*slot).key, &(*b).key);
        (*slot).level = (*b).c.level;
        (*slot).alive = 1;
    }
    super::cache::bch2_recalc_btree_reserve(c);
}

pub unsafe fn bch2_btree_root_alloc_fake(c: *mut super::types::bch_fs, id: u8, level: u8) {
    if c.is_null() {
        return;
    }
    let mut trans = super::iter::btree_trans::default();
    super::iter::bch2_trans_init(&mut trans, c);
    let _ = bch2_btree_root_alloc_fake_trans(&mut trans, id, level);
}

pub unsafe fn bch2_btree_node_check_topology(
    trans: *mut super::iter::btree_trans,
    b: *mut btree,
) -> i32 {
    if trans.is_null() || b.is_null() || (*b).data.is_null() {
        return -22;
    }
    let node_min = if (*b).key.k.type_ == super::bset::KEY_TYPE_btree_ptr_v2 {
        super::bset::bkey_i_to_btree_ptr_v2(&mut (*b).key)
            .as_ref()
            .unwrap()
            .v
            .min_key
    } else {
        (*(*b).data).min_key
    };
    if (*b).key.k.type_ == super::bset::KEY_TYPE_btree_ptr_v2
        && !super::bkey::bpos_eq(node_min, (*(*b).data).min_key)
    {
        return -1;
    }
    let root = super::types::bch2_btree_id_root_b((*trans).c, (*b).c.btree_id as usize);
    if root == b
        && (!super::bkey::bpos_eq((*(*b).data).min_key, super::bkey::POS_MIN)
            || !super::bkey::bpos_eq((*(*b).data).max_key, super::bkey::SPOS_MAX))
    {
        return -1;
    }
    if (*b).c.level == 0 {
        return 0;
    }

    let mut iter = super::types::btree_node_iter::default();
    super::node_iter::bch2_btree_node_iter_init_from_start(&mut iter, b);
    let mut prev = super::bkey::bkey_i::default();
    let mut have_prev = false;
    loop {
        let packed = super::node_iter::bch2_btree_node_iter_peek_all(&mut iter, b);
        if packed.is_null() {
            break;
        }
        let mut full = super::bkey::bkey_i::default();
        super::bkey::bch2_bkey_unpack(b, &mut full, packed);
        if full.k.type_ != super::bset::KEY_TYPE_btree_ptr_v2 {
            return 0;
        }
        let ptr = super::bset::bkey_i_to_btree_ptr_v2(&mut full);
        let expected = if have_prev && prev.k.type_ != super::bset::KEY_TYPE_deleted {
            super::bkey::bpos_successor(prev.k.p)
        } else {
            node_min
        };
        if !super::bkey::bpos_eq(expected, (*ptr).v.min_key) {
            return -1;
        }
        prev = full;
        have_prev = true;
        super::node_iter::bch2_btree_node_iter_advance(&mut iter, b);
    }
    if !have_prev
        || prev.k.type_ == super::bset::KEY_TYPE_deleted
        || !super::bkey::bpos_eq(prev.k.p, (*b).key.k.p)
    {
        return -1;
    }
    0
}

unsafe fn btree_set_min(b: *mut btree, pos: super::bkey::bpos) {
    if (*b).key.k.type_ == super::bset::KEY_TYPE_btree_ptr_v2 {
        (*super::bset::bkey_i_to_btree_ptr_v2(&mut (*b).key))
            .v
            .min_key = pos;
    }
    (*(*b).data).min_key = pos;
}

unsafe fn btree_set_max(b: *mut btree, pos: super::bkey::bpos) {
    (*b).key.k.p = pos;
    (*(*b).data).max_key = pos;
}

unsafe fn btree_node_reset_sib_u64s(b: *mut btree) {
    (*b).sib_u64s[0] = if !super::bkey::bpos_eq((*(*b).data).min_key, super::bkey::POS_MIN) {
        (*b).nr.live_u64s
    } else {
        u16::MAX
    };
    (*b).sib_u64s[1] = if !super::bkey::bpos_eq((*b).key.k.p, super::bkey::SPOS_MAX) {
        (*b).nr.live_u64s
    } else {
        u16::MAX
    };
}

pub(crate) unsafe fn bch2_btree_split_leaf(
    trans: *mut super::iter::btree_trans,
    path_idx: super::iter::btree_path_idx_t,
    _new_key_u64s: u32,
    _flags: u32,
) -> i32 {
    crate::rewrite_log_debug!("btree split begin path={path_idx}");
    let path = (*trans).paths.add(path_idx as usize);
    let src = (*path).l[0].b;
    let c = (*trans).c;
    if src.is_null() || (*src).c.level != 0 {
        crate::rewrite_log_error!("btree split rejected: source is not a leaf");
        return -5;
    }
    let topology = bch2_btree_node_check_topology(trans, src);
    if topology != 0 {
        crate::rewrite_log_error!("btree split rejected: topology check failed ret={topology}");
        return topology;
    }
    let parent = (*path).l[1].b;
    let root_level = super::types::bch2_btree_root_unpack_level(
        super::types::bch2_btree_id_root_packed(c, (*path).btree_id as usize),
    );
    if !parent.is_null() && super::iter::bch2_btree_path_upgrade(trans, path, root_level + 1) != 0 {
        return -7;
    }

    let target = ((*src).nr.live_u64s as usize * 3) / 5;
    let mut states = [
        super::bkey::bkey_format_state::default(),
        super::bkey::bkey_format_state::default(),
    ];
    super::bkey::bch2_bkey_format_init(&mut states[0]);
    super::bkey::bch2_bkey_format_init(&mut states[1]);
    let mut nr_keys = [0usize; 2];
    let mut val_u64s = [0usize; 2];
    let mut cumulative = 0usize;
    let mut pivot = super::bkey::POS_MIN;
    let mut iter = super::types::btree_node_iter::default();
    super::node_iter::bch2_btree_node_iter_init_from_start(&mut iter, src);
    loop {
        let key = super::node_iter::bch2_btree_node_iter_peek(&mut iter, src);
        if key.is_null() {
            break;
        }
        let mut unpacked = super::bkey::bkey::default();
        if super::bkey::bkey_packed(&*key) {
            super::bkey::__bch2_bkey_unpack_key(&(*src).format, &mut unpacked, &*key);
        } else {
            unpacked = *key.cast::<super::bkey::bkey>();
        }
        let side = (cumulative >= target) as usize;
        cumulative += (*key).u64s as usize;
        if side == 0 {
            pivot = unpacked.p;
        }
        super::bkey::bch2_bkey_format_add_key(&mut states[side], &unpacked);
        nr_keys[side] += 1;
        val_u64s[side] += super::bkey::bkeyp_val_u64s(&(*src).format, &*key) as usize;
        super::node_iter::bch2_btree_node_iter_advance(&mut iter, src);
    }
    if nr_keys[0] == 0 || nr_keys[1] == 0 {
        crate::rewrite_log_error!("btree split rejected: unable to form two non-empty leaves");
        return -6;
    }

    super::bkey::bch2_bkey_format_add_pos(&mut states[0], (*(*src).data).min_key);
    super::bkey::bch2_bkey_format_add_pos(&mut states[0], pivot);
    super::bkey::bch2_bkey_format_add_pos(&mut states[1], super::bkey::bpos_successor(pivot));
    super::bkey::bch2_bkey_format_add_pos(&mut states[1], (*(*src).data).max_key);
    let left_format = super::bkey::bch2_bkey_format_done(&mut states[0]);
    let right_format = super::bkey::bch2_bkey_format_done(&mut states[1]);
    let mut formats = [left_format, right_format];
    for side in 0..2 {
        let output_u64s = nr_keys[side] * formats[side].key_u64s as usize + val_u64s[side];
        if core::mem::size_of::<super::bset::btree_node>() + output_u64s * 8
            > btree_buf_bytes(&*src)
        {
            formats[side] = (*src).format;
        }
    }

    let allocate_node = |level: u8| {
        let node = super::cache::bch2_btree_node_mem_alloc(trans, level != 0);
        assert!(!node.is_null());
        /* bch2_btree_node_mem_alloc() returns a preallocated node with
         * intent + write held in fs/btree/cache.c.  The Rust cache allocator
         * intentionally remains usable by the read path too, so establish
         * those update-owned references at this matching split call site. */
        assert_eq!(crate::lock::six::six_lock_intent(&(*node).c.lock), 0);
        assert_eq!(crate::lock::six::six_lock_write(&(*node).c.lock), 0);
        (*node).c.level = level;
        (*node).c.btree_id = (*src).c.btree_id;
        (*node).version_ondisk = crate::sb::bcachefs_metadata_version_current;
        super::bset_build::bch2_bset_init_first(node, &mut (*(*node).data).keys);
        super::bset_build::bch2_btree_build_aux_trees(node);
        node
    };
    let cache_ptr = &mut (*c).btree.cache as *mut super::types::bch_fs_btree_cache;
    let cache_initialized = (*cache_ptr).table_init_done;
    let release_node = |node: *mut btree| {
        if node.is_null() {
            return;
        }
        if cache_initialized {
            super::types::clear_btree_node_dirty(node);
            let _ = super::cache::bch2_btree_node_transition_state(
                cache_ptr,
                node,
                super::types::btree_node_cache_state::BTREE_NODE_CACHE_FREEABLE,
            );
        } else {
            super::cache::bch2_btree_node_data_free(node);
        }
        crate::lock::six::six_unlock_write(&(*node).c.lock);
        crate::lock::six::six_unlock_intent(&(*node).c.lock);
        if !cache_initialized {
            super::cache::bch2_btree_node_mem_free(c, node);
        }
    };
    let retire_node = |node: *mut btree| {
        if node.is_null() {
            return;
        }
        if !(*c)
            .btree
            .cache
            .allocations
            .lock()
            .unwrap()
            .contains(&(node as usize))
        {
            return;
        }
        super::types::clear_btree_node_permanent(node);
        super::types::clear_btree_node_noevict(node);
        if cache_initialized {
            /* bcachefs leaves the superseded root hashed until its normal
             * btree write completes.  This engine persists this mutation
             * through the transaction journal instead of a node-write
             * pipeline, so the replaced in-memory node has no remaining
             * durable work before it becomes freeable. */
            super::types::clear_btree_node_dirty(node);
            let _ = super::cache::bch2_btree_node_transition_state(
                cache_ptr,
                node,
                super::types::btree_node_cache_state::BTREE_NODE_CACHE_FREEABLE,
            );
        } else {
            super::cache::bch2_btree_node_data_free(node);
            super::cache::bch2_btree_node_mem_free(c, node);
        }
    };

    let left = allocate_node(0);
    let right = allocate_node(0);
    btree_set_min(left, (*(*src).data).min_key);
    btree_set_max(left, pivot);
    btree_set_min(right, super::bkey::bpos_successor(pivot));
    btree_set_max(right, (*(*src).data).max_key);

    let nodes = [left, right];
    let mut output = [
        super::types::btree_bset_first(left)
            .cast::<u64>()
            .add(3)
            .cast::<super::bkey::bkey_packed>(),
        super::types::btree_bset_first(right)
            .cast::<u64>()
            .add(3)
            .cast::<super::bkey::bkey_packed>(),
    ];
    for side in 0..2 {
        (*nodes[side]).format = formats[side];
        (*nodes[side]).nr_key_bits = super::bkey::bkey_format_key_bits(&formats[side]) as u8;
        super::bkey::bch2_compute_bkey_unpack_consts(nodes[side]);
        (*(*nodes[side]).data).format = formats[side];
    }

    super::node_iter::bch2_btree_node_iter_init_from_start(&mut iter, src);
    loop {
        let key = super::node_iter::bch2_btree_node_iter_peek(&mut iter, src);
        if key.is_null() {
            break;
        }
        let pos = super::node_iter::bkey_unpack_pos(src, key);
        let side = (super::bkey::bpos_cmp(pos, pivot) > 0) as usize;
        let input_format = if super::bkey::bkey_packed(&*key) {
            &(*src).format
        } else {
            &super::bkey::BKEY_FORMAT_CURRENT
        };
        if super::bkey::bch2_bkey_transform(
            &(*nodes[side]).format,
            &mut *output[side],
            input_format,
            &*key,
        ) {
            (*output[side]).format = super::bkey::KEY_FORMAT_LOCAL_BTREE;
        } else {
            super::bkey::bch2_bkey_unpack(src, output[side].cast(), key);
        }
        (*output[side]).format &= 0x7f;
        super::bset_update::btree_keys_account_key(&mut (*nodes[side]).nr, 0, output[side], 1);
        output[side] = super::bkey::bkey_p_next(output[side]);
        super::node_iter::bch2_btree_node_iter_advance(&mut iter, src);
    }
    for side in 0..2 {
        let disk_set = super::types::btree_bset_first(nodes[side]);
        (*disk_set).u64s = output[side]
            .cast::<u64>()
            .offset_from(disk_set.cast::<u64>().add(3)) as u16;
        super::types::set_btree_bset_end(nodes[side], (*nodes[side]).set.as_mut_ptr());
        btree_node_reset_sib_u64s(nodes[side]);
        super::bset_build::bch2_btree_build_aux_trees(nodes[side]);
        #[cfg(debug_assertions)]
        super::bset_update::__bch2_verify_btree_nr_keys(nodes[side]);
    }

    let child_ptr = |child: *mut btree| super::bset::bkey_i_btree_ptr_v2 {
        k: super::bkey::bkey {
            u64s: 10,
            format: super::bkey::KEY_FORMAT_CURRENT,
            type_: super::bset::KEY_TYPE_btree_ptr_v2,
            p: (*(*child).data).max_key,
            ..Default::default()
        },
        v: super::bset::bch_btree_ptr_v2 {
            mem_ptr: child as usize as u64,
            seq: (*(*child).data).keys.seq,
            min_key: (*(*child).data).min_key,
            ..Default::default()
        },
    };

    for side in 0..2 {
        let ptr = child_ptr(nodes[side]);
        super::bkey::bkey_copy(
            &mut (*nodes[side]).key,
            (&ptr as *const super::bset::bkey_i_btree_ptr_v2).cast(),
        );
    }

    if cache_initialized {
        for node in nodes {
            let _ = super::cache::bch2_btree_node_transition_state(
                cache_ptr,
                node,
                super::types::btree_node_cache_state::BTREE_NODE_CACHE_CLEAN,
            );
            super::cache::bch2_btree_node_set_dirty(c, node);
        }
    }

    let mut replacement_paths = [0; 2];
    for side in 0..2 {
        let node = nodes[side];
        let path_idx = super::iter::bch2_path_get_unlocked_mut(
            trans,
            (*node).c.btree_id,
            (*node).c.level,
            (*node).key.k.p,
            false,
        );
        super::iter::btree_path_take_new_node(trans, (*trans).paths.add(path_idx as usize), node);
        replacement_paths[side] = path_idx;
    }
    let release_paths = |paths: &[super::iter::btree_path_idx_t]| {
        for path_idx in paths.iter().rev() {
            if *path_idx != 0 {
                super::iter::bch2_path_put(trans, *path_idx, true);
            }
        }
    };

    let mut old_node = src;
    let mut replacement = [left, right];
    loop {
        let parent_level = (*old_node).c.level as usize + 1;
        let parent = if parent_level < super::bset::BTREE_MAX_DEPTH as usize {
            (*path).l[parent_level].b
        } else {
            core::ptr::null_mut()
        };
        let old_pos = (*(*old_node).data).max_key;

        if parent.is_null() {
            if parent_level >= super::bset::BTREE_MAX_DEPTH as usize {
                release_paths(&replacement_paths);
                for node in replacement {
                    release_node(node);
                }
                return -12;
            }
            let root = allocate_node(parent_level as u8);
            (*root).format = super::bkey::BKEY_FORMAT_CURRENT;
            (*root).nr_key_bits = super::bkey::bkey_format_key_bits(&(*root).format) as u8;
            super::bkey::bch2_compute_bkey_unpack_consts(root);
            (*(*root).data).format = (*root).format;
            btree_set_min(root, super::bkey::POS_MIN);
            btree_set_max(root, super::bkey::SPOS_MAX);
            let root_path = (*trans).paths.add(replacement_paths[1] as usize);
            (*root_path).locks_want += 1;
            assert!((*root_path).l[parent_level].b.is_null());
            super::iter::btree_path_take_new_node(trans, root_path, root);
            for child in replacement {
                let mut ptr = child_ptr(child);
                let last = super::types::bset_tree_last(root);
                let mut insert_iter = super::types::btree_node_iter::default();
                super::node_iter::bch2_btree_node_iter_init(c, root, &mut insert_iter, &ptr.k.p);
                let where_ =
                    super::node_iter::bch2_btree_node_iter_bset_pos(&mut insert_iter, root, last);
                if (*trans).journal_replay_not_finished {
                    let journal_keys = core::ptr::addr_of!((*c).journal_keys);
                    let _overwrite_lock = (&(*journal_keys).overwrite_lock).lock().unwrap();
                    crate::journal::bch2_journal_key_check_or_overwrite(
                        c,
                        (*root).c.btree_id,
                        (*root).c.level,
                        ptr.k.p,
                        false,
                    );
                }
                super::bset_update::bch2_bset_insert(
                    root,
                    where_,
                    (&mut ptr as *mut super::bset::bkey_i_btree_ptr_v2).cast(),
                    0,
                );
            }
            btree_node_reset_sib_u64s(root);
            super::bset_build::bch2_btree_build_aux_trees(root);
            let root_ptr = child_ptr(root);
            super::bkey::bkey_copy(
                &mut (*root).key,
                (&root_ptr as *const super::bset::bkey_i_btree_ptr_v2).cast(),
            );
            if cache_initialized {
                let _ = super::cache::bch2_btree_node_transition_state(
                    cache_ptr,
                    root,
                    super::types::btree_node_cache_state::BTREE_NODE_CACHE_CLEAN,
                );
                super::cache::bch2_btree_node_set_dirty(c, root);
            }
            #[cfg(debug_assertions)]
            super::bset_update::__bch2_verify_btree_nr_keys(root);
            bch2_btree_set_root_for_read(c, root);

            retire_node(old_node);
            super::iter::bch2_trans_node_add(trans, root);
            for node in replacement.iter().rev() {
                super::iter::bch2_trans_node_add(trans, *node);
            }
            super::iter::bch2_trans_node_verify_not_in_iters(trans, old_node);

            release_paths(&replacement_paths);
            /* The temporary paths drop their recursive references first;
             * consume the allocator-owned primary references afterwards, as
             * btree_update_done() does after interior.c's out: cleanup. */
            crate::lock::six::six_unlock_write(&(*root).c.lock);
            crate::lock::six::six_unlock_intent(&(*root).c.lock);
            for node in replacement {
                crate::lock::six::six_unlock_write(&(*node).c.lock);
                crate::lock::six::six_unlock_intent(&(*node).c.lock);
            }

            return 0;
        }

        let mut parent_iter = super::types::btree_node_iter::default();
        super::node_iter::bch2_btree_node_iter_init(c, parent, &mut parent_iter, &old_pos);
        let old = super::node_iter::bch2_btree_node_iter_peek(&mut parent_iter, parent);
        if old.is_null()
            || !super::bkey::bpos_eq(super::node_iter::bkey_unpack_pos(parent, old), old_pos)
        {
            release_paths(&replacement_paths);
            for node in replacement {
                release_node(node);
            }
            return -8;
        }
        let last = super::types::bset_tree_last(parent);
        let old_writeable = super::types::__btree_node_key_to_offset(parent, old)
            >= super::types::btree_bkey_first_offset(last);
        let required = if old_writeable { 10 } else { 20 };

        if bch2_btree_node_insert_fits(parent, required) {
            if crate::lock::six::six_lock_write(&(*old_node).c.lock) != 0 {
                release_paths(&replacement_paths);
                for node in replacement {
                    release_node(node);
                }
                return -10;
            }
            if crate::lock::six::six_lock_write(&(*parent).c.lock) != 0 {
                crate::lock::six::six_unlock_write(&(*old_node).c.lock);
                release_paths(&replacement_paths);
                for node in replacement {
                    release_node(node);
                }
                return -10;
            }

            let old_set = super::types::bch2_bkey_to_bset_inlined(parent, old);
            let old_set_idx = old_set.offset_from((*parent).set.as_ptr()) as usize;
            super::bset_update::btree_keys_account_key(&mut (*parent).nr, old_set_idx, old, -1);
            let old_u64s = (*old).u64s as u32;
            (*old).type_ = 0;
            if old_writeable {
                super::bset_update::bch2_bset_delete(parent, old, old_u64s);
            }

            let mut left_key = child_ptr(replacement[0]);
            let mut right_key = child_ptr(replacement[1]);
            for ptr in [&mut left_key, &mut right_key] {
                let mut insert_iter = super::types::btree_node_iter::default();
                super::node_iter::bch2_btree_node_iter_init(c, parent, &mut insert_iter, &ptr.k.p);
                let where_ =
                    super::node_iter::bch2_btree_node_iter_bset_pos(&mut insert_iter, parent, last);
                if (*trans).journal_replay_not_finished {
                    let journal_keys = core::ptr::addr_of!((*c).journal_keys);
                    let _overwrite_lock = (&(*journal_keys).overwrite_lock).lock().unwrap();
                    crate::journal::bch2_journal_key_check_or_overwrite(
                        c,
                        (*parent).c.btree_id,
                        (*parent).c.level,
                        ptr.k.p,
                        false,
                    );
                }
                super::bset_update::bch2_bset_insert(
                    parent,
                    where_,
                    (ptr as *mut super::bset::bkey_i_btree_ptr_v2).cast(),
                    0,
                );
            }
            super::cache::bch2_btree_node_set_dirty(c, parent);
            retire_node(old_node);
            /* The source node's write reference is an update-local lock;
             * the iterator still owns its intent reference.  Drop the
             * former before bch2_trans_node_add() transfers and releases the
             * latter, matching the write-path handoff in
             * fs/btree/locking.h. */
            crate::lock::six::six_unlock_write(&(*old_node).c.lock);
            for node in replacement.iter().rev() {
                super::iter::bch2_trans_node_add(trans, *node);
            }
            super::iter::bch2_trans_node_verify_not_in_iters(trans, old_node);
            release_paths(&replacement_paths);
            for node in replacement {
                crate::lock::six::six_unlock_write(&(*node).c.lock);
                crate::lock::six::six_unlock_intent(&(*node).c.lock);
            }
            crate::lock::six::six_unlock_write(&(*parent).c.lock);
            return 0;
        }

        let mut parent_key_words = [0u64; 20];
        let mut parent_keys = crate::data::keylist::keylist::default();
        crate::data::keylist::bch2_keylist_init(&mut parent_keys, parent_key_words.as_mut_ptr());
        let left_key = child_ptr(replacement[0]);
        let right_key = child_ptr(replacement[1]);
        crate::data::keylist::bch2_keylist_add(
            &mut parent_keys,
            (&left_key as *const super::bset::bkey_i_btree_ptr_v2).cast(),
        );
        crate::data::keylist::bch2_keylist_add(
            &mut parent_keys,
            (&right_key as *const super::bset::bkey_i_btree_ptr_v2).cast(),
        );

        let target = ((*parent).nr.live_u64s as usize * 3) / 5;
        let mut states = [
            super::bkey::bkey_format_state::default(),
            super::bkey::bkey_format_state::default(),
        ];
        super::bkey::bch2_bkey_format_init(&mut states[0]);
        super::bkey::bch2_bkey_format_init(&mut states[1]);
        let mut nr_keys = [0usize; 2];
        let mut val_u64s = [0usize; 2];
        let mut cumulative = 0usize;
        let mut pivot = super::bkey::POS_MIN;
        let mut scan = super::types::btree_node_iter::default();
        super::node_iter::bch2_btree_node_iter_init_from_start(&mut scan, parent);
        loop {
            let key = super::node_iter::bch2_btree_node_iter_peek(&mut scan, parent);
            if key.is_null() {
                break;
            }
            let mut unpacked = super::bkey::bkey::default();
            if super::bkey::bkey_packed(&*key) {
                super::bkey::__bch2_bkey_unpack_key(&(*parent).format, &mut unpacked, &*key);
            } else {
                unpacked = *key.cast::<super::bkey::bkey>();
            }
            let side = (cumulative >= target) as usize;
            cumulative += (*key).u64s as usize;
            if side == 0 {
                pivot = unpacked.p;
            }
            super::bkey::bch2_bkey_format_add_key(&mut states[side], &unpacked);
            nr_keys[side] += 1;
            val_u64s[side] += super::bkey::bkeyp_val_u64s(&(*parent).format, &*key) as usize;
            super::node_iter::bch2_btree_node_iter_advance(&mut scan, parent);
        }
        if nr_keys[0] == 0 || nr_keys[1] == 0 {
            release_paths(&replacement_paths);
            for node in replacement {
                release_node(node);
            }
            return -6;
        }

        super::bkey::bch2_bkey_format_add_pos(&mut states[0], (*(*parent).data).min_key);
        super::bkey::bch2_bkey_format_add_pos(&mut states[0], pivot);
        super::bkey::bch2_bkey_format_add_pos(&mut states[1], super::bkey::bpos_successor(pivot));
        super::bkey::bch2_bkey_format_add_pos(&mut states[1], (*(*parent).data).max_key);
        let left_format = super::bkey::bch2_bkey_format_done(&mut states[0]);
        let right_format = super::bkey::bch2_bkey_format_done(&mut states[1]);
        let mut formats = [left_format, right_format];
        for side in 0..2 {
            let output_u64s = nr_keys[side] * formats[side].key_u64s as usize + val_u64s[side];
            if core::mem::size_of::<super::bset::btree_node>() + output_u64s * 8
                > btree_buf_bytes(&*parent)
            {
                formats[side] = (*parent).format;
            }
        }

        let old_min = (*(*parent).data).min_key;
        let old_max = (*(*parent).data).max_key;
        let left_parent = allocate_node((*parent).c.level);
        let right_parent = allocate_node((*parent).c.level);
        btree_set_min(left_parent, old_min);
        btree_set_max(left_parent, pivot);
        btree_set_min(right_parent, super::bkey::bpos_successor(pivot));
        btree_set_max(right_parent, old_max);
        let parent_nodes = [left_parent, right_parent];
        let mut output = [
            super::types::btree_bset_first(left_parent)
                .cast::<u64>()
                .add(3)
                .cast::<super::bkey::bkey_packed>(),
            super::types::btree_bset_first(right_parent)
                .cast::<u64>()
                .add(3)
                .cast::<super::bkey::bkey_packed>(),
        ];
        for side in 0..2 {
            (*parent_nodes[side]).format = formats[side];
            (*parent_nodes[side]).nr_key_bits =
                super::bkey::bkey_format_key_bits(&formats[side]) as u8;
            super::bkey::bch2_compute_bkey_unpack_consts(parent_nodes[side]);
            (*(*parent_nodes[side]).data).format = formats[side];
        }

        super::node_iter::bch2_btree_node_iter_init_from_start(&mut scan, parent);
        loop {
            let key = super::node_iter::bch2_btree_node_iter_peek(&mut scan, parent);
            if key.is_null() {
                break;
            }
            let pos = super::node_iter::bkey_unpack_pos(parent, key);
            let side = (super::bkey::bpos_cmp(pos, pivot) > 0) as usize;
            let input_format = if super::bkey::bkey_packed(&*key) {
                &(*parent).format
            } else {
                &super::bkey::BKEY_FORMAT_CURRENT
            };
            if super::bkey::bch2_bkey_transform(
                &(*parent_nodes[side]).format,
                &mut *output[side],
                input_format,
                &*key,
            ) {
                (*output[side]).format = super::bkey::KEY_FORMAT_LOCAL_BTREE;
            } else {
                super::bkey::bch2_bkey_unpack(parent, output[side].cast(), key);
            }
            (*output[side]).format &= 0x7f;
            super::bset_update::btree_keys_account_key(
                &mut (*parent_nodes[side]).nr,
                0,
                output[side],
                1,
            );
            output[side] = super::bkey::bkey_p_next(output[side]);
            super::node_iter::bch2_btree_node_iter_advance(&mut scan, parent);
        }
        for side in 0..2 {
            let set = super::types::btree_bset_first(parent_nodes[side]);
            (*set).u64s = output[side]
                .cast::<u64>()
                .offset_from(set.cast::<u64>().add(3)) as u16;
            super::types::set_btree_bset_end(
                parent_nodes[side],
                (*parent_nodes[side]).set.as_mut_ptr(),
            );
            super::bset_build::bch2_btree_build_aux_trees(parent_nodes[side]);
        }
        for side in 0..2 {
            let ptr = child_ptr(parent_nodes[side]);
            super::bkey::bkey_copy(
                &mut (*parent_nodes[side]).key,
                (&ptr as *const super::bset::bkey_i_btree_ptr_v2).cast(),
            );
            if cache_initialized {
                let _ = super::cache::bch2_btree_node_transition_state(
                    cache_ptr,
                    parent_nodes[side],
                    super::types::btree_node_cache_state::BTREE_NODE_CACHE_CLEAN,
                );
                super::cache::bch2_btree_node_set_dirty(c, parent_nodes[side]);
            }
        }

        let mut parent_paths = [0; 2];
        for side in 0..2 {
            let node = parent_nodes[side];
            let path_idx = super::iter::bch2_path_get_unlocked_mut(
                trans,
                (*node).c.btree_id,
                (*node).c.level,
                (*node).key.k.p,
                false,
            );
            super::iter::btree_path_take_new_node(
                trans,
                (*trans).paths.add(path_idx as usize),
                node,
            );
            parent_paths[side] = path_idx;
        }

        let old_side = (super::bkey::bpos_cmp(old_pos, pivot) > 0) as usize;
        let mut old_iter = super::types::btree_node_iter::default();
        super::node_iter::bch2_btree_node_iter_init(
            c,
            parent_nodes[old_side],
            &mut old_iter,
            &old_pos,
        );
        let old_key =
            super::node_iter::bch2_btree_node_iter_peek(&mut old_iter, parent_nodes[old_side]);
        if old_key.is_null()
            || !super::bkey::bpos_eq(
                super::node_iter::bkey_unpack_pos(parent_nodes[old_side], old_key),
                old_pos,
            )
        {
            release_paths(&parent_paths);
            release_paths(&replacement_paths);
            for node in replacement {
                release_node(node);
            }
            release_node(left_parent);
            release_node(right_parent);
            return -8;
        }
        super::bset_update::btree_keys_account_key(
            &mut (*parent_nodes[old_side]).nr,
            0,
            old_key,
            -1,
        );
        super::bset_update::bch2_bset_delete(
            parent_nodes[old_side],
            old_key,
            (*old_key).u64s as u32,
        );

        let mut insert = crate::data::keylist::bch2_keylist_front(&mut parent_keys);
        while insert != parent_keys.end.top {
            let side = (super::bkey::bpos_cmp((*insert).k.p, pivot) > 0) as usize;
            let node = parent_nodes[side];
            let last = super::types::bset_tree_last(node);
            let mut insert_iter = super::types::btree_node_iter::default();
            super::node_iter::bch2_btree_node_iter_init(c, node, &mut insert_iter, &(*insert).k.p);
            let where_ =
                super::node_iter::bch2_btree_node_iter_bset_pos(&mut insert_iter, node, last);
            if (*trans).journal_replay_not_finished {
                let journal_keys = core::ptr::addr_of!((*c).journal_keys);
                let _overwrite_lock = (&(*journal_keys).overwrite_lock).lock().unwrap();
                crate::journal::bch2_journal_key_check_or_overwrite(
                    c,
                    (*node).c.btree_id,
                    (*node).c.level,
                    (*insert).k.p,
                    false,
                );
            }
            super::bset_update::bch2_bset_insert(node, where_, insert, 0);
            insert = super::bkey::bkey_next(insert);
        }
        for node in parent_nodes {
            btree_node_reset_sib_u64s(node);
            #[cfg(debug_assertions)]
            super::bset_update::__bch2_verify_btree_nr_keys(node);
        }

        retire_node(old_node);
        for node in replacement.iter().rev() {
            super::iter::bch2_trans_node_add(trans, *node);
        }
        super::iter::bch2_trans_node_verify_not_in_iters(trans, old_node);
        release_paths(&replacement_paths);
        for node in replacement {
            crate::lock::six::six_unlock_write(&(*node).c.lock);
            crate::lock::six::six_unlock_intent(&(*node).c.lock);
        }

        old_node = parent;
        replacement = [left_parent, right_parent];
        replacement_paths = parent_paths;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btree::bset::{bset as disk_bset, btree_node as disk_btree_node};
    use crate::btree::types::{bset_tree, BSET_NO_AUX_TREE_VAL};

    #[test]
    fn insert_fit_leaves_varint_slop_u64() {
        let mut words = vec![0u64; 64];
        let mut b = btree::default();
        b.data = words.as_mut_ptr().cast::<disk_btree_node>();
        b.byte_order = 9;
        b.nsets = 1;
        unsafe {
            let set = words.as_mut_ptr().add(17).cast::<disk_bset>();
            (*set).u64s = 42;
            b.set[0] = bset_tree {
                size: 0,
                extra: BSET_NO_AUX_TREE_VAL,
                data_offset: 17,
                aux_data_offset: u16::MAX,
                end_offset: 62,
            };
            assert_eq!(bch2_btree_keys_u64s_remaining(&mut b), 1);
            assert!(bch2_btree_node_insert_fits(&mut b, 1));
            assert!(!bch2_btree_node_insert_fits(&mut b, 2));
            b.flags |= 1 << BTREE_NODE_need_rewrite;
            assert!(!bch2_btree_node_insert_fits(&mut b, 1));
        }
    }

    #[test]
    fn allocates_fake_root_for_recovery() {
        unsafe {
            let mut c = crate::btree::types::bch_fs::default();
            assert_eq!(crate::sb::io::bch2_sb_realloc(&mut c.disk_sb, 0), 0);
            (*c.disk_sb.sb).flags[0] = 1 << 12;
            assert_eq!(super::super::cache::bch2_fs_btree_cache_init(&mut c), 0);
            super::bch2_btree_root_alloc_fake(&mut c, 0, 1);
            let root = crate::btree::types::bch2_btree_id_root_b(&c, 0);
            assert!(!root.is_null());
            assert_eq!((*root).c.level, 1);
            assert!(crate::btree::types::btree_node_fake(root));
            assert!(crate::btree::types::btree_node_need_rewrite(root));
            super::super::cache::bch2_fs_btree_cache_exit(&mut c);
            crate::sb::io::bch2_free_super(&mut c.disk_sb);
        }
    }

    #[test]
    fn full_root_leaf_splits_grows_root_and_retries_insert() {
        use crate::btree::bkey::{
            bkey, bkey_format_key_bits, BKEY_FORMAT_CURRENT, BKEY_U64S, KEY_FORMAT_CURRENT,
            POS_MIN, SPOS, SPOS_MAX,
        };
        use crate::btree::iter::{
            bch2_btree_iter_next, bch2_btree_iter_peek, bch2_trans_begin, bch2_trans_init,
            bch2_trans_iter_exit, bch2_trans_iter_init, btree_iter, btree_trans, BTREE_ITER_intent,
        };
        use crate::btree::types::{bch2_btree_id_root_set, bch_fs};
        use crate::btree::update::{bch2_trans_commit, bch2_trans_update};

        unsafe {
            let mut words = vec![0u64; 64];
            let mut leaf = Box::new(btree::default());
            leaf.data = words.as_mut_ptr().cast::<disk_btree_node>();
            leaf.byte_order = 9;
            leaf.format = BKEY_FORMAT_CURRENT;
            leaf.nr_key_bits = bkey_format_key_bits(&leaf.format) as u8;
            leaf.nsets = 1;
            (*leaf.data).min_key = POS_MIN;
            (*leaf.data).max_key = SPOS_MAX;
            let disk_set = words.as_mut_ptr().add(17).cast::<disk_bset>();
            (*disk_set).u64s = 40;
            for idx in 0..8 {
                *words.as_mut_ptr().add(20 + idx * 5).cast::<bkey>() = bkey {
                    u64s: BKEY_U64S,
                    format: KEY_FORMAT_CURRENT,
                    type_: 6,
                    p: SPOS(1, idx as u64 + 1, 0),
                    ..Default::default()
                };
            }
            leaf.set[0] = bset_tree {
                size: 0,
                extra: BSET_NO_AUX_TREE_VAL,
                data_offset: 17,
                aux_data_offset: u16::MAX,
                end_offset: 60,
            };
            leaf.nr.live_u64s = 40;
            leaf.nr.bset_u64s[0] = 40;
            leaf.nr.unpacked_keys = 8;

            let mut c = bch_fs::default();
            assert_eq!(crate::sb::io::bch2_sb_realloc(&mut c.disk_sb, 0), 0);
            (*c.disk_sb.sb).flags[0] = 1 << 12;
            bch2_btree_id_root_set(&mut c, 0, &mut *leaf);
            let mut insertion = crate::btree::bkey::bkey_i {
                k: bkey {
                    u64s: BKEY_U64S,
                    format: KEY_FORMAT_CURRENT,
                    type_: 6,
                    p: SPOS(1, 9, 0),
                    ..Default::default()
                },
                ..Default::default()
            };

            let mut trans = btree_trans::default();
            bch2_trans_init(&mut trans, &mut c);
            let mut iter = btree_iter::default();
            bch2_trans_iter_init(&mut trans, &mut iter, 0, insertion.k.p, BTREE_ITER_intent);
            assert!(bch2_btree_iter_peek(&mut iter).k.is_null());
            assert_eq!(
                bch2_trans_update(&mut trans, &mut iter, &mut insertion, 0),
                0
            );
            assert_eq!(bch2_trans_commit(&mut trans), -4);
            /* A split is a transaction restart boundary: reset the old
             * transaction before another transaction attempts traversal, as
             * bch2_trans_begin() does in the local retry macros. */
            bch2_trans_begin(&mut trans);
            bch2_trans_iter_exit(&mut iter);
            assert_eq!(
                (*crate::btree::types::bch2_btree_id_root_b(&c, 0)).c.level,
                1
            );

            let mut retry_trans = btree_trans::default();
            bch2_trans_init(&mut retry_trans, &mut c);
            let mut retry = btree_iter::default();
            bch2_trans_iter_init(
                &mut retry_trans,
                &mut retry,
                0,
                insertion.k.p,
                BTREE_ITER_intent,
            );
            assert!(bch2_btree_iter_peek(&mut retry).k.is_null());
            assert_eq!(
                bch2_trans_update(&mut retry_trans, &mut retry, &mut insertion, 0),
                0
            );
            assert_eq!(bch2_trans_commit(&mut retry_trans), 0);
            bch2_trans_iter_exit(&mut retry);

            let mut read_trans = btree_trans::default();
            bch2_trans_init(&mut read_trans, &mut c);
            let mut read = btree_iter::default();
            bch2_trans_iter_init(&mut read_trans, &mut read, 0, SPOS(1, 0, 0), 0);
            let mut seen = Vec::new();
            let mut k = bch2_btree_iter_peek(&mut read);
            while !k.k.is_null() {
                seen.push((*k.k).p.offset);
                k = bch2_btree_iter_next(&mut read);
            }
            assert_eq!(seen, [1, 2, 3, 4, 5, 6, 7, 8, 9]);
            bch2_trans_iter_exit(&mut read);

            let mut restart_offsets = Vec::new();
            for offset in 10..=69 {
                let mut key = crate::btree::bkey::bkey_i {
                    k: bkey {
                        u64s: BKEY_U64S,
                        format: KEY_FORMAT_CURRENT,
                        type_: 6,
                        p: SPOS(1, offset, 0),
                        ..Default::default()
                    },
                    ..Default::default()
                };
                let mut grow_trans = btree_trans::default();
                bch2_trans_init(&mut grow_trans, &mut c);
                let mut grow = btree_iter::default();
                bch2_trans_iter_init(&mut grow_trans, &mut grow, 0, key.k.p, BTREE_ITER_intent);
                assert!(bch2_btree_iter_peek(&mut grow).k.is_null());
                assert_eq!(
                    bch2_trans_update(&mut grow_trans, &mut grow, &mut key, 0),
                    0
                );
                let ret = bch2_trans_commit(&mut grow_trans);
                bch2_trans_iter_exit(&mut grow);
                if ret == -4 {
                    bch2_trans_begin(&mut grow_trans);
                    restart_offsets.push(offset);
                    let mut split_retry_trans = btree_trans::default();
                    bch2_trans_init(&mut split_retry_trans, &mut c);
                    let mut split_retry = btree_iter::default();
                    bch2_trans_iter_init(
                        &mut split_retry_trans,
                        &mut split_retry,
                        0,
                        key.k.p,
                        BTREE_ITER_intent,
                    );
                    assert!(bch2_btree_iter_peek(&mut split_retry).k.is_null());
                    assert_eq!(
                        bch2_trans_update(&mut split_retry_trans, &mut split_retry, &mut key, 0,),
                        0
                    );
                    assert_eq!(bch2_trans_commit(&mut split_retry_trans), 0);
                    bch2_trans_iter_exit(&mut split_retry);
                } else {
                    assert_eq!(ret, 0);
                }
            }

            let root = crate::btree::types::bch2_btree_id_root_b(&c, 0);
            assert_eq!(restart_offsets, [19, 27, 35, 43, 51, 59, 67]);
            assert_eq!((*root).c.level, 2);
            assert_eq!((*root).nr.live_u64s, 30);
            assert_eq!((*root).nr.packed_keys, 3);
            assert_eq!((*root).nr.unpacked_keys, 0);
            let mut root_iter = crate::btree::types::btree_node_iter::default();
            crate::btree::node_iter::bch2_btree_node_iter_init_from_start(&mut root_iter, root);
            loop {
                let ptr = crate::btree::node_iter::bch2_btree_node_iter_peek(&mut root_iter, root);
                if ptr.is_null() {
                    break;
                }
                let key_u64s = crate::btree::bkey::bkeyp_key_u64s(&(*root).format, &*ptr);
                let child = *ptr.cast::<u64>().add(key_u64s as usize) as usize as *mut btree;
                assert!(!child.is_null());
                assert_eq!((*child).c.level, 1);
                assert!((*child).format.key_u64s < BKEY_U64S);
                assert!((*child).nr.packed_keys != 0);
                crate::btree::bset_update::__bch2_verify_btree_nr_keys(child);
                crate::btree::node_iter::bch2_btree_node_iter_advance(&mut root_iter, root);
            }
            let mut final_trans = btree_trans::default();
            bch2_trans_init(&mut final_trans, &mut c);
            let mut final_iter = btree_iter::default();
            bch2_trans_iter_init(&mut final_trans, &mut final_iter, 0, SPOS(1, 0, 0), 0);
            let mut final_seen = Vec::new();
            let mut k = bch2_btree_iter_peek(&mut final_iter);
            while !k.k.is_null() {
                final_seen.push((*k.k).p.offset);
                k = bch2_btree_iter_next(&mut final_iter);
            }
            assert_eq!(final_seen, (1..=69).collect::<Vec<_>>());
            bch2_trans_iter_exit(&mut final_iter);

            for offset in (1..=69).step_by(2) {
                loop {
                    let mut delete_trans = btree_trans::default();
                    bch2_trans_init(&mut delete_trans, &mut c);
                    assert_eq!(
                        crate::btree::update::bch2_btree_delete(
                            &mut delete_trans,
                            0,
                            SPOS(1, offset, 0),
                            0,
                        ),
                        0
                    );
                    let ret = bch2_trans_commit(&mut delete_trans);
                    if ret == 0 {
                        break;
                    }
                    assert_eq!(ret, -4, "delete split retry offset={offset}");
                    bch2_trans_begin(&mut delete_trans);
                }
            }

            let mut after_delete_trans = btree_trans::default();
            bch2_trans_init(&mut after_delete_trans, &mut c);
            let mut after_delete = btree_iter::default();
            bch2_trans_iter_init(
                &mut after_delete_trans,
                &mut after_delete,
                0,
                SPOS(1, 0, 0),
                0,
            );
            let mut after_delete_seen = Vec::new();
            let mut k = bch2_btree_iter_peek(&mut after_delete);
            while !k.k.is_null() {
                after_delete_seen.push((*k.k).p.offset);
                k = bch2_btree_iter_next(&mut after_delete);
            }
            assert_eq!(after_delete_seen, (2..=68).step_by(2).collect::<Vec<_>>());
            bch2_trans_iter_exit(&mut after_delete);
            crate::sb::io::bch2_free_super(&mut c.disk_sb);
        }
    }
}
