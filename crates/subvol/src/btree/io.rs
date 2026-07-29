use super::bkey::{bkey_p_next, bkeyp_key_u64s, bkeyp_u64s_valid, bpos_cmp, bpos_gt, bpos_lt};
use super::bset::{
    bch2_bkey_ptrs_c, bkey_i_to_btree_ptr_v2, btree_node, btree_node_entry, KEY_TYPE_btree_ptr_v2,
    BCH_EXTENT_PTR_DEV, BCH_EXTENT_PTR_OFFSET, BSET_BIG_ENDIAN, BSET_CSUM_TYPE, BSET_OFFSET,
    BSET_SEPARATE_WHITEOUTS, BTREE_NODE_ID, BTREE_NODE_LEVEL, SET_BSET_BIG_ENDIAN,
    SET_BSET_CSUM_TYPE, SET_BSET_OFFSET, SET_BSET_SEPARATE_WHITEOUTS, SET_BTREE_NODE_ID,
    SET_BTREE_NODE_LEVEL,
};
use super::types::{
    bset, bset_tree, btree, btree_bkey_first, btree_bkey_last, BSET_NO_AUX_TREE_VAL,
};
use crate::checksum::{bch2_checksum, BCH_CSUM_xxhash, BCH_CSUM_NR};
use crate::lock::six::{
    six_lock_downgrade, six_lock_intent, six_lock_read, six_lock_tryupgrade, six_lock_write,
    six_trylock_write, six_unlock_intent, six_unlock_read, six_unlock_write,
};
use crate::sb::{bcachefs_metadata_version_current, bch_sb_handle};

pub const BSET_MAGIC: u64 = 0x9013_5c78_b99e_07f5;
pub const BTREE_WRITE_init_next_bset: u32 = 1;
pub const BTREE_WRITE_cache_reclaim: u32 = 2;
pub const BTREE_WRITE_initial: u32 = 0;
pub const BTREE_WRITE_journal_reclaim: u32 = 3;
pub const BTREE_WRITE_interior: u32 = 4;
pub const BTREE_WRITE_TYPE_MASK: u32 = 7;
pub const BTREE_WRITE_TYPE_BITS: u32 = 3;
pub const BTREE_WRITE_only_if_need: u32 = 1 << 3;
pub const BTREE_WRITE_already_started: u32 = 1 << 4;
pub use super::types::{
    btree_node_just_written, btree_node_write_in_flight, BTREE_NODE_dirty, BTREE_NODE_just_written,
    BTREE_NODE_need_write, BTREE_NODE_write_idx, BTREE_NODE_write_in_flight,
    BTREE_NODE_write_in_flight_inner,
};

pub unsafe fn bch2_btree_node_io_unlock(b: *mut btree) {
    assert!(!b.is_null());
    assert!(btree_node_write_in_flight(b));
    super::types::clear_btree_node_write_in_flight_inner(b);
    super::types::clear_btree_node_write_in_flight(b);
}

pub unsafe fn bch2_btree_node_io_lock(b: *mut btree) {
    assert!(!b.is_null());
    while btree_node_write_in_flight(b) {
        std::thread::yield_now();
    }
    super::types::set_btree_node_write_in_flight(b);
}

pub unsafe fn bch2_btree_node_wait_on_read(
    _trans: *mut super::iter::btree_trans,
    b: *mut btree,
) {
    assert!(!b.is_null());
    while super::types::btree_node_read_in_flight(b) {
        std::thread::yield_now();
    }
}

pub unsafe fn bch2_btree_node_wait_on_write(
    _trans: *mut super::iter::btree_trans,
    b: *mut btree,
) {
    assert!(!b.is_null());
    while btree_node_write_in_flight(b) {
        std::thread::yield_now();
    }
}

pub unsafe fn bch2_btree_post_write_cleanup(c: *mut super::types::bch_fs, b: *mut btree) -> bool {
    if !btree_node_just_written(b) {
        return false;
    }
    assert_eq!((*b).whiteout_u64s, 0);
    (*b).flags &= !(1usize << BTREE_NODE_just_written);

    let invalidated_iter = if (*b).nsets > 1 {
        super::bset_build::bch2_btree_node_sort(c, b, 0, (*b).nsets as usize);
        true
    } else {
        super::bset_build::bch2_drop_whiteouts(b, super::bset_build::compact_mode::COMPACT_ALL)
    };
    for idx in 0..(*b).nsets as usize {
        super::bset_build::bch2_set_bset_needs_whiteout(bset(b, (*b).set.as_ptr().add(idx)), 1);
    }
    assert!(
        btree_bkey_last(b, super::types::bset_tree_last(b)).cast::<u8>()
            <= (*b).data.cast::<u8>().add((*b).written as usize * 512)
    );
    let bne = super::interior::want_new_bset(c, b);
    if !bne.is_null() {
        super::bset_build::bch2_bset_init_next(b, bne);
    }
    super::bset_build::bch2_btree_build_aux_trees(b);
    invalidated_iter
}

pub unsafe fn bch2_btree_node_write_trans(
    trans: *mut super::iter::btree_trans,
    b: *mut btree,
    lock_type_held: crate::lock::six::six_lock_type,
    flags: u32,
) {
    let already_started = flags & BTREE_WRITE_already_started != 0;
    if !already_started
        && ((*b).flags & (1usize << BTREE_NODE_dirty) == 0
            || btree_node_write_in_flight(b)
            || (flags & BTREE_WRITE_only_if_need != 0
                && !super::types::btree_node_need_write(b))
            || super::types::btree_node_never_write(b)
            || super::types::btree_node_write_blocked(b)
            || ((*b).written != 0 && super::types::btree_node_will_make_reachable(b)))
    {
        return;
    }
    if already_started && !btree_node_write_in_flight(b) {
        return;
    }

    if !already_started {
        (*b).flags &= !(1usize << BTREE_NODE_dirty);
        (*b).flags &= !(1usize << BTREE_NODE_need_write);
        (*b).flags |= 1usize << BTREE_NODE_write_in_flight;
        (*b).flags |= 1usize << BTREE_NODE_write_in_flight_inner;
        (*b).flags |= 1usize << BTREE_NODE_just_written;
        (*b).flags ^= 1usize << BTREE_NODE_write_idx;
    }

    let cache = &(*(*trans).c).btree.cache;
    if !already_started {
        cache
            .nr_in_flight
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        cache
            .nr_in_flight_inner
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    }

    let ret = __bch2_btree_node_write(&mut (*(*trans).c).disk_sb, b);
    (*b).flags &= !(1usize << BTREE_NODE_write_in_flight_inner);
    cache
        .nr_in_flight_inner
        .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    (*b).flags &= !(1usize << BTREE_NODE_write_in_flight);
    cache
        .nr_in_flight
        .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    if ret != 0 {
        crate::rewrite_log_error!("btree node write failed ret={ret}");
        (*b).flags &= !(1usize << BTREE_NODE_just_written);
        (*b).flags |= 1usize << BTREE_NODE_dirty;
        return;
    }
    crate::rewrite_log_debug!(
        "btree node write complete level={} seq={}",
        (*b).c.level,
        (*(*b).data).keys.seq
    );
    let w = super::types::btree_prev_write(b);
    crate::journal::bch2_journal_pin_drop(&(*(*trans).c).journal, &mut (*w).journal);
    if lock_type_held == crate::lock::six::six_lock_type::SIX_LOCK_write {
        bch2_btree_post_write_cleanup((*trans).c, b);
    } else if lock_type_held == crate::lock::six::six_lock_type::SIX_LOCK_intent {
        if btree_node_just_written(b) && six_trylock_write(&(*b).c.lock) {
            bch2_btree_post_write_cleanup((*trans).c, b);
            six_unlock_write(&(*b).c.lock);
        }
    } else if lock_type_held == crate::lock::six::six_lock_type::SIX_LOCK_read
        && six_lock_tryupgrade(&(*b).c.lock)
    {
        if btree_node_just_written(b) && six_trylock_write(&(*b).c.lock) {
            bch2_btree_post_write_cleanup((*trans).c, b);
            six_unlock_write(&(*b).c.lock);
        }
        six_lock_downgrade(&(*b).c.lock);
    }
    super::cache::bch2_btree_node_write_done_clean((*trans).c, b);
}

pub unsafe fn bch2_btree_flush_all_reads(c: *mut super::types::bch_fs) -> bool {
    if c.is_null() || !(*c).btree.cache.table_init_done {
        return false;
    }
    let mut ret = false;
    loop {
        let waiting = {
            let _cache_lock = (*c).btree.cache.lock.lock().unwrap();
            let list_offset = core::mem::offset_of!(btree, list);
            let mut found = false;
            for head in [
                &mut (*c).btree.cache.live[0].clean as *mut super::types::list_head,
                &mut (*c).btree.cache.live[0].dirty as *mut super::types::list_head,
                &mut (*c).btree.cache.live[1].clean as *mut super::types::list_head,
                &mut (*c).btree.cache.live[1].dirty as *mut super::types::list_head,
                &mut (*c).btree.cache.freeable as *mut super::types::list_head,
            ] {
                let mut pos = (*head).next;
                while pos != head {
                    let node = pos.cast::<u8>().sub(list_offset).cast::<btree>();
                    if super::types::btree_node_read_in_flight(node) {
                        found = true;
                        break;
                    }
                    pos = (*pos).next;
                }
                if found {
                    break;
                }
            }
            found
        };
        if !waiting {
            break;
        }
        ret = true;
        std::thread::yield_now();
    }
    ret
}

pub unsafe fn bch2_btree_flush_all_writes(c: *mut super::types::bch_fs) -> bool {
    if c.is_null() || !(*c).btree.cache.table_init_done {
        return false;
    }
    let mut ret = false;
    loop {
        let waiting = {
            let _cache_lock = (*c).btree.cache.lock.lock().unwrap();
            let list_offset = core::mem::offset_of!(btree, list);
            let mut found = false;
            for head in [
                &mut (*c).btree.cache.live[0].clean as *mut super::types::list_head,
                &mut (*c).btree.cache.live[0].dirty as *mut super::types::list_head,
                &mut (*c).btree.cache.live[1].clean as *mut super::types::list_head,
                &mut (*c).btree.cache.live[1].dirty as *mut super::types::list_head,
                &mut (*c).btree.cache.freeable as *mut super::types::list_head,
            ] {
                let mut pos = (*head).next;
                while pos != head {
                    let node = pos.cast::<u8>().sub(list_offset).cast::<btree>();
                    if super::types::btree_node_write_in_flight(node) {
                        found = true;
                        break;
                    }
                    pos = (*pos).next;
                }
                if found {
                    break;
                }
            }
            found
        };
        if !waiting {
            break;
        }
        ret = true;
        std::thread::yield_now();
    }
    ret
}

pub unsafe fn bch2_btree_cancel_all_writes(c: *mut super::types::bch_fs) {
    if c.is_null() || !(*c).btree.cache.table_init_done {
        return;
    }
    {
        let _cache_lock = (*c).btree.cache.lock.lock().unwrap();
        let list_offset = core::mem::offset_of!(btree, list);
        for head in [
            &mut (*c).btree.cache.live[0].dirty as *mut super::types::list_head,
            &mut (*c).btree.cache.live[1].dirty as *mut super::types::list_head,
        ] {
            let mut pos = (*head).next;
            while pos != head {
                let next = (*pos).next;
                let node = pos.cast::<u8>().sub(list_offset).cast::<btree>();
                super::types::clear_btree_node_dirty(node);
                super::cache::bch2_btree_node_transition_state_locked(
                    &mut (*c).btree.cache,
                    node,
                    super::cache::btree_node_live_state(node),
                );
                pos = next;
            }
        }
    }
    bch2_btree_flush_all_writes(c);
}

pub unsafe fn __bch2_btree_node_write(sb: *mut bch_sb_handle, b: *mut btree) -> i32 {
    use std::os::unix::fs::FileExt;

    if sb.is_null()
        || (*sb).sb.is_null()
        || (*sb).s_bdev_file.is_null()
        || b.is_null()
        || (*b).data.is_null()
        || (*b).nsets == 0
        || (*b).byte_order < 9
    {
        return -1;
    }

    let ptrs = bch2_bkey_ptrs_c(super::bkey::bkey_s_c {
        k: &(*b).key.k,
        v: &(*b).key.v,
    });
    if ptrs.start.is_null() || ptrs.start >= ptrs.end {
        return -2;
    }
    let ptr = (*ptrs.start).ptr;
    if BCH_EXTENT_PTR_DEV(&ptr) != (*(*sb).sb).dev_idx as u64 {
        return -3;
    }

    let node_bytes = 1usize << (*b).byte_order;
    if (*(*b).data).keys.seq == 0 {
        return -5;
    }
    if (*b).nsets as usize > super::types::MAX_BSETS {
        return -7;
    }

    super::bset_build::bch2_sort_whiteouts(core::ptr::null_mut(), b);

    let first = (*b).written == 0;
    let write_byte = (*b).written as usize * 512;
    let block_sectors = ((*(*sb).sb).block_size as usize).max(1);
    if (*b).written as usize >= node_bytes / 512 || (*b).written as usize % block_sectors != 0 {
        return -4;
    }
    let header_bytes = if first {
        core::mem::size_of::<btree_node>()
    } else {
        core::mem::size_of::<btree_node_entry>()
    };
    let mut sort = super::bset_build::sort_iter_stack::default();
    super::bset_build::sort_iter_stack_init(&mut sort, b);
    let mut bytes = header_bytes + (*b).whiteout_u64s as usize * 8;
    let mut journal_seq = 0u64;
    for set_idx in 0..(*b).nsets as usize {
        let tree = (*b).set.as_ptr().add(set_idx);
        let disk_set = bset(b, tree);
        if super::interior::bset_written(b, disk_set) {
            continue;
        }
        bytes += (*disk_set).u64s as usize * 8;
        super::bset_build::sort_iter_add(
            &mut sort.iter,
            btree_bkey_first(b, tree),
            btree_bkey_last(b, tree),
        );
        journal_seq = journal_seq.max((*disk_set).journal_seq);
    }
    assert!(first || journal_seq != 0);

    bytes += 8;
    let block_bytes = ((*(*sb).sb).block_size as usize).max(1) * 512;
    bytes = bytes.next_multiple_of(block_bytes);

    let mut data = vec![0u8; bytes];
    let (set, checksum) = if first {
        let node = data.as_mut_ptr().cast::<btree_node>();
        core::ptr::copy_nonoverlapping(
            (*b).data.cast::<u8>(),
            node.cast::<u8>(),
            core::mem::size_of::<btree_node>(),
        );
        let uuid_lo = u64::from_le_bytes((&(*(*sb).sb).uuid)[..8].try_into().unwrap());
        (*node).magic = uuid_lo ^ BSET_MAGIC;
        SET_BTREE_NODE_ID(&mut *node, (*b).c.btree_id as u64);
        SET_BTREE_NODE_LEVEL(&mut *node, (*b).c.level as u64);
        (*node).format = (*b).format;
        (
            &mut (*node).keys as *mut super::bset::bset,
            &mut (*node).csum,
        )
    } else {
        let entry = data.as_mut_ptr().cast::<btree_node_entry>();
        (*entry).keys = (*(*b).data).keys;
        (
            &mut (*entry).keys as *mut super::bset::bset,
            &mut (*entry).csum,
        )
    };
    (*set).journal_seq = journal_seq;
    (*set).u64s = 0;
    super::bset_build::sort_iter_add(
        &mut sort.iter,
        super::interior::unwritten_whiteouts_start(b),
        super::interior::unwritten_whiteouts_end(b),
    );
    SET_BSET_SEPARATE_WHITEOUTS(&mut *set, 0);
    (*set).u64s = super::bset_build::bch2_sort_keys_keep_unwritten_whiteouts(
        set.cast::<u64>().add(3).cast(),
        &mut sort.iter,
    ) as u16;
    (*b).whiteout_u64s = 0;
    if first {
        assert_eq!((*set).u64s, (*(*b).data).keys.u64s);
    }
    super::bset_build::bch2_set_bset_needs_whiteout(set, 0);
    if !first && (*set).u64s == 0 {
        return 0;
    }

    let bytes_to_write = header_bytes + (*set).u64s as usize * 8;
    let write_bytes = bytes_to_write.next_multiple_of(block_bytes);
    let sectors_to_write = write_bytes / 512;
    if write_byte + write_bytes > node_bytes || sectors_to_write > u16::MAX as usize {
        return -4;
    }
    data.truncate(write_bytes);

    (*set).version = bcachefs_metadata_version_current;
    SET_BSET_OFFSET(&mut *set, (*b).written as u32);
    SET_BSET_BIG_ENDIAN(&mut *set, 0);
    SET_BSET_CSUM_TYPE(&mut *set, BCH_CSUM_xxhash);
    let checksum_data = core::slice::from_raw_parts(data.as_ptr().add(16), bytes_to_write - 16);
    *checksum = bch2_checksum(BCH_CSUM_xxhash, checksum_data);

    let file = &*(*sb).s_bdev_file.cast::<std::fs::File>();
    let disk_offset = (BCH_EXTENT_PTR_OFFSET(&ptr) + (*b).written as u64) * 512;
    if disk_offset + data.len() as u64 > file.metadata().map(|m| m.len()).unwrap_or(0) {
        return -6;
    }
    let mut written = 0usize;
    while written < data.len() {
        match file.write_at(&data[written..], disk_offset + written as u64) {
            Ok(0) => return -6,
            Ok(nr) => written += nr,
            Err(_) => return -6,
        }
    }

    let btree_ptr = bkey_i_to_btree_ptr_v2(&mut (*b).key);
    (*b).written += sectors_to_write as u16;
    (*btree_ptr).v.sectors_written = (*b).written;
    0
}

pub unsafe fn bch2_btree_node_read(sb: *mut bch_sb_handle, b: *mut btree) -> i32 {
    use std::os::unix::fs::FileExt;

    if sb.is_null()
        || (*sb).sb.is_null()
        || (*sb).s_bdev_file.is_null()
        || b.is_null()
        || (*b).data.is_null()
        || (*b).byte_order < 9
    {
        return -1;
    }
    if (*b).key.k.type_ != KEY_TYPE_btree_ptr_v2 {
        return -2;
    }
    let ptrs = bch2_bkey_ptrs_c(super::bkey::bkey_s_c {
        k: &(*b).key.k,
        v: &(*b).key.v,
    });
    if ptrs.start.is_null() || ptrs.start >= ptrs.end {
        return -2;
    }
    let ptr = (*ptrs.start).ptr;
    if BCH_EXTENT_PTR_DEV(&ptr) != (*(*sb).sb).dev_idx as u64 {
        return -3;
    }

    let node_bytes = 1usize << (*b).byte_order;
    let file = &*(*sb).s_bdev_file.cast::<std::fs::File>();
    let buffer = core::slice::from_raw_parts_mut((*b).data.cast::<u8>(), node_bytes);
    let disk_offset = BCH_EXTENT_PTR_OFFSET(&ptr) * 512;
    if disk_offset + buffer.len() as u64 > file.metadata().map(|m| m.len()).unwrap_or(0) {
        return -4;
    }
    let mut read = 0usize;
    while read < buffer.len() {
        match file.read_at(&mut buffer[read..], disk_offset + read as u64) {
            Ok(0) => return -4,
            Ok(nr) => read += nr,
            Err(_) => return -4,
        }
    }

    let node = &mut *(*b).data;
    (*b).version_ondisk = u16::MAX;
    (*b).written = 0;
    let uuid_lo = u64::from_le_bytes((&(*(*sb).sb).uuid)[..8].try_into().unwrap());
    if node.magic != uuid_lo ^ BSET_MAGIC {
        crate::rewrite_log_error!("btree node read rejected: bad magic");
        return -5;
    }
    let btree_ptr = bkey_i_to_btree_ptr_v2(&mut (*b).key);
    if node.keys.seq != (*btree_ptr).v.seq {
        return -11;
    }
    if BTREE_NODE_ID(node) != (*b).c.btree_id as u64
        || BTREE_NODE_LEVEL(node) != (*b).c.level as u64
    {
        return -12;
    }
    if node.min_key != (*btree_ptr).v.min_key || node.max_key != (*b).key.k.p {
        return -13;
    }
    if node.format.key_u64s == 0
        || node.format.key_u64s as usize > core::mem::size_of::<super::bkey::bkey>() / 8
        || node.format.nr_fields != super::bkey::BKEY_NR_FIELDS
    {
        return -14;
    }

    (*b).format = node.format;
    (*b).nr_key_bits = super::bkey::bkey_format_key_bits(&(*b).format) as u8;
    super::bkey::bch2_compute_bkey_unpack_consts(b);
    let block_bytes = ((*(*sb).sb).block_size as usize).max(1) * 512;
    let ptr_written = (*btree_ptr).v.sectors_written as usize;
    let limit = if ptr_written != 0 {
        ptr_written
    } else {
        node_bytes / 512
    };
    if limit * 512 > node_bytes {
        return -6;
    }
    let mut sort = super::bset_build::sort_iter_stack::default();
    super::bset_build::sort_iter_stack_init(&mut sort, b);
    let mut written = 0usize;
    let mut max_journal_seq = 0u64;
    while written < limit {
        let first = written == 0;
        let (set, expected_csum, header_bytes) = if first {
            (
                &mut node.keys as *mut super::bset::bset,
                node.csum,
                core::mem::size_of::<btree_node>(),
            )
        } else {
            let entry = buffer
                .as_mut_ptr()
                .add(written * 512)
                .cast::<btree_node_entry>();
            if (*entry).keys.seq != node.keys.seq {
                break;
            }
            (
                &mut (*entry).keys as *mut super::bset::bset,
                (*entry).csum,
                core::mem::size_of::<btree_node_entry>(),
            )
        };
        let key_u64s = (*set).u64s as usize;
        let bytes = header_bytes + key_u64s * 8;
        let sectors = bytes.next_multiple_of(block_bytes) / 512;
        if written + sectors > limit || written * 512 + bytes > node_bytes {
            return -6;
        }
        let csum_type = BSET_CSUM_TYPE(&*set);
        if csum_type >= BCH_CSUM_NR {
            return -7;
        }
        let header = if first {
            node as *mut btree_node as *mut u8
        } else {
            buffer.as_mut_ptr().add(written * 512)
        };
        let checksum_data = core::slice::from_raw_parts(header.add(16), bytes - 16);
        if bch2_checksum(csum_type, checksum_data) != expected_csum {
            return -8;
        }
        if (*set).version != bcachefs_metadata_version_current {
            return -9;
        }
        (*b).version_ondisk = (*b).version_ondisk.min((*set).version);
        if BSET_SEPARATE_WHITEOUTS(&*set) != 0
            || BSET_BIG_ENDIAN(&*set) != 0
            || BSET_OFFSET(&*set) != written as u32
        {
            return -10;
        }
        max_journal_seq = max_journal_seq.max((*set).journal_seq);

        let mut key = set.cast::<u64>().add(3).cast::<super::bkey::bkey_packed>();
        let key_start = key;
        let end = (key as *mut u64)
            .add(key_u64s)
            .cast::<super::bkey::bkey_packed>();
        let mut prev: *mut super::bkey::bkey_packed = core::ptr::null_mut();
        while key < end {
            if (*key).u64s == 0 || bkey_p_next(key) > end || !bkeyp_u64s_valid(&(*b).format, &*key)
            {
                return -15;
            }
            let pos = super::node_iter::bkey_unpack_pos(b, key);
            if bpos_lt(pos, node.min_key) || bpos_gt(pos, node.max_key) {
                return -16;
            }
            if !prev.is_null() && bpos_cmp(super::node_iter::bkey_unpack_pos(b, prev), pos) >= 0 {
                return -17;
            }
            if (*key).type_ == KEY_TYPE_btree_ptr_v2 {
                let key_words = bkeyp_key_u64s(&(*b).format, &*key) as usize;
                let value_words = (*key).u64s as usize - key_words;
                if value_words < core::mem::size_of::<super::bset::bch_btree_ptr_v2>() / 8 {
                    return -18;
                }
                *((key as *mut u64).add(key_words)) = 0;
            }
            prev = key;
            key = bkey_p_next(key);
        }
        if key != end {
            return -15;
        }
        super::bset_build::sort_iter_add(&mut sort.iter, key_start, end);
        written += sectors;
    }
    if ptr_written != 0 && written != ptr_written {
        return -19;
    }
    if ptr_written == 0 {
        let mut trailing = written;
        while trailing < node_bytes / 512 {
            let entry = buffer
                .as_ptr()
                .add(trailing * 512)
                .cast::<btree_node_entry>();
            if (*entry).keys.seq == node.keys.seq {
                return -20;
            }
            trailing += block_bytes / 512;
        }
    }

    let mut sorted = vec![0u64; node_bytes / 8];
    let sorted_set = sorted.as_mut_ptr().add(17).cast::<super::bset::bset>();
    let nr = super::bset_build::bch2_key_sort_fix_overlapping(
        core::ptr::null_mut(),
        sorted_set,
        &mut sort.iter,
    );
    let compact_u64s = (*sorted_set).u64s as usize;
    node.keys.u64s = (*sorted_set).u64s;
    core::ptr::copy_nonoverlapping(
        node as *const btree_node as *const u64,
        sorted.as_mut_ptr(),
        20,
    );
    core::ptr::copy_nonoverlapping(
        sorted.as_ptr(),
        node as *mut btree_node as *mut u64,
        sorted.len(),
    );
    node.keys.journal_seq = max_journal_seq;

    (*b).nsets = 1;
    (*b).set[0] = bset_tree {
        size: 0,
        extra: BSET_NO_AUX_TREE_VAL,
        data_offset: 17,
        aux_data_offset: u16::MAX,
        end_offset: (20 + compact_u64s) as u16,
    };
    (*b).nr = nr;
    (*b).written = written as u16;
    super::bset_build::bch2_btree_build_aux_trees(b);
    super::bset_build::bch2_set_bset_needs_whiteout(
        super::types::bset(b, (*b).set.as_mut_ptr()),
        1,
    );
    crate::rewrite_log_debug!(
        "btree node read complete level={} sets={} written={written}",
        (*b).c.level,
        (*b).nsets
    );
    0
}

pub unsafe fn bch2_btree_node_drop_keys_outside_node(b: *mut btree) {
    for idx in 0..(*b).nsets as usize {
        let tree = (*b).set.as_mut_ptr().add(idx);
        let set = bset(b, tree);
        let start = set.cast::<u64>().add(3).cast::<super::bkey::bkey_packed>();
        let mut key = start;
        let mut end = btree_bkey_last(b, tree);

        while key != end {
            if super::bkey::bkey_cmp_left_packed(b, key, &(*(*b).data).min_key) >= 0 {
                break;
            }
            key = bkey_p_next(key);
        }

        if key != start {
            let shift = key.cast::<u64>().offset_from(start.cast::<u64>()) as usize;
            core::ptr::copy(
                key.cast::<u64>(),
                start.cast::<u64>(),
                end.cast::<u64>().offset_from(key.cast::<u64>()) as usize,
            );
            (*set).u64s -= shift as u16;
            super::types::set_btree_bset_end(b, tree);
            end = btree_bkey_last(b, tree);
        }

        key = start;
        while key != end {
            if super::bkey::bkey_cmp_left_packed(b, key, &(*(*b).data).max_key) > 0 {
                break;
            }
            key = bkey_p_next(key);
        }

        if key != end {
            (*set).u64s = key.cast::<u64>().offset_from(start.cast::<u64>()) as u16;
            super::types::set_btree_bset_end(b, tree);
        }
    }

    super::bset_build::bch2_bset_set_no_aux_tree(b, (*b).set.as_mut_ptr());
    super::bset_build::bch2_btree_build_aux_trees(b);
    (*b).nr = super::bset_update::bch2_btree_node_count_keys(b);

    let mut iter = super::types::btree_node_iter::default();
    super::node_iter::bch2_btree_node_iter_init_from_start(&mut iter, b);
    loop {
        let key = super::node_iter::bch2_btree_node_iter_peek(&mut iter, b);
        if key.is_null() {
            break;
        }
        let pos = super::node_iter::bkey_unpack_pos(b, key);
        assert!(!bpos_lt(pos, (*(*b).data).min_key));
        assert!(!bpos_gt(pos, (*(*b).data).max_key));
        super::node_iter::bch2_btree_node_iter_advance(&mut iter, b);
    }
}

pub(crate) unsafe fn bch2_btree_node_get_noiter_unlocked(
    trans: *mut super::iter::btree_trans,
    key: *const super::bkey::bkey_i,
    btree_id: u8,
    level: u8,
    nofill: bool,
) -> *mut btree {
    if trans.is_null() || key.is_null() || (*key).k.type_ != KEY_TYPE_btree_ptr_v2 {
        return core::ptr::null_mut();
    }
    let c = (*trans).c;
    if c.is_null() {
        return core::ptr::null_mut();
    }
    let cached = super::bset::btree_node_mem_ptr(key);
    let cache_initialized = (*c).btree.cache.table_init_done;
    let hash_val = super::cache::btree_ptr_hash_val(key.cast());
    if !cached.is_null()
        && (!cache_initialized
            || ((*cached).hash_val == hash_val
                && (*cached).c.btree_id == btree_id
                && (*cached).c.level == level))
    {
        if super::types::btree_node_read_in_flight(cached) {
            return cached;
        }
        if super::types::btree_node_read_error(cached) {
            if cache_initialized {
                let _ = super::cache::bch2_btree_node_transition_state(
                    &mut (*c).btree.cache,
                    cached,
                    super::types::btree_node_cache_state::BTREE_NODE_CACHE_FREEABLE,
                );
            } else {
                return core::ptr::null_mut();
            }
        } else {
            if !super::types::btree_node_accessed(cached) {
                super::types::set_btree_node_accessed(cached);
            }
            return cached;
        }
    }
    if cache_initialized {
        if hash_val != 0 {
            let found = crate::util::rhashtable::rhashtable_lookup_fast(
                &mut (*c).btree.cache.table,
                &hash_val as *const u64 as *const _,
            );
            if !found.is_null() {
                if super::types::btree_node_read_in_flight(found.cast()) {
                    return found.cast();
                }
                if super::types::btree_node_read_error(found.cast()) {
                    let _ = super::cache::bch2_btree_node_transition_state(
                        &mut (*c).btree.cache,
                        found.cast(),
                        super::types::btree_node_cache_state::BTREE_NODE_CACHE_FREEABLE,
                    );
                } else {
                    if !super::types::btree_node_accessed(found.cast()) {
                        super::types::set_btree_node_accessed(found.cast());
                    }
                    return found.cast();
                }
            }
        }
    }
    if nofill {
        return core::ptr::null_mut();
    }
    if (*c).disk_sb.sb.is_null() {
        return core::ptr::null_mut();
    }
    let node = super::cache::bch2_btree_node_mem_alloc(trans, level != 0);
    if node.is_null() {
        return core::ptr::null_mut();
    }
    super::bkey::bkey_copy(&mut (*node).key, key);
    (*node).c.level = level;
    (*node).c.btree_id = btree_id;
    if cache_initialized {
        let transition = super::cache::bch2_btree_node_transition_state(
            &mut (*c).btree.cache,
            node,
            super::types::btree_node_cache_state::BTREE_NODE_CACHE_CLEAN,
        );
        if transition != 0 {
            let raced = crate::util::rhashtable::rhashtable_lookup_fast(
                &mut (*c).btree.cache.table,
                &hash_val as *const u64 as *const _,
            );
                if !raced.is_null() {
                    if !super::types::btree_node_accessed(raced.cast()) {
                        super::types::set_btree_node_accessed(raced.cast());
                    }
                    let _ = super::cache::bch2_btree_node_transition_state(
                    &mut (*c).btree.cache,
                    node,
                    super::types::btree_node_cache_state::BTREE_NODE_CACHE_FREED,
                );
                return raced.cast();
            }
            let _ = super::cache::bch2_btree_node_transition_state(
                &mut (*c).btree.cache,
                node,
                super::types::btree_node_cache_state::BTREE_NODE_CACHE_FREED,
            );
            return core::ptr::null_mut();
        }
    }
    super::types::set_btree_node_read_in_flight(node);
    let ret = bch2_btree_node_read(&mut (*c).disk_sb, node);
    super::types::clear_btree_node_read_in_flight(node);
    if ret != 0 {
        super::types::set_btree_node_read_error(node);
        if cache_initialized {
            let _ = super::cache::bch2_btree_node_transition_state(
                &mut (*c).btree.cache,
                node,
                super::types::btree_node_cache_state::BTREE_NODE_CACHE_FREEABLE,
            );
        }
        return core::ptr::null_mut();
    }
    super::types::set_btree_node_accessed(node);
    node
}

pub unsafe fn bch2_btree_node_get_noiter(
    trans: *mut super::iter::btree_trans,
    key: *const super::bkey::bkey_i,
    btree_id: u8,
    level: u8,
    nofill: bool,
) -> *mut btree {
    loop {
        let node = bch2_btree_node_get_noiter_unlocked(trans, key, btree_id, level, nofill);
        if node.is_null() {
            return core::ptr::null_mut();
        }
        if six_lock_read(&(*node).c.lock) != 0 {
            return core::ptr::null_mut();
        }
        if (*node).hash_val != super::cache::btree_ptr_hash_val(key.cast())
            || (*node).c.btree_id != btree_id
            || (*node).c.level != level
        {
            six_unlock_read(&(*node).c.lock);
            continue;
        }
        bch2_btree_node_wait_on_read(trans, node);
        if super::types::btree_node_read_error(node) {
            six_unlock_read(&(*node).c.lock);
            return core::ptr::null_mut();
        }
        return node;
    }
}

pub unsafe fn bch2_btree_node_get(
    trans: *mut super::iter::btree_trans,
    path: *mut super::iter::btree_path,
    key: *const super::bkey::bkey_i,
    level: u8,
    lock_type: crate::lock::six::six_lock_type,
    flags: u16,
) -> *mut btree {
    if trans.is_null() || path.is_null() || key.is_null() {
        return core::ptr::null_mut();
    }
    if level as usize >= super::bset::BTREE_MAX_DEPTH as usize
        || (*path).level != level.saturating_add(1)
    {
        return core::ptr::null_mut();
    }
    if !super::iter::bch2_btree_path_relock_norestart(trans, path) {
        return core::ptr::null_mut();
    }
    let node = bch2_btree_node_get_noiter_unlocked(
        trans,
        key,
        (*path).btree_id,
        level,
        flags & super::iter::BTREE_ITER_nofill != 0,
    );
    if node.is_null() {
        return core::ptr::null_mut();
    }

    if (*path).l[level as usize + 1].b != core::ptr::null_mut()
        && (((*path).nodes_locked >> ((level as usize + 1) * 2)) & 3)
            == super::iter::BTREE_NODE_READ_LOCKED
    {
        super::iter::btree_node_unlock(path, level as usize + 1);
    }

    let path_lock = match lock_type {
        crate::lock::six::six_lock_type::SIX_LOCK_intent => {
            super::iter::BTREE_NODE_INTENT_LOCKED
        }
        crate::lock::six::six_lock_type::SIX_LOCK_read => {
            super::iter::BTREE_NODE_READ_LOCKED
        }
        crate::lock::six::six_lock_type::SIX_LOCK_write => {
            super::iter::BTREE_NODE_WRITE_LOCKED
        }
    };
    if super::iter::btree_node_lock_type(trans, path, node, level as usize, path_lock) != 0 {
        return core::ptr::null_mut();
    }

    if (*node).c.btree_id != (*path).btree_id
        || (*node).c.level != level
    {
        match lock_type {
            crate::lock::six::six_lock_type::SIX_LOCK_intent => {
                crate::lock::six::six_unlock_intent(&(*node).c.lock)
            }
            crate::lock::six::six_lock_type::SIX_LOCK_read => {
                crate::lock::six::six_unlock_read(&(*node).c.lock)
            }
            crate::lock::six::six_lock_type::SIX_LOCK_write => {
                crate::lock::six::six_unlock_write(&(*node).c.lock)
            }
        }
        return core::ptr::null_mut();
    }

    bch2_btree_node_wait_on_read(trans, node);
    if super::types::btree_node_read_error(node) {
        match lock_type {
            crate::lock::six::six_lock_type::SIX_LOCK_intent => {
                crate::lock::six::six_unlock_intent(&(*node).c.lock)
            }
            crate::lock::six::six_lock_type::SIX_LOCK_read => {
                crate::lock::six::six_unlock_read(&(*node).c.lock)
            }
            crate::lock::six::six_lock_type::SIX_LOCK_write => {
                crate::lock::six::six_unlock_write(&(*node).c.lock)
            }
        }
        return core::ptr::null_mut();
    }

    if !super::types::btree_node_accessed(node) {
        super::types::set_btree_node_accessed(node);
    }
    node
}

pub unsafe fn bch2_btree_node_prefetch(
    trans: *mut super::iter::btree_trans,
    path: *mut super::iter::btree_path,
    key: *const super::bkey::bkey_i,
    btree_id: u8,
    level: u8,
) -> i32 {
    if trans.is_null() || key.is_null() {
        return 0;
    }
    assert!((level as usize) < super::bset::BTREE_MAX_DEPTH as usize);
    if !path.is_null() {
        let parent = level as usize + 1;
        assert!(parent < super::bset::BTREE_MAX_DEPTH as usize);
        assert!(!(*path).l[parent].b.is_null());
        assert_ne!(((*path).nodes_locked >> (parent * 2)) & 3, 0);
    }

    let node = bch2_btree_node_get_noiter(trans, key, btree_id, level, false);
    if node.is_null() {
        return 0;
    }

    /* A prefetch returns immediately: the cache lookup/read path owns the
     * node, and this API deliberately does not retain a SIX lock. */
    six_unlock_read(&(*node).c.lock);
    let _ = path;
    0
}

pub unsafe fn bch2_btree_root_read(
    c: *mut super::types::bch_fs,
    id: u8,
    key: *const super::bkey::bkey_i,
    level: u8,
) -> i32 {
    if c.is_null() || key.is_null() {
        return -1;
    }
    if !(*c).btree.cache.table_init_done && super::cache::bch2_fs_btree_cache_init(c) != 0 {
        return -1;
    }
    let mut trans = super::iter::btree_trans::default();
    super::iter::bch2_trans_init(&mut trans, c);
    let node = super::cache::bch2_btree_node_mem_alloc(&mut trans, level != 0);
    if node.is_null() {
        return -1;
    }
    if six_lock_intent(&(*node).c.lock) != 0 {
        return -1;
    }
    if six_lock_write(&(*node).c.lock) != 0 {
        six_unlock_intent(&(*node).c.lock);
        return -1;
    }
    super::bkey::bkey_copy(&mut (*node).key, key);
    (*node).c.level = level;
    (*node).c.btree_id = id;
    if super::cache::bch2_btree_node_transition_state(
        &mut (*c).btree.cache,
        node,
        super::types::btree_node_cache_state::BTREE_NODE_CACHE_CLEAN,
    ) != 0
    {
        six_unlock_write(&(*node).c.lock);
        six_unlock_intent(&(*node).c.lock);
        return -1;
    }
    super::types::set_btree_node_read_in_flight(node);
    super::iter::bch2_trans_unlock(&mut trans);
    let ret = bch2_btree_node_read(&mut (*c).disk_sb, node);
    super::types::clear_btree_node_read_in_flight(node);
    if ret != 0 {
        super::types::set_btree_node_read_error(node);
        let _ = super::cache::bch2_btree_node_transition_state(
            &mut (*c).btree.cache,
            node,
            super::types::btree_node_cache_state::BTREE_NODE_CACHE_FREEABLE,
        );
        six_unlock_write(&(*node).c.lock);
        six_unlock_intent(&(*node).c.lock);
        return ret;
    }
    core::ptr::copy_nonoverlapping(
        key.cast::<u64>(),
        (&mut (*super::types::bch2_btree_id_root(c, id as usize)).key
            as *mut super::bkey::bkey_i)
            .cast::<u64>(),
        (*key).k.u64s as usize,
    );
    super::interior::bch2_btree_set_root_for_read(c, node);
    six_unlock_write(&(*node).c.lock);
    six_unlock_intent(&(*node).c.lock);
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btree::bkey::{
        bkey, bkey_format_key_bits, BKEY_FORMAT_CURRENT, BKEY_U64S, KEY_FORMAT_CURRENT, POS_MIN,
        SPOS, SPOS_MAX,
    };
    use crate::btree::bset::{
        bch2_bkey_append_ptr, bch_extent_ptr, BCH_EXTENT_PTR_OFFSET, SET_BCH_EXTENT_PTR_DEV,
        SET_BCH_EXTENT_PTR_OFFSET,
    };
    use crate::btree::types::btree_nr_keys;
    use crate::sb::io::{bch2_free_super, bch2_sb_realloc};

    #[test]
    fn nofill_lookup_does_not_allocate_missing_node() {
        unsafe {
            let mut c = crate::btree::types::bch_fs::default();
            let mut trans = crate::btree::iter::btree_trans::default();
            crate::btree::iter::bch2_trans_init(&mut trans, &mut c);
            let key = crate::btree::bset::bkey_i_btree_ptr_v2 {
                k: bkey {
                    u64s: 10,
                    format: KEY_FORMAT_CURRENT,
                    type_: crate::btree::bset::KEY_TYPE_btree_ptr_v2,
                    p: SPOS(1, 1, 0),
                    ..Default::default()
                },
                v: crate::btree::bset::bch_btree_ptr_v2 {
                    seq: 1,
                    ..Default::default()
                },
            };
            assert!(bch2_btree_node_get_noiter(
                &mut trans,
                (&key as *const crate::btree::bset::bkey_i_btree_ptr_v2).cast(),
                0,
                0,
                true,
            )
            .is_null());
        }
    }

    #[test]
    fn post_write_cleanup_drops_single_bset_whiteouts() {
        unsafe {
            let mut words = vec![0u64; 256];
            let mut aux = vec![0u64; crate::btree::types::__btree_aux_data_bytes(11) / 8];
            let mut node = btree::default();
            node.data = words.as_mut_ptr().cast::<btree_node>();
            node.aux_data = aux.as_mut_ptr().cast();
            node.byte_order = 11;
            node.format = BKEY_FORMAT_CURRENT;
            node.nr_key_bits = bkey_format_key_bits(&node.format) as u8;
            crate::btree::bset_build::bch2_btree_keys_init(&mut node);
            node.nsets = 1;
            node.written = 1;
            node.flags |= 1 << BTREE_NODE_just_written;
            (*node.data).min_key = POS_MIN;
            (*node.data).max_key = SPOS_MAX;
            (*node.data).keys.seq = 88;
            (*node.data).keys.journal_seq = 4;
            (*node.data).keys.u64s = 15;
            for (idx, (offset, type_)) in [(1, 2), (2, 0), (3, 4)].into_iter().enumerate() {
                *words.as_mut_ptr().add(20 + idx * 5).cast::<bkey>() = bkey {
                    u64s: BKEY_U64S,
                    format: KEY_FORMAT_CURRENT,
                    type_,
                    p: SPOS(1, offset, 0),
                    ..Default::default()
                };
            }
            node.set[0] = bset_tree {
                size: 0,
                extra: BSET_NO_AUX_TREE_VAL,
                data_offset: 17,
                aux_data_offset: u16::MAX,
                end_offset: 35,
            };
            node.nr.live_u64s = 10;
            node.nr.bset_u64s[0] = 10;
            node.nr.unpacked_keys = 2;

            let mut c = super::super::types::bch_fs::default();
            assert_eq!(bch2_sb_realloc(&mut c.disk_sb, 0), 0);
            (*c.disk_sb.sb).block_size = 1;
            assert!(bch2_btree_post_write_cleanup(&mut c, &mut node));

            assert!(!btree_node_just_written(&node));
            assert_eq!((*node.data).keys.u64s, 10);
            assert_eq!(node.set[0].end_offset, 30);
            assert_eq!(node.nr.live_u64s, 10);
            assert_eq!(node.nr.bset_u64s, [10, 0, 0]);
            assert_eq!((*(words.as_ptr().add(20).cast::<bkey>())).p, SPOS(1, 1, 0));
            assert_eq!((*(words.as_ptr().add(25).cast::<bkey>())).p, SPOS(1, 3, 0));
            assert_eq!(
                (*(words.as_ptr().add(20).cast::<bkey>())).format & 0x80,
                0x80
            );
            assert_eq!(
                (*(words.as_ptr().add(25).cast::<bkey>())).format & 0x80,
                0x80
            );
            assert_eq!(node.nsets, 2);
            assert_eq!(node.set[1].data_offset, 66);
            assert_eq!(node.set[1].end_offset, 69);
            bch2_free_super(&mut c.disk_sb);
        }
    }

    #[test]
    fn drops_keys_outside_repaired_node_range_and_rebuilds_accounting() {
        unsafe {
            let mut words = vec![0u64; 256];
            let mut aux = vec![0u64; crate::btree::types::__btree_aux_data_bytes(11) / 8];
            let mut node = btree::default();
            node.data = words.as_mut_ptr().cast::<btree_node>();
            node.aux_data = aux.as_mut_ptr().cast();
            node.byte_order = 11;
            node.format = BKEY_FORMAT_CURRENT;
            node.nr_key_bits = bkey_format_key_bits(&node.format) as u8;
            node.nsets = 1;
            (*node.data).min_key = SPOS(1, 2, 0);
            (*node.data).max_key = SPOS(1, 4, 0);
            (*node.data).keys.u64s = 25;
            for (idx, offset) in [1u64, 2, 3, 4, 5].into_iter().enumerate() {
                *words.as_mut_ptr().add(20 + idx * 5).cast::<bkey>() = bkey {
                    u64s: BKEY_U64S,
                    format: KEY_FORMAT_CURRENT,
                    type_: 2,
                    p: SPOS(1, offset, 0),
                    ..Default::default()
                };
            }
            node.set[0] = bset_tree {
                size: 0,
                extra: BSET_NO_AUX_TREE_VAL,
                data_offset: 17,
                aux_data_offset: u16::MAX,
                end_offset: 45,
            };
            node.nr.live_u64s = 25;
            node.nr.bset_u64s[0] = 25;
            node.nr.unpacked_keys = 5;

            bch2_btree_node_drop_keys_outside_node(&mut node);
            assert_eq!((*node.data).keys.u64s, 15);
            assert_eq!(node.set[0].end_offset, 35);
            assert_eq!(node.nr.live_u64s, 15);
            assert_eq!(node.nr.bset_u64s, [15, 0, 0]);
            assert_eq!(node.nr.unpacked_keys, 3);
            for (idx, offset) in [2u64, 3, 4].into_iter().enumerate() {
                assert_eq!(
                    (*(words.as_ptr().add(20 + idx * 5).cast::<bkey>())).p,
                    SPOS(1, offset, 0)
                );
            }
            assert!(crate::btree::types::bset_has_rw_aux_tree(
                node.set.as_mut_ptr()
            ));
        }
    }

    #[test]
    fn writes_reads_and_checksums_current_leaf_node() {
        use std::os::unix::fs::FileExt;

        unsafe {
            let path = std::env::temp_dir().join(format!(
                "subvol-btree-io-{}",
                std::process::id()
            ));
            let file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .read(true)
                .write(true)
                .open(&path)
                .unwrap();
            file.set_len(128 * 512).unwrap();

            let mut handle = bch_sb_handle::default();
            handle.s_bdev_file = Box::into_raw(Box::new(file.try_clone().unwrap())).cast();
            assert_eq!(bch2_sb_realloc(&mut handle, 0), 0);
            (*handle.sb).uuid = [0x5a; 16];
            (*handle.sb).dev_idx = 0;

            let mut words = vec![0u64; 64];
            let mut node = btree::default();
            node.data = words.as_mut_ptr().cast::<btree_node>();
            node.byte_order = 9;
            node.c.btree_id = 3;
            node.c.level = 0;
            node.format = BKEY_FORMAT_CURRENT;
            node.nr_key_bits = bkey_format_key_bits(&node.format) as u8;
            node.nsets = 1;
            (*node.data).min_key = SPOS(4, 1, 0);
            (*node.data).max_key = SPOS(4, 2, 0);
            (*node.data).keys.seq = 77;
            (*node.data).keys.u64s = 10;
            for (index, offset) in [1, 2].into_iter().enumerate() {
                *words.as_mut_ptr().add(20 + index * 5).cast::<bkey>() = bkey {
                    u64s: BKEY_U64S,
                    format: KEY_FORMAT_CURRENT,
                    type_: 6,
                    p: SPOS(4, offset, 0),
                    ..Default::default()
                };
            }
            node.set[0] = bset_tree {
                size: 0,
                extra: BSET_NO_AUX_TREE_VAL,
                data_offset: 17,
                aux_data_offset: u16::MAX,
                end_offset: 30,
            };
            node.nr.live_u64s = 10;
            node.nr.bset_u64s[0] = 10;
            node.nr.unpacked_keys = 2;

            node.key.k = bkey {
                u64s: 10,
                format: KEY_FORMAT_CURRENT,
                type_: KEY_TYPE_btree_ptr_v2,
                p: (*node.data).max_key,
                ..Default::default()
            };
            let node_ptr = bkey_i_to_btree_ptr_v2(&mut node.key);
            (*node_ptr).v.mem_ptr = (&mut node as *mut btree) as usize as u64;
            (*node_ptr).v.seq = 77;
            (*node_ptr).v.min_key = (*node.data).min_key;
            let mut extent = bch_extent_ptr::default();
            SET_BCH_EXTENT_PTR_OFFSET(&mut extent, 64);
            SET_BCH_EXTENT_PTR_DEV(&mut extent, 0);
            bch2_bkey_append_ptr(core::ptr::null(), &mut node.key, extent);

            assert_eq!(__bch2_btree_node_write(&mut handle, &mut node), 0);
            assert_eq!(node.written, 1);
            assert_eq!((*node_ptr).v.sectors_written, 1);
            let ptrs = bch2_bkey_ptrs_c(crate::btree::bkey::bkey_s_c {
                k: &node.key.k,
                v: &node.key.v,
            });
            assert_eq!(BCH_EXTENT_PTR_OFFSET(&(*ptrs.start).ptr), 64);

            words.fill(0);
            node.nsets = 0;
            node.nr = btree_nr_keys::default();
            node.written = u16::MAX;
            assert_eq!(bch2_btree_node_read(&mut handle, &mut node), 0);
            assert_eq!((*node.data).min_key, SPOS(4, 1, 0));
            assert_eq!((*node.data).max_key, SPOS(4, 2, 0));
            assert_eq!((*node.data).keys.u64s, 10);
            assert_eq!(node.version_ondisk, bcachefs_metadata_version_current);
            assert_eq!(node.nsets, 1);
            assert_eq!(node.nr.live_u64s, 10);
            assert_eq!(node.nr.unpacked_keys, 2);
            assert_eq!((*(words.as_ptr().add(20).cast::<bkey>())).p, SPOS(4, 1, 0));
            assert_eq!((*(words.as_ptr().add(25).cast::<bkey>())).p, SPOS(4, 2, 0));

            let mut byte = [0u8; 1];
            assert_eq!(file.read_at(&mut byte, 64 * 512 + 200).unwrap(), 1);
            byte[0] ^= 1;
            assert_eq!(file.write_at(&byte, 64 * 512 + 200).unwrap(), 1);
            assert_eq!(bch2_btree_node_read(&mut handle, &mut node), -8);

            bch2_free_super(&mut handle);
            drop(file);
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn root_read_lazily_loads_child_from_disk_pointer() {
        use crate::btree::iter::{
            bch2_btree_iter_next, bch2_btree_iter_peek, bch2_trans_init, bch2_trans_iter_exit,
            bch2_trans_iter_init, btree_iter, btree_trans,
        };
        use crate::btree::types::bch_fs;

        unsafe {
            let path = std::env::temp_dir().join(format!(
                "subvol-btree-root-{}",
                std::process::id()
            ));
            let file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .read(true)
                .write(true)
                .open(&path)
                .unwrap();
            file.set_len(128 * 512).unwrap();

            let mut write_handle = bch_sb_handle::default();
            write_handle.s_bdev_file = Box::into_raw(Box::new(file.try_clone().unwrap())).cast();
            assert_eq!(bch2_sb_realloc(&mut write_handle, 0), 0);
            (*write_handle.sb).uuid = [0x71; 16];
            (*write_handle.sb).dev_idx = 0;
            (*write_handle.sb).flags[0] = 1 << 12;

            let mut leaf_words = vec![0u64; 64];
            let mut leaf = Box::new(btree::default());
            leaf.data = leaf_words.as_mut_ptr().cast();
            leaf.byte_order = 9;
            leaf.c.btree_id = 0;
            leaf.c.level = 0;
            leaf.format = BKEY_FORMAT_CURRENT;
            leaf.nr_key_bits = bkey_format_key_bits(&leaf.format) as u8;
            leaf.nsets = 1;
            (*leaf.data).min_key = POS_MIN;
            (*leaf.data).max_key = SPOS_MAX;
            (*leaf.data).keys.seq = 101;
            (*leaf.data).keys.u64s = 10;
            for (index, offset) in [1, 2].into_iter().enumerate() {
                *leaf_words.as_mut_ptr().add(20 + index * 5).cast::<bkey>() = bkey {
                    u64s: BKEY_U64S,
                    format: KEY_FORMAT_CURRENT,
                    type_: 6,
                    p: SPOS(9, offset, 0),
                    ..Default::default()
                };
            }
            leaf.set[0] = bset_tree {
                size: 0,
                extra: BSET_NO_AUX_TREE_VAL,
                data_offset: 17,
                aux_data_offset: u16::MAX,
                end_offset: 30,
            };
            leaf.key.k = bkey {
                u64s: 10,
                format: KEY_FORMAT_CURRENT,
                type_: KEY_TYPE_btree_ptr_v2,
                p: (*leaf.data).max_key,
                ..Default::default()
            };
            let leaf_ptr = bkey_i_to_btree_ptr_v2(&mut leaf.key);
            (*leaf_ptr).v.mem_ptr = (&mut *leaf as *mut btree) as usize as u64;
            (*leaf_ptr).v.seq = 101;
            (*leaf_ptr).v.min_key = (*leaf.data).min_key;
            let mut leaf_extent = bch_extent_ptr::default();
            SET_BCH_EXTENT_PTR_OFFSET(&mut leaf_extent, 64);
            SET_BCH_EXTENT_PTR_DEV(&mut leaf_extent, 0);
            bch2_bkey_append_ptr(core::ptr::null(), &mut leaf.key, leaf_extent);
            assert_eq!(__bch2_btree_node_write(&mut write_handle, &mut *leaf), 0);

            let mut root_words = vec![0u64; 64];
            let mut root = Box::new(btree::default());
            root.data = root_words.as_mut_ptr().cast();
            root.byte_order = 9;
            root.c.btree_id = 0;
            root.c.level = 1;
            root.format = BKEY_FORMAT_CURRENT;
            root.nr_key_bits = bkey_format_key_bits(&root.format) as u8;
            root.nsets = 1;
            (*root.data).min_key = POS_MIN;
            (*root.data).max_key = SPOS_MAX;
            (*root.data).keys.seq = 202;
            (*root.data).keys.u64s = leaf.key.k.u64s as u16;
            core::ptr::copy_nonoverlapping(
                (&leaf.key as *const crate::btree::bkey::bkey_i).cast::<u64>(),
                root_words.as_mut_ptr().add(20),
                leaf.key.k.u64s as usize,
            );
            root.set[0] = bset_tree {
                size: 0,
                extra: BSET_NO_AUX_TREE_VAL,
                data_offset: 17,
                aux_data_offset: u16::MAX,
                end_offset: 20 + leaf.key.k.u64s as u16,
            };
            root.key.k = bkey {
                u64s: 10,
                format: KEY_FORMAT_CURRENT,
                type_: KEY_TYPE_btree_ptr_v2,
                p: (*root.data).max_key,
                ..Default::default()
            };
            let root_ptr = bkey_i_to_btree_ptr_v2(&mut root.key);
            (*root_ptr).v.mem_ptr = (&mut *root as *mut btree) as usize as u64;
            (*root_ptr).v.seq = 202;
            (*root_ptr).v.min_key = (*root.data).min_key;
            let mut root_extent = bch_extent_ptr::default();
            SET_BCH_EXTENT_PTR_OFFSET(&mut root_extent, 72);
            SET_BCH_EXTENT_PTR_DEV(&mut root_extent, 0);
            bch2_bkey_append_ptr(core::ptr::null(), &mut root.key, root_extent);
            assert_eq!(__bch2_btree_node_write(&mut write_handle, &mut *root), 0);

            let mut root_key_words = [0u64; 20];
            core::ptr::copy_nonoverlapping(
                (&root.key as *const crate::btree::bkey::bkey_i).cast::<u64>(),
                root_key_words.as_mut_ptr(),
                root.key.k.u64s as usize,
            );
            let root_key = root_key_words
                .as_mut_ptr()
                .cast::<crate::btree::bkey::bkey_i>();
            (*bkey_i_to_btree_ptr_v2(root_key)).v.mem_ptr = 0;

            let mut recovered = bch_fs::default();
            recovered.disk_sb.s_bdev_file =
                Box::into_raw(Box::new(file.try_clone().unwrap())).cast();
            assert_eq!(bch2_sb_realloc(&mut recovered.disk_sb, 0), 0);
            (*recovered.disk_sb.sb).uuid = [0x71; 16];
            (*recovered.disk_sb.sb).dev_idx = 0;
            (*recovered.disk_sb.sb).flags[0] = 1 << 12;
            assert_eq!(bch2_btree_root_read(&mut recovered, 0, root_key, 1), 0);

            let recovered_root = crate::btree::types::bch2_btree_id_root_b(&recovered, 0);
            let child_on_disk = ((*recovered_root).data as *mut u64)
                .add(20)
                .cast::<crate::btree::bset::bkey_i_btree_ptr_v2>();
            assert_eq!((*child_on_disk).v.mem_ptr, 0);

            let mut trans = btree_trans::default();
            bch2_trans_init(&mut trans, &mut recovered);
            let mut iter = btree_iter::default();
            bch2_trans_iter_init(&mut trans, &mut iter, 0, SPOS(9, 1, 0), 0);
            let mut seen = Vec::new();
            let mut key = bch2_btree_iter_peek(&mut iter);
            while !key.k.is_null() {
                seen.push((*key.k).p.offset);
                key = bch2_btree_iter_next(&mut iter);
            }
            assert_eq!(seen, [1, 2]);
            assert_ne!((*child_on_disk).v.mem_ptr, 0);
            bch2_trans_iter_exit(&mut iter);

            bch2_free_super(&mut recovered.disk_sb);
            bch2_free_super(&mut write_handle);
            drop(file);
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn appends_and_replays_multiple_bsets_in_sequence_order() {
        use crate::btree::bset::btree_node_entry;
        use crate::btree::bset_build::bch2_bset_init_next;
        use crate::btree::node_iter::{
            bch2_btree_node_iter_advance, bch2_btree_node_iter_init_from_start,
            bch2_btree_node_iter_peek, bkey_unpack_pos,
        };
        use crate::btree::types::{btree_node_iter, set_btree_bset_end};
        use std::os::unix::fs::FileExt;

        unsafe {
            let path = std::env::temp_dir().join(format!(
                "subvol-btree-multiset-{}",
                std::process::id()
            ));
            let file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .read(true)
                .write(true)
                .open(&path)
                .unwrap();
            file.set_len(128 * 512).unwrap();

            let mut handle = bch_sb_handle::default();
            handle.s_bdev_file = Box::into_raw(Box::new(file.try_clone().unwrap())).cast();
            assert_eq!(bch2_sb_realloc(&mut handle, 0), 0);
            (*handle.sb).uuid = [0x93; 16];
            (*handle.sb).dev_idx = 0;
            (*handle.sb).block_size = 1;
            (*handle.sb).flags[0] = 4 << 12;

            let mut words = vec![0u64; 256];
            let mut node = btree::default();
            node.data = words.as_mut_ptr().cast();
            node.byte_order = 11;
            node.c.btree_id = 2;
            node.c.level = 0;
            node.format = BKEY_FORMAT_CURRENT;
            node.nr_key_bits = bkey_format_key_bits(&node.format) as u8;
            node.nsets = 1;
            (*node.data).min_key = POS_MIN;
            (*node.data).max_key = SPOS_MAX;
            (*node.data).keys.seq = 500;
            (*node.data).keys.journal_seq = 1;
            (*node.data).keys.u64s = 10;
            for (index, offset) in [1, 2].into_iter().enumerate() {
                *words.as_mut_ptr().add(20 + index * 5).cast::<bkey>() = bkey {
                    u64s: BKEY_U64S,
                    format: KEY_FORMAT_CURRENT,
                    type_: 6,
                    p: SPOS(12, offset, 0),
                    ..Default::default()
                };
            }
            node.set[0] = bset_tree {
                size: 0,
                extra: BSET_NO_AUX_TREE_VAL,
                data_offset: 17,
                aux_data_offset: u16::MAX,
                end_offset: 30,
            };
            node.key.k = bkey {
                u64s: 10,
                format: KEY_FORMAT_CURRENT,
                type_: KEY_TYPE_btree_ptr_v2,
                p: SPOS_MAX,
                ..Default::default()
            };
            let node_ptr = bkey_i_to_btree_ptr_v2(&mut node.key);
            (*node_ptr).v.mem_ptr = (&mut node as *mut btree) as usize as u64;
            (*node_ptr).v.seq = 500;
            (*node_ptr).v.min_key = POS_MIN;
            let mut extent = bch_extent_ptr::default();
            SET_BCH_EXTENT_PTR_OFFSET(&mut extent, 32);
            SET_BCH_EXTENT_PTR_DEV(&mut extent, 0);
            bch2_bkey_append_ptr(core::ptr::null(), &mut node.key, extent);

            assert_eq!(__bch2_btree_node_write(&mut handle, &mut node), 0);
            assert_eq!(node.written, 1);

            let entry = words.as_mut_ptr().add(64).cast::<btree_node_entry>();
            bch2_bset_init_next(&mut node, entry);
            (*entry).keys.journal_seq = 2;
            (*entry).keys.u64s = 10;
            for (index, (offset, type_)) in [(2, 3), (3, 6)].into_iter().enumerate() {
                *words.as_mut_ptr().add(69 + index * 5).cast::<bkey>() = bkey {
                    u64s: BKEY_U64S,
                    format: KEY_FORMAT_CURRENT,
                    type_,
                    p: SPOS(12, offset, 0),
                    ..Default::default()
                };
            }
            set_btree_bset_end(&mut node, node.set.as_mut_ptr().add(1));
            crate::btree::interior::bch2_push_whiteout(&mut node, SPOS(12, 1, 0));
            assert_eq!(__bch2_btree_node_write(&mut handle, &mut node), 0);
            assert_eq!(node.written, 2);
            assert_eq!((*node_ptr).v.sectors_written, 2);

            let mut recovered_words = vec![0u64; 256];
            let mut recovered_aux =
                vec![0u64; crate::btree::types::__btree_aux_data_bytes(11) / 8];
            let mut recovered = btree::default();
            recovered.data = recovered_words.as_mut_ptr().cast();
            recovered.aux_data = recovered_aux.as_mut_ptr().cast();
            recovered.byte_order = 11;
            recovered.c.btree_id = 2;
            recovered.c.level = 0;
            core::ptr::copy_nonoverlapping(
                (&node.key as *const crate::btree::bkey::bkey_i).cast::<u64>(),
                (&mut recovered.key as *mut crate::btree::bkey::bkey_i).cast::<u64>(),
                node.key.k.u64s as usize,
            );
            (*bkey_i_to_btree_ptr_v2(&mut recovered.key)).v.mem_ptr = 0;
            assert_eq!(bch2_btree_node_read(&mut handle, &mut recovered), 0);
            assert_eq!(recovered.written, 2);
            assert_eq!(recovered.nsets, 1);
            assert_eq!((*recovered.data).keys.journal_seq, 2);
            assert_eq!((*recovered.data).keys.u64s, 10);

            let mut iter = btree_node_iter::default();
            bch2_btree_node_iter_init_from_start(&mut iter, &mut recovered);
            let mut seen = Vec::new();
            loop {
                let key = bch2_btree_node_iter_peek(&mut iter, &mut recovered);
                if key.is_null() {
                    break;
                }
                seen.push((bkey_unpack_pos(&recovered, key).offset, (*key).type_));
                bch2_btree_node_iter_advance(&mut iter, &mut recovered);
            }
            assert_eq!(seen, [(2, 3), (3, 6)]);

            let mut byte = [0u8; 1];
            let corrupt_offset = (32 + 1) * 512 + 100;
            assert_eq!(file.read_at(&mut byte, corrupt_offset).unwrap(), 1);
            byte[0] ^= 1;
            assert_eq!(file.write_at(&byte, corrupt_offset).unwrap(), 1);
            assert_eq!(bch2_btree_node_read(&mut handle, &mut recovered), -8);

            bch2_free_super(&mut handle);
            drop(file);
            std::fs::remove_file(path).unwrap();
        }
    }
}
