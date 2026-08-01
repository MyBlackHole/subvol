use super::bkey::{
    __bch2_bkey_unpack_key, __bkey_unpack_pos, bkey, bkey_deleted, bkey_pack_pos_ret, bkey_packed,
    bkey_packed as bkey_packed_type, bpos, bpos_cmp,
};
use super::bset_search::{
    __bch2_bset_search, bch2_bkey_prev_all, bch2_bset_search_linear, prepare_search_key,
};
use super::types::{
    __btree_node_iter_set_end, __btree_node_iter_used, __btree_node_key_to_offset,
    __btree_node_offset_to_key, bch2_btree_node_iter_end, bch2_btree_node_iter_set_drop, bch_fs,
    btree, btree_bkey_first, btree_bkey_last, btree_node_iter, btree_node_iter_set,
    btree_node_iter_set_find, MAX_BSETS,
};

pub unsafe fn bkey_unpack_pos(b: *const btree, k: *const bkey_packed_type) -> bpos {
    if bkey_packed(&*k) {
        __bkey_unpack_pos(&(*b).format, &*k)
    } else {
        (*(k as *const bkey)).p
    }
}

unsafe fn __bkey_cmp_bits(mut l: *const u64, mut r: *const u64, mut nr_key_bits: u32) -> i32 {
    while nr_key_bits >= 64 {
        let l_v = *l;
        let r_v = *r;
        nr_key_bits -= 64;
        if l_v != r_v {
            return if l_v > r_v { 1 } else { -1 };
        }
        l = l.sub(1);
        r = r.sub(1);
    }

    if nr_key_bits == 0 {
        0
    } else {
        let l_v = *l >> (64 - nr_key_bits);
        let r_v = *r >> (64 - nr_key_bits);
        if l_v > r_v {
            1
        } else if l_v < r_v {
            -1
        } else {
            0
        }
    }
}

pub unsafe fn bch2_bkey_cmp_packed(
    b: *const btree,
    l: *const bkey_packed_type,
    r: *const bkey_packed_type,
) -> i32 {
    if bkey_packed(&*l) && bkey_packed(&*r) {
        assert_eq!(
            (*b).nr_key_bits as u32,
            super::bkey::bkey_format_key_bits(&(*b).format)
        );
        let high_word_offset = (*b).format.key_u64s as usize - 1;
        __bkey_cmp_bits(
            (l as *const u64).add(high_word_offset),
            (r as *const u64).add(high_word_offset),
            (*b).nr_key_bits as u32,
        )
    } else {
        let mut unpacked = bkey::default();
        let (l, r) = if bkey_packed(&*l) {
            if unsafe { (*l).format } & 0x7f != super::bkey::KEY_FORMAT_LOCAL_BTREE {
                let bset_u64s = unsafe { (*(*b).data).keys.u64s };
                let set0 = unsafe { &*(*b).set.as_ptr() };
                let data_off = (l as usize).wrapping_sub(unsafe { (*b).data as usize });
                let words = unsafe {
                    core::slice::from_raw_parts(
                        ((*b).data as *const u64).add(set0.data_offset as usize),
                        78,
                    )
                };
                crate::rewrite_log_error!(
                    "cmp_packed bad key: b={b:p} data={:p} byte_order={} id={} level={} key={l:p} key_off_from_data={data_off:#x} set0_do={} set0_eo={} set0_sz={} set0_u64s={bset_u64s} at_l={:#x} words={words:x?}",
                    unsafe { (*b).data },
                    unsafe { (*b).byte_order },
                    unsafe { (*b).c.btree_id },
                    unsafe { (*b).c.level },
                    set0.data_offset,
                    set0.end_offset,
                    set0.size,
                    (l as *const u64).read_unaligned(),
                );
            }
            __bch2_bkey_unpack_key(&(*b).format, &mut unpacked, &*l);
            (&unpacked as *const bkey, r as *const bkey)
        } else if bkey_packed(&*r) {
            __bch2_bkey_unpack_key(&(*b).format, &mut unpacked, &*r);
            (l as *const bkey, &unpacked as *const bkey)
        } else {
            (l as *const bkey, r as *const bkey)
        };
        bpos_cmp((*l).p, (*r).p)
    }
}

pub unsafe fn bkey_iter_cmp(
    b: *const btree,
    l: *const bkey_packed_type,
    r: *const bkey_packed_type,
) -> i32 {
    let cmp = bch2_bkey_cmp_packed(b, l, r);
    if cmp != 0 {
        cmp
    } else {
        let deleted_cmp = bkey_deleted(&*r) as i32 - bkey_deleted(&*l) as i32;
        if deleted_cmp != 0 {
            deleted_cmp
        } else {
            (l as usize).cmp(&(r as usize)) as i32
        }
    }
}

pub unsafe fn bkey_iter_pos_cmp(
    b: *const btree,
    l: *const bkey_packed_type,
    r: *const bpos,
) -> i32 {
    let cmp = super::bkey::bkey_cmp_left_packed(b, l, r);
    if cmp != 0 {
        cmp
    } else {
        -(bkey_deleted(&*l) as i32)
    }
}

pub unsafe fn btree_node_iter_cmp(
    b: *const btree,
    l: btree_node_iter_set,
    r: btree_node_iter_set,
) -> i32 {
    bkey_iter_cmp(
        b,
        __btree_node_offset_to_key(b, l.k),
        __btree_node_offset_to_key(b, r.k),
    )
}

unsafe fn __bch2_btree_node_iter_push(
    iter: *mut btree_node_iter,
    b: *mut btree,
    k: *const bkey_packed_type,
    end: *const bkey_packed_type,
) {
    if k != end {
        let pos = (*iter)
            .data
            .as_mut_ptr()
            .add(__btree_node_iter_used(iter) as usize);
        assert!(pos < (*iter).data.as_mut_ptr().add(MAX_BSETS));
        *pos = btree_node_iter_set {
            k: __btree_node_key_to_offset(b, k),
            end: __btree_node_key_to_offset(b, end),
        };
    }
}

pub unsafe fn bch2_btree_node_iter_push(
    iter: *mut btree_node_iter,
    b: *mut btree,
    k: *const bkey_packed_type,
    end: *const bkey_packed_type,
) {
    __bch2_btree_node_iter_push(iter, b, k, end);
    bch2_btree_node_iter_sort(iter, b);
}

unsafe fn btree_node_iter_sort_two(
    iter: *mut btree_node_iter,
    b: *mut btree,
    first: usize,
) -> bool {
    let ret = btree_node_iter_cmp(b, (*iter).data[first], (*iter).data[first + 1]) > 0;
    if ret {
        (*iter).data.swap(first, first + 1);
    }
    ret
}

pub unsafe fn bch2_btree_node_iter_sort(iter: *mut btree_node_iter, b: *mut btree) {
    if !__btree_node_iter_set_end(iter, 2) {
        btree_node_iter_sort_two(iter, b, 0);
        btree_node_iter_sort_two(iter, b, 1);
    }

    if !__btree_node_iter_set_end(iter, 1) {
        btree_node_iter_sort_two(iter, b, 0);
    }
}

pub unsafe fn bch2_btree_node_iter_init_from_start(iter: *mut btree_node_iter, b: *mut btree) {
    *iter = btree_node_iter::default();

    for i in 0..(*b).nsets as usize {
        let t = (*b).set.as_ptr().add(i);
        __bch2_btree_node_iter_push(iter, b, btree_bkey_first(b, t), btree_bkey_last(b, t));
    }
    bch2_btree_node_iter_sort(iter, b);
}

pub unsafe fn bch2_btree_node_iter_init(
    _c: *mut bch_fs,
    b: *mut btree,
    iter: *mut btree_node_iter,
    search: *const bpos,
) {
    assert!(bpos_cmp(*search, (*(*b).data).min_key) >= 0);
    assert!(bpos_cmp(*search, (*(*b).data).max_key) <= 0);
    *iter = btree_node_iter::default();

    let mut packed = bkey_packed_type::default();
    let packed_search = match prepare_search_key(b, &*search, &mut packed) {
        bkey_pack_pos_ret::BKEY_PACK_POS_EXACT => Some(&packed as *const bkey_packed_type),
        bkey_pack_pos_ret::BKEY_PACK_POS_SMALLER => None,
        bkey_pack_pos_ret::BKEY_PACK_POS_FAIL => {
            bch2_btree_node_iter_init_from_start(iter, b);
            loop {
                let k = bch2_btree_node_iter_peek(iter, b);
                if k.is_null() || bkey_iter_pos_cmp(b, k, search) >= 0 {
                    return;
                }
                bch2_btree_node_iter_advance(iter, b);
            }
        }
    };

    let mut keys = [core::ptr::null_mut(); MAX_BSETS];
    for (i, key) in keys.iter_mut().enumerate().take((*b).nsets as usize) {
        *key = __bch2_bset_search(b, (*b).set.as_ptr().add(i), &*search, &packed);
    }

    let mut pos = (*iter).data.as_mut_ptr();
    for (i, key) in keys.iter_mut().enumerate().take((*b).nsets as usize) {
        let t = (*b).set.as_ptr().add(i);
        let end = btree_bkey_last(b, t);
        *key = bch2_bset_search_linear(b, t, &*search, packed_search, &packed, *key);
        if *key != end {
            *pos = btree_node_iter_set {
                k: __btree_node_key_to_offset(b, *key),
                end: __btree_node_key_to_offset(b, end),
            };
            pos = pos.add(1);
        }
    }
    bch2_btree_node_iter_sort(iter, b);
    let (si, so, ss) = unsafe {
        (
            core::ptr::addr_of!((*search).inode).read_unaligned(),
            core::ptr::addr_of!((*search).offset).read_unaligned(),
            core::ptr::addr_of!((*search).snapshot).read_unaligned(),
        )
    };
    let kk = bch2_btree_node_iter_peek_all(iter, b);
    if !kk.is_null() {
        let kpos = bkey_unpack_pos(b, kk);
        let (ki, ko, ks) = unsafe {
            (
                core::ptr::addr_of!(kpos.inode).read_unaligned(),
                core::ptr::addr_of!(kpos.offset).read_unaligned(),
                core::ptr::addr_of!(kpos.snapshot).read_unaligned(),
            )
        };
        crate::rewrite_log_debug!(
            "node_iter_init b={b:p} L{} search=({si},{so},{ss}) -> pk=({ki},{ko},{ks})t{} off={:#x}",
            (*b).c.level,
            (*kk).type_,
            (kk as usize).wrapping_sub((*b).data as usize),
        );
    } else {
        crate::rewrite_log_debug!(
            "node_iter_init b={b:p} L{} search=({si},{so},{ss}) -> END",
            (*b).c.level,
        );
    }
}

pub unsafe fn __bch2_btree_node_iter_peek_all(
    iter: *mut btree_node_iter,
    b: *mut btree,
) -> *mut bkey_packed_type {
    __btree_node_offset_to_key(b, (*iter).data[0].k)
}

pub unsafe fn bch2_btree_node_iter_peek_all(
    iter: *mut btree_node_iter,
    b: *mut btree,
) -> *mut bkey_packed_type {
    if (*iter).data[0].k != (*iter).data[0].end {
        __btree_node_offset_to_key(b, (*iter).data[0].k)
    } else {
        core::ptr::null_mut()
    }
}

unsafe fn __bch2_btree_node_iter_advance(iter: *mut btree_node_iter, b: *mut btree) {
    (*iter).data[0].k += (*__bch2_btree_node_iter_peek_all(iter, b)).u64s as u16;
    assert!((*iter).data[0].k <= (*iter).data[0].end);

    if __btree_node_iter_set_end(iter, 0) {
        (*iter).data[0] = (*iter).data[1];
        (*iter).data[1] = (*iter).data[2];
        (*iter).data[2] = btree_node_iter_set { k: 0, end: 0 };
        return;
    }

    if __btree_node_iter_set_end(iter, 1) {
        return;
    }

    if !btree_node_iter_sort_two(iter, b, 0) {
        return;
    }

    if __btree_node_iter_set_end(iter, 2) {
        return;
    }

    btree_node_iter_sort_two(iter, b, 1);
}

pub unsafe fn bch2_btree_node_iter_advance(iter: *mut btree_node_iter, b: *mut btree) {
    __bch2_btree_node_iter_advance(iter, b);
}

pub unsafe fn bch2_btree_node_iter_peek(
    iter: *mut btree_node_iter,
    b: *mut btree,
) -> *mut bkey_packed_type {
    loop {
        let k = bch2_btree_node_iter_peek_all(iter, b);
        if k.is_null() || !bkey_deleted(&*k) {
            return k;
        }
        bch2_btree_node_iter_advance(iter, b);
    }
}

pub unsafe fn bch2_btree_node_iter_next_all(
    iter: *mut btree_node_iter,
    b: *mut btree,
) -> *mut bkey_packed_type {
    let ret = bch2_btree_node_iter_peek_all(iter, b);
    if !ret.is_null() {
        bch2_btree_node_iter_advance(iter, b);
    }
    ret
}

pub unsafe fn bch2_btree_node_iter_bset_pos(
    iter: *mut btree_node_iter,
    b: *mut btree,
    t: *mut super::types::bset_tree,
) -> *mut bkey_packed_type {
    let set = btree_node_iter_set_find(iter, (*t).end_offset as u32);
    if !set.is_null() {
        __btree_node_offset_to_key(b, (*set).k)
    } else {
        btree_bkey_last(b, t)
    }
}

unsafe fn btree_node_iter_set_set_pos(
    iter: *mut btree_node_iter,
    b: *mut btree,
    t: *mut super::types::bset_tree,
    k: *mut bkey_packed_type,
) {
    let set = btree_node_iter_set_find(iter, (*t).end_offset as u32);
    if !set.is_null() {
        (*set).k = __btree_node_key_to_offset(b, k);
        bch2_btree_node_iter_sort(iter, b);
    } else {
        bch2_btree_node_iter_push(iter, b, k, btree_bkey_last(b, t));
    }
}

unsafe fn __bch2_btree_node_iter_fix(
    path: *mut super::iter::btree_path,
    b: *mut btree,
    node_iter: *mut btree_node_iter,
    t: *mut super::types::bset_tree,
    where_: *mut bkey_packed_type,
    clobber_u64s: u32,
    new_u64s: u32,
) {
    let end = btree_bkey_last(b, t);
    let offset = __btree_node_key_to_offset(b, where_);
    let shift = new_u64s as i32 - clobber_u64s as i32;
    let old_end = (*t).end_offset as i32 - shift;
    let set = btree_node_iter_set_find(node_iter, old_end as u32);

    if set.is_null() {
        if new_u64s != 0 && bkey_iter_pos_cmp(b, where_, &(*path).pos) >= 0 {
            bch2_btree_node_iter_push(node_iter, b, where_, end);
        }
    } else {
        (*set).end = (*t).end_offset;
        if (*set).k >= offset {
            if new_u64s != 0 && bkey_iter_pos_cmp(b, where_, &(*path).pos) >= 0 {
                (*set).k = offset;
            } else if (*set).k < offset.saturating_add(clobber_u64s as u16) {
                (*set).k = offset.saturating_add(new_u64s as u16);
                if (*set).k == (*set).end {
                    bch2_btree_node_iter_set_drop(node_iter, set);
                }
            } else {
                (*set).k = ((*set).k as i32 + shift) as u16;
            }
            bch2_btree_node_iter_sort(node_iter, b);
        }
    }

    if !bch2_btree_node_iter_end(node_iter) && (*b).c.level != 0 {
        let k = bch2_btree_node_iter_peek_all(node_iter, b);
        for idx in 0..(*b).nsets as usize {
            let t2 = (*b).set.as_mut_ptr().add(idx);
            if (*node_iter).data[0].end == (*t2).end_offset {
                continue;
            }
            let mut k2 = bch2_btree_node_iter_bset_pos(node_iter, b, t2);
            let mut set_pos = false;
            loop {
                let p = bch2_bkey_prev_all(b, t2, k2);
                if p.is_null() || bkey_iter_cmp(b, k, p) >= 0 {
                    break;
                }
                k2 = p;
                set_pos = true;
            }
            if set_pos {
                btree_node_iter_set_set_pos(node_iter, b, t2, k2);
            }
        }
    }
}

pub unsafe fn bch2_btree_node_iter_fix(
    trans: *mut super::iter::btree_trans,
    path: *mut super::iter::btree_path,
    b: *mut btree,
    node_iter: *mut btree_node_iter,
    where_: *mut bkey_packed_type,
    clobber_u64s: u32,
    new_u64s: u32,
) {
    if trans.is_null() || path.is_null() || b.is_null() || where_.is_null() {
        return;
    }
    let t = super::types::bch2_bkey_to_bset_inlined(b, where_);
    let level = (*b).c.level as usize;
    if node_iter != &mut (*path).l[level].iter {
        __bch2_btree_node_iter_fix(path, b, node_iter, t, where_, clobber_u64s, new_u64s);
    }
    for idx in 1..super::iter::BTREE_ITER_INITIAL {
        if (*trans).paths_allocated & (1u64 << idx) == 0 {
            continue;
        }
        let linked = (*trans).paths.add(idx);
        if (*linked).l[level].b != b {
            continue;
        }
        __bch2_btree_node_iter_fix(
            linked,
            b,
            &mut (*linked).l[level].iter,
            t,
            where_,
            clobber_u64s,
            new_u64s,
        );
    }
}

pub unsafe fn bch2_btree_node_iter_prev_all(
    iter: *mut btree_node_iter,
    b: *mut btree,
) -> *mut bkey_packed_type {
    let mut prev: *mut bkey_packed_type = core::ptr::null_mut();
    let mut end = 0u16;

    for i in 0..(*b).nsets as usize {
        let t = (*b).set.as_mut_ptr().add(i);
        let k = bch2_bkey_prev_all(b, t, bch2_btree_node_iter_bset_pos(iter, b, t));
        if !k.is_null() && (prev.is_null() || bkey_iter_cmp(b, k, prev) > 0) {
            prev = k;
            end = (*t).end_offset;
        }
    }
    if prev.is_null() {
        return core::ptr::null_mut();
    }

    let mut set = btree_node_iter_set_find(iter, end as u32);
    if set.is_null() {
        set = (*iter)
            .data
            .as_mut_ptr()
            .add(__btree_node_iter_used(iter) as usize);
    }
    assert!(set < (*iter).data.as_mut_ptr().add(MAX_BSETS));
    core::ptr::copy(
        (*iter).data.as_ptr(),
        (*iter).data.as_mut_ptr().add(1),
        set.offset_from((*iter).data.as_mut_ptr()) as usize,
    );
    (*iter).data[0] = btree_node_iter_set {
        k: __btree_node_key_to_offset(b, prev),
        end,
    };
    prev
}

pub unsafe fn bch2_btree_node_iter_prev(
    iter: *mut btree_node_iter,
    b: *mut btree,
) -> *mut bkey_packed_type {
    loop {
        let prev = bch2_btree_node_iter_prev_all(iter, b);
        if prev.is_null() || !bkey_deleted(&*prev) {
            return prev;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btree::bkey::{bkey_format_key_bits, bpos, BKEY_U64S, KEY_FORMAT_CURRENT};
    use crate::btree::bset::{bset as disk_bset, btree_node as disk_btree_node};
    use crate::btree::types::{bset_tree, BSET_NO_AUX_TREE_VAL};

    unsafe fn put_key(words: &mut [u64], offset: usize, inode: u64, key_offset: u64, type_: u8) {
        let k = words.as_mut_ptr().add(offset) as *mut bkey;
        *k = bkey {
            u64s: BKEY_U64S,
            format: KEY_FORMAT_CURRENT,
            type_,
            p: bpos {
                inode,
                offset: key_offset,
                snapshot: 0,
            },
            ..Default::default()
        };
    }

    #[test]
    fn merges_three_bsets_and_skips_deleted_keys() {
        let mut words = vec![0u64; 80];
        let mut b = btree::default();
        b.data = words.as_mut_ptr() as *mut disk_btree_node;
        b.format = crate::btree::bkey::BKEY_FORMAT_CURRENT;
        b.nr_key_bits = bkey_format_key_bits(&b.format) as u8;
        b.nsets = 3;

        let set_data = [(17usize, [1u64, 4]), (30, [2, 5]), (43, [3, 6])];
        unsafe {
            (*b.data).min_key = bpos {
                inode: 1,
                offset: 0,
                snapshot: 0,
            };
            (*b.data).max_key = bpos {
                inode: 1,
                offset: 10,
                snapshot: 0,
            };
            for (i, (data_offset, keys)) in set_data.into_iter().enumerate() {
                let s = words.as_mut_ptr().add(data_offset) as *mut disk_bset;
                (*s).u64s = 10;
                put_key(
                    &mut words,
                    data_offset + 3,
                    1,
                    keys[0],
                    if keys[0] == 3 { 0 } else { 1 },
                );
                put_key(&mut words, data_offset + 8, 1, keys[1], 1);
                b.set[i] = bset_tree {
                    size: 0,
                    extra: BSET_NO_AUX_TREE_VAL,
                    data_offset: data_offset as u16,
                    aux_data_offset: 0,
                    end_offset: (data_offset + 13) as u16,
                };
            }

            let mut iter = btree_node_iter::default();
            bch2_btree_node_iter_init_from_start(&mut iter, &mut b);
            let mut offsets = Vec::new();
            loop {
                let k = bch2_btree_node_iter_peek(&mut iter, &mut b);
                if k.is_null() {
                    break;
                }
                offsets.push(bkey_unpack_pos(&b, k).offset);
                bch2_btree_node_iter_advance(&mut iter, &mut b);
            }
            assert_eq!(offsets, [1, 2, 4, 5, 6]);

            let mut iter = btree_node_iter::default();
            let search = bpos {
                inode: 1,
                offset: 3,
                snapshot: 0,
            };
            bch2_btree_node_iter_init(core::ptr::null_mut(), &mut b, &mut iter, &search);
            let found = bkey_unpack_pos(&b, bch2_btree_node_iter_peek(&mut iter, &mut b));
            let found_offset = found.offset;
            assert_eq!(found_offset, 4);
            let previous = bkey_unpack_pos(&b, bch2_btree_node_iter_prev(&mut iter, &mut b));
            let previous_offset = previous.offset;
            assert_eq!(previous_offset, 2);
        }
    }

    #[test]
    fn searches_rw_aux_tree_then_scans_within_range() {
        use crate::btree::bset::rw_aux_tree;
        use crate::btree::types::BSET_RW_AUX_TREE_VAL;

        let mut words = vec![0u64; 48];
        let mut aux = vec![0u64; 8];
        let mut b = btree::default();
        b.data = words.as_mut_ptr() as *mut disk_btree_node;
        b.aux_data = aux.as_mut_ptr().cast();
        b.format = crate::btree::bkey::BKEY_FORMAT_CURRENT;
        b.nr_key_bits = bkey_format_key_bits(&b.format) as u8;
        b.nsets = 1;

        unsafe {
            (*b.data).min_key = bpos {
                inode: 1,
                offset: 0,
                snapshot: 0,
            };
            (*b.data).max_key = bpos {
                inode: 1,
                offset: 10,
                snapshot: 0,
            };
            let s = words.as_mut_ptr().add(17) as *mut disk_bset;
            (*s).u64s = 15;
            put_key(&mut words, 20, 1, 1, 1);
            put_key(&mut words, 25, 1, 4, 1);
            put_key(&mut words, 30, 1, 7, 1);
            b.set[0] = bset_tree {
                size: 2,
                extra: BSET_RW_AUX_TREE_VAL,
                data_offset: 17,
                aux_data_offset: 0,
                end_offset: 35,
            };

            let rw = aux.as_mut_ptr() as *mut rw_aux_tree;
            *rw.add(0) = rw_aux_tree {
                offset: 20,
                k: bpos {
                    inode: 1,
                    offset: 1,
                    snapshot: 0,
                },
            };
            *rw.add(1) = rw_aux_tree {
                offset: 30,
                k: bpos {
                    inode: 1,
                    offset: 7,
                    snapshot: 0,
                },
            };

            let search = bpos {
                inode: 1,
                offset: 5,
                snapshot: 0,
            };
            let mut iter = btree_node_iter::default();
            bch2_btree_node_iter_init(core::ptr::null_mut(), &mut b, &mut iter, &search);
            let found = bkey_unpack_pos(&b, bch2_btree_node_iter_peek(&mut iter, &mut b));
            let found_offset = found.offset;
            assert_eq!(found_offset, 7);
            let previous = bkey_unpack_pos(&b, bch2_btree_node_iter_prev(&mut iter, &mut b));
            let previous_offset = previous.offset;
            assert_eq!(previous_offset, 4);
        }
    }
}
