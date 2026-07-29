use super::bkey::{
    bch2_bkey_pack_pos_lossy, bkey, bkey_deleted, bkey_p_next, bkey_pack_pos_ret, bkey_packed,
    bkey_packed as bkey_packed_type, bpos, bpos_cmp,
};
use super::bset::{bkey_float, rw_aux_tree, BFLOAT_FAILED};
use super::node_iter::{bch2_bkey_cmp_packed, bkey_unpack_pos};
use super::types::{
    __btree_node_key_to_offset, __btree_node_offset_to_key, bset_aux_tree_type,
    bset_aux_tree_type as aux_type, bset_tree, btree, btree_bkey_first, btree_bkey_first_offset,
    btree_bkey_last, BSET_CACHELINE,
};
use crate::util::eytzinger::{__eytzinger1_to_inorder, __inorder_to_eytzinger1, eytzinger1_prev};

unsafe fn __aux_tree_base(b: *const btree, t: *const bset_tree) -> *mut u8 {
    ((*b).aux_data as *mut u8).add((*t).aux_data_offset as usize * 8)
}

pub unsafe fn ro_aux_tree_base(b: *const btree, t: *const bset_tree) -> *mut bkey_float {
    assert_eq!(bset_aux_tree_type(t), aux_type::BSET_RO_AUX_TREE);
    __aux_tree_base(b, t).cast()
}

pub unsafe fn rw_aux_tree_base(b: *const btree, t: *const bset_tree) -> *mut rw_aux_tree {
    assert_eq!(bset_aux_tree_type(t), aux_type::BSET_RW_AUX_TREE);
    __aux_tree_base(b, t).cast()
}

unsafe fn bset_cacheline(b: *const btree, t: *const bset_tree, cacheline: u32) -> *mut u8 {
    let first = btree_bkey_first(b, t) as usize;
    ((first & !(64usize - 1)) + cacheline as usize * BSET_CACHELINE) as *mut u8
}

unsafe fn cacheline_to_bkey(
    b: *const btree,
    t: *const bset_tree,
    cacheline: u32,
    offset: u32,
) -> *mut bkey_packed_type {
    bset_cacheline(b, t, cacheline)
        .add(offset as usize * 8)
        .cast()
}

unsafe fn bkey_to_cacheline(
    b: *const btree,
    t: *const bset_tree,
    k: *const bkey_packed_type,
) -> u32 {
    ((k as usize - bset_cacheline(b, t, 0) as usize) / BSET_CACHELINE) as u32
}

unsafe fn tree_to_bkey(b: *const btree, t: *const bset_tree, j: u32) -> *mut bkey_packed_type {
    let f = &*ro_aux_tree_base(b, t).add(j as usize);
    cacheline_to_bkey(
        b,
        t,
        __eytzinger1_to_inorder(j, (*t).size as u32 - 1, (*t).extra as u32),
        f.key_offset as u32,
    )
}

pub unsafe fn rw_aux_to_bkey(
    b: *const btree,
    t: *const bset_tree,
    j: u32,
) -> *mut bkey_packed_type {
    __btree_node_offset_to_key(b, (*rw_aux_tree_base(b, t).add(j as usize)).offset)
}

pub unsafe fn rw_aux_tree_bsearch(b: *const btree, t: *const bset_tree, offset: u32) -> u32 {
    let bset_offs = offset - btree_bkey_first_offset(t) as u32;
    let bset_u64s = (*t).end_offset as u32 - btree_bkey_first_offset(t) as u32;
    let mut idx = if bset_u64s != 0 {
        bset_offs * (*t).size as u32 / bset_u64s
    } else {
        0
    };

    assert_eq!(bset_aux_tree_type(t), aux_type::BSET_RW_AUX_TREE);
    assert_ne!((*t).size, 0);
    assert!(idx <= (*t).size as u32);

    while idx < (*t).size as u32
        && (*rw_aux_tree_base(b, t).add(idx as usize)).offset < offset as u16
    {
        idx += 1;
    }
    while idx != 0 && (*rw_aux_tree_base(b, t).add(idx as usize - 1)).offset >= offset as u16 {
        idx -= 1;
    }
    idx
}

unsafe fn bkey_mantissa(k: *const bkey_packed_type, f: *const bkey_float) -> u16 {
    assert!(bkey_packed(&*k));
    let p = (k as *const u8).add((*f).exponent as usize >> 3) as *const u64;
    (core::ptr::read_unaligned(p) >> ((*f).exponent & 7)) as u16
}

unsafe fn bkey_cmp_p_or_unp(
    b: *const btree,
    l: *const bkey_packed_type,
    r_packed: Option<*const bkey_packed_type>,
    r: &bpos,
) -> i32 {
    if !bkey_packed(&*l) {
        return bpos_cmp((*(l as *const bkey)).p, *r);
    }
    if let Some(r_packed) = r_packed {
        assert!(bkey_packed(&*r_packed));
        return bch2_bkey_cmp_packed(b, l, r_packed);
    }
    bpos_cmp(bkey_unpack_pos(b, l), *r)
}

unsafe fn bkey_iter_cmp_p_or_unp(
    b: *const btree,
    l: *const bkey_packed_type,
    r_packed: Option<*const bkey_packed_type>,
    r: &bpos,
) -> i32 {
    let ret = bkey_cmp_p_or_unp(b, l, r_packed, r);
    if ret != 0 {
        ret
    } else {
        -(bkey_deleted(&*l) as i32)
    }
}

unsafe fn bkey_iter_pos_cmp(b: *const btree, l: *const bkey_packed_type, r: &bpos) -> i32 {
    let ret = bpos_cmp(bkey_unpack_pos(b, l), *r);
    if ret != 0 {
        ret
    } else {
        -(bkey_deleted(&*l) as i32)
    }
}

unsafe fn bset_search_write_set(
    b: *const btree,
    t: *const bset_tree,
    search: &bpos,
) -> *mut bkey_packed_type {
    let mut l = 0u32;
    let mut r = (*t).size as u32;
    while l + 1 != r {
        let m = (l + r) >> 1;
        if bpos_cmp((*rw_aux_tree_base(b, t).add(m as usize)).k, *search) < 0 {
            l = m;
        } else {
            r = m;
        }
    }
    rw_aux_to_bkey(b, t, l)
}

unsafe fn bkey_mantissa_bits_dropped(b: *const btree, f: *const bkey_float) -> bool {
    let key_bits_start = (*b).format.key_u64s as u32 * 64 - (*b).nr_key_bits as u32;
    (*f).exponent as u32 > key_bits_start
}

unsafe fn bset_search_tree(
    b: *const btree,
    t: *const bset_tree,
    search: &bpos,
    packed_search: *const bkey_packed_type,
) -> *mut bkey_packed_type {
    let base = ro_aux_tree_base(b, t);
    let mut n = 1u32;
    let mut f: *mut bkey_float;

    loop {
        f = base.add(n as usize);
        if (*f).exponent < BFLOAT_FAILED {
            let l = (*f).mantissa;
            let r = bkey_mantissa(packed_search, f);
            if l != r || !bkey_mantissa_bits_dropped(b, f) {
                n = n * 2 + (l < r) as u32;
                if n < (*t).size as u32 {
                    continue;
                }
                break;
            }
        }

        let k = tree_to_bkey(b, t, n);
        let cmp = bkey_cmp_p_or_unp(b, k, Some(packed_search), search);
        if cmp == 0 {
            return k;
        }
        n = n * 2 + (cmp < 0) as u32;
        if n >= (*t).size as u32 {
            break;
        }
    }

    let mut inorder = __eytzinger1_to_inorder(n >> 1, (*t).size as u32 - 1, (*t).extra as u32);
    if n & 1 == 0 {
        inorder -= 1;
        if inorder == 0 {
            return btree_bkey_first(b, t);
        }
        f = base.add(eytzinger1_prev(n >> 1, (*t).size as u32 - 1) as usize);
    }
    cacheline_to_bkey(b, t, inorder, (*f).key_offset as u32)
}

pub unsafe fn __bch2_bset_search(
    b: *const btree,
    t: *const bset_tree,
    search: &bpos,
    lossy_packed_search: *const bkey_packed_type,
) -> *mut bkey_packed_type {
    match bset_aux_tree_type(t) {
        aux_type::BSET_NO_AUX_TREE => btree_bkey_first(b, t),
        aux_type::BSET_RW_AUX_TREE => bset_search_write_set(b, t, search),
        aux_type::BSET_RO_AUX_TREE => bset_search_tree(b, t, search, lossy_packed_search),
    }
}

pub unsafe fn bch2_bset_search_linear(
    b: *const btree,
    t: *const bset_tree,
    search: &bpos,
    packed_search: Option<*const bkey_packed_type>,
    lossy_packed_search: *const bkey_packed_type,
    mut m: *mut bkey_packed_type,
) -> *mut bkey_packed_type {
    let end = btree_bkey_last(b, t);
    if !lossy_packed_search.is_null() {
        while m != end && bkey_iter_cmp_p_or_unp(b, m, Some(lossy_packed_search), search) < 0 {
            m = bkey_p_next(m);
        }
    }
    if packed_search.is_none() {
        while m != end && bkey_iter_pos_cmp(b, m, search) < 0 {
            m = bkey_p_next(m);
        }
    }
    m
}

unsafe fn __bkey_prev(
    b: *const btree,
    t: *const bset_tree,
    k: *mut bkey_packed_type,
) -> *mut bkey_packed_type {
    let first = btree_bkey_first(b, t);
    assert!((k as usize) >= first as usize && (k as usize) <= btree_bkey_last(b, t) as usize);
    if k == first {
        return core::ptr::null_mut();
    }

    match bset_aux_tree_type(t) {
        aux_type::BSET_NO_AUX_TREE => first,
        aux_type::BSET_RO_AUX_TREE => {
            let mut j = ((*t).size as u32 - 1).min(bkey_to_cacheline(b, t, k));
            loop {
                let p = if j != 0 {
                    let ret = tree_to_bkey(
                        b,
                        t,
                        __inorder_to_eytzinger1(j, (*t).size as u32 - 1, (*t).extra as u32),
                    );
                    j -= 1;
                    ret
                } else {
                    first
                };
                if (p as usize) < k as usize {
                    break p;
                }
            }
        }
        aux_type::BSET_RW_AUX_TREE => {
            let offset = __btree_node_key_to_offset(b, k) as u32;
            let j = rw_aux_tree_bsearch(b, t, offset);
            if j != 0 {
                rw_aux_to_bkey(b, t, j - 1)
            } else {
                first
            }
        }
    }
}

pub unsafe fn bch2_bkey_prev_filter(
    b: *const btree,
    t: *const bset_tree,
    mut k: *mut bkey_packed_type,
    min_key_type: u8,
) -> *mut bkey_packed_type {
    let mut ret: *mut bkey_packed_type = core::ptr::null_mut();
    loop {
        let p = __bkey_prev(b, t, k);
        if p.is_null() || !ret.is_null() {
            break;
        }
        let mut i = p;
        while i != k {
            if (*i).type_ >= min_key_type {
                ret = i;
            }
            i = bkey_p_next(i);
        }
        k = p;
    }
    ret
}

pub unsafe fn bch2_bkey_prev_all(
    b: *const btree,
    t: *const bset_tree,
    k: *mut bkey_packed_type,
) -> *mut bkey_packed_type {
    bch2_bkey_prev_filter(b, t, k, 0)
}

pub unsafe fn prepare_search_key(
    b: *const btree,
    search: &bpos,
    packed: &mut bkey_packed_type,
) -> bkey_pack_pos_ret {
    bch2_bkey_pack_pos_lossy(packed, search, &*b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btree::bkey::{
        bkey_format_key_bits, BKEY_FORMAT_CURRENT, BKEY_U64S, KEY_FORMAT_CURRENT,
    };
    use crate::btree::bset::{bset as disk_bset, btree_node as disk_btree_node, BFLOAT_FAILED};
    use crate::btree::types::bset_tree;
    use crate::util::eytzinger::{eytzinger1_extra, inorder_to_eytzinger1};

    unsafe fn put_key(words: &mut [u64], offset: usize, key_offset: u64) {
        *(words.as_mut_ptr().add(offset) as *mut bkey) = bkey {
            u64s: BKEY_U64S,
            format: KEY_FORMAT_CURRENT,
            type_: 1,
            p: bpos {
                inode: 1,
                offset: key_offset,
                snapshot: 0,
            },
            ..Default::default()
        };
    }

    #[test]
    fn searches_ro_aux_eytzinger_tree_and_linear_tail() {
        let mut words = vec![0u64; 160];
        let mut aux = vec![0u64; 8];
        let mut b = btree::default();
        b.data = words.as_mut_ptr() as *mut disk_btree_node;
        b.aux_data = aux.as_mut_ptr().cast();
        b.format = BKEY_FORMAT_CURRENT;
        b.nr_key_bits = bkey_format_key_bits(&b.format) as u8;
        b.nsets = 1;

        unsafe {
            let disk_set = words.as_mut_ptr().add(17) as *mut disk_bset;
            (*disk_set).u64s = 100;
            for i in 0..20usize {
                put_key(&mut words, 20 + i * 5, (i + 1) as u64);
            }
            b.set[0] = bset_tree {
                size: 3,
                extra: eytzinger1_extra(2) as u16,
                data_offset: 17,
                aux_data_offset: 0,
                end_offset: 120,
            };

            for inorder in 1..=2u32 {
                let mut k = btree_bkey_first(&b, &b.set[0]);
                while bkey_to_cacheline(&b, &b.set[0], k) < inorder {
                    k = bkey_p_next(k);
                }
                let node = inorder_to_eytzinger1(inorder, 2);
                let cacheline = bset_cacheline(&b, &b.set[0], inorder) as usize;
                *ro_aux_tree_base(&b, &b.set[0]).add(node as usize) = bkey_float {
                    exponent: BFLOAT_FAILED,
                    key_offset: ((k as usize - cacheline) / 8) as u8,
                    mantissa: 0,
                };
            }

            let search = bpos {
                inode: 1,
                offset: 13,
                snapshot: 0,
            };
            let mut packed = bkey_packed_type::default();
            assert_eq!(
                prepare_search_key(&b, &search, &mut packed),
                bkey_pack_pos_ret::BKEY_PACK_POS_EXACT
            );
            let start = __bch2_bset_search(&b, &b.set[0], &search, &packed);
            let found =
                bch2_bset_search_linear(&b, &b.set[0], &search, Some(&packed), &packed, start);
            let found_pos = bkey_unpack_pos(&b, found);
            let found_offset = found_pos.offset;
            assert_eq!(found_offset, 13);

            let previous = bch2_bkey_prev_all(&b, &b.set[0], found);
            let previous_pos = bkey_unpack_pos(&b, previous);
            let previous_offset = previous_pos.offset;
            assert_eq!(previous_offset, 12);
        }
    }
}
