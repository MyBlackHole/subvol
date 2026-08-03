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

pub unsafe fn bch2_btree_node_wait_on_read(_trans: *mut super::iter::btree_trans, b: *mut btree) {
    assert!(!b.is_null());
    while super::types::btree_node_read_in_flight(b) {
        std::thread::yield_now();
    }
}

pub unsafe fn bch2_btree_node_wait_on_write(_trans: *mut super::iter::btree_trans, b: *mut btree) {
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
            || (flags & BTREE_WRITE_only_if_need != 0 && !super::types::btree_node_need_write(b))
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
    crate::rewrite_log_debug!(
        "btree node read: disk seq={} key v.seq={}",
        node.keys.seq,
        (*btree_ptr).v.seq
    );
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
        let mut key_u64s = (*set).u64s as usize;
        let mut bytes = header_bytes + key_u64s * 8;
        let mut sectors = bytes.next_multiple_of(block_bytes) / 512;
        if written + sectors > limit || written * 512 + bytes > node_bytes {
            // 对齐 read.c bset_past_end_of_btree_node（FSCK_CAN_FIX）：截断该 bset
            crate::rewrite_log_error!("btree node read: bset past end, truncating");
            (*set).u64s = 0;
            key_u64s = 0;
            bytes = header_bytes;
            sectors = bytes.next_multiple_of(block_bytes) / 512;
        }
        let csum_type = BSET_CSUM_TYPE(&*set);
        let good_csum_type = csum_type < BCH_CSUM_NR;
        if !good_csum_type {
            // 对齐 read.c bset_unknown_csum（FSCK_CAN_FIX，域内无加密 csum）：跳过校验
            crate::rewrite_log_error!("btree node read: unknown csum type {csum_type}");
        }
        if good_csum_type {
            let header = if first {
                node as *mut btree_node as *mut u8
            } else {
                buffer.as_mut_ptr().add(written * 512)
            };
            let checksum_data = core::slice::from_raw_parts(header.add(16), bytes - 16);
            if bch2_checksum(csum_type, checksum_data) != expected_csum {
                // 对齐 read.c bset_bad_csum（FSCK_CAN_FIX）：继续
                crate::rewrite_log_error!("btree node read: bad csum");
            }
        }
        if (*set).version != bcachefs_metadata_version_current {
            return -9;
        }
        (*b).version_ondisk = (*b).version_ondisk.min((*set).version);
        if BSET_SEPARATE_WHITEOUTS(&*set) != 0 || BSET_BIG_ENDIAN(&*set) != 0 {
            return -10;
        }
        if BSET_OFFSET(&*set) != written as u32 {
            // 对齐 read.c bset_wrong_sector_offset（FSCK_CAN_FIX）：继续
            crate::rewrite_log_error!("btree node read: bset wrong sector offset");
        }
        max_journal_seq = max_journal_seq.max((*set).journal_seq);

        let mut key = set.cast::<u64>().add(3).cast::<super::bkey::bkey_packed>();
        let key_start = key;
        let mut end = (key as *mut u64)
            .add(key_u64s)
            .cast::<super::bkey::bkey_packed>();
        let mut prev: *mut super::bkey::bkey_packed = core::ptr::null_mut();
        while key < end {
            if bkey_p_next(key) > end {
                // 对齐 read.c btree_node_bkey_past_bset_end（FSCK_CAN_FIX）：截断到当前键
                (*set).u64s = (key as *mut u64).offset_from(set.cast::<u64>().add(3)) as u16;
                end = key;
                break;
            }
            let mut drop_key = false;
            if (*key).u64s == 0 || !bkeyp_u64s_valid(&(*b).format, &*key) {
                // 对齐 read.c btree_node_bkey_bad_u64s（FSCK_CAN_FIX）：drop_this_key
                drop_key = true;
            } else {
                let pos = super::node_iter::bkey_unpack_pos(b, key);
                if bpos_lt(pos, node.min_key) || bpos_gt(pos, node.max_key) {
                    // 对齐 read.c bch2_bkey_in_btree_node → fsck_delete_bkey：drop_this_key
                    drop_key = true;
                } else if !prev.is_null()
                    && bpos_cmp(super::node_iter::bkey_unpack_pos(b, prev), pos) >= 0
                {
                    // 对齐 read.c btree_node_bkey_out_of_order（FSCK_CAN_FIX）：drop_this_key
                    drop_key = true;
                } else if (*key).type_ == KEY_TYPE_btree_ptr_v2 {
                    let key_words = bkeyp_key_u64s(&(*b).format, &*key) as usize;
                    let value_words = (*key).u64s as usize - key_words;
                    if value_words < core::mem::size_of::<super::bset::bch_btree_ptr_v2>() / 8 {
                        // 对齐 read.c btree_node_bkey_val_validate → fsck_delete_bkey：drop_this_key
                        drop_key = true;
                    }
                }
            }
            if drop_key {
                // 对齐 read.c drop_this_key：删当前键，扫描后续好键，无好键则截断剩余
                crate::rewrite_log_debug!("btree node read: dropping bad key");
                let mut next_good_key = (*key).u64s as usize;
                if !read_bkey_packed_valid(b, (key as *mut u64).add(next_good_key).cast(), end) {
                    // 扫描找下一个好键
                    let total = (end as *mut u64).offset_from(key as *mut u64) as usize;
                    let mut found = 0usize;
                    for cand in 1..total {
                        if read_bkey_packed_valid(b, (key as *mut u64).add(cand).cast(), end) {
                            found = cand;
                            break;
                        }
                    }
                    if found != 0 {
                        next_good_key = found;
                    } else {
                        // 没找到好键，截断剩余
                        next_good_key = total;
                    }
                }
                crate::rewrite_log_debug!(
                    "drop: set u64s before={} next_good_key={} total={}",
                    (*set).u64s,
                    next_good_key,
                    (end as *mut u64).offset_from(key as *mut u64)
                );
                (*set).u64s -= next_good_key as u16;
                let remaining =
                    (end as *mut u64).offset_from(key as *mut u64) as usize - next_good_key;
                core::ptr::copy(
                    (key as *mut u64).add(next_good_key),
                    key as *mut u64,
                    remaining,
                );
                end = (key as *mut u64).add(remaining).cast();
                super::types::set_btree_node_need_rewrite(b);
                super::types::set_btree_node_need_rewrite_error(b);
                if key >= end {
                    break;
                }
                continue;
            }
            if (*key).type_ == KEY_TYPE_btree_ptr_v2 {
                let key_words = bkeyp_key_u64s(&(*b).format, &*key) as usize;
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
        // 对齐 read.c btree_node_data_missing（FSCK_CAN_FIX）：报告后继续
        crate::rewrite_log_error!(
            "btree node read: data missing: expected {ptr_written} sectors, found {written}"
        );
    }
    if ptr_written == 0 {
        let mut trailing = written;
        while trailing < node_bytes / 512 {
            let entry = buffer
                .as_ptr()
                .add(trailing * 512)
                .cast::<btree_node_entry>();
            if (*entry).keys.seq == node.keys.seq {
                // 对齐 read.c btree_node_bset_after_end（FSCK_CAN_FIX）：报告后继续
                crate::rewrite_log_error!("btree node read: bset signature after last bset");
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
    if ptr_written == 0 {
        // 对齐 read.c bch2_btree_node_read_done（read.c:871-872）：
        // !ptr_written → need_rewrite + need_rewrite_ptr_written_zero
        super::types::set_btree_node_need_rewrite(b);
        super::types::set_btree_node_need_rewrite_ptr_written_zero(b);
    }
    crate::rewrite_log_debug!(
        "btree node read complete level={} sets={} written={written}",
        (*b).c.level,
        (*b).nsets
    );
    0
}

/// 对齐 read.c bkey_packed_valid：检查一个 packed key 是否结构有效
unsafe fn read_bkey_packed_valid(
    b: *mut btree,
    k: *mut super::bkey::bkey_packed,
    end: *mut super::bkey::bkey_packed,
) -> bool {
    if k.is_null() || k >= end || bkey_p_next(k) > end {
        return false;
    }
    if (*k).format > super::bkey::KEY_FORMAT_CURRENT {
        return false;
    }
    if !bkeyp_u64s_valid(&(*b).format, &*k) {
        return false;
    }
    let pos = super::node_iter::bkey_unpack_pos(b, k);
    let node = &*(*b).data;
    !bpos_lt(pos, node.min_key) && !bpos_gt(pos, node.max_key)
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
    /* read.c:968 读完成触发：读成功且节点标记 need_rewrite →
     * ASYNC_BTREE_rewrite。上游经 async work（interior.c:3406）
     * 执行（不持锁）；域内同步执行会与外层路径锁互斥死锁（实测，
     * AC-3），故此处仅入队 btree.node_rewrites（对齐上游 a->key
     * bkey_buf 拷贝语义），由无锁时机（root_read 末尾 / engine
     * 操作边界）bch2_do_pending_node_rewrites 执行 */
    if super::types::btree_node_need_rewrite(node) {
        super::interior::bch2_btree_node_need_rewrite_add(c, node);
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
        crate::lock::six::six_lock_type::SIX_LOCK_intent => super::iter::BTREE_NODE_INTENT_LOCKED,
        crate::lock::six::six_lock_type::SIX_LOCK_read => super::iter::BTREE_NODE_READ_LOCKED,
        crate::lock::six::six_lock_type::SIX_LOCK_write => super::iter::BTREE_NODE_WRITE_LOCKED,
    };
    if super::iter::btree_node_lock_type(trans, path, node, level as usize, path_lock) != 0 {
        return core::ptr::null_mut();
    }

    if (*node).c.btree_id != (*path).btree_id || (*node).c.level != level {
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
        (&mut (*super::types::bch2_btree_id_root(c, id as usize)).key as *mut super::bkey::bkey_i)
            .cast::<u64>(),
        (*key).k.u64s as usize,
    );
    super::interior::bch2_btree_set_root_for_read(c, node);
    six_unlock_write(&(*node).c.lock);
    six_unlock_intent(&(*node).c.lock);
    /* read.c:968 读完成触发（root 场景）：入队后立即 drain——root_read
     * 上下文无外层路径锁（节点读写锁已释放），可安全执行重写；队列中
     * 若有 get 路径残留项亦一并处理（对齐 interior.c:3462 上游 drain
     * 语义） */
    if super::types::btree_node_need_rewrite(node) {
        super::interior::bch2_btree_node_need_rewrite_add(c, node);
    }
    super::interior::bch2_do_pending_node_rewrites(c);
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
            let path = std::env::temp_dir().join(format!("subvol-btree-io-{}", std::process::id()));
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
            // 破坏第二个键的 u64s 字段（5 ^ 1 = 4 < key_u64s）：触发 drop_this_key
            assert_eq!(file.read_at(&mut byte, 64 * 512 + 200).unwrap(), 1);
            byte[0] ^= 1;
            assert_eq!(file.write_at(&byte, 64 * 512 + 200).unwrap(), 1);
            assert_eq!(bch2_btree_node_read(&mut handle, &mut node), 0);
            assert!(crate::btree::types::btree_node_need_rewrite(&node));
            assert!(crate::btree::types::btree_node_need_rewrite_error(&node));
            assert_eq!((*node.data).keys.u64s, 5);
            assert_eq!(node.nr.unpacked_keys, 1);
            assert_eq!((*(words.as_ptr().add(20).cast::<bkey>())).p, SPOS(4, 1, 0));

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
            let path =
                std::env::temp_dir().join(format!("subvol-btree-root-{}", std::process::id()));
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
            let path =
                std::env::temp_dir().join(format!("subvol-btree-multiset-{}", std::process::id()));
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
            let mut recovered_aux = vec![0u64; crate::btree::types::__btree_aux_data_bytes(11) / 8];
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
            // 破坏第二 bset 第二个键的 u64s 字段（5 ^ 1 = 4 < key_u64s）：触发 drop_this_key
            let corrupt_offset = 33 * 512 + 80;
            assert_eq!(file.read_at(&mut byte, corrupt_offset).unwrap(), 1);
            byte[0] ^= 1;
            assert_eq!(file.write_at(&byte, corrupt_offset).unwrap(), 1);
            assert_eq!(bch2_btree_node_read(&mut handle, &mut recovered), 0);
            assert!(crate::btree::types::btree_node_need_rewrite(&recovered));
            assert!(crate::btree::types::btree_node_need_rewrite_error(
                &recovered
            ));
            // whiteout 键使第二 bset 布局前移：破坏位置命中 (12,2,0)type3 键；
            // drop 后第一 bset 的 (12,2,0)type6 保留，与 (3,6) 合并
            assert_eq!((*recovered.data).keys.u64s, 10);
            assert_eq!(recovered.nr.unpacked_keys, 2);
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
            assert_eq!(seen, [(2, 6), (3, 6)]);

            bch2_free_super(&mut handle);
            drop(file);
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn root_read_need_rewrite_triggers_sync_rewrite() {
        use crate::btree::node_iter::{
            bch2_btree_node_iter_init_from_start, bch2_btree_node_iter_peek,
            bch2_btree_node_iter_peek_all, bkey_unpack_pos,
        };
        use crate::btree::types::{bch_fs, btree_node_iter};
        use std::os::unix::fs::FileExt;

        unsafe {
            /* AC-3（read.c:968 语义）：读完成触发重写。构造单节点树
             * （root 即叶，level 0，2 键 seq=101 @ 偏移 64），破坏第 2
             * 键 u64s（5→4，AC-2 同款模式）→ bch2_btree_root_read 读
             * 成功（截断修复 + need_rewrite）→ 读完成点同步触发
             * bch2_btree_node_rewrite_key → root 分支 set_root_for_read
             * 替换 root slot（新节点 seq=102）。 */
            let path =
                std::env::temp_dir().join(format!("subvol-root-rewrite-{}", std::process::id()));
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

            let mut root_words = vec![0u64; 64];
            let mut root = Box::new(btree::default());
            root.data = root_words.as_mut_ptr().cast();
            root.byte_order = 9;
            root.c.btree_id = 0;
            root.c.level = 0;
            root.format = BKEY_FORMAT_CURRENT;
            root.nr_key_bits = bkey_format_key_bits(&root.format) as u8;
            root.nsets = 1;
            (*root.data).min_key = POS_MIN;
            (*root.data).max_key = SPOS_MAX;
            (*root.data).keys.seq = 101;
            (*root.data).keys.u64s = 10;
            for (index, offset) in [1, 2].into_iter().enumerate() {
                *root_words.as_mut_ptr().add(20 + index * 5).cast::<bkey>() = bkey {
                    u64s: BKEY_U64S,
                    format: KEY_FORMAT_CURRENT,
                    type_: 6,
                    p: SPOS(9, offset, 0),
                    ..Default::default()
                };
            }
            root.set[0] = bset_tree {
                size: 0,
                extra: BSET_NO_AUX_TREE_VAL,
                data_offset: 17,
                aux_data_offset: u16::MAX,
                end_offset: 30,
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
            (*root_ptr).v.seq = 101;
            (*root_ptr).v.min_key = (*root.data).min_key;
            let mut root_extent = bch_extent_ptr::default();
            SET_BCH_EXTENT_PTR_OFFSET(&mut root_extent, 64);
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

            let corrupt_offset = 64 * 512 + 200;
            let mut byte = [0u8; 1];
            assert_eq!(file.read_at(&mut byte, corrupt_offset).unwrap(), 1);
            byte[0] ^= 1;
            assert_eq!(file.write_at(&byte, corrupt_offset).unwrap(), 1);

            let mut recovered = bch_fs::default();
            recovered.disk_sb.s_bdev_file =
                Box::into_raw(Box::new(file.try_clone().unwrap())).cast();
            assert_eq!(bch2_sb_realloc(&mut recovered.disk_sb, 0), 0);
            (*recovered.disk_sb.sb).uuid = [0x71; 16];
            (*recovered.disk_sb.sb).dev_idx = 0;
            (*recovered.disk_sb.sb).flags[0] = 1 << 12;
            assert_eq!(bch2_btree_root_read(&mut recovered, 0, root_key, 0), 0);

            /* 重写触发证据：root slot 已由 rewrite 的 root 分支替换为
             * 新节点（seq=101+1=102，interior.rs root 分支）。重写会
             * 重新打包键（bch2_btree_sort_into + transform），故新节点
             * 为 1 个 packed 键（u64s=1），unpacked 计数为 0。 */
            let recovered_root = crate::btree::types::bch2_btree_id_root_b(&recovered, 0);
            assert!(!recovered_root.is_null());
            let slot_ptr = bkey_i_to_btree_ptr_v2(
                &mut (*crate::btree::types::bch2_btree_id_root(&mut recovered, 0)).key,
            );
            assert_eq!((*slot_ptr).v.seq, 102);
            assert_eq!((*(*recovered_root).data).keys.u64s, 1);
            assert_eq!((*recovered_root).nr.packed_keys, 1);
            assert_eq!((*recovered_root).nr.unpacked_keys, 0);
            /* 新节点键集 = 修复后内容（坏键截断，仅剩键 1 @ SPOS(9,1,0)） */
            let mut iter = btree_node_iter::default();
            bch2_btree_node_iter_init_from_start(&mut iter, recovered_root);
            let key = bch2_btree_node_iter_peek(&mut iter, recovered_root);
            assert!(!key.is_null());
            assert_eq!(bkey_unpack_pos(recovered_root, key), SPOS(9, 1, 0));
            crate::btree::node_iter::bch2_btree_node_iter_advance(&mut iter, recovered_root);
            let next = bch2_btree_node_iter_peek(&mut iter, recovered_root);
            assert!(next.is_null());

            bch2_free_super(&mut write_handle);
            drop(file);
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn rewritten_node_revalidates_on_reopen() {
        use crate::btree::iter::{bch2_trans_init, btree_trans};
        use crate::btree::node_iter::{
            bch2_btree_node_iter_advance, bch2_btree_node_iter_init_from_start,
            bch2_btree_node_iter_peek, bkey_unpack_pos,
        };
        use crate::btree::types::{bch_fs, btree_node_iter};
        use std::os::unix::fs::FileExt;

        unsafe {
            /* AC-5（read.c:1233 scrub_work 语义）：重写后重新校验必须
             * 通过。构造 root 即叶（2 键 seq=101 @ 64）→ 损坏键 2
             * u64s → root_read 触发重写（root 分支 set_root_for_read
             * 更新 slot，新节点 seq=102，key 继承旧 extent @ 64，
             * interior.rs child_ptr）→ 模拟提交 flush 写盘（对齐
             * __btree_node_flush，commit.c:254）→ 模拟关闭后重开：
             * 从写盘后的节点 key 序列化 root 记录（io.c
             * bch2_write_super 语义，mem_ptr 清零 = 持久化记录）→
             * 第二次 root_read 重新读盘校验（magic/seq/level/范围
             * 逐项，io.rs:478-503）→ 断言：读解析通过、无
             * need_rewrite（磁盘字节干净）、seq=102 持久化、键集
             * = 修复后内容、拓扑校验通过。 */
            let path =
                std::env::temp_dir().join(format!("subvol-rewrite-reopen-{}", std::process::id()));
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

            let mut root_words = vec![0u64; 64];
            let mut root = Box::new(btree::default());
            root.data = root_words.as_mut_ptr().cast();
            root.byte_order = 9;
            root.c.btree_id = 0;
            root.c.level = 0;
            root.format = BKEY_FORMAT_CURRENT;
            root.nr_key_bits = bkey_format_key_bits(&root.format) as u8;
            root.nsets = 1;
            (*root.data).min_key = POS_MIN;
            (*root.data).max_key = SPOS_MAX;
            (*root.data).keys.seq = 101;
            (*root.data).keys.u64s = 10;
            for (index, offset) in [1, 2].into_iter().enumerate() {
                *root_words.as_mut_ptr().add(20 + index * 5).cast::<bkey>() = bkey {
                    u64s: BKEY_U64S,
                    format: KEY_FORMAT_CURRENT,
                    type_: 6,
                    p: SPOS(9, offset, 0),
                    ..Default::default()
                };
            }
            root.set[0] = bset_tree {
                size: 0,
                extra: BSET_NO_AUX_TREE_VAL,
                data_offset: 17,
                aux_data_offset: u16::MAX,
                end_offset: 30,
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
            (*root_ptr).v.seq = 101;
            (*root_ptr).v.min_key = (*root.data).min_key;
            let mut root_extent = bch_extent_ptr::default();
            SET_BCH_EXTENT_PTR_OFFSET(&mut root_extent, 64);
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

            let corrupt_offset = 64 * 512 + 200;
            let mut byte = [0u8; 1];
            assert_eq!(file.read_at(&mut byte, corrupt_offset).unwrap(), 1);
            byte[0] ^= 1;
            assert_eq!(file.write_at(&byte, corrupt_offset).unwrap(), 1);

            /* 第一次恢复：root_read 触发重写（root 分支覆盖写盘） */
            let mut recovered1 = bch_fs::default();
            recovered1.disk_sb.s_bdev_file =
                Box::into_raw(Box::new(file.try_clone().unwrap())).cast();
            assert_eq!(bch2_sb_realloc(&mut recovered1.disk_sb, 0), 0);
            (*recovered1.disk_sb.sb).uuid = [0x71; 16];
            (*recovered1.disk_sb.sb).dev_idx = 0;
            (*recovered1.disk_sb.sb).flags[0] = 1 << 12;
            assert_eq!(bch2_btree_root_read(&mut recovered1, 0, root_key, 0), 0);
            let slot1 = crate::btree::types::bch2_btree_id_root(&mut recovered1, 0);
            let root1 = (*slot1).b;
            assert_eq!((*(*root1).data).keys.seq, 102);
            assert!(!crate::btree::types::btree_node_need_rewrite(root1));
            crate::rewrite_log_debug!(
                "AC-5 slot1 key u64s={} type={}",
                (*slot1).key.k.u64s,
                (*slot1).key.k.type_
            );
            let s1p = crate::btree::bset::bch2_bkey_ptrs_c(crate::btree::bkey::bkey_s_c {
                k: &(*slot1).key.k,
                v: &(*slot1).key.v,
            });
            crate::rewrite_log_debug!(
                "AC-5 slot1 ptrs start={} end={}",
                s1p.start as usize,
                s1p.end as usize
            );

            /* AC-5：重写仅 set_dirty（journal-first，节点落盘由事务
             * 提交/日志 flush 驱动）。对齐 __btree_node_flush
             * （fs/btree/commit.c:254 → bch2_btree_node_write_trans）
             * 语义，此处模拟重写提交后的持久化写盘，使磁盘字节 =
             * 修复后内容，随后重开才能读回验证。 */
            assert!(crate::btree::types::btree_node_dirty(root1));
            let mut write_trans = btree_trans::default();
            bch2_trans_init(&mut write_trans, &mut recovered1);
            crate::btree::io::bch2_btree_node_write_trans(
                &mut write_trans,
                root1,
                crate::lock::six::six_lock_type::SIX_LOCK_write,
                BTREE_WRITE_initial,
            );
            assert!(!crate::btree::types::btree_node_dirty(root1));

            /* 模拟关闭后重开：对齐上游关闭时从内存节点 key 序列化
             * root 记录（io.c bch2_write_super → bch2_btree_roots），
             * 写盘后 root1.key 含最新 seq=102 与 sectors_written
             * （io.rs:431），mem_ptr 清零（跨进程无效）后重放。
             * 不能取 slot.key：其为重写时快照（sectors_written=0），
             * 重开时 ptr_written==0 会触发二次重写（seq 103）。 */
            let mut new_root_key_words = [0u64; 20];
            core::ptr::copy_nonoverlapping(
                (&(*root1).key as *const crate::btree::bkey::bkey_i).cast::<u64>(),
                new_root_key_words.as_mut_ptr(),
                (*root1).key.k.u64s as usize,
            );
            let new_root_key = new_root_key_words
                .as_mut_ptr()
                .cast::<crate::btree::bkey::bkey_i>();
            (*bkey_i_to_btree_ptr_v2(new_root_key)).v.mem_ptr = 0;

            let mut recovered2 = bch_fs::default();
            recovered2.disk_sb.s_bdev_file =
                Box::into_raw(Box::new(file.try_clone().unwrap())).cast();
            assert_eq!(bch2_sb_realloc(&mut recovered2.disk_sb, 0), 0);
            (*recovered2.disk_sb.sb).uuid = [0x71; 16];
            (*recovered2.disk_sb.sb).dev_idx = 0;
            (*recovered2.disk_sb.sb).flags[0] = 1 << 12;
            assert_eq!(bch2_btree_root_read(&mut recovered2, 0, new_root_key, 0), 0);

            /* AC-5：重写后重新读盘校验通过——magic/seq(102)/范围/
             * 格式逐项匹配（io.rs:478-503），无 need_rewrite（磁盘
             * 字节干净），键集 = 修复后内容（键 1 @ SPOS(9,1,0)） */
            let root2 = crate::btree::types::bch2_btree_id_root_b(&recovered2, 0);
            assert!(!root2.is_null());
            assert!(!crate::btree::types::btree_node_need_rewrite(root2));
            assert_eq!((*(*root2).data).keys.seq, 102);
            assert_eq!((*(*root2).data).keys.u64s, 1);
            let mut iter = btree_node_iter::default();
            bch2_btree_node_iter_init_from_start(&mut iter, root2);
            let key = bch2_btree_node_iter_peek(&mut iter, root2);
            assert!(!key.is_null());
            assert_eq!(bkey_unpack_pos(root2, key), SPOS(9, 1, 0));
            bch2_btree_node_iter_advance(&mut iter, root2);
            assert!(bch2_btree_node_iter_peek(&mut iter, root2).is_null());

            /* AC-5：重写后拓扑校验通过（bch2_btree_node_check_topology） */
            let mut trans = btree_trans::default();
            bch2_trans_init(&mut trans, &mut recovered2);
            assert_eq!(
                crate::btree::interior::bch2_btree_node_check_topology(&mut trans, root2),
                0
            );

            bch2_free_super(&mut write_handle);
            drop(file);
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn child_read_need_rewrite_triggers_sync_rewrite_via_iter() {
        use crate::btree::iter::{
            bch2_btree_iter_peek, bch2_trans_init, bch2_trans_iter_exit, bch2_trans_iter_init,
            bch2_trans_put, btree_iter, btree_trans,
        };
        use crate::btree::types::bch_fs;
        use std::os::unix::fs::FileExt;

        unsafe {
            /* AC-3（read.c:968 语义）：get 路径读完成触发重写。布局
             * 同 root_read 测试：leaf（level 0，2 键 seq=101 @ 64）+
             * root（level 1，含 leaf 指针键 @ 72）。损坏 leaf 第 2 键
             * u64s → 经 bch2_btree_iter_peek 遍历触发 leaf 读取（修复 +
             * need_rewrite）→ 读完成点入队 → 无锁时机 drain 执行
             * rewrite_key → parent 分支更新 root 中 child 指针
             * （seq 101→102）。实测：读路径内同步执行会与外层路径
             * 锁互斥死锁，故入队延迟（对齐上游 async work 解耦）。 */
            let path =
                std::env::temp_dir().join(format!("subvol-child-rewrite-{}", std::process::id()));
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

            let corrupt_offset = 64 * 512 + 200;
            let mut byte = [0u8; 1];
            assert_eq!(file.read_at(&mut byte, corrupt_offset).unwrap(), 1);
            byte[0] ^= 1;
            assert_eq!(file.write_at(&byte, corrupt_offset).unwrap(), 1);

            let mut recovered = bch_fs::default();
            recovered.disk_sb.s_bdev_file =
                Box::into_raw(Box::new(file.try_clone().unwrap())).cast();
            assert_eq!(bch2_sb_realloc(&mut recovered.disk_sb, 0), 0);
            (*recovered.disk_sb.sb).uuid = [0x71; 16];
            (*recovered.disk_sb.sb).dev_idx = 0;
            (*recovered.disk_sb.sb).flags[0] = 1 << 12;
            assert_eq!(bch2_btree_root_read(&mut recovered, 0, root_key, 1), 0);

            /* 经 iter 遍历触发 leaf 读取（get 路径）→ 读完成点触发重写 */
            let mut trans = btree_trans::default();
            bch2_trans_init(&mut trans, &mut recovered);
            let mut iter = btree_iter::default();
            bch2_trans_iter_init(&mut trans, &mut iter, 0, SPOS(9, 1, 0), 0);
            let found = bch2_btree_iter_peek(&mut iter);
            assert_eq!(crate::btree::bkey::bkey_err(found), 0);
            assert!(!found.k.is_null());
            assert_eq!((*found.k).p, SPOS(9, 1, 0));
            bch2_trans_iter_exit(&mut iter);
            bch2_trans_put(&mut trans);

            /* 读完成点已入队（无锁上下文执行会与外层路径锁死锁，
             * 上游以 async work 解耦，域内延迟到操作边界）；
             * 无 engine 时手动 drain（等价 engine 操作返回前的
             * EngineFsGuard::drop 时机） */
            crate::btree::interior::bch2_do_pending_node_rewrites(&mut recovered);

            /* 重写证据：root（内存）中 child 指针键 seq 101→102（parent
             * 分支 bch2_btree_insert_node 替换旧键） */
            let recovered_root = crate::btree::types::bch2_btree_id_root_b(&recovered, 0);
            assert!(!recovered_root.is_null());
            let child_on_disk = ((*recovered_root).data as *mut u64)
                .add(20)
                .cast::<crate::btree::bset::bkey_i_btree_ptr_v2>();
            assert_eq!((*child_on_disk).v.seq, 102);

            bch2_free_super(&mut write_handle);
            drop(file);
            std::fs::remove_file(path).unwrap();
        }
    }
}
