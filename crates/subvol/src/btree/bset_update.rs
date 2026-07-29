use super::bkey::{
    bch2_bkey_pack_key, bkey_deleted, bkey_i, bkey_p_next, bkey_packed, bkeyp_key_u64s,
    bkeyp_val_u64s,
};
use super::bset::rw_aux_tree;
use super::bset_search::{rw_aux_to_bkey, rw_aux_tree_base, rw_aux_tree_bsearch};
use super::types::{
    __btree_aux_data_bytes, __btree_node_key_to_offset, bset, bset_has_rw_aux_tree, bset_tree,
    bset_tree_last, btree, btree_bkey_first, btree_bkey_last, btree_nr_keys, set_btree_bset_end,
};

const L1_CACHE_BYTES: usize = 64;

unsafe fn bset_rw_tree_capacity(b: *const btree, t: *const bset_tree) -> usize {
    (__btree_aux_data_bytes((*b).byte_order as u32) - (*t).aux_data_offset as usize * 8)
        / core::mem::size_of::<rw_aux_tree>()
}

unsafe fn rw_aux_tree_set(b: *const btree, t: *mut bset_tree, j: usize, k: *mut bkey_packed) {
    assert!((k as usize) < btree_bkey_last(b, t) as usize);
    *rw_aux_tree_base(b, t).add(j) = rw_aux_tree {
        offset: __btree_node_key_to_offset(b, k),
        k: super::node_iter::bkey_unpack_pos(b, k),
    };
}

unsafe fn rw_aux_tree_insert_entry(b: *mut btree, t: *mut bset_tree, idx: usize) {
    assert!(idx != 0 && idx <= (*t).size as usize);
    let start = rw_aux_to_bkey(b, t, idx as u32 - 1);
    let end = if idx < (*t).size as usize {
        rw_aux_to_bkey(b, t, idx as u32)
    } else {
        btree_bkey_last(b, t)
    };

    if ((*t).size as usize) < bset_rw_tree_capacity(b, t)
        && end as usize - start as usize > L1_CACHE_BYTES
    {
        let mut k = start;
        loop {
            k = super::bkey::bkey_p_next(k);
            if k == end {
                break;
            }
            if k as usize - start as usize >= L1_CACHE_BYTES {
                let tree = rw_aux_tree_base(b, t);
                core::ptr::copy(tree.add(idx), tree.add(idx + 1), (*t).size as usize - idx);
                (*t).size += 1;
                rw_aux_tree_set(b, t, idx, k);
                break;
            }
        }
    }
}

unsafe fn __bch2_bset_fix_lookup_table(
    b: *mut btree,
    t: *mut bset_tree,
    where_: *mut bkey_packed,
    clobber_u64s: u32,
    new_u64s: u32,
) {
    let shift = new_u64s as i32 - clobber_u64s as i32;
    let where_offset = __btree_node_key_to_offset(b, where_) as u32;
    let tree = rw_aux_tree_base(b, t);

    if where_offset > (*tree.add((*t).size as usize - 1)).offset as u32 {
        rw_aux_tree_insert_entry(b, t, (*t).size as usize);
        return;
    }

    let mut idx = rw_aux_tree_bsearch(b, t, where_offset) as usize;
    if (*tree.add(idx)).offset as u32 == where_offset {
        if idx == 0 {
            idx += 1;
        } else if where_offset < (*t).end_offset as u32 {
            rw_aux_tree_set(b, t, idx, where_);
            idx += 1;
        } else {
            assert_eq!(where_offset, (*t).end_offset as u32);
            (*t).size -= 1;
            rw_aux_tree_insert_entry(b, t, (*t).size as usize);
            return;
        }
    }

    assert!(idx >= (*t).size as usize || (*tree.add(idx)).offset as u32 > where_offset);
    if idx < (*t).size as usize
        && (*tree.add(idx)).offset as i32 + shift == (*tree.add(idx - 1)).offset as i32
    {
        core::ptr::copy(
            tree.add(idx + 1),
            tree.add(idx),
            (*t).size as usize - idx - 1,
        );
        (*t).size -= 1;
    }

    for j in idx..(*t).size as usize {
        (*tree.add(j)).offset = ((*tree.add(j)).offset as i32 + shift) as u16;
    }
    rw_aux_tree_insert_entry(b, t, idx);
}

unsafe fn bch2_bset_fix_lookup_table(
    b: *mut btree,
    t: *mut bset_tree,
    where_: *mut bkey_packed,
    clobber_u64s: u32,
    new_u64s: u32,
) {
    if bset_has_rw_aux_tree(t) {
        __bch2_bset_fix_lookup_table(b, t, where_, clobber_u64s, new_u64s);
    }
}

pub unsafe fn btree_keys_account_key(
    nr: *mut btree_nr_keys,
    bset_idx: usize,
    k: *const bkey_packed,
    sign: i32,
) {
    let delta = (*k).u64s as i32 * sign;
    (*nr).live_u64s = ((*nr).live_u64s as i32 + delta) as u16;
    (*nr).bset_u64s[bset_idx] = ((*nr).bset_u64s[bset_idx] as i32 + delta) as u16;
    if super::bkey::bkey_packed(&*k) {
        (*nr).packed_keys = ((*nr).packed_keys as i32 + sign) as u16;
    } else {
        (*nr).unpacked_keys = ((*nr).unpacked_keys as i32 + sign) as u16;
    }
}

pub unsafe fn bch2_btree_node_count_keys(b: *mut btree) -> btree_nr_keys {
    let mut nr = btree_nr_keys::default();
    for idx in 0..(*b).nsets as usize {
        let t = (*b).set.as_ptr().add(idx);
        let mut key = btree_bkey_first(b, t);
        let end = btree_bkey_last(b, t);
        while key != end {
            if !bkey_deleted(&*key) {
                btree_keys_account_key(&mut nr, idx, key, 1);
            }
            key = bkey_p_next(key);
        }
    }
    nr
}

pub unsafe fn __bch2_verify_btree_nr_keys(b: *mut btree) {
    assert_eq!(bch2_btree_node_count_keys(b), (*b).nr);
}

pub(crate) unsafe fn bch2_bset_insert(
    b: *mut btree,
    where_: *mut bkey_packed,
    insert: *mut bkey_i,
    clobber_u64s: u32,
) {
    let format = (*b).format;
    let t = bset_tree_last(b);
    let mut packed = bkey_packed::default();
    let mut src = insert.cast::<bkey_packed>();

    if bch2_bkey_pack_key(&mut packed, &(*insert).k, &format) {
        src = &mut packed;
    }

    if !bkey_deleted(&*(insert.cast::<bkey_packed>())) {
        let bset_idx = t.offset_from((*b).set.as_mut_ptr()) as usize;
        btree_keys_account_key(&mut (*b).nr, bset_idx, src, 1);
    }

    if (*src).u64s as u32 != clobber_u64s {
        let src_p = (where_ as *mut u64).add(clobber_u64s as usize);
        let dst_p = (where_ as *mut u64).add((*src).u64s as usize);
        let end = btree_bkey_last(b, t) as *mut u64;
        let count = end.offset_from(src_p) as usize;
        core::ptr::copy(src_p, dst_p, count);

        let disk_set = bset(b, t);
        let new_u64s = (*disk_set).u64s as i32 + (*src).u64s as i32 - clobber_u64s as i32;
        assert!(new_u64s >= 0 && new_u64s <= u16::MAX as i32);
        (*disk_set).u64s = new_u64s as u16;
        set_btree_bset_end(b, t);
    }

    let key_u64s = bkeyp_key_u64s(&format, &*src) as usize;
    core::ptr::copy_nonoverlapping(src as *const u64, where_ as *mut u64, key_u64s);
    let val_u64s = bkeyp_val_u64s(&format, &*src) as usize;
    core::ptr::copy_nonoverlapping(
        (insert as *const u64).add(5),
        (where_ as *mut u64).add(key_u64s),
        val_u64s,
    );

    if (*src).u64s as u32 != clobber_u64s {
        bch2_bset_fix_lookup_table(b, t, where_, clobber_u64s, (*src).u64s as u32);
    }
}

pub(crate) unsafe fn bch2_bset_delete(b: *mut btree, where_: *mut bkey_packed, clobber_u64s: u32) {
    let t = bset_tree_last(b);
    let src_p = (where_ as *mut u64).add(clobber_u64s as usize);
    let dst_p = where_ as *mut u64;
    let end = btree_bkey_last(b, t) as *mut u64;
    core::ptr::copy(src_p, dst_p, end.offset_from(src_p) as usize);

    let disk_set = bset(b, t);
    assert!((*disk_set).u64s as u32 >= clobber_u64s);
    (*disk_set).u64s -= clobber_u64s as u16;
    set_btree_bset_end(b, t);
    bch2_bset_fix_lookup_table(b, t, where_, clobber_u64s, 0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btree::bkey::{bkey, BKEY_FORMAT_CURRENT, BKEY_U64S, KEY_FORMAT_CURRENT, SPOS};
    use crate::btree::bset::{bset as disk_bset, btree_node as disk_btree_node};
    use crate::btree::node_iter::{
        bch2_btree_node_iter_advance, bch2_btree_node_iter_init_from_start,
        bch2_btree_node_iter_peek, bkey_unpack_pos,
    };
    use crate::btree::types::{bset_tree, btree_node_iter, BSET_NO_AUX_TREE_VAL};

    #[test]
    fn inserts_replaces_and_deletes_in_last_bset() {
        let mut words = vec![0u64; 64];
        let mut b = btree::default();
        b.data = words.as_mut_ptr() as *mut disk_btree_node;
        b.format = BKEY_FORMAT_CURRENT;
        b.nsets = 1;
        unsafe {
            let disk_set = words.as_mut_ptr().add(17) as *mut disk_bset;
            (*disk_set).u64s = 0;
            b.set[0] = bset_tree {
                size: 0,
                extra: BSET_NO_AUX_TREE_VAL,
                data_offset: 17,
                aux_data_offset: u16::MAX,
                end_offset: 20,
            };

            let mut first = bkey_i {
                k: bkey {
                    u64s: BKEY_U64S,
                    format: KEY_FORMAT_CURRENT,
                    type_: 1,
                    p: SPOS(1, 2, 0),
                    ..Default::default()
                },
                ..Default::default()
            };
            bch2_bset_insert(&mut b, words.as_mut_ptr().add(20).cast(), &mut first, 0);
            let mut second = bkey_i {
                k: bkey {
                    u64s: BKEY_U64S,
                    format: KEY_FORMAT_CURRENT,
                    type_: 1,
                    p: SPOS(1, 4, 0),
                    ..Default::default()
                },
                ..Default::default()
            };
            bch2_bset_insert(&mut b, words.as_mut_ptr().add(25).cast(), &mut second, 0);
            assert_eq!((*disk_set).u64s, 10);
            assert_eq!(b.nr.live_u64s, 10);

            let mut iter = btree_node_iter::default();
            bch2_btree_node_iter_init_from_start(&mut iter, &mut b);
            let first_key = bch2_btree_node_iter_peek(&mut iter, &mut b);
            let first_pos = bkey_unpack_pos(&b, first_key);
            let first_offset = first_pos.offset;
            assert_eq!(first_offset, 2);
            bch2_btree_node_iter_advance(&mut iter, &mut b);
            let second_key = bch2_btree_node_iter_peek(&mut iter, &mut b);
            let second_pos = bkey_unpack_pos(&b, second_key);
            let second_offset = second_pos.offset;
            assert_eq!(second_offset, 4);

            bch2_bset_delete(&mut b, words.as_mut_ptr().add(20).cast(), 5);
            assert_eq!((*disk_set).u64s, 5);
            let remaining = words.as_mut_ptr().add(20).cast();
            let remaining_pos = bkey_unpack_pos(&b, remaining);
            let remaining_offset = remaining_pos.offset;
            assert_eq!(remaining_offset, 4);
        }
    }
}
