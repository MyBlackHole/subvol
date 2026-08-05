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
    crate::rewrite_log_debug!(
        "journal root entry to root id={} entry_level={}",
        (*entry).btree_id,
        (*entry).level
    );
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
            crate::rewrite_log_debug!(
                "journal root entry id={id} level={} alive={}",
                (*root).level,
                (*root).alive
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
        crate::rewrite_log_debug!(
            "set_root_for_read id={} b_level={} slot_level={}",
            id,
            (*b).c.level,
            (*slot).level
        );
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

unsafe fn bch2_btree_node_lock_write(trans: *mut super::iter::btree_trans, b: *mut btree) -> i32 {
    let level = (*b).c.level as usize;
    /* bcachefs fs/btree/locking.c bch2_btree_node_lock_write():
     * six_unlock() 在 reader 计数归零前不会唤醒写者，因此获取
     * write 锁前必须先释放本事务内所有路径对该节点的 read 锁
     * （bch2_btree_node_lock_counts() + six_lock_readers_add(-readers)），
     * 获取成功后再恢复，避免自身路径的 read 锁永久阻塞 write。 */
    let mut readers = 0u32;
    for pid in 1..super::iter::BTREE_ITER_INITIAL {
        if (*trans).paths_allocated & (1u64 << pid) == 0 {
            continue;
        }
        let p = (*trans).paths.add(pid);
        if (*p).l[level].b == b
            && super::iter::btree_node_locked_type(p, level) == super::iter::BTREE_NODE_READ_LOCKED
        {
            readers += 1;
        }
    }
    if readers > 0 {
        crate::lock::six::six_lock_readers_add(&(*b).c.lock, -(readers as i32));
    }
    let ret = crate::lock::six::six_lock_write(&(*b).c.lock);
    if readers > 0 {
        crate::lock::six::six_lock_readers_add(&(*b).c.lock, readers as i32);
    }
    ret
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
        /* 节点内容不足以形成两个非空叶子（空/过稀疏节点）：bcachefs 节点
         * 容量由 superblock 固定（BCH_SB_BTREE_NODE_SIZE，256KB 量级），
         * 空节点上的事务不会触发 btree_node_full；subvol 小节点配置
         * （512B）下单事务可超出节点容量。此处扩容节点（byte_order + 1）
         * 使容量满足本次事务，等效 bcachefs 固定节点容量的保证，
         * 随后 restart 重试（bcachefs btree_split 的 BUG_ON 路径
         * fs/btree/interior.c 假定分裂总能成功）。 */
        crate::rewrite_log_debug!(
            "btree split rejected: unable to form two non-empty leaves, growing leaf byte_order={}",
            (*src).byte_order
        );
        if (*src).byte_order >= 16 {
            crate::rewrite_log_error!("btree split rejected: leaf byte_order limit exceeded");
            return -12;
        }
        let old_order = (*src).byte_order;
        let new_order = old_order + 1;
        let new_data = std::alloc::alloc_zeroed(std::alloc::Layout::from_size_align_unchecked(
            1usize << new_order,
            core::mem::align_of::<u64>(),
        ))
        .cast::<u64>();
        let new_aux = std::alloc::alloc_zeroed(std::alloc::Layout::from_size_align_unchecked(
            super::types::__btree_aux_data_bytes(new_order as u32),
            core::mem::align_of::<u64>(),
        ))
        .cast::<u64>();
        if new_data.is_null() || new_aux.is_null() {
            if !new_data.is_null() {
                std::alloc::dealloc(
                    new_data.cast(),
                    std::alloc::Layout::from_size_align_unchecked(
                        1usize << new_order,
                        core::mem::align_of::<u64>(),
                    ),
                );
            }
            if !new_aux.is_null() {
                std::alloc::dealloc(
                    new_aux.cast(),
                    std::alloc::Layout::from_size_align_unchecked(
                        super::types::__btree_aux_data_bytes(new_order as u32),
                        core::mem::align_of::<u64>(),
                    ),
                );
            }
            crate::rewrite_log_error!("btree split rejected: leaf grow allocation failed");
            return -12;
        }
        core::ptr::copy_nonoverlapping(
            (*src).data.cast::<u64>(),
            new_data,
            (1usize << old_order) / 8,
        );
        let cache_owned = (*c)
            .btree
            .cache
            .allocations
            .lock()
            .unwrap()
            .contains(&(src as usize));
        if cache_owned {
            super::cache::bch2_btree_node_data_free(src);
        }
        (*src).data = new_data.cast::<super::bset::btree_node>();
        (*src).aux_data = new_aux.cast();
        (*src).byte_order = new_order;
        /* The copied bset_tree entries carry aux-tree state that points
         * into the freed old aux buffer; force a rebuild into the new
         * aux buffer (bch2_btree_build_aux_tree() skips trees that
         * already carry a tree type). */
        super::bset_build::bch2_bset_set_no_aux_tree(src, (*src).set.as_mut_ptr());
        super::bset_build::bch2_btree_build_aux_trees(src);
        (*trans).restarted = 4;
        return -4;
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
        let c = (*trans).c;
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
        /* Split nodes inherit the source node's capacity: bcachefs node
         * size is fixed by the superblock (BCH_SB_BTREE_NODE_SIZE), so a
         * split never changes node capacity.  subvol nodes grow on demand
         * (see the grow path below), so children must inherit the grown
         * size or a small split target would immediately overflow. */
        if (*node).byte_order < (*src).byte_order {
            let target_order = (*src).byte_order;
            let new_data = std::alloc::alloc_zeroed(std::alloc::Layout::from_size_align_unchecked(
                1usize << target_order,
                core::mem::align_of::<u64>(),
            ))
            .cast::<u64>();
            let new_aux = std::alloc::alloc_zeroed(std::alloc::Layout::from_size_align_unchecked(
                super::types::__btree_aux_data_bytes(target_order as u32),
                core::mem::align_of::<u64>(),
            ))
            .cast::<u64>();
            assert!(!new_data.is_null() && !new_aux.is_null());
            super::cache::bch2_btree_node_data_free(node);
            (*node).data = new_data.cast::<super::bset::btree_node>();
            (*node).aux_data = new_aux.cast();
            (*node).byte_order = target_order;
        }
        super::bset_build::bch2_bset_init_first(node, &mut (*(*node).data).keys);
        super::bset_build::bch2_btree_build_aux_trees(node);
        /* __bch2_btree_node_alloc（interior.c:451-505）的扇区部分：
         * 新节点 key 经 bch2_alloc_sectors_append_ptrs 必带磁盘 extent
         * （dev/offset/gen），节点写盘（io.rs __bch2_btree_node_write）
         * 依赖 key 中的 extent ptr；分配失败（如磁盘空间不足）释放节点
         * 并返回 errno，对齐 reserve_get 失败路径（interior.c:714-721）。 */
        let sectors_ret = super::alloc::bch2_btree_node_alloc_sectors(c, node);
        if sectors_ret != 0 {
            crate::lock::six::six_unlock_write(&(*node).c.lock);
            crate::lock::six::six_unlock_intent(&(*node).c.lock);
            super::cache::bch2_btree_node_data_free(node);
            super::cache::bch2_btree_node_mem_free(c, node);
            return Err(sectors_ret);
        }
        Ok(node)
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
        crate::rewrite_log_debug!(
            "retire_node node=0x{:x} state={:x}",
            node as usize,
            (*node)
                .c
                .lock
                .state
                .load(core::sync::atomic::Ordering::Relaxed)
        );
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

    let left = match allocate_node(0) {
        Ok(node) => node,
        Err(ret) => return ret,
    };
    let right = match allocate_node(0) {
        Ok(node) => node,
        Err(ret) => {
            release_node(left);
            return ret;
        }
    };
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
        /* 对齐 sort.c:498-501 "Make sure we preserve bset journal_seq"：
         * 拆分内容继承 src 各 bset 的最大 journal_seq，保证节点二次
         * 写盘满足 write.c:470 BUG_ON(b->written && !seq)。 */
        let mut src_seq = 0u64;
        for set_idx in 0..(*src).nsets as usize {
            src_seq = src_seq
                .max((*super::types::bset(src, (*src).set.as_ptr().add(set_idx))).journal_seq);
        }
        (*disk_set).journal_seq = src_seq;
        super::types::set_btree_bset_end(nodes[side], (*nodes[side]).set.as_mut_ptr());
        btree_node_reset_sib_u64s(nodes[side]);
        super::bset_build::bch2_btree_build_aux_trees(nodes[side]);
        #[cfg(debug_assertions)]
        super::bset_update::__bch2_verify_btree_nr_keys(nodes[side]);
    }

    let child_ptr = |child: *mut btree, out: *mut super::bset::bkey_i_btree_ptr_v2| {
        (*out).k = super::bkey::bkey {
            u64s: 10,
            format: super::bkey::KEY_FORMAT_CURRENT,
            type_: super::bset::KEY_TYPE_btree_ptr_v2,
            p: (*(*child).data).max_key,
            ..Default::default()
        };
        (*out).v = super::bset::bch_btree_ptr_v2 {
            mem_ptr: child as usize as u64,
            seq: (*(*child).data).keys.seq,
            min_key: (*(*child).data).min_key,
            ..Default::default()
        };
        /* 同 rewrite（2088-2104 注释）：allocate_node 时 alloc_sectors 已把
         * 磁盘 extent 写入节点 key（bch2_alloc_sectors_append_ptrs 语义，
         * interior.c:515-518），child_ptr 重建 key 时必须继承该 extent，
         * 否则节点写盘（io.rs __bch2_btree_node_write 依赖 key extent）
         * 返回 -2。mem_ptr 键场景（无 extent）则跳过。 */
        let old_ptrs = super::bset::bch2_bkey_ptrs_c(super::bkey::bkey_s_c {
            k: &(*child).key.k,
            v: &(*child).key.v,
        });
        if !old_ptrs.start.is_null() && old_ptrs.start < old_ptrs.end {
            super::bset::bch2_bkey_append_ptr(c, out.cast(), (*old_ptrs.start).ptr);
        }
    };

    for side in 0..2 {
        let mut ptr_buf = [0u64; 16];
        child_ptr(nodes[side], ptr_buf.as_mut_ptr().cast());
        super::bkey::bkey_copy(&mut (*nodes[side]).key, ptr_buf.as_ptr().cast());
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
        crate::rewrite_log_debug!(
            "btree split up-level old_level={} parent_level={} parent_null={}",
            (*old_node).c.level,
            parent_level,
            parent.is_null()
        );
        let old_pos = (*(*old_node).data).max_key;

        if parent.is_null() {
            crate::rewrite_log_debug!(
                "btree split making new root old_node=0x{:x} parent_level={}",
                old_node as usize,
                parent_level
            );
            if parent_level >= super::bset::BTREE_MAX_DEPTH as usize {
                release_paths(&replacement_paths);
                for node in replacement {
                    release_node(node);
                }
                return -12;
            }
            let root = match allocate_node(parent_level as u8) {
                Ok(node) => node,
                Err(ret) => {
                    release_paths(&replacement_paths);
                    for node in replacement {
                        release_node(node);
                    }
                    return ret;
                }
            };
            crate::rewrite_log_debug!(
                "split new root=0x{:x} replacement=[0x{:x}, 0x{:x}]",
                root as usize,
                replacement[0] as usize,
                replacement[1] as usize
            );
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
                let mut ptr_buf = [0u64; 16];
                child_ptr(child, ptr_buf.as_mut_ptr().cast());
                let ptr = ptr_buf
                    .as_mut_ptr()
                    .cast::<super::bset::bkey_i_btree_ptr_v2>();
                let last = super::types::bset_tree_last(root);
                let mut insert_iter = super::types::btree_node_iter::default();
                super::node_iter::bch2_btree_node_iter_init(c, root, &mut insert_iter, &(*ptr).k.p);
                let where_ =
                    super::node_iter::bch2_btree_node_iter_bset_pos(&mut insert_iter, root, last);
                if (*trans).journal_replay_not_finished {
                    let journal_keys = core::ptr::addr_of!((*c).journal_keys);
                    let _overwrite_lock = (&(*journal_keys).overwrite_lock).lock().unwrap();
                    crate::journal::bch2_journal_key_check_or_overwrite(
                        c,
                        (*root).c.btree_id,
                        (*root).c.level,
                        (*ptr).k.p,
                        false,
                    );
                }
                super::bset_update::bch2_bset_insert(root, where_, ptr_buf.as_mut_ptr().cast(), 0);
            }
            btree_node_reset_sib_u64s(root);
            /* 同 left/right 填充（sort.c:498-501）：root 内容为指向
             * replacement 的 child key，journal_seq 继承其最大 seq。 */
            let mut root_seq = 0u64;
            for node in replacement {
                for set_idx in 0..(*node).nsets as usize {
                    root_seq = root_seq.max(
                        (*super::types::bset(node, (*node).set.as_ptr().add(set_idx))).journal_seq,
                    );
                }
            }
            (*(*root).data).keys.journal_seq = root_seq;
            super::bset_build::bch2_btree_build_aux_trees(root);
            let mut root_buf = [0u64; 16];
            child_ptr(root, root_buf.as_mut_ptr().cast());
            super::bkey::bkey_copy(&mut (*root).key, root_buf.as_ptr().cast());
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
            crate::rewrite_log_debug!(
                "split making new root after retire: old_node=0x{:x} state={:x}",
                old_node as usize,
                (*old_node)
                    .c
                    .lock
                    .state
                    .load(core::sync::atomic::Ordering::Relaxed)
            );
            super::iter::bch2_trans_node_add(trans, root);
            for node in replacement.iter().rev() {
                super::iter::bch2_trans_node_add(trans, *node);
            }
            super::iter::bch2_trans_node_verify_not_in_iters(trans, old_node);

            release_paths(&replacement_paths);
            /* 对齐 bch2_btree_update_new_node（interior.c:1303）：
             * 新节点创建完成即写盘（write_trans 要求 dirty，root 已
             * set_dirty）。写序：节点 bset 头 journal_seq 落后于
             * journal 记录（write.c:485 读时过滤），崩溃时以 journal
             * 为准，节点实体写盘不破坏一致性。 */
            super::io::bch2_btree_node_write_trans(
                trans,
                root,
                crate::lock::six::six_lock_type::SIX_LOCK_write,
                0,
            );
            for node in replacement {
                super::io::bch2_btree_node_write_trans(
                    trans,
                    node,
                    crate::lock::six::six_lock_type::SIX_LOCK_write,
                    0,
                );
            }
            /* The temporary paths drop their recursive references first;
             * consume the allocator-owned primary references afterwards, as
             * btree_update_done() does after interior.c's out: cleanup. */
            crate::lock::six::six_unlock_write(&(*root).c.lock);
            crate::lock::six::six_unlock_intent(&(*root).c.lock);
            crate::rewrite_log_debug!(
                "split making new root done: old_node=0x{:x} state={:x} root=0x{:x} state={:x}",
                old_node as usize,
                (*old_node)
                    .c
                    .lock
                    .state
                    .load(core::sync::atomic::Ordering::Relaxed),
                root as usize,
                (*root)
                    .c
                    .lock
                    .state
                    .load(core::sync::atomic::Ordering::Relaxed)
            );
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
            crate::rewrite_log_debug!("btree split fits in parent");
            crate::rewrite_log_debug!(
                "btree split locks old_node: {:?} parent: {:?}",
                crate::lock::six::six_lock_counts(&(*old_node).c.lock),
                crate::lock::six::six_lock_counts(&(*parent).c.lock)
            );
            crate::rewrite_log_debug!("btree split lock old_node write");
            if bch2_btree_node_lock_write(trans, old_node) != 0 {
                release_paths(&replacement_paths);
                for node in replacement {
                    release_node(node);
                }
                return -10;
            }
            crate::rewrite_log_debug!("btree split lock parent write");
            for pid in 1..super::iter::BTREE_ITER_INITIAL {
                if (*trans).paths_allocated & (1u64 << pid) == 0 {
                    continue;
                }
                let p = (*trans).paths.add(pid);
                let l0 = (*p).nodes_locked & 3;
                let pos = core::ptr::addr_of!((*p).pos).read_unaligned();
                let l0b = core::ptr::addr_of!((*p).l[0].b).read_unaligned();
                let l1b = core::ptr::addr_of!((*p).l[1].b).read_unaligned();
                let inode = core::ptr::addr_of!(pos.inode).read_unaligned();
                let offset = core::ptr::addr_of!(pos.offset).read_unaligned();
                let snapshot = core::ptr::addr_of!(pos.snapshot).read_unaligned();
                crate::rewrite_log_debug!(
                    "btree split path#{pid} pos=({inode},{offset},{snapshot}) locks=0x{l0:x} l0b={l0b:p} l1b={l1b:p} ref={}",
                    (*p).ref_
                );
            }
            if bch2_btree_node_lock_write(trans, parent) != 0 {
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
            let old_had_whiteout = (*old).format & 0x80 != 0;
            (*old).type_ = 0;
            /* 对齐 commit.c:198-203 bch2_btree_bset_insert_key_inlined：
             * 置白的旧 key 若带 needs_whiteout（已写盘），新 key 继承该
             * 标记（其进入 last bset 后仍视作已写盘区域），旧 key 清除。 */
            if old_had_whiteout {
                (*old).format &= 0x7f;
            }
            if old_writeable {
                super::bset_update::bch2_bset_delete(parent, old, old_u64s);
            }

            let mut left_key = [0u64; 16];
            let mut right_key = [0u64; 16];
            child_ptr(replacement[0], left_key.as_mut_ptr().cast());
            child_ptr(replacement[1], right_key.as_mut_ptr().cast());
            let old_pos = super::node_iter::bkey_unpack_pos(parent, old);
            if old_had_whiteout {
                for key in [&mut left_key, &mut right_key] {
                    let k = key.as_mut_ptr().cast::<super::bset::bkey_i_btree_ptr_v2>();
                    if super::bkey::bpos_eq((*k).k.p, old_pos) {
                        (*k).k.format |= 0x80;
                    }
                }
            }
            {
                let lp = (*(left_key.as_ptr().cast::<super::bset::bkey_i_btree_ptr_v2>()))
                    .k
                    .p;
                let rp = (*(right_key
                    .as_ptr()
                    .cast::<super::bset::bkey_i_btree_ptr_v2>()))
                .k
                .p;
                crate::rewrite_log_debug!(
                    "fits parent level={} old={:?} old_w={} nsets={} left={:?} right={:?}",
                    (*parent).c.level,
                    old_pos,
                    old_writeable,
                    (*parent).nsets,
                    lp,
                    rp
                );
            }
            for ptr in [
                left_key
                    .as_mut_ptr()
                    .cast::<super::bset::bkey_i_btree_ptr_v2>(),
                right_key
                    .as_mut_ptr()
                    .cast::<super::bset::bkey_i_btree_ptr_v2>(),
            ] {
                let mut insert_iter = super::types::btree_node_iter::default();
                super::node_iter::bch2_btree_node_iter_init(
                    c,
                    parent,
                    &mut insert_iter,
                    &(*ptr).k.p,
                );
                let where_ =
                    super::node_iter::bch2_btree_node_iter_bset_pos(&mut insert_iter, parent, last);
                crate::rewrite_log_debug!(
                    "fits insert pos={:?} where_off={}",
                    (*ptr).k.p,
                    super::types::__btree_node_key_to_offset(parent, where_)
                );
                if (*trans).journal_replay_not_finished {
                    let journal_keys = core::ptr::addr_of!((*c).journal_keys);
                    let _overwrite_lock = (&(*journal_keys).overwrite_lock).lock().unwrap();
                    crate::journal::bch2_journal_key_check_or_overwrite(
                        c,
                        (*parent).c.btree_id,
                        (*parent).c.level,
                        (*ptr).k.p,
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
            /* 同 root 分支：replacement 为新节点，创建完成即写盘
             * （对齐 bch2_btree_update_new_node interior.c:1303）。 */
            for node in replacement {
                super::io::bch2_btree_node_write_trans(
                    trans,
                    node,
                    crate::lock::six::six_lock_type::SIX_LOCK_write,
                    0,
                );
            }
            for node in replacement {
                crate::lock::six::six_unlock_write(&(*node).c.lock);
                crate::lock::six::six_unlock_intent(&(*node).c.lock);
            }
            crate::lock::six::six_unlock_write(&(*parent).c.lock);

            /* T0204 split 后逐层合并（fs/btree/interior.c:2308-2314）：
             * split 完成、parent 写锁释放后，从父层起对 intent 已锁定的
             * 各层尝试前台合并，防止分裂出的兄弟节点在相邻层堆积；
             * 合并成功同样交由调用方 restart 重遍历。 */
            let mut l = (*path).level as usize + 1;
            let mut merge_ret = 0;
            while super::iter::btree_node_intent_locked(path, l) && merge_ret == 0 {
                merge_ret =
                    bch2_foreground_maybe_merge(trans, path_idx, l, 0, core::ptr::null_mut());
                l += 1;
            }
            if merge_ret != 0 {
                crate::rewrite_log_error!("btree split merge after failed ret={merge_ret}");
                for node in replacement {
                    crate::lock::six::six_unlock_write(&(*node).c.lock);
                    crate::lock::six::six_unlock_intent(&(*node).c.lock);
                }
                return merge_ret;
            }
            return 0;
        }

        /* keylist 无容量检查（keylist.rs 与上游同，调用方保证空间）：
         * 2 个子节点 key 含 extent 后为 11 u64/个（T0210 child_ptr
         * 继承磁盘 extent），buffer 需 ≥ 22 u64。 */
        let mut parent_key_words = [0u64; 24];
        let mut parent_keys = crate::data::keylist::keylist::default();
        crate::data::keylist::bch2_keylist_init(&mut parent_keys, parent_key_words.as_mut_ptr());
        let mut left_key = [0u64; 16];
        let mut right_key = [0u64; 16];
        child_ptr(replacement[0], left_key.as_mut_ptr().cast());
        child_ptr(replacement[1], right_key.as_mut_ptr().cast());
        crate::data::keylist::bch2_keylist_add(&mut parent_keys, left_key.as_ptr().cast());
        crate::data::keylist::bch2_keylist_add(&mut parent_keys, right_key.as_ptr().cast());

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
        let left_parent = match allocate_node((*parent).c.level) {
            Ok(node) => node,
            Err(ret) => {
                release_paths(&replacement_paths);
                for node in replacement {
                    release_node(node);
                }
                return ret;
            }
        };
        let right_parent = match allocate_node((*parent).c.level) {
            Ok(node) => node,
            Err(ret) => {
                release_paths(&replacement_paths);
                for node in replacement {
                    release_node(node);
                }
                release_node(left_parent);
                return ret;
            }
        };
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
            /* 同 left/right 填充（sort.c:498-501）：parent_nodes 内容
             * 继承 parent 各 bset 的最大 journal_seq。 */
            let mut parent_seq = 0u64;
            for set_idx in 0..(*parent).nsets as usize {
                parent_seq = parent_seq.max(
                    (*super::types::bset(parent, (*parent).set.as_ptr().add(set_idx))).journal_seq,
                );
            }
            (*set).journal_seq = parent_seq;
            super::types::set_btree_bset_end(
                parent_nodes[side],
                (*parent_nodes[side]).set.as_mut_ptr(),
            );
            super::bset_build::bch2_btree_build_aux_trees(parent_nodes[side]);
        }
        for side in 0..2 {
            let mut ptr_buf = [0u64; 16];
            child_ptr(parent_nodes[side], ptr_buf.as_mut_ptr().cast());
            super::bkey::bkey_copy(&mut (*parent_nodes[side]).key, ptr_buf.as_ptr().cast());
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

/* ============================================================================
 * 前台合并（树收缩）
 *
 * 对应 bcachefs fs/btree/interior.c 的 __bch2_foreground_maybe_merge() 与
 * btree_merge_push_pos()/compute_merge()/merge_fail_reset_sib_u64s()：
 * 节点因删除而变空时，把自身与相邻兄弟（sib_u64s 估计 + threshold 门控）
 * 合并进一个新节点，删除被替换节点并同步更新 parent 键，使树随删除
 * 收缩，与 split 对称（interior.rs:380 bch2_btree_split_leaf）。
 *
 * 域内差异（详见 T0204 ac1-source-anchors.md）：
 *  - 无 evicted-size hash / nofill（D1）：sibling 直接 path_get + traverse 真读
 *  - 无 deferred srcs（D1）：push_pos 后每个 src 都有 ->b，二次 compute_merge
 *    与首次结果一致，保留二次调用以对齐上游控制流
 *  - 无 update_start / 锁升级重验（D6）：commit 全程持 fs 锁 + 单写者，
 *    push 时完成的 parent 一致性检查无竞态窗口
 *  - nr_dsts 恒为 1（D3/D10）：bcachefs 的 2-dst 打包（find_balanced_split）
 *    在 subvol 域内不可达，其"降级 1-dst 否则毒化"分支按原样保留
 *  - 路径生命周期（D13）：merge 成功/失败均显式 put 非 pivot 的 src 路径，
 *    pivot 路径留给调用方（commit 重启后由 iter 重遍历）
 * ========================================================================== */

pub(crate) const fn btree_foreground_merge_threshold(b: &btree) -> usize {
    /* BTREE_FOREGROUND_MERGE_THRESHOLD (fs/btree/cache.h:191) */
    btree_buf_max_u64s(b) / 3
}

pub(crate) const fn btree_foreground_merge_hysteresis(b: &btree) -> usize {
    /* BTREE_FOREGROUND_MERGE_HYSTERESIS (fs/btree/cache.h:192) */
    let threshold = btree_foreground_merge_threshold(b);
    threshold + (threshold >> 2)
}

pub(crate) const fn btree_foreground_merge_higher_threshold(b: &btree) -> usize {
    /* BTREE_FOREGROUND_MERGE_HIGHER_THRESHOLD (fs/btree/cache.h:193) */
    btree_buf_max_u64s(b) * 3 / 5
}

pub(crate) unsafe fn btree_node_needs_merge(
    _c: *mut super::types::bch_fs,
    b: *mut btree,
    d: i32,
) -> bool {
    /* fs/btree/interior.h:194 btree_node_needs_merge():
     * min(sib_u64s[0], sib_u64s[1]) + d <= foreground_merge_threshold */
    let threshold = btree_foreground_merge_threshold(&*b) as i32;
    ((*b).sib_u64s[0].min((*b).sib_u64s[1]) as i32) + d <= threshold
}

#[derive(Clone, Copy)]
pub(crate) struct merge_node {
    /* 对应 fs/btree/interior.h:174 struct btree_merge_node
     * （bcachefs 的 ->trans 字段在 subvol 事务域内以参数传递，不落结构） */
    b: *mut btree,
    path: super::iter::btree_path_idx_t,
    live_u64s: usize,
}

unsafe fn merge_fail_reset_sib_u64s_at(
    _c: *mut super::types::bch_fs,
    b: *mut btree,
    sib: usize,
    sib_live_u64s: usize,
) {
    /* interior.c:2577 merge_fail_reset_sib_u64s_at() */
    let mut sib_u64s = (*b).nr.live_u64s as usize + sib_live_u64s;
    let hysteresis = btree_foreground_merge_hysteresis(&*b);

    if sib_u64s > hysteresis {
        sib_u64s -= (sib_u64s - hysteresis) / 2;
    }

    sib_u64s = sib_u64s.min(u16::MAX as usize - 1);
    (*b).sib_u64s[sib] = sib_u64s as u16;
}

unsafe fn merge_fail_reset_sib_u64s(c: *mut super::types::bch_fs, b: *mut btree, s: &merge_node) {
    /* interior.c:2591 merge_fail_reset_sib_u64s()：
     * 节点 key.k.p 即 max_key，prev/next 侧由 bpos 比较判定 */
    if s.b == b {
        return;
    }
    let sib = if super::bkey::bpos_lt((*(*s.b).data).max_key, (*(*b).data).max_key) {
        0
    } else {
        1
    };
    merge_fail_reset_sib_u64s_at(c, b, sib, s.live_u64s);
}

unsafe fn bch2_btree_merge_push_pos(
    trans: *mut super::iter::btree_trans,
    level: usize,
    pivot_path: super::iter::btree_path_idx_t,
    sib: usize,
    dst: &mut [merge_node; 3],
    dst_nr: &mut usize,
) -> i32 {
    /* interior.c:2447 btree_merge_push_pos()（域内简化：无 nofill /
     * evicted-size hash 分支，直接真读；失败路径全部 put 路径引用） */
    let path = (*trans).paths.add(pivot_path as usize);
    let pivot = (*path).l[level].b;
    let parent = (*path).l[level + 1].b;

    if (sib == 0 && super::bkey::bpos_eq((*(*pivot).data).min_key, super::bkey::POS_MIN))
        || (sib == 1 && super::bkey::bpos_eq((*(*pivot).data).max_key, super::bkey::SPOS_MAX))
    {
        (*pivot).sib_u64s[sib] = u16::MAX;
        return 0;
    }

    if (*pivot).sib_u64s[sib] as usize > btree_foreground_merge_threshold(&*pivot) {
        return 0;
    }

    let pos = if sib == 0 {
        /* bpos_predecessor(min_key)：POS_MIN 已排除，offset 必 > 0 */
        let mut p = (*(*pivot).data).min_key;
        p.offset -= 1;
        p
    } else {
        super::bkey::bpos_successor((*(*pivot).data).max_key)
    };

    let sib_path_idx = super::iter::bch2_path_get(
        trans,
        (*path).btree_id,
        &pos,
        level as u8 + 1,
        level as u8,
        super::iter::BTREE_ITER_intent as u16,
    );
    let ret = super::iter::bch2_btree_path_traverse(trans, sib_path_idx, 0);
    if ret != 0 {
        super::iter::bch2_path_put(trans, sib_path_idx, true);
        return ret;
    }
    let sib_path = (*trans).paths.add(sib_path_idx as usize);
    let b = (*sib_path).l[level].b;
    if b.is_null() {
        super::iter::bch2_path_put(trans, sib_path_idx, true);
        return 0;
    }
    let live_u64s = (*b).nr.live_u64s as usize;
    /* parent 一致性（interior.c:2541）：sibling 必须与 pivot 同 parent，
     * 否则毒化该侧估计并跳过 */
    if b == pivot || (*sib_path).l[level + 1].b != parent {
        super::iter::bch2_path_put(trans, sib_path_idx, true);
        (*pivot).sib_u64s[sib] = u16::MAX;
        return 0;
    }
    super::iter::btree_path_set_should_be_locked(trans, sib_path);

    dst[*dst_nr] = merge_node {
        b,
        path: sib_path_idx,
        live_u64s,
    };
    *dst_nr += 1;
    0
}

unsafe fn __bch2_btree_calc_format(state: &mut super::bkey::bkey_format_state, b: *mut btree) {
    /* btree_io.c __bch2_btree_calc_format()：遍历节点全部键（跳过已删），
     * 按 pos 累加进 format state */
    let mut iter = super::types::btree_node_iter::default();
    super::node_iter::bch2_btree_node_iter_init_from_start(&mut iter, b);
    loop {
        let k = super::node_iter::bch2_btree_node_iter_peek(&mut iter, b);
        if k.is_null() {
            break;
        }
        if (*k).type_ != super::bset::KEY_TYPE_deleted {
            let pos = super::node_iter::bkey_unpack_pos(b, &*k);
            super::bkey::bch2_bkey_format_add_pos(state, pos);
        }
        super::node_iter::bch2_btree_node_iter_advance(&mut iter, b);
    }
}

unsafe fn btree_node_u64s_with_format(
    b: *mut btree,
    src_f: &super::bkey::bkey_format,
    new_f: &super::bkey::bkey_format,
) -> usize {
    /* btree_io.c btree_node_u64s_with_format()：按 new_f 重算节点全部键的
     * 打包后大小；pack 不进 new_f 的键（unpacked/位宽溢出）按 unpacked
     * 全宽计，与 bch2_sort_repack 的 transform 失败分支一致 */
    let mut total = 0usize;
    let mut iter = super::types::btree_node_iter::default();
    super::node_iter::bch2_btree_node_iter_init_from_start(&mut iter, b);
    loop {
        let k = super::node_iter::bch2_btree_node_iter_peek(&mut iter, b);
        if k.is_null() {
            break;
        }
        if (*k).type_ != super::bset::KEY_TYPE_deleted {
            let val_u64s = super::bkey::bkeyp_val_u64s(src_f, &*k) as usize;
            if super::bkey::bkey_packed(&*k) {
                let pos = super::node_iter::bkey_unpack_pos(b, &*k);
                let mut words = [0u64; super::bkey::BKEY_U64S as usize];
                let probe = words.as_mut_ptr().cast::<super::bkey::bkey_packed>();
                if super::bkey::bch2_bkey_pack_pos(&mut *probe, pos, &*b) {
                    total += new_f.key_u64s as usize + val_u64s;
                } else {
                    total += super::bkey::BKEY_U64S as usize + val_u64s;
                }
            } else {
                total += super::bkey::BKEY_U64S as usize + val_u64s;
            }
        }
        super::node_iter::bch2_btree_node_iter_advance(&mut iter, b);
    }
    total
}

unsafe fn merge_node_u64s_and_format(
    srcs: &[merge_node; 3],
    nr: usize,
    new_f: &mut super::bkey::bkey_format,
) -> usize {
    /* interior.c:2512 merge_node_u64s_and_format()：所有 src 都在场时的
     * format-aware 精确路径（subvol 无 deferred，恒走此分支） */
    let mut state = super::bkey::bkey_format_state::default();
    super::bkey::bch2_bkey_format_init(&mut state);
    super::bkey::bch2_bkey_format_add_pos(&mut state, (*(*srcs[0].b).data).min_key);
    for i in 0..nr {
        __bch2_btree_calc_format(&mut state, srcs[i].b);
    }
    super::bkey::bch2_bkey_format_add_pos(&mut state, (*(*srcs[nr - 1].b).data).max_key);
    *new_f = super::bkey::bch2_bkey_format_done(&mut state);
    let mut total = 0usize;
    for i in 0..nr {
        total += btree_node_u64s_with_format(srcs[i].b, &(*srcs[i].b).format, new_f);
    }
    total
}

unsafe fn compute_merge(
    trans: *mut super::iter::btree_trans,
    c: *mut super::types::bch_fs,
    b: *mut btree,
    srcs: &mut [merge_node; 3],
    nr: &mut usize,
    new_f: &mut super::bkey::bkey_format,
) -> usize {
    /* interior.c:2832 compute_merge()。
     * 域内差异（D10）：bcachefs 的 nr_dsts==2 打包（find_balanced_split，
     * interior.c:2880）在 subvol 域内不可达；其不可行降级路径按原样保留 */
    let mut total_u64s = merge_node_u64s_and_format(srcs, *nr, new_f);
    let higher = btree_foreground_merge_higher_threshold(&*b);
    let mut nr_dsts = 1usize.max((total_u64s + higher - 1) / higher);

    if nr_dsts >= *nr {
        if *nr == 3 {
            /* interior.c:2839-2851：移除 live_u64s 较小的端侧后重算
             * （btree_merge_node_put 释放其路径引用） */
            let remove_idx = if srcs[0].live_u64s > srcs[2].live_u64s {
                2
            } else {
                0
            };
            merge_fail_reset_sib_u64s(c, b, &srcs[remove_idx]);
            super::iter::bch2_path_put(trans, srcs[remove_idx].path, true);
            if remove_idx == 0 {
                srcs[0] = srcs[1];
                srcs[1] = srcs[2];
            }
            *nr = 2;
            total_u64s = merge_node_u64s_and_format(srcs, *nr, new_f);
            nr_dsts = 1usize.max(
                (total_u64s + btree_buf_max_u64s(&*b) / 2 - 1) / (btree_buf_max_u64s(&*b) / 2),
            );
        }
    }

    if nr_dsts >= *nr {
        /* interior.c:2853-2856：全毒化放弃 */
        for i in 0..*nr {
            merge_fail_reset_sib_u64s(c, b, &srcs[i]);
        }
        return nr_dsts;
    }

    if nr_dsts == 2 {
        /* interior.c:2889-2901 find_balanced_split 失败路径：单节点装得下
         * 则降级 1-dst，否则全毒化 */
        if core::mem::size_of::<super::bset::btree_node>() + total_u64s * 8 < btree_buf_bytes(&*b) {
            nr_dsts = 1;
        } else {
            for i in 0..*nr {
                merge_fail_reset_sib_u64s(c, b, &srcs[i]);
            }
            return *nr;
        }
    }

    nr_dsts
}

unsafe fn btree_merge_topology_check(
    _c: *mut super::types::bch_fs,
    srcs: &[merge_node; 3],
    nr: usize,
) -> bool {
    /* interior.c:2399 btree_merge_topology_check()：相邻 src 键区间必须
     * 连续相接（prev.max successor == next.min） */
    for i in 1..nr {
        let prev = srcs[i - 1].b;
        let next = srcs[i].b;
        if !super::bkey::bpos_eq(
            super::bkey::bpos_successor((*(*prev).data).max_key),
            (*(*next).data).min_key,
        ) {
            return false;
        }
    }
    true
}

unsafe fn merge_put_sibling_paths(
    trans: *mut super::iter::btree_trans,
    srcs: &[merge_node; 3],
    nr: usize,
    pivot_path: super::iter::btree_path_idx_t,
) {
    /* 释放非 pivot 的 src 路径引用（bcachefs 由 darray_merge_node 析构
     * 与 btree_merge_node_put 统一处理；subvol 显式收尾，pivot 路径
     * 由调用方在事务重启时清理） */
    for i in 0..nr {
        if srcs[i].path != pivot_path {
            super::iter::bch2_path_put(trans, srcs[i].path, true);
        }
    }
}

pub(crate) unsafe fn bch2_foreground_maybe_merge(
    trans: *mut super::iter::btree_trans,
    path_idx: super::iter::btree_path_idx_t,
    level: usize,
    u64s_delta: i32,
    merge_count: *mut u64,
) -> i32 {
    /* fs/btree/interior.h:203 bch2_foreground_maybe_merge() wrapper：
     * needs_merge 门控不满足直接返回 0；merge_count 输出实际合并次数
     * （commit.c:1462 用其区分"未合并"与"合并后需 restart"） */
    let path = (*trans).paths.add(path_idx as usize);
    let b = (*path).l[level].b;
    if b.is_null() {
        return 0;
    }
    if !btree_node_needs_merge((*trans).c, b, u64s_delta) {
        return 0;
    }
    __bch2_foreground_maybe_merge(trans, path_idx, level, merge_count)
}

unsafe fn __bch2_foreground_maybe_merge(
    trans: *mut super::iter::btree_trans,
    path_idx: super::iter::btree_path_idx_t,
    level: usize,
    merge_count: *mut u64,
) -> i32 {
    /* interior.c:2907 __bch2_foreground_maybe_merge() 主体（域内简化见文件头注释） */
    let c = (*trans).c;
    let path = (*trans).paths.add(path_idx as usize);
    let btree_id = (*path).btree_id;
    let b = (*path).l[level].b;

    /* 边界门槛（interior.c:2938-2948） */
    if super::bkey::bpos_eq((*(*b).data).min_key, super::bkey::POS_MIN) {
        (*b).sib_u64s[0] = u16::MAX;
    }
    if super::bkey::bpos_eq((*(*b).data).max_key, super::bkey::SPOS_MAX) {
        (*b).sib_u64s[1] = u16::MAX;
    }

    /* srcs 收集：左到右 prev/自身/next（interior.c:2946-2956） */
    let mut srcs: [merge_node; 3] = [merge_node {
        b: core::ptr::null_mut(),
        path: 0,
        live_u64s: 0,
    }; 3];
    let mut srcs_nr = 0usize;

    let ret = bch2_btree_merge_push_pos(trans, level, path_idx, 0, &mut srcs, &mut srcs_nr);
    if ret != 0 {
        return ret;
    }
    srcs[srcs_nr] = merge_node {
        b,
        path: path_idx,
        live_u64s: (*b).nr.live_u64s as usize,
    };
    srcs_nr += 1;
    let ret = bch2_btree_merge_push_pos(trans, level, path_idx, 1, &mut srcs, &mut srcs_nr);
    if ret != 0 {
        merge_put_sibling_paths(trans, &srcs, srcs_nr, path_idx);
        return ret;
    }

    if srcs_nr == 1 {
        return 0;
    }

    /* 估算门控（interior.c:2966-2968，首次 compute 不填 dst） */
    let mut new_f = super::bkey::bkey_format::default();
    let mut nr_dsts = compute_merge(trans, c, b, &mut srcs, &mut srcs_nr, &mut new_f);
    if nr_dsts >= srcs_nr {
        merge_put_sibling_paths(trans, &srcs, srcs_nr, path_idx);
        return 0;
    }

    if srcs_nr == 1 {
        merge_put_sibling_paths(trans, &srcs, srcs_nr, path_idx);
        return 0;
    }

    /* 拓扑校验（interior.c:3057） */
    if !btree_merge_topology_check(c, &srcs, srcs_nr) {
        for i in 0..srcs_nr {
            merge_fail_reset_sib_u64s(c, b, &srcs[i]);
        }
        merge_put_sibling_paths(trans, &srcs, srcs_nr, path_idx);
        return -8;
    }

    /* 二次精确计算（interior.c:3060-3066）：subvol 无 deferred srcs，
     * 结果与首算一致，保留以对齐上游控制流 */
    nr_dsts = compute_merge(trans, c, b, &mut srcs, &mut srcs_nr, &mut new_f);
    if nr_dsts >= srcs_nr {
        merge_put_sibling_paths(trans, &srcs, srcs_nr, path_idx);
        return 0;
    }

    /* 路径锁升级覆盖父层（interior.c:3068 bch2_btree_update_start 内
     * bch2_btree_path_upgrade，commit.c:1432 同语义）：lock_write(parent)
     * 要求本线程持有 parent 的 intent 锁，父层仅 read 锁时先升级；
     * 失败返回 restart 类错误由调用方重试 */
    if super::iter::bch2_btree_path_upgrade(trans, path, (level + 2) as u8) != 0 {
        for i in 0..srcs_nr {
            merge_fail_reset_sib_u64s(c, b, &srcs[i]);
        }
        merge_put_sibling_paths(trans, &srcs, srcs_nr, path_idx);
        return -7;
    }

    /* 逐 src 写锁（interior.c:3114-3118） */
    for i in 0..srcs_nr {
        let src_b = srcs[i].b;
        if bch2_btree_node_lock_write(trans, src_b) != 0 {
            for j in 0..i {
                crate::lock::six::six_unlock_write(&(*srcs[j].b).c.lock);
            }
            for j in 0..srcs_nr {
                merge_fail_reset_sib_u64s(c, b, &srcs[j]);
            }
            merge_put_sibling_paths(trans, &srcs, srcs_nr, path_idx);
            return -10;
        }
    }

    /* 分配 dst（interior.c:3120-3124 bch2_btree_node_alloc）：
     * 容量取 srcs 中最大 byte_order（同层节点容量一致，门控保证
     * total_u64s 不超过 dst 容量） */
    let allocate_node = |level: u8, byte_order: usize| {
        let c = (*trans).c;
        let node = super::cache::bch2_btree_node_mem_alloc(trans, level != 0);
        assert!(!node.is_null());
        assert_eq!(crate::lock::six::six_lock_intent(&(*node).c.lock), 0);
        assert_eq!(crate::lock::six::six_lock_write(&(*node).c.lock), 0);
        (*node).c.level = level;
        (*node).c.btree_id = btree_id;
        (*node).version_ondisk = crate::sb::bcachefs_metadata_version_current;
        if (*node).byte_order < byte_order as u8 {
            let new_data = std::alloc::alloc_zeroed(std::alloc::Layout::from_size_align_unchecked(
                1usize << byte_order,
                core::mem::align_of::<u64>(),
            ))
            .cast::<u64>();
            let new_aux = std::alloc::alloc_zeroed(std::alloc::Layout::from_size_align_unchecked(
                super::types::__btree_aux_data_bytes(byte_order as u32),
                core::mem::align_of::<u64>(),
            ))
            .cast::<u64>();
            assert!(!new_data.is_null() && !new_aux.is_null());
            super::cache::bch2_btree_node_data_free(node);
            (*node).data = new_data.cast::<super::bset::btree_node>();
            (*node).aux_data = new_aux.cast();
            (*node).byte_order = byte_order as u8;
        }
        super::bset_build::bch2_bset_init_first(node, &mut (*(*node).data).keys);
        super::bset_build::bch2_btree_build_aux_trees(node);
        /* 同 split allocate_node：__bch2_btree_node_alloc 的扇区部分，
         * 新节点 key 必带磁盘 extent（写盘依赖）；失败释放节点返回
         * errno（对齐 reserve_get 失败路径 interior.c:714-721）。 */
        let sectors_ret = super::alloc::bch2_btree_node_alloc_sectors(c, node);
        if sectors_ret != 0 {
            crate::lock::six::six_unlock_write(&(*node).c.lock);
            crate::lock::six::six_unlock_intent(&(*node).c.lock);
            super::cache::bch2_btree_node_data_free(node);
            super::cache::bch2_btree_node_mem_free(c, node);
            return Err(sectors_ret);
        }
        Ok(node)
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
    let child_ptr = |child: *mut btree, out: *mut super::bset::bkey_i_btree_ptr_v2| {
        (*out).k = super::bkey::bkey {
            u64s: 10,
            format: super::bkey::KEY_FORMAT_CURRENT,
            type_: super::bset::KEY_TYPE_btree_ptr_v2,
            p: (*(*child).data).max_key,
            ..Default::default()
        };
        (*out).v = super::bset::bch_btree_ptr_v2 {
            mem_ptr: child as usize as u64,
            seq: (*(*child).data).keys.seq,
            min_key: (*(*child).data).min_key,
            ..Default::default()
        };
        /* 同 split child_ptr（718 注释）：merge dst 节点 key 继承
         * allocate_node 时 alloc_sectors 写入的磁盘 extent。 */
        let old_ptrs = super::bset::bch2_bkey_ptrs_c(super::bkey::bkey_s_c {
            k: &(*child).key.k,
            v: &(*child).key.v,
        });
        if !old_ptrs.start.is_null() && old_ptrs.start < old_ptrs.end {
            super::bset::bch2_bkey_append_ptr(c, out.cast(), (*old_ptrs.start).ptr);
        }
    };

    let mut dst_order = 0usize;
    for i in 0..srcs_nr {
        dst_order = dst_order.max((*srcs[i].b).byte_order as usize);
    }
    let dst_node = match allocate_node((*b).c.level, dst_order) {
        Ok(node) => node,
        Err(ret) => {
            merge_put_sibling_paths(trans, &srcs, srcs_nr, path_idx);
            return ret;
        }
    };

    /* N->1 打包（interior.c:3126-3141） */
    let mut max_seq = 0u64;
    for i in 0..srcs_nr {
        max_seq = max_seq.max((*(*srcs[i].b).data).keys.seq);
    }
    (*(*dst_node).data).keys.seq = max_seq + 1;
    btree_set_min(dst_node, (*(*srcs[0].b).data).min_key);
    btree_set_max(dst_node, (*(*srcs[srcs_nr - 1].b).data).max_key);
    (*(*dst_node).data).format = new_f;
    (*dst_node).format = new_f;
    (*dst_node).nr_key_bits = super::bkey::bkey_format_key_bits(&new_f) as u8;
    super::bkey::bch2_compute_bkey_unpack_consts(dst_node);
    for i in 0..srcs_nr {
        super::bset_build::bch2_btree_sort_into(c, dst_node, srcs[i].b);
    }
    /* 对齐 sort.c:498-501：dst 继承 srcs 各 bset 的最大 journal_seq，
     * 满足 write.c:470 二次写盘断言。 */
    let mut dst_seq = 0u64;
    for i in 0..srcs_nr {
        for set_idx in 0..(*srcs[i].b).nsets as usize {
            dst_seq = dst_seq.max(
                (*super::types::bset(srcs[i].b, (*srcs[i].b).set.as_ptr().add(set_idx)))
                    .journal_seq,
            );
        }
    }
    (*(*dst_node).data).keys.journal_seq = dst_seq;
    /* 打包容量诊断（interior.c:3144-3148 BUG_ON）：
     * compute_merge 的 format-aware 精确计算保证不溢出 */
    assert!(
        core::mem::size_of::<super::bset::btree_node>()
            + (*(*dst_node).data).keys.u64s as usize * 8
            < btree_buf_bytes(&*dst_node),
        "merge dst overflow"
    );
    assert_eq!(bch2_btree_node_check_topology(trans, dst_node), 0);
    btree_node_reset_sib_u64s(dst_node);

    /* parent 键更新（interior.c:3145-3175 的 parent_keys merge walk 对应）：
     * 删除每个 src 的旧键、插入 dst 新键。merge 删 N 插 1 净减，parent
     * 必有空间；insert_fits 检查与 split 同语义保留（约束 6）。 */
    let parent = (*path).l[level + 1].b;
    if bch2_btree_node_lock_write(trans, parent) != 0 {
        release_node(dst_node);
        for i in 0..srcs_nr {
            crate::lock::six::six_unlock_write(&(*srcs[i].b).c.lock);
        }
        for i in 0..srcs_nr {
            merge_fail_reset_sib_u64s(c, b, &srcs[i]);
        }
        merge_put_sibling_paths(trans, &srcs, srcs_nr, path_idx);
        return -10;
    }

    let last = super::types::bset_tree_last(parent);
    for i in (0..srcs_nr).rev() {
        let src_key_p = (*(*srcs[i].b).data).max_key;
        let mut parent_iter = super::types::btree_node_iter::default();
        super::node_iter::bch2_btree_node_iter_init(c, parent, &mut parent_iter, &src_key_p);
        let old = super::node_iter::bch2_btree_node_iter_peek(&mut parent_iter, parent);
        if old.is_null()
            || !super::bkey::bpos_eq(super::node_iter::bkey_unpack_pos(parent, old), src_key_p)
        {
            release_node(dst_node);
            for j in 0..srcs_nr {
                crate::lock::six::six_unlock_write(&(*srcs[j].b).c.lock);
            }
            for j in 0..srcs_nr {
                merge_fail_reset_sib_u64s(c, b, &srcs[j]);
            }
            merge_put_sibling_paths(trans, &srcs, srcs_nr, path_idx);
            crate::lock::six::six_unlock_write(&(*parent).c.lock);
            return -8;
        }
        let old_writeable = super::types::__btree_node_key_to_offset(parent, old)
            >= super::types::btree_bkey_first_offset(last);
        let old_set = super::types::bch2_bkey_to_bset_inlined(parent, old);
        let old_set_idx = old_set.offset_from((*parent).set.as_ptr()) as usize;
        super::bset_update::btree_keys_account_key(&mut (*parent).nr, old_set_idx, old, -1);
        let old_u64s = (*old).u64s as u32;
        (*old).type_ = 0;
        /* 对齐 commit.c:198-203 bch2_btree_bset_insert_key_inlined：
         * 置白的旧 key 若带 needs_whiteout（已写盘），其删除必须
         * push_whiteout 落盘，否则读回时旧 key 复活（T0210 ac2）。 */
        if (*old).format & 0x80 != 0 {
            bch2_push_whiteout(parent, src_key_p);
            (*old).format &= 0x7f;
        }
        if old_writeable {
            super::bset_update::bch2_bset_delete(parent, old, old_u64s);
        }
    }

    let mut dst_key = [0u64; 16];
    child_ptr(dst_node, dst_key.as_mut_ptr().cast());
    let dst_key = dst_key
        .as_mut_ptr()
        .cast::<super::bset::bkey_i_btree_ptr_v2>();
    /* 同 split（764 行）：dst_node.data.keys.seq 已在上面设为
     * max_seq + 1，节点自身 key（写盘 ptr / 读回 seq 校验）必须同步
     * 为 child_ptr 重建值（v.seq = data.keys.seq），否则读回校验
     * io.rs:596（node.keys.seq != key.v.seq）返回 -11（T0210 ac2）。 */
    super::bkey::bkey_copy(&mut (*dst_node).key, dst_key.cast());
    let mut insert_iter = super::types::btree_node_iter::default();
    super::node_iter::bch2_btree_node_iter_init(c, parent, &mut insert_iter, &(*dst_key).k.p);
    let where_ = super::node_iter::bch2_btree_node_iter_bset_pos(&mut insert_iter, parent, last);
    if (*trans).journal_replay_not_finished {
        let journal_keys = core::ptr::addr_of!((*c).journal_keys);
        let _overwrite_lock = (&(*journal_keys).overwrite_lock).lock().unwrap();
        crate::journal::bch2_journal_key_check_or_overwrite(
            c,
            (*parent).c.btree_id,
            (*parent).c.level,
            (*dst_key).k.p,
            false,
        );
    }
    super::bset_update::bch2_bset_insert(parent, where_, dst_key.cast(), 0);
    super::cache::bch2_btree_node_set_dirty(c, parent);

    for i in 0..srcs_nr {
        retire_node(srcs[i].b);
    }
    /* 对齐 bch2_btree_update_new_node（interior.c:1296-1303）：
     * dst 新节点 set_dirty 后创建完成即写盘。 */
    super::cache::bch2_btree_node_set_dirty(c, dst_node);
    super::io::bch2_btree_node_write_trans(
        trans,
        dst_node,
        crate::lock::six::six_lock_type::SIX_LOCK_write,
        0,
    );
    for i in 0..srcs_nr {
        crate::lock::six::six_unlock_write(&(*srcs[i].b).c.lock);
    }
    super::iter::bch2_trans_node_add(trans, dst_node);
    for i in 0..srcs_nr {
        super::iter::bch2_trans_node_verify_not_in_iters(trans, srcs[i].b);
    }
    merge_put_sibling_paths(trans, &srcs, srcs_nr, path_idx);
    crate::lock::six::six_unlock_write(&(*parent).c.lock);
    crate::lock::six::six_unlock_write(&(*dst_node).c.lock);
    crate::lock::six::six_unlock_intent(&(*dst_node).c.lock);
    /* interior.c:3168：合并成功计数 */
    if !merge_count.is_null() {
        *merge_count += 1;
    }
    0
}

pub(crate) unsafe fn bch2_btree_node_rewrite(
    trans: *mut super::iter::btree_trans,
    path_idx: super::iter::btree_path_idx_t,
) -> i32 {
    /* interior.c:3276 bch2_btree_node_rewrite()：为 path 指向的节点生成
     * 替换节点（格式重算 + seq+1 + 全键搬移），经 parent pivot 更新
     * （或 root 替换）提交，旧节点 retire。域内差异（D1/D2/D8）：
     * pending_interior 提交语义 = 同步内存修改 + dirty（merge/split
     * 已验证模式）；路径经 take_new_node 直接换新，不触发事务 restart。
     * hash 匹配（rewrite_key 的 -ENOENT 语义）由调用方在定位时处理。 */
    crate::rewrite_log_debug!("btree node rewrite begin path={path_idx}");
    let path = (*trans).paths.add(path_idx as usize);
    let b = (*path).l[(*path).level as usize].b;
    if b.is_null() || (*b).data.is_null() {
        crate::rewrite_log_error!("btree node rewrite rejected: no node at path");
        return -5;
    }
    let c = (*trans).c;
    let level = (*b).c.level;
    let btree_id = (*b).c.btree_id;

    /* update_start 的锁升级语义（interior.c:3068，同 merge 挂载点） */
    if super::iter::bch2_btree_path_upgrade(trans, path, (level as usize + 2) as u8) != 0 {
        return -7;
    }

    if bch2_btree_node_lock_write(trans, b) != 0 {
        return -10;
    }

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

    /* bch2_btree_node_alloc_replacement（interior.c:593-616） */
    let n = super::cache::bch2_btree_node_mem_alloc(trans, level != 0);
    if n.is_null() {
        crate::lock::six::six_unlock_write(&(*b).c.lock);
        return -12;
    }
    assert_eq!(crate::lock::six::six_lock_intent(&(*n).c.lock), 0);
    assert_eq!(crate::lock::six::six_lock_write(&(*n).c.lock), 0);
    (*n).c.level = level;
    (*n).c.btree_id = btree_id;
    (*n).version_ondisk = crate::sb::bcachefs_metadata_version_current;
    if (*n).byte_order < (*b).byte_order as u8 {
        let new_data = std::alloc::alloc_zeroed(std::alloc::Layout::from_size_align_unchecked(
            1usize << (*b).byte_order as usize,
            core::mem::align_of::<u64>(),
        ))
        .cast::<u64>();
        let new_aux = std::alloc::alloc_zeroed(std::alloc::Layout::from_size_align_unchecked(
            super::types::__btree_aux_data_bytes((*b).byte_order as u32),
            core::mem::align_of::<u64>(),
        ))
        .cast::<u64>();
        assert!(!new_data.is_null() && !new_aux.is_null());
        super::cache::bch2_btree_node_data_free(n);
        (*n).data = new_data.cast::<super::bset::btree_node>();
        (*n).aux_data = new_aux.cast();
        (*n).byte_order = (*b).byte_order;
    }
    super::bset_build::bch2_bset_init_first(n, &mut (*(*n).data).keys);

    /* 格式重算（interior.c:598-604）：calc_format → format_fits 回退
     * 旧格式（严格小于，interior.c:346-361/2843） */
    let mut state = super::bkey::bkey_format_state::default();
    super::bkey::bch2_bkey_format_init(&mut state);
    __bch2_btree_calc_format(&mut state, b);
    let mut format = super::bkey::bch2_bkey_format_done(&mut state);
    if core::mem::size_of::<super::bset::btree_node>()
        + btree_node_u64s_with_format(b, &(*b).format, &format) * 8
        >= btree_buf_bytes(&*b)
    {
        format = (*b).format;
    }
    (*(*n).data).format = format;
    (*n).format = format;
    (*n).nr_key_bits = super::bkey::bkey_format_key_bits(&format) as u8;
    super::bkey::bch2_compute_bkey_unpack_consts(n);

    /* interior.c:606 SET_BTREE_NODE_SEQ(旧+1) + min/max 继承 */
    (*(*n).data).keys.seq = (*(*b).data).keys.seq + 1;
    btree_set_min(n, (*(*b).data).min_key);
    btree_set_max(n, (*(*b).data).max_key);

    /* 全键搬移（interior.c:613 bch2_btree_sort_into）+ 容量诊断 +
     * 拓扑校验 + sib 重置 */
    super::bset_build::bch2_btree_sort_into(c, n, b);
    assert!(
        core::mem::size_of::<super::bset::btree_node>() + (*(*n).data).keys.u64s as usize * 8
            < btree_buf_bytes(&*n),
        "rewrite replacement overflow"
    );
    assert_eq!(bch2_btree_node_check_topology(trans, n), 0);
    btree_node_reset_sib_u64s(n);
    super::bset_build::bch2_btree_build_aux_trees(n);

    let child_ptr = |child: *mut btree, out: *mut super::bset::bkey_i_btree_ptr_v2| {
        (*out).k = super::bkey::bkey {
            u64s: 10,
            format: super::bkey::KEY_FORMAT_CURRENT,
            type_: super::bset::KEY_TYPE_btree_ptr_v2,
            p: (*(*child).data).max_key,
            ..Default::default()
        };
        (*out).v = super::bset::bch_btree_ptr_v2 {
            mem_ptr: child as usize as u64,
            seq: (*(*child).data).keys.seq,
            min_key: (*(*child).data).min_key,
            ..Default::default()
        };
        /* 上游 __bch2_btree_node_alloc（interior.c:515-518）新节点 key 经
         * bch2_alloc_sectors_append_ptrs 必带 extent（磁盘位置），set_root
         * 后 root 记录含 extent；域内差异（T0205 D 覆盖写原位置）：新键
         * 继承旧节点 b.key 的 extent（mem_ptr 键场景无 extent 则跳过）。
         * 缺此合并则重写后 slot.key 无磁盘位置，io 层重开 root_read 读盘
         * bch2_bkey_ptrs_c 空返回 -2（AC-5 验证测试暴露）。 */
        let old_ptrs = super::bset::bch2_bkey_ptrs_c(super::bkey::bkey_s_c {
            k: &(*b).key.k,
            v: &(*b).key.v,
        });
        if !old_ptrs.start.is_null() && old_ptrs.start < old_ptrs.end {
            super::bset::bch2_bkey_append_ptr(c, out.cast(), (*old_ptrs.start).ptr);
        }
    };
    let mut n_key_buf = [0u64; 16];
    child_ptr(n, n_key_buf.as_mut_ptr().cast());
    let n_key = n_key_buf.as_mut_ptr().cast::<super::bkey::bkey_i>();
    super::bkey::bkey_copy(&mut (*n).key, n_key);

    /* 路径换新（interior.c:3299-3302 bch2_path_get_unlocked_mut +
     * btree_path_take_new_node） */
    let new_path =
        super::iter::bch2_path_get_unlocked_mut(trans, btree_id, level, (*n).key.k.p, false);
    super::iter::btree_path_take_new_node(trans, (*trans).paths.add(new_path as usize), n);

    let parent_level = level as usize + 1;
    let parent = if parent_level < super::bset::BTREE_MAX_DEPTH as usize {
        (*path).l[parent_level].b
    } else {
        core::ptr::null_mut()
    };
    if !parent.is_null() {
        /* parent 分支（interior.c:3307-3310 bch2_btree_insert_node）：
         * 定位旧键（b 的 max_key）→ 删除 → 插入 n 的新键（同位置
         * 替换，同 merge 的 parent_keys 模式） */
        if bch2_btree_node_lock_write(trans, parent) != 0 {
            if new_path != 0 {
                super::iter::bch2_path_put(trans, new_path, true);
            }
            release_node(n);
            crate::lock::six::six_unlock_write(&(*b).c.lock);
            return -10;
        }
        let src_key_p = (*(*b).data).max_key;
        let last = super::types::bset_tree_last(parent);
        let mut parent_iter = super::types::btree_node_iter::default();
        super::node_iter::bch2_btree_node_iter_init(c, parent, &mut parent_iter, &src_key_p);
        let old = super::node_iter::bch2_btree_node_iter_peek(&mut parent_iter, parent);
        if old.is_null()
            || !super::bkey::bpos_eq(super::node_iter::bkey_unpack_pos(parent, old), src_key_p)
        {
            if new_path != 0 {
                super::iter::bch2_path_put(trans, new_path, true);
            }
            release_node(n);
            crate::lock::six::six_unlock_write(&(*b).c.lock);
            crate::lock::six::six_unlock_write(&(*parent).c.lock);
            return -8;
        }
        let old_writeable = super::types::__btree_node_key_to_offset(parent, old)
            >= super::types::btree_bkey_first_offset(last);
        let old_set = super::types::bch2_bkey_to_bset_inlined(parent, old);
        let old_set_idx = old_set.offset_from((*parent).set.as_ptr()) as usize;
        super::bset_update::btree_keys_account_key(&mut (*parent).nr, old_set_idx, old, -1);
        let old_u64s = (*old).u64s as u32;
        (*old).type_ = 0;
        if old_writeable {
            super::bset_update::bch2_bset_delete(parent, old, old_u64s);
        }
        let mut insert_iter = super::types::btree_node_iter::default();
        super::node_iter::bch2_btree_node_iter_init(c, parent, &mut insert_iter, &(*n_key).k.p);
        let where_ =
            super::node_iter::bch2_btree_node_iter_bset_pos(&mut insert_iter, parent, last);
        if (*trans).journal_replay_not_finished {
            let journal_keys = core::ptr::addr_of!((*c).journal_keys);
            let _overwrite_lock = (&(*journal_keys).overwrite_lock).lock().unwrap();
            crate::journal::bch2_journal_key_check_or_overwrite(
                c,
                (*parent).c.btree_id,
                (*parent).c.level,
                (*n_key).k.p,
                false,
            );
        }
        super::bset_update::bch2_bset_insert(parent, where_, n_key, 0);
        super::cache::bch2_btree_node_set_dirty(c, parent);
    } else {
        /* root 分支（interior.c:3310-3312 bch2_btree_set_root）：
         * root.key 更新为自身指针 + set_root_for_read（split root
         * 分支模式 interior.rs:800-850） */
        let mut root_ptr_buf = [0u64; 16];
        child_ptr(n, root_ptr_buf.as_mut_ptr().cast());
        super::bkey::bkey_copy(
            &mut (*n).key,
            root_ptr_buf.as_mut_ptr().cast::<super::bkey::bkey_i>(),
        );
        if cache_initialized {
            let _ = super::cache::bch2_btree_node_transition_state(
                cache_ptr,
                n,
                super::types::btree_node_cache_state::BTREE_NODE_CACHE_CLEAN,
            );
            super::cache::bch2_btree_node_set_dirty(c, n);
        }
        bch2_btree_set_root_for_read(c, n);
    }

    /* will_free_node + free_inmem（旧节点 retire，interior.c:3314-3320） */
    retire_node(b);
    crate::lock::six::six_unlock_write(&(*b).c.lock);
    super::iter::bch2_trans_node_add(trans, n);
    super::iter::bch2_trans_node_verify_not_in_iters(trans, b);
    if new_path != 0 {
        super::iter::bch2_path_put(trans, new_path, true);
    }
    if !parent.is_null() {
        crate::lock::six::six_unlock_write(&(*parent).c.lock);
    }
    crate::lock::six::six_unlock_write(&(*n).c.lock);
    crate::lock::six::six_unlock_intent(&(*n).c.lock);
    0
}

pub(crate) unsafe fn bch2_btree_node_rewrite_key(
    c: *mut super::types::bch_fs,
    btree: u8,
    level: u8,
    key: *const super::bkey::bkey_i,
) -> i32 {
    /* interior.c:3345 bch2_btree_node_rewrite_key() 语义：CLASS iter
     * (trans, btree, k->k.p, BTREE_MAX_DEPTH, level, 0) →
     * bch2_btree_iter_peek_node → 仅当定位节点 b 与给定键 k 的指针
     * 键 hash 匹配才 bch2_btree_node_rewrite，否则 -ENOENT。
     * 上游 async work（interior.c:3406，read.c:968 读完成触发）经
     * bch2_trans_do 新建独立 trans 调用；域内同步触发（AC-3，
     * io.rs 两个读完成点）同样自建 trans，与调用方 trans 解耦。 */
    if c.is_null() || key.is_null() {
        return -2;
    }
    let mut trans = super::iter::btree_trans::default();
    super::iter::bch2_trans_init(&mut trans, c);
    super::iter::bch2_trans_begin(&mut trans);

    let mut iter = super::iter::btree_iter::default();
    super::iter::bch2_trans_iter_init_common(
        &mut trans,
        &mut iter,
        btree,
        (*key).k.p,
        super::bset::BTREE_MAX_DEPTH,
        level,
        super::iter::BTREE_ITER_intent,
    );
    let b = super::iter::bch2_btree_iter_peek_node(&mut iter);
    let found = !b.is_null()
        && !(*b).data.is_null()
        && super::cache::btree_ptr_hash_val(&(*b).key) == super::cache::btree_ptr_hash_val(key);
    let ret = if found {
        bch2_btree_node_rewrite(&mut trans, iter.path)
    } else {
        -2
    };
    super::iter::bch2_trans_iter_exit(&mut iter);
    super::iter::bch2_trans_put(&mut trans);
    ret
}

/* read.c:968 读完成（btree_node_need_rewrite）→ 上游经 async_btree_rewrite
 * work（interior.c:3406）排队执行。域内差异（AC-1 D1 同步触发）：无
 * async worker，读完成点仅把节点入队 btree.node_rewrites（对齐上游
 * a->key 的 bch2_bkey_buf_copy 语义，拷贝 key 而非持有节点引用，避免
 * 节点 retire 后悬垂）；执行推迟到无锁时机（root_read 末尾 / engine
 * 操作边界）的 bch2_do_pending_node_rewrites。 */
pub(crate) unsafe fn bch2_btree_node_need_rewrite_add(
    c: *mut super::types::bch_fs,
    b: *mut super::types::btree,
) {
    if c.is_null() || b.is_null() || (*b).data.is_null() {
        return;
    }
    if !super::types::btree_node_need_rewrite(b) {
        return;
    }
    let key = &(*b).key;
    let mut words = vec![0u64; key.k.u64s as usize];
    core::ptr::copy_nonoverlapping(
        (key as *const super::bkey::bkey_i).cast::<u64>(),
        words.as_mut_ptr(),
        key.k.u64s as usize,
    );
    (*c).btree
        .node_rewrites
        .lock()
        .unwrap()
        .push(super::types::btree_node_rewrite_item {
            btree_id: (*b).c.btree_id,
            level: (*b).c.level,
            key: words,
        });
}

/* interior.c:3462 bch2_do_pending_node_rewrites() 语义：把待重写列表
 * 交给执行者。上游移入 list 后 queue_work（异步、不持锁）；域内同步
 * drain：逐项 bch2_btree_node_rewrite_key，忽略 -2（-ENOENT，节点
 * 已被定位不到）与 -5（no_btree_node_nofill，对齐上游
 * no_btree_node_nofill 忽略），其余错误记日志（对齐上游对其它错误
 * 仅报错不中止）。调用方必须处于无节点锁上下文（否则与路径锁
 * 互斥死锁，见 AC-3 测试）。 */
pub(crate) unsafe fn bch2_do_pending_node_rewrites(c: *mut super::types::bch_fs) {
    if c.is_null() {
        return;
    }
    let items = core::mem::take(&mut *(*c).btree.node_rewrites.lock().unwrap());
    for mut item in items {
        let ret = bch2_btree_node_rewrite_key(c, item.btree_id, item.level, {
            let key = item.key.as_mut_ptr().cast::<super::bkey::bkey_i>();
            key
        });
        if ret != 0 && ret != -2 && ret != -5 {
            crate::rewrite_log_warn!(
                "pending node rewrite failed (btree {}, level {}): {}",
                item.btree_id,
                item.level,
                ret
            );
        }
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
            /* merge 与 split 同属事务 restart 边界（返回 -4）：重试循环
             * 须重建 iter 并重新 update（bch2_trans_begin 已清 update 列表） */
            let mut retry_ret = 0;
            loop {
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
                retry_ret = bch2_trans_commit(&mut retry_trans);
                if retry_ret == 0 {
                    break;
                }
                assert_eq!(retry_ret, -4);
                bch2_trans_begin(&mut retry_trans);
                bch2_trans_iter_exit(&mut retry);
            }
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
                    let mut sret;
                    loop {
                        sret = bch2_trans_commit(&mut split_retry_trans);
                        if sret == 0 {
                            break;
                        }
                        assert_eq!(sret, -4);
                        bch2_trans_begin(&mut split_retry_trans);
                        bch2_trans_iter_exit(&mut split_retry);
                        bch2_trans_iter_init(
                            &mut split_retry_trans,
                            &mut split_retry,
                            0,
                            key.k.p,
                            BTREE_ITER_intent,
                        );
                        assert!(bch2_btree_iter_peek(&mut split_retry).k.is_null());
                        assert_eq!(
                            bch2_trans_update(
                                &mut split_retry_trans,
                                &mut split_retry,
                                &mut key,
                                0
                            ),
                            0
                        );
                    }
                    bch2_trans_iter_exit(&mut split_retry);
                } else {
                    assert_eq!(ret, 0);
                }
            }

            let root = crate::btree::types::bch2_btree_id_root_b(&c, 0);
            /* T0204：merge 合法参与分裂节奏（offset 14 的 restart 由 merge
             * 触发，节点合并后分裂点整体提前），序列反映 merge 后的真实行为 */
            assert_eq!(restart_offsets, [14, 22, 30, 38, 46, 54, 62]);
            assert_eq!((*root).c.level, 2);
            /* T0204：merge 把三个子树合并为两个（root 键数 3→2），
             * root 内容断言随之更新 */
            assert_eq!((*root).nr.live_u64s, 20);
            assert_eq!((*root).nr.packed_keys, 2);
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

    #[test]
    fn multi_level_split_preserves_parent_pivot_invariants() {
        /* 多级分裂 pivot/边界不变量（T0168 P1 interior 对齐）：leaf 分裂
         * 后 parent 放不下继续分裂 parent（interior.rs split loop 对应
         * bcachefs btree_split 递归 + bch2_btree_insert_node 的 split:
         * 分支，interior.c:1962/2191/2271），直至新建 root（对应
         * __btree_root_alloc + bch2_btree_set_root，interior.c:2095）。
         * 递归验证：节点内 key 严格递增；child 指针 key.p == child
         * max_key、相邻 child 区间连续无空洞（后继相接）、child 区间
         * 覆盖 parent 的 [min_key, max_key]；叶子收集 key 全集。 */
        use crate::btree::bkey::{
            bkey, bkey_format_key_bits, bkeyp_key_u64s, bpos, bpos_cmp, bpos_eq, bpos_lt,
            bpos_successor, BKEY_FORMAT_CURRENT, BKEY_U64S, KEY_FORMAT_CURRENT, POS_MIN, SPOS,
            SPOS_MAX,
        };
        use crate::btree::iter::{
            bch2_btree_iter_peek, bch2_trans_begin, bch2_trans_init, bch2_trans_iter_exit,
            bch2_trans_iter_init, btree_iter, btree_trans, BTREE_ITER_intent,
        };
        use crate::btree::types::{bch2_btree_id_root_set, bch_fs, btree};
        use crate::btree::update::{bch2_trans_commit, bch2_trans_update};

        unsafe fn verify_subtree(node: *mut btree, out: &mut Vec<u64>) {
            let mut iter = crate::btree::types::btree_node_iter::default();
            crate::btree::node_iter::bch2_btree_node_iter_init_from_start(&mut iter, node);
            let mut prev = None;
            let mut child_prev_max = None;
            let mut first_child_min = None;
            let mut last_child_max = None;
            loop {
                let key = crate::btree::node_iter::bch2_btree_node_iter_peek(&mut iter, node);
                if key.is_null() {
                    break;
                }
                let pos = crate::btree::node_iter::bkey_unpack_pos(node, key);
                if let Some(p) = prev {
                    assert!(bpos_lt(p, pos), "keys not strictly increasing");
                }
                prev = Some(pos);
                if (*node).c.level > 0 {
                    let key_u64s = bkeyp_key_u64s(&(*node).format, &*key);
                    let child = *key.cast::<u64>().add(key_u64s as usize) as usize as *mut btree;
                    assert!(!child.is_null());
                    assert_eq!((*child).c.level, (*node).c.level - 1);
                    assert!(
                        bpos_eq((*(*child).data).max_key, pos),
                        "child pointer key.p != child max_key"
                    );
                    let child_min = (*(*child).data).min_key;
                    let child_max = (*(*child).data).max_key;
                    if first_child_min.is_none() {
                        first_child_min = Some(child_min);
                    }
                    last_child_max = Some(child_max);
                    if let Some(prev_max) = child_prev_max {
                        assert!(
                            bpos_eq(bpos_successor(prev_max), child_min),
                            "adjacent child key ranges not contiguous"
                        );
                    }
                    child_prev_max = Some(child_max);
                    verify_subtree(child, out);
                } else {
                    out.push(pos.offset);
                }
                crate::btree::node_iter::bch2_btree_node_iter_advance(&mut iter, node);
            }
            if (*node).c.level > 0 {
                assert!(
                    bpos_eq(first_child_min.unwrap(), (*(*node).data).min_key),
                    "first child min_key != node min_key"
                );
                assert!(
                    bpos_eq(last_child_max.unwrap(), (*(*node).data).max_key),
                    "last child max_key != node max_key"
                );
            }
        }

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

            /* 连续插入直至 root 深度 >= 2（触发 leaf → parent → root
             * 多级级联分裂；200 键 >> 512B 节点容量，同
             * full_root_leaf_splits_grows_root_and_retries_insert）。 */
            let mut restart_offsets = Vec::new();
            for offset in 9..=208u64 {
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
                loop {
                    let mut trans = btree_trans::default();
                    bch2_trans_init(&mut trans, &mut c);
                    let mut iter = btree_iter::default();
                    bch2_trans_iter_init(&mut trans, &mut iter, 0, key.k.p, BTREE_ITER_intent);
                    assert!(bch2_btree_iter_peek(&mut iter).k.is_null());
                    assert_eq!(bch2_trans_update(&mut trans, &mut iter, &mut key, 0), 0);
                    let ret = bch2_trans_commit(&mut trans);
                    bch2_trans_iter_exit(&mut iter);
                    if ret == -4 {
                        bch2_trans_begin(&mut trans);
                        restart_offsets.push(offset);
                        continue;
                    }
                    assert_eq!(ret, 0, "insert offset={offset}");
                    break;
                }
            }

            let root = crate::btree::types::bch2_btree_id_root_b(&c, 0);
            assert!(
                (*root).c.level >= 2,
                "expected multi-level split, root level={}",
                (*root).c.level
            );
            assert!(
                !restart_offsets.is_empty(),
                "expected split restarts during multi-level growth"
            );

            let mut seen = Vec::new();
            verify_subtree(root, &mut seen);
            assert_eq!(seen, (1..=208u64).collect::<Vec<_>>());
            crate::sb::io::bch2_free_super(&mut c.disk_sb);
        }
    }
}
