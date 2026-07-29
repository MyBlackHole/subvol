use std::io::Read;

use super::bkey::{
    bch2_bkey_pack_pos, bkey, bkey_deleted, bkey_init, bkey_p_next, bkey_packed as bkey_packed_type,
};
use super::bset::{
    bkey_float, bset as disk_bset, btree_node_entry, rw_aux_tree, BFLOAT_FAILED_UNPACKED,
    BKEY_MANTISSA_BITS,
};
use super::bset_search::{ro_aux_tree_base, rw_aux_to_bkey, rw_aux_tree_base};
use super::node_iter::bkey_unpack_pos;
use super::types::{
    __btree_aux_data_bytes, __btree_node_key_to_offset, bset_aux_tree_type,
    bset_aux_tree_type as aux_type, bset_has_ro_aux_tree, bset_has_rw_aux_tree, bset_tree, btree,
    btree_bkey_first, btree_bkey_last, btree_bset_first, set_btree_bset, BSET_CACHELINE,
    BSET_NO_AUX_TREE_VAL, BSET_RW_AUX_TREE_VAL, MAX_BSETS,
};
use crate::util::eytzinger::{eytzinger1_extra, eytzinger1_first, eytzinger1_next};

const SMP_CACHE_BYTES: usize = 64;
const L1_CACHE_BYTES: usize = 64;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct sort_iter_set {
    pub k: *mut bkey_packed_type,
    pub end: *mut bkey_packed_type,
}

#[repr(C)]
#[derive(Debug)]
pub struct sort_iter {
    pub b: *mut btree,
    pub used: u32,
    pub size: u32,
    pub data: [sort_iter_set; 0],
}

impl Default for sort_iter {
    fn default() -> Self {
        Self {
            b: core::ptr::null_mut(),
            used: 0,
            size: 0,
            data: [],
        }
    }
}

#[repr(C)]
#[derive(Debug, Default)]
pub struct sort_iter_stack {
    pub iter: sort_iter,
    pub sets: [sort_iter_set; MAX_BSETS + 1],
}

unsafe fn sort_iter_data(iter: *mut sort_iter) -> *mut sort_iter_set {
    (iter.cast::<u8>())
        .add(core::mem::size_of::<sort_iter>())
        .cast()
}

pub unsafe fn sort_iter_init(iter: *mut sort_iter, b: *mut btree, size: u32) {
    (*iter).b = b;
    (*iter).used = 0;
    (*iter).size = size;
}

pub unsafe fn sort_iter_stack_init(iter: *mut sort_iter_stack, b: *mut btree) {
    sort_iter_init(&mut (*iter).iter, b, (*iter).sets.len() as u32);
}

pub unsafe fn sort_iter_add(
    iter: *mut sort_iter,
    k: *mut bkey_packed_type,
    end: *mut bkey_packed_type,
) {
    assert!((*iter).used < (*iter).size);
    if k != end {
        *sort_iter_data(iter).add((*iter).used as usize) = sort_iter_set { k, end };
        (*iter).used += 1;
    }
}

type sort_cmp_fn = unsafe fn(*const btree, *const bkey_packed_type, *const bkey_packed_type) -> i32;

unsafe fn sort_iter_sift(iter: *mut sort_iter, from: u32, cmp: sort_cmp_fn) {
    let data = sort_iter_data(iter);
    let mut i = from;
    while i + 1 < (*iter).used
        && cmp(
            (*iter).b,
            (*data.add(i as usize)).k,
            (*data.add(i as usize + 1)).k,
        ) > 0
    {
        core::ptr::swap(data.add(i as usize), data.add(i as usize + 1));
        i += 1;
    }
}

unsafe fn sort_iter_sort(iter: *mut sort_iter, cmp: sort_cmp_fn) {
    let mut i = (*iter).used;
    while i != 0 {
        i -= 1;
        sort_iter_sift(iter, i, cmp);
    }
}

unsafe fn sort_iter_peek(iter: *mut sort_iter) -> *mut bkey_packed_type {
    if (*iter).used != 0 {
        (*sort_iter_data(iter)).k
    } else {
        core::ptr::null_mut()
    }
}

unsafe fn sort_iter_advance(iter: *mut sort_iter, cmp: sort_cmp_fn) {
    assert_ne!((*iter).used, 0);
    let data = sort_iter_data(iter);
    (*data).k = bkey_p_next((*data).k);
    assert!((*data).k <= (*data).end);
    if (*data).k == (*data).end {
        core::ptr::copy(data.add(1), data, (*iter).used as usize - 1);
        (*iter).used -= 1;
    } else {
        sort_iter_sift(iter, 0, cmp);
    }
}

unsafe fn sort_iter_next(iter: *mut sort_iter, cmp: sort_cmp_fn) -> *mut bkey_packed_type {
    let ret = sort_iter_peek(iter);
    if !ret.is_null() {
        sort_iter_advance(iter, cmp);
    }
    ret
}

unsafe fn key_sort_fix_overlapping_cmp(
    b: *const btree,
    l: *const bkey_packed_type,
    r: *const bkey_packed_type,
) -> i32 {
    let ret = super::node_iter::bch2_bkey_cmp_packed(b, l, r);
    if ret != 0 {
        ret
    } else if (l as usize) < r as usize {
        -1
    } else if (l as usize) > r as usize {
        1
    } else {
        0
    }
}

unsafe fn should_drop_next_key(iter: *mut sort_iter) -> bool {
    let data = sort_iter_data(iter);
    (*iter).used >= 2
        && super::node_iter::bch2_bkey_cmp_packed((*iter).b, (*data).k, (*data.add(1)).k) == 0
}

pub unsafe fn bch2_key_sort_fix_overlapping(
    _c: *mut super::types::bch_fs,
    dst: *mut disk_bset,
    iter: *mut sort_iter,
) -> super::types::btree_nr_keys {
    let mut out = dst.cast::<u64>().add(3).cast::<bkey_packed_type>();
    let mut nr = super::types::btree_nr_keys::default();
    sort_iter_sort(iter, key_sort_fix_overlapping_cmp);
    loop {
        let k = sort_iter_peek(iter);
        if k.is_null() {
            break;
        }
        if !bkey_deleted(&*k) && !should_drop_next_key(iter) {
            core::ptr::copy_nonoverlapping(k.cast::<u64>(), out.cast::<u64>(), (*k).u64s as usize);
            super::bset_update::btree_keys_account_key(&mut nr, 0, out, 1);
            out = bkey_p_next(out);
        }
        sort_iter_advance(iter, key_sort_fix_overlapping_cmp);
    }
    (*dst).u64s = out.cast::<u64>().offset_from(dst.cast::<u64>().add(3)) as u16;
    nr
}

pub unsafe fn bch2_sort_repack(
    dst: *mut disk_bset,
    src: *mut btree,
    src_iter: *mut super::types::btree_node_iter,
    out_f: *const super::bkey::bkey_format,
    filter_whiteouts: bool,
) -> super::types::btree_nr_keys {
    let mut out = dst.cast::<u64>().add(3).cast::<bkey_packed_type>();
    let mut nr = super::types::btree_nr_keys::default();
    let transform = *out_f != (*src).format;

    loop {
        let input = super::node_iter::bch2_btree_node_iter_next_all(src_iter, src);
        if input.is_null() {
            break;
        }
        if filter_whiteouts && bkey_deleted(&*input) {
            continue;
        }

        if !transform {
            core::ptr::copy(
                input.cast::<u64>(),
                out.cast::<u64>(),
                (*input).u64s as usize,
            );
        } else {
            let in_f = if super::bkey::bkey_packed(&*input) {
                &(*src).format
            } else {
                &super::bkey::BKEY_FORMAT_CURRENT
            };
            if super::bkey::bch2_bkey_transform(&*out_f, &mut *out, in_f, &*input) {
                (*out).format = ((*out).format & 0x80) | super::bkey::KEY_FORMAT_LOCAL_BTREE;
            } else {
                super::bkey::bch2_bkey_unpack(src, out.cast(), input);
            }
        }
        (*out).format &= 0x7f;
        super::bset_update::btree_keys_account_key(&mut nr, 0, out, 1);
        out = bkey_p_next(out);
    }

    (*dst).u64s = out.cast::<u64>().offset_from(dst.cast::<u64>().add(3)) as u16;
    nr
}

unsafe fn keep_unwritten_whiteouts_cmp(
    b: *const btree,
    l: *const bkey_packed_type,
    r: *const bkey_packed_type,
) -> i32 {
    let ret = super::node_iter::bch2_bkey_cmp_packed(b, l, r);
    if ret != 0 {
        return ret;
    }
    let deleted = bkey_deleted(&*r) as i32 - bkey_deleted(&*l) as i32;
    if deleted != 0 {
        return deleted;
    }
    if (l as usize) < r as usize {
        -1
    } else if (l as usize) > r as usize {
        1
    } else {
        0
    }
}

pub unsafe fn bch2_sort_keys_keep_unwritten_whiteouts(
    dst: *mut bkey_packed_type,
    iter: *mut sort_iter,
) -> u32 {
    let mut out = dst;
    sort_iter_sort(iter, keep_unwritten_whiteouts_cmp);
    loop {
        let input = sort_iter_next(iter, keep_unwritten_whiteouts_cmp);
        if input.is_null() {
            break;
        }
        if bkey_deleted(&*input) && input < super::interior::unwritten_whiteouts_start((*iter).b) {
            continue;
        }
        let next = sort_iter_peek(iter);
        if !next.is_null() && super::node_iter::bch2_bkey_cmp_packed((*iter).b, input, next) == 0 {
            continue;
        }
        core::ptr::copy(
            input.cast::<u64>(),
            out.cast::<u64>(),
            (*input).u64s as usize,
        );
        out = bkey_p_next(out);
    }
    out.cast::<u64>().offset_from(dst.cast::<u64>()) as u32
}

pub unsafe fn bch2_sort_keys(dst: *mut bkey_packed_type, iter: *mut sort_iter) -> u32 {
    let mut out = dst;
    sort_iter_sort(iter, super::node_iter::bch2_bkey_cmp_packed);
    loop {
        let input = sort_iter_next(iter, super::node_iter::bch2_bkey_cmp_packed);
        if input.is_null() {
            break;
        }
        if bkey_deleted(&*input) {
            continue;
        }
        core::ptr::copy(
            input.cast::<u64>(),
            out.cast::<u64>(),
            (*input).u64s as usize,
        );
        out = bkey_p_next(out);
    }
    out.cast::<u64>().offset_from(dst.cast::<u64>()) as u32
}

unsafe fn sort_bkey_ptrs(bt: *const btree, ptrs: &mut [*mut bkey_packed_type]) {
    let mut n = ptrs.len();
    let mut a = n / 2;
    if a == 0 {
        return;
    }

    loop {
        if a != 0 {
            a -= 1;
        } else {
            n -= 1;
            if n != 0 {
                ptrs.swap(0, n);
            } else {
                break;
            }
        }

        let mut b = a;
        let mut c;
        let mut d;
        loop {
            c = 2 * b + 1;
            d = c + 1;
            if d >= n {
                break;
            }
            b = if super::node_iter::bch2_bkey_cmp_packed(bt, ptrs[c], ptrs[d]) >= 0 {
                c
            } else {
                d
            };
        }
        if d == n {
            b = c;
        }

        while b != a && super::node_iter::bch2_bkey_cmp_packed(bt, ptrs[a], ptrs[b]) >= 0 {
            b = (b - 1) / 2;
        }
        c = b;
        while b != a {
            b = (b - 1) / 2;
            ptrs.swap(b, c);
        }
    }
}

unsafe fn verify_no_dups(b: *mut btree, start: *mut bkey_packed_type, end: *mut bkey_packed_type) {
    #[cfg(debug_assertions)]
    {
        if start == end {
            return;
        }

        let mut previous = start;
        let mut key = bkey_p_next(start);
        while key != end {
            let mut left = bkey::default();
            let mut right = bkey::default();
            if super::bkey::bkey_packed(&*previous) {
                super::bkey::__bch2_bkey_unpack_key(&(*b).format, &mut left, &*previous);
            } else {
                left = *previous.cast::<bkey>();
            }
            if super::bkey::bkey_packed(&*key) {
                super::bkey::__bch2_bkey_unpack_key(&(*b).format, &mut right, &*key);
            } else {
                right = *key.cast::<bkey>();
            }
            assert!(!super::bkey::bpos_ge(
                left.p,
                super::bkey::bkey_start_pos(&right)
            ));
            previous = key;
            key = bkey_p_next(key);
        }
    }
    #[cfg(not(debug_assertions))]
    let _ = (b, start, end);
}

pub unsafe fn bch2_sort_whiteouts(_c: *mut super::types::bch_fs, b: *mut btree) {
    if (*b).whiteout_u64s == 0 {
        return;
    }

    let start = super::interior::unwritten_whiteouts_start(b);
    let end = super::interior::unwritten_whiteouts_end(b);
    let mut ptrs = Vec::new();
    let mut key = start;
    while key != end {
        ptrs.push(key);
        key = bkey_p_next(key);
        assert!(key <= end);
    }
    sort_bkey_ptrs(b, &mut ptrs);

    let mut words = vec![0u64; (*b).whiteout_u64s as usize];
    let mut out = words.as_mut_ptr().cast::<bkey_packed_type>();
    for input in ptrs {
        core::ptr::copy_nonoverlapping(
            input.cast::<u64>(),
            out.cast::<u64>(),
            (*input).u64s as usize,
        );
        out = bkey_p_next(out);
    }
    assert_eq!(out.cast::<u64>(), words.as_mut_ptr().add(words.len()));
    verify_no_dups(
        b,
        words.as_mut_ptr().cast(),
        words.as_mut_ptr().add(words.len()).cast(),
    );
    core::ptr::copy_nonoverlapping(words.as_ptr(), start.cast::<u64>(), words.len());
}

pub unsafe fn bch2_bset_set_no_aux_tree(b: *mut btree, mut t: *mut bset_tree) {
    assert!(t >= (*b).set.as_mut_ptr());
    let end = (*b).set.as_mut_ptr().add(MAX_BSETS);
    while t < end {
        (*t).size = 0;
        (*t).extra = BSET_NO_AUX_TREE_VAL;
        (*t).aux_data_offset = u16::MAX;
        t = t.add(1);
    }
}

pub unsafe fn bch2_btree_keys_init(b: *mut btree) {
    (*b).nsets = 0;
    (*b).nr = Default::default();
    for i in 0..MAX_BSETS {
        (*b).set[i].data_offset = u16::MAX;
    }
    bch2_bset_set_no_aux_tree(b, (*b).set.as_mut_ptr());
}

fn random_u64() -> u64 {
    let mut bytes = [0u8; 8];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .expect("get_random_bytes failed");
    u64::from_ne_bytes(bytes)
}

pub unsafe fn bch2_bset_init_first(b: *mut btree, i: *mut disk_bset) {
    assert_eq!((*b).nsets, 0);
    *i = disk_bset::default();
    (*i).seq = random_u64();
    let t = (*b).set.as_mut_ptr().add((*b).nsets as usize);
    (*b).nsets += 1;
    set_btree_bset(b, t, i);
}

pub unsafe fn bch2_bset_init_next(b: *mut btree, bne: *mut btree_node_entry) {
    assert!((*b).nsets < MAX_BSETS as u8);
    let i = &mut (*bne).keys;
    *i = disk_bset::default();
    i.seq = (*btree_bset_first(b)).seq;
    let t = (*b).set.as_mut_ptr().add((*b).nsets as usize);
    (*b).nsets += 1;
    set_btree_bset(b, t, i);
}

pub unsafe fn bch2_btree_init_next(trans: *mut super::iter::btree_trans, b: *mut btree) {
    let mut reinit_iter = false;
    if (*b).nsets == MAX_BSETS as u8
        && !super::io::btree_node_write_in_flight(b)
        && should_compact_all((*trans).c, b)
    {
        super::io::bch2_btree_node_write_trans(
            trans,
            b,
            crate::lock::six::six_lock_type::SIX_LOCK_write,
            super::io::BTREE_WRITE_init_next_bset,
        );
        reinit_iter = true;
    }
    if (*b).nsets == MAX_BSETS as u8 && bch2_btree_node_compact((*trans).c, b) {
        reinit_iter = true;
    }
    assert!((*b).nsets < MAX_BSETS as u8);
    let bne = super::interior::want_new_bset((*trans).c, b);
    if !bne.is_null() {
        bch2_bset_init_next(b, bne);
    }
    bch2_btree_build_aux_trees(b);
    if reinit_iter {
        super::update::bch2_trans_node_reinit_iter(trans, b);
    }
}

unsafe fn bset_aux_tree_buf_end(t: *const bset_tree) -> usize {
    match bset_aux_tree_type(t) {
        aux_type::BSET_NO_AUX_TREE => (*t).aux_data_offset as usize,
        aux_type::BSET_RO_AUX_TREE => {
            (*t).aux_data_offset as usize
                + ((*t).size as usize * core::mem::size_of::<bkey_float>()).div_ceil(8)
        }
        aux_type::BSET_RW_AUX_TREE => {
            (*t).aux_data_offset as usize
                + (core::mem::size_of::<rw_aux_tree>() * (*t).size as usize).div_ceil(8)
        }
    }
}

unsafe fn bset_aux_tree_buf_start(b: *const btree, t: *const bset_tree) -> usize {
    if t == (*b).set.as_ptr() {
        ((*b).unpack_fn_len as usize).div_ceil(8)
    } else {
        bset_aux_tree_buf_end(t.sub(1))
    }
}

unsafe fn bset_alloc_tree(b: *mut btree, t: *mut bset_tree) {
    bch2_bset_set_no_aux_tree(b, t);
    let start = bset_aux_tree_buf_start(b, t);
    (*t).aux_data_offset =
        start.div_ceil(SMP_CACHE_BYTES / 8) as u16 * (SMP_CACHE_BYTES / 8) as u16;
}

unsafe fn bset_tree_capacity_bytes(b: *const btree, t: *const bset_tree) -> usize {
    __btree_aux_data_bytes((*b).byte_order as u32) - (*t).aux_data_offset as usize * 8
}

unsafe fn bset_ro_tree_capacity(b: *const btree, t: *const bset_tree) -> usize {
    bset_tree_capacity_bytes(b, t) / core::mem::size_of::<bkey_float>()
}

unsafe fn bset_rw_tree_capacity(b: *const btree, t: *const bset_tree) -> usize {
    bset_tree_capacity_bytes(b, t) / core::mem::size_of::<rw_aux_tree>()
}

unsafe fn bset_cacheline(b: *const btree, t: *const bset_tree, cacheline: usize) -> usize {
    (btree_bkey_first(b, t) as usize & !(L1_CACHE_BYTES - 1)) + cacheline * BSET_CACHELINE
}

unsafe fn bkey_to_cacheline(
    b: *const btree,
    t: *const bset_tree,
    k: *const bkey_packed_type,
) -> usize {
    (k as usize - bset_cacheline(b, t, 0)) / BSET_CACHELINE
}

unsafe fn bkey_to_cacheline_offset(
    b: *const btree,
    t: *const bset_tree,
    cacheline: usize,
    k: *const bkey_packed_type,
) -> u8 {
    let offset = (k as usize - bset_cacheline(b, t, cacheline)) / 8;
    u8::try_from(offset).expect("cacheline key offset exceeds u8")
}

unsafe fn tree_to_bkey(b: *const btree, t: *const bset_tree, j: usize) -> *mut bkey_packed_type {
    let inorder = crate::util::eytzinger::__eytzinger1_to_inorder(
        j as u32,
        (*t).size as u32 - 1,
        (*t).extra as u32,
    ) as usize;
    (bset_cacheline(b, t, inorder) + (*ro_aux_tree_base(b, t).add(j)).key_offset as usize * 8)
        as *mut bkey_packed_type
}

unsafe fn rw_aux_tree_set(b: *const btree, t: *mut bset_tree, j: usize, k: *mut bkey_packed_type) {
    *rw_aux_tree_base(b, t).add(j) = rw_aux_tree {
        offset: __btree_node_key_to_offset(b, k),
        k: bkey_unpack_pos(b, k),
    };
}

unsafe fn build_rw_aux_tree(b: *mut btree, t: *mut bset_tree) {
    (*t).size = 1;
    (*t).extra = BSET_RW_AUX_TREE_VAL;
    (*rw_aux_tree_base(b, t)).offset = __btree_node_key_to_offset(b, btree_bkey_first(b, t));

    let mut k = btree_bkey_first(b, t);
    let end = btree_bkey_last(b, t);
    while k != end {
        if (*t).size as usize == bset_rw_tree_capacity(b, t) {
            break;
        }
        if k as usize - rw_aux_to_bkey(b, t, (*t).size as u32 - 1) as usize > L1_CACHE_BYTES {
            let idx = (*t).size as usize;
            (*t).size += 1;
            rw_aux_tree_set(b, t, idx, k);
        }
        k = bkey_p_next(k);
    }
}

unsafe fn greatest_differing_bit(
    b: *const btree,
    l: *const bkey_packed_type,
    r: *const bkey_packed_type,
) -> u32 {
    let mut l = (l as *const u64).add((*b).format.key_u64s as usize - 1);
    let mut r = (r as *const u64).add((*b).format.key_u64s as usize - 1);
    let mut nr = (*b).nr_key_bits as u32;
    while nr != 0 {
        let mut lv = *l;
        let mut rv = *r;
        if nr < 64 {
            lv >>= 64 - nr;
            rv >>= 64 - nr;
            nr = 0;
        } else {
            nr -= 64;
        }
        if lv != rv {
            return 63 - (lv ^ rv).leading_zeros() + nr;
        }
        l = l.sub(1);
        r = r.sub(1);
    }
    0
}

unsafe fn key_mantissa(k: *const bkey_packed_type, f: *const bkey_float) -> u16 {
    let p = (k as *const u8)
        .add((*f).exponent as usize >> 3)
        .cast::<u64>();
    (core::ptr::read_unaligned(p) >> ((*f).exponent & 7)) as u16
}

unsafe fn make_bfloat(
    b: *mut btree,
    t: *mut bset_tree,
    j: usize,
    min_key: *mut bkey_packed_type,
    max_key: *mut bkey_packed_type,
) {
    let f = ro_aux_tree_base(b, t).add(j);
    let m = tree_to_bkey(b, t, j);
    let l = if j.is_power_of_two() {
        min_key
    } else {
        tree_to_bkey(b, t, j >> (j.trailing_zeros() + 1))
    };
    let r = if (j + 1).is_power_of_two() {
        max_key
    } else {
        tree_to_bkey(b, t, j >> (j.trailing_ones() + 1))
    };

    if !super::bkey::bkey_packed(&*l)
        || !super::bkey::bkey_packed(&*r)
        || !super::bkey::bkey_packed(&*m)
        || (*b).nr_key_bits == 0
    {
        (*f).exponent = BFLOAT_FAILED_UNPACKED;
        return;
    }

    let high_bit =
        greatest_differing_bit(b, l, r).max(BKEY_MANTISSA_BITS.min((*b).nr_key_bits as u32) - 1);
    let exponent = high_bit as i32 - (BKEY_MANTISSA_BITS as i32 - 1);
    let shift = ((*b).format.key_u64s as i32 * 64 - (*b).nr_key_bits as i32) + exponent;
    assert!(shift >= 0 && shift < u8::MAX as i32);
    (*f).exponent = shift as u8;
    let mut mantissa = key_mantissa(m, f) as u32;
    if exponent < 0 {
        mantissa |= !(!0u32 << (-exponent as u32));
    }
    (*f).mantissa = mantissa as u16;
}

unsafe fn build_ro_aux_tree(b: *mut btree, t: *mut bset_tree) {
    let mut k = btree_bkey_first(b, t);
    let mut cacheline = 1usize;
    (*t).size =
        bkey_to_cacheline(b, t, btree_bkey_last(b, t)).min(bset_ro_tree_capacity(b, t)) as u16;

    loop {
        if (*t).size < 2 {
            (*t).size = 0;
            (*t).extra = BSET_NO_AUX_TREE_VAL;
            return;
        }
        (*t).extra = eytzinger1_extra((*t).size as u32 - 1) as u16;
        let mut j = eytzinger1_first((*t).size as u32 - 1);
        let mut failed = false;
        while j != 0 {
            while bkey_to_cacheline(b, t, k) < cacheline {
                k = bkey_p_next(k);
            }
            if (k as usize) >= btree_bkey_last(b, t) as usize {
                (*t).size -= 1;
                k = btree_bkey_first(b, t);
                cacheline = 1;
                failed = true;
                break;
            }
            (*ro_aux_tree_base(b, t).add(j as usize)).key_offset =
                bkey_to_cacheline_offset(b, t, cacheline, k);
            cacheline += 1;
            j = eytzinger1_next(j, (*t).size as u32 - 1);
        }
        if !failed {
            break;
        }
    }

    let mut min_key = bkey_packed_type::default();
    if !bch2_bkey_pack_pos(&mut min_key, (*(*b).data).min_key, &*b) {
        let k = &mut min_key as *mut bkey_packed_type as *mut bkey;
        bkey_init(&mut *k);
        (*k).p = (*(*b).data).min_key;
    }
    let mut max_key = bkey_packed_type::default();
    if !bch2_bkey_pack_pos(&mut max_key, (*(*b).data).max_key, &*b) {
        let k = &mut max_key as *mut bkey_packed_type as *mut bkey;
        bkey_init(&mut *k);
        (*k).p = (*(*b).data).max_key;
    }

    let mut j = eytzinger1_first((*t).size as u32 - 1);
    while j != 0 {
        make_bfloat(b, t, j as usize, &mut min_key, &mut max_key);
        j = eytzinger1_next(j, (*t).size as u32 - 1);
    }
}

pub unsafe fn bch2_bset_build_aux_tree(b: *mut btree, t: *mut bset_tree, writeable: bool) {
    if if writeable {
        bset_has_rw_aux_tree(t)
    } else {
        bset_has_ro_aux_tree(t)
    } {
        return;
    }
    bset_alloc_tree(b, t);
    if bset_tree_capacity_bytes(b, t) == 0 {
        return;
    }
    if writeable {
        build_rw_aux_tree(b, t);
    } else {
        build_ro_aux_tree(b, t);
    }
}

pub unsafe fn bch2_btree_build_aux_trees(b: *mut btree) {
    for idx in 0..(*b).nsets as usize {
        let t = (*b).set.as_mut_ptr().add(idx);
        let writeable = !super::interior::bset_written(b, super::types::bset(b, t))
            && idx + 1 == (*b).nsets as usize;
        bch2_bset_build_aux_tree(b, t, writeable);
    }
}

pub unsafe fn bch2_btree_node_sort(
    _c: *mut super::types::bch_fs,
    b: *mut btree,
    start_idx: usize,
    end_idx: usize,
) {
    assert!(start_idx < end_idx && end_idx <= (*b).nsets as usize);
    let start_bset = super::types::bset(b, (*b).set.as_ptr().add(start_idx));
    let shift = end_idx - start_idx - 1;
    let sorting_entire_node = start_idx == 0 && end_idx == (*b).nsets as usize;
    let mut sort = sort_iter_stack::default();
    sort_iter_stack_init(&mut sort, b);
    let mut input_u64s = 0usize;
    for idx in start_idx..end_idx {
        let t = (*b).set.as_ptr().add(idx);
        input_u64s += (*super::types::bset(b, t)).u64s as usize;
        sort_iter_add(
            &mut sort.iter,
            btree_bkey_first(b, t),
            btree_bkey_last(b, t),
        );
    }

    let bytes = if sorting_entire_node {
        super::interior::btree_buf_bytes(&*b)
    } else {
        core::mem::size_of::<super::bset::btree_node>() + input_u64s * 8
    };
    let mut bounce = vec![0u64; bytes / 8];
    let out = bounce.as_mut_ptr().cast::<super::bset::btree_node>();
    (*out).keys.u64s = bch2_sort_keys(
        core::ptr::addr_of_mut!((*out).keys)
            .cast::<u64>()
            .add(3)
            .cast(),
        &mut sort.iter,
    ) as u16;
    assert!(
        core::ptr::addr_of!((*out).keys)
            .cast::<u64>()
            .add(3 + (*out).keys.u64s as usize)
            .cast::<u8>()
            <= out.cast::<u8>().add(bytes)
    );

    let mut journal_seq = 0u64;
    for idx in start_idx..end_idx {
        journal_seq =
            journal_seq.max((*super::types::bset(b, (*b).set.as_ptr().add(idx))).journal_seq);
    }
    (*start_bset).journal_seq = journal_seq;
    (*start_bset).u64s = (*out).keys.u64s;
    core::ptr::copy_nonoverlapping(
        core::ptr::addr_of!((*out).keys).cast::<u64>().add(3),
        start_bset.cast::<u64>().add(3),
        (*out).keys.u64s as usize,
    );

    for idx in start_idx + 1..end_idx {
        (*b).nr.bset_u64s[start_idx] = (*b).nr.bset_u64s[start_idx]
            .checked_add((*b).nr.bset_u64s[idx])
            .expect("bset live u64 count overflow");
    }
    (*b).nsets -= shift as u8;
    for idx in start_idx + 1..(*b).nsets as usize {
        (*b).nr.bset_u64s[idx] = (*b).nr.bset_u64s[idx + shift];
        (*b).set[idx] = (*b).set[idx + shift];
    }
    for idx in (*b).nsets as usize..MAX_BSETS {
        (*b).nr.bset_u64s[idx] = 0;
    }
    super::types::set_btree_bset_end(b, (*b).set.as_mut_ptr().add(start_idx));
    bch2_bset_set_no_aux_tree(b, (*b).set.as_mut_ptr().add(start_idx));

    super::bset_update::__bch2_verify_btree_nr_keys(b);
}

pub unsafe fn bch2_btree_sort_into(
    _c: *mut super::types::bch_fs,
    dst: *mut btree,
    src: *mut btree,
) {
    assert_eq!((*dst).nsets, 1);
    bch2_bset_set_no_aux_tree(dst, (*dst).set.as_mut_ptr());

    let mut src_iter = super::types::btree_node_iter::default();
    super::node_iter::bch2_btree_node_iter_init_from_start(&mut src_iter, src);
    let nr = bch2_sort_repack(
        super::types::btree_bset_first(dst),
        src,
        &mut src_iter,
        &(*dst).format,
        true,
    );

    super::types::set_btree_bset_end(dst, (*dst).set.as_mut_ptr());
    (*dst).nr.live_u64s += nr.live_u64s;
    (*dst).nr.bset_u64s[0] += nr.bset_u64s[0];
    (*dst).nr.packed_keys += nr.packed_keys;
    (*dst).nr.unpacked_keys += nr.unpacked_keys;

    super::bset_update::__bch2_verify_btree_nr_keys(dst);
}

pub unsafe fn bch2_btree_node_compact(c: *mut super::types::bch_fs, b: *mut btree) -> bool {
    let mut unwritten_idx = 0usize;
    while unwritten_idx < (*b).nsets as usize
        && super::interior::bset_written(
            b,
            super::types::bset(b, (*b).set.as_ptr().add(unwritten_idx)),
        )
    {
        unwritten_idx += 1;
    }

    let mut ret = false;
    if (*b).nsets as usize - unwritten_idx > 1 {
        bch2_btree_node_sort(c, b, unwritten_idx, (*b).nsets as usize);
        ret = true;
    }
    if unwritten_idx > 1 {
        bch2_btree_node_sort(c, b, 0, unwritten_idx);
        ret = true;
    }
    ret
}

pub unsafe fn should_compact_all(_c: *mut super::types::bch_fs, b: *mut btree) -> bool {
    let max_u64s = super::interior::btree_buf_max_u64s(&*b);
    let mid_u64s_bits = ((usize::BITS - 1 - max_u64s.leading_zeros()) + 9) / 2;
    super::types::bset_u64s((*b).set.as_ptr().add(1)) > 1usize.wrapping_shl(mid_u64s_bits) as u32
}

pub unsafe fn bch2_set_bset_needs_whiteout(i: *mut disk_bset, v: i32) {
    let mut k = (i.cast::<u64>()).add(3).cast::<bkey_packed_type>();
    let end = (i.cast::<u64>()).add(3 + (*i).u64s as usize);
    while k.cast::<u64>() != end {
        if v != 0 {
            (*k).format |= 0x80;
        } else {
            (*k).format &= 0x7f;
        }
        k = bkey_p_next(k);
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum compact_mode {
    COMPACT_LAZY,
    COMPACT_ALL,
}

pub unsafe fn should_compact_bset_lazy(b: *mut btree, t: *mut bset_tree) -> bool {
    let total_u64s = super::types::bset_u64s(t);
    let dead_u64s = super::types::bset_dead_u64s(b, t);
    dead_u64s > 64 && dead_u64s * 3 > total_u64s
}

unsafe fn should_compact_bset(
    b: *mut btree,
    t: *mut bset_tree,
    compacting: bool,
    mode: compact_mode,
) -> bool {
    if super::types::bset_dead_u64s(b, t) == 0 {
        return false;
    }
    match mode {
        compact_mode::COMPACT_LAZY => {
            should_compact_bset_lazy(b, t)
                || (compacting && !super::interior::bset_written(b, super::types::bset(b, t)))
        }
        compact_mode::COMPACT_ALL => true,
    }
}

pub unsafe fn bch2_drop_whiteouts(b: *mut btree, mode: compact_mode) -> bool {
    let mut ret = false;
    for idx in 0..(*b).nsets as usize {
        let t = (*b).set.as_mut_ptr().add(idx);
        let mut i = super::types::bset(b, t);
        let mut src: *mut btree_node_entry = core::ptr::null_mut();
        let mut dst: *mut btree_node_entry = core::ptr::null_mut();

        if idx != 0 && !super::interior::bset_written(b, i) {
            src = i.cast::<u8>().sub(16).cast();
            let write_block = (*b).data.cast::<u8>().add((*b).written as usize * 512);
            let previous_end = btree_bkey_last(b, t.sub(1)).cast::<u8>();
            dst = core::cmp::max(write_block, previous_end).cast();
        }
        if src != dst {
            ret = true;
        }

        if !should_compact_bset(b, t, ret, mode) {
            if src != dst {
                let bytes =
                    core::mem::size_of::<btree_node_entry>() + (*src).keys.u64s as usize * 8;
                core::ptr::copy(src.cast::<u8>(), dst.cast::<u8>(), bytes);
                i = core::ptr::addr_of_mut!((*dst).keys);
                set_btree_bset(b, t, i);
            }
            continue;
        }

        let start = btree_bkey_first(b, t);
        let end = btree_bkey_last(b, t);
        if src != dst {
            core::ptr::copy(
                src.cast::<u8>(),
                dst.cast::<u8>(),
                core::mem::size_of::<btree_node_entry>(),
            );
            i = core::ptr::addr_of_mut!((*dst).keys);
            set_btree_bset(b, t, i);
        }

        let mut out = (i.cast::<u64>()).add(3).cast::<bkey_packed_type>();
        let mut k = start;
        while k != end {
            let next = bkey_p_next(k);
            if !bkey_deleted(&*k) {
                core::ptr::copy(k.cast::<u64>(), out.cast::<u64>(), (*k).u64s as usize);
                out = bkey_p_next(out);
            } else {
                assert_eq!((*k).format & 0x80, 0);
            }
            k = next;
        }
        (*i).u64s = out.cast::<u64>().offset_from((i.cast::<u64>()).add(3)) as u16;
        super::types::set_btree_bset_end(b, t);
        bch2_bset_set_no_aux_tree(b, t);
        ret = true;
    }
    bch2_btree_build_aux_trees(b);
    ret
}

pub unsafe fn bch2_compact_whiteouts(
    _c: *mut super::types::bch_fs,
    b: *mut btree,
    mode: compact_mode,
) -> bool {
    bch2_drop_whiteouts(b, mode)
}

pub unsafe fn bset_byte_offset(b: *const btree, i: *const core::ffi::c_void) -> usize {
    i as usize - (*b).data as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btree::bkey::{
        bch2_bkey_pack_key, bkey_format, BKEY_NR_FIELDS, BKEY_U64S, KEY_FORMAT_CURRENT, SPOS,
    };
    use crate::btree::bset::{bset as disk_bset, btree_node as disk_btree_node};
    use crate::btree::node_iter::{
        bch2_btree_node_iter_init, bch2_btree_node_iter_peek, bkey_unpack_pos,
    };
    use crate::btree::types::{bset_tree, btree_node_iter};

    #[test]
    fn key_sort_fix_overlapping_keeps_newest_non_deleted_key() {
        unsafe {
            assert_eq!(core::mem::size_of::<sort_iter_set>(), 16);
            assert_eq!(core::mem::size_of::<sort_iter>(), 16);
            assert_eq!(core::mem::size_of::<sort_iter_stack>(), 80);

            let mut words = vec![0u64; 80];
            let mut b = btree::default();
            b.data = words.as_mut_ptr().cast::<disk_btree_node>();
            b.format = crate::btree::bkey::BKEY_FORMAT_CURRENT;
            b.nr_key_bits = crate::btree::bkey::bkey_format_key_bits(&b.format) as u8;
            b.nsets = 3;
            let layouts = [
                (17usize, [(1u64, 1u8), (2, 1)]),
                (30usize, [(1u64, 2u8), (3, 3)]),
                (43usize, [(1u64, 0u8), (2, 4)]),
            ];
            for (idx, (offset, keys)) in layouts.into_iter().enumerate() {
                let set = words.as_mut_ptr().add(offset).cast::<disk_bset>();
                (*set).u64s = 10;
                for (key_idx, (key_offset, type_)) in keys.into_iter().enumerate() {
                    *words
                        .as_mut_ptr()
                        .add(offset + 3 + key_idx * 5)
                        .cast::<bkey>() = bkey {
                        u64s: BKEY_U64S,
                        format: KEY_FORMAT_CURRENT,
                        type_,
                        p: SPOS(1, key_offset, 0),
                        ..Default::default()
                    };
                }
                b.set[idx] = bset_tree {
                    size: 0,
                    extra: BSET_NO_AUX_TREE_VAL,
                    data_offset: offset as u16,
                    aux_data_offset: u16::MAX,
                    end_offset: (offset + 13) as u16,
                };
            }

            let mut iter = sort_iter_stack::default();
            sort_iter_stack_init(&mut iter, &mut b);
            for idx in 0..3 {
                sort_iter_add(
                    &mut iter.iter,
                    btree_bkey_first(&b, b.set.as_ptr().add(idx)),
                    btree_bkey_last(&b, b.set.as_ptr().add(idx)),
                );
            }
            let mut output = vec![0u64; 20];
            let dst = output.as_mut_ptr().cast::<disk_bset>();
            let nr = bch2_key_sort_fix_overlapping(core::ptr::null_mut(), dst, &mut iter.iter);
            assert_eq!((*dst).u64s, 10);
            assert_eq!(nr.live_u64s, 10);
            assert_eq!(nr.bset_u64s, [10, 0, 0]);
            assert_eq!(nr.unpacked_keys, 2);
            let first = output.as_ptr().add(3).cast::<bkey>();
            let second = output.as_ptr().add(8).cast::<bkey>();
            assert_eq!(((*first).p, (*first).type_), (SPOS(1, 2, 0), 4));
            assert_eq!(((*second).p, (*second).type_), (SPOS(1, 3, 0), 3));
        }
    }

    #[test]
    fn sort_keys_keeps_only_live_keys() {
        unsafe {
            let mut words = vec![0u64; 128];
            let mut b = btree::default();
            b.data = words.as_mut_ptr().cast::<disk_btree_node>();
            b.byte_order = 10;
            b.format = crate::btree::bkey::BKEY_FORMAT_CURRENT;
            b.nr_key_bits = crate::btree::bkey::bkey_format_key_bits(&b.format) as u8;
            b.nsets = 1;

            let set = words.as_mut_ptr().add(17).cast::<disk_bset>();
            (*set).u64s = 15;
            for (idx, (offset, type_)) in [(1, 2), (2, 0), (3, 4)].into_iter().enumerate() {
                *words.as_mut_ptr().add(20 + idx * 5).cast::<bkey>() = bkey {
                    u64s: BKEY_U64S,
                    format: KEY_FORMAT_CURRENT,
                    type_,
                    p: SPOS(1, offset, 0),
                    ..Default::default()
                };
            }
            b.set[0] = bset_tree {
                size: 0,
                extra: BSET_NO_AUX_TREE_VAL,
                data_offset: 17,
                aux_data_offset: u16::MAX,
                end_offset: 35,
            };

            let mut iter = sort_iter_stack::default();
            sort_iter_stack_init(&mut iter, &mut b);
            sort_iter_add(
                &mut iter.iter,
                btree_bkey_first(&b, b.set.as_ptr()),
                btree_bkey_last(&b, b.set.as_ptr()),
            );
            let mut output = vec![0u64; 10];
            assert_eq!(
                bch2_sort_keys(output.as_mut_ptr().cast(), &mut iter.iter),
                10
            );
            assert_eq!((*(output.as_ptr().cast::<bkey>())).p, SPOS(1, 1, 0));
            assert_eq!((*(output.as_ptr().add(5).cast::<bkey>())).p, SPOS(1, 3, 0));
        }
    }

    #[test]
    fn sort_whiteouts_and_keep_only_unwritten_winners() {
        unsafe {
            let mut words = vec![0u64; 256];
            let mut b = btree::default();
            b.data = words.as_mut_ptr().cast::<disk_btree_node>();
            b.byte_order = 11;
            b.format = crate::btree::bkey::BKEY_FORMAT_CURRENT;
            b.nr_key_bits = crate::btree::bkey::bkey_format_key_bits(&b.format) as u8;
            b.nsets = 2;

            for (set_idx, (set_offset, keys)) in [
                (17usize, [(1u64, 2u8), (2, 0)]),
                (30usize, [(2u64, 3u8), (3, 4)]),
            ]
            .into_iter()
            .enumerate()
            {
                let set = words.as_mut_ptr().add(set_offset).cast::<disk_bset>();
                (*set).u64s = 10;
                for (key_idx, (offset, type_)) in keys.into_iter().enumerate() {
                    *words
                        .as_mut_ptr()
                        .add(set_offset + 3 + key_idx * 5)
                        .cast::<bkey>() = bkey {
                        u64s: BKEY_U64S,
                        format: KEY_FORMAT_CURRENT,
                        type_,
                        p: SPOS(1, offset, 0),
                        ..Default::default()
                    };
                }
                b.set[set_idx] = bset_tree {
                    size: 0,
                    extra: BSET_NO_AUX_TREE_VAL,
                    data_offset: set_offset as u16,
                    aux_data_offset: u16::MAX,
                    end_offset: (set_offset + 13) as u16,
                };
            }

            crate::btree::interior::bch2_push_whiteout(&mut b, SPOS(1, 3, 0));
            crate::btree::interior::bch2_push_whiteout(&mut b, SPOS(1, 4, 0));
            bch2_sort_whiteouts(core::ptr::null_mut(), &mut b);
            assert_eq!(
                bkey_unpack_pos(
                    &b,
                    crate::btree::interior::unwritten_whiteouts_start(&mut b)
                ),
                SPOS(1, 3, 0)
            );

            let mut iter = sort_iter_stack::default();
            sort_iter_stack_init(&mut iter, &mut b);
            for idx in 0..2 {
                sort_iter_add(
                    &mut iter.iter,
                    btree_bkey_first(&b, b.set.as_ptr().add(idx)),
                    btree_bkey_last(&b, b.set.as_ptr().add(idx)),
                );
            }
            sort_iter_add(
                &mut iter.iter,
                crate::btree::interior::unwritten_whiteouts_start(&mut b),
                crate::btree::interior::unwritten_whiteouts_end(&mut b),
            );
            let mut output = vec![0u64; 20];
            assert_eq!(
                bch2_sort_keys_keep_unwritten_whiteouts(output.as_mut_ptr().cast(), &mut iter.iter),
                20
            );
            for (idx, (offset, type_)) in [(1, 2), (2, 3), (3, 4), (4, 0)].into_iter().enumerate() {
                let key = output.as_ptr().add(idx * 5).cast::<bkey>();
                assert_eq!(((*key).p, (*key).type_), (SPOS(1, offset, 0), type_));
            }
        }
    }

    #[test]
    fn sort_repack_transforms_keys_and_falls_back_to_current_format() {
        unsafe {
            let mut words = vec![0u64; 128];
            let mut b = btree::default();
            b.data = words.as_mut_ptr().cast::<disk_btree_node>();
            b.byte_order = 10;
            b.format = crate::btree::bkey::BKEY_FORMAT_CURRENT;
            b.nr_key_bits = crate::btree::bkey::bkey_format_key_bits(&b.format) as u8;
            b.nsets = 1;

            let set = words.as_mut_ptr().add(17).cast::<disk_bset>();
            (*set).u64s = 10;
            for (idx, offset) in [5u64, 300].into_iter().enumerate() {
                *words.as_mut_ptr().add(20 + idx * 5).cast::<bkey>() = bkey {
                    u64s: BKEY_U64S,
                    format: KEY_FORMAT_CURRENT | 0x80,
                    type_: 2,
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

            let out_format = bkey_format {
                key_u64s: 1,
                nr_fields: BKEY_NR_FIELDS,
                bits_per_field: [8, 8, 4, 0, 0, 0],
                field_offset: [1, 1, 0, 0, 0, 0],
            };
            let mut iter = btree_node_iter::default();
            crate::btree::node_iter::bch2_btree_node_iter_init_from_start(&mut iter, &mut b);
            let mut output = vec![0u64; 16];
            let dst = output.as_mut_ptr().cast::<disk_bset>();
            let nr = bch2_sort_repack(dst, &mut b, &mut iter, &out_format, false);
            assert_eq!((*dst).u64s, 6);
            assert_eq!(nr.live_u64s, 6);
            assert_eq!(nr.unpacked_keys, 1);

            let packed = output.as_ptr().add(3).cast::<bkey_packed_type>();
            let mut unpacked = bkey::default();
            crate::btree::bkey::__bch2_bkey_unpack_key(&out_format, &mut unpacked, &*packed);
            assert_eq!(unpacked.p, SPOS(1, 5, 0));
            assert_eq!((*packed).format, crate::btree::bkey::KEY_FORMAT_LOCAL_BTREE);

            let fallback = output.as_ptr().add(4).cast::<bkey>();
            assert_eq!(
                ((*fallback).p, (*fallback).format),
                (SPOS(1, 300, 0), KEY_FORMAT_CURRENT)
            );
        }
    }

    #[test]
    fn btree_node_sort_merges_sets_and_preserves_accounting_and_journal_seq() {
        unsafe {
            let mut words = vec![0u64; 256];
            let mut b = btree::default();
            b.data = words.as_mut_ptr().cast::<disk_btree_node>();
            b.byte_order = 11;
            b.format = crate::btree::bkey::BKEY_FORMAT_CURRENT;
            b.nr_key_bits = crate::btree::bkey::bkey_format_key_bits(&b.format) as u8;
            b.nsets = 3;

            for (set_idx, (set_offset, journal_seq, keys)) in [
                (17usize, 1u64, [(1u64, 2u8), (2, 3)]),
                (32usize, 7u64, [(3u64, 0u8), (4, 4)]),
                (47usize, 4u64, [(5u64, 5u8), (6, 6)]),
            ]
            .into_iter()
            .enumerate()
            {
                let set = words.as_mut_ptr().add(set_offset).cast::<disk_bset>();
                (*set).journal_seq = journal_seq;
                (*set).u64s = 10;
                for (key_idx, (offset, type_)) in keys.into_iter().enumerate() {
                    *words
                        .as_mut_ptr()
                        .add(set_offset + 3 + key_idx * 5)
                        .cast::<bkey>() = bkey {
                        u64s: BKEY_U64S,
                        format: KEY_FORMAT_CURRENT,
                        type_,
                        p: SPOS(1, offset, 0),
                        ..Default::default()
                    };
                }
                b.set[set_idx] = bset_tree {
                    size: 0,
                    extra: BSET_NO_AUX_TREE_VAL,
                    data_offset: set_offset as u16,
                    aux_data_offset: u16::MAX,
                    end_offset: (set_offset + 13) as u16,
                };
            }
            b.nr.live_u64s = 25;
            b.nr.bset_u64s = [10, 5, 10];
            b.nr.unpacked_keys = 5;

            bch2_btree_node_sort(core::ptr::null_mut(), &mut b, 0, 3);
            assert_eq!(b.nsets, 1);
            assert_eq!(
                (*crate::btree::types::btree_bset_first(&mut b)).journal_seq,
                7
            );
            assert_eq!((*crate::btree::types::btree_bset_first(&mut b)).u64s, 25);
            assert_eq!(b.nr.live_u64s, 25);
            assert_eq!(b.nr.bset_u64s, [25, 0, 0]);
            assert_eq!(b.nr.unpacked_keys, 5);

            for (idx, offset) in [1u64, 2, 4, 5, 6].into_iter().enumerate() {
                let key = words.as_ptr().add(20 + idx * 5).cast::<bkey>();
                assert_eq!((*key).p, SPOS(1, offset, 0));
            }
            assert_eq!(
                crate::btree::bset_update::bch2_btree_node_count_keys(&mut b),
                b.nr
            );
        }
    }

    #[test]
    fn btree_sort_into_repacks_and_filters_deleted_keys() {
        unsafe {
            let mut src_words = vec![0u64; 128];
            let mut src = btree::default();
            src.data = src_words.as_mut_ptr().cast::<disk_btree_node>();
            src.byte_order = 10;
            src.format = crate::btree::bkey::BKEY_FORMAT_CURRENT;
            src.nr_key_bits = crate::btree::bkey::bkey_format_key_bits(&src.format) as u8;
            src.nsets = 1;
            let src_set = src_words.as_mut_ptr().add(17).cast::<disk_bset>();
            (*src_set).u64s = 15;
            for (idx, (offset, type_)) in [(2u64, 2u8), (3, 0), (4, 4)].into_iter().enumerate() {
                *src_words.as_mut_ptr().add(20 + idx * 5).cast::<bkey>() = bkey {
                    u64s: BKEY_U64S,
                    format: KEY_FORMAT_CURRENT | 0x80,
                    type_,
                    p: SPOS(1, offset, 0),
                    ..Default::default()
                };
            }
            src.set[0] = bset_tree {
                size: 0,
                extra: BSET_NO_AUX_TREE_VAL,
                data_offset: 17,
                aux_data_offset: u16::MAX,
                end_offset: 35,
            };

            let mut dst_words = vec![0u64; 128];
            let mut dst = btree::default();
            dst.data = dst_words.as_mut_ptr().cast::<disk_btree_node>();
            dst.byte_order = 10;
            dst.format = bkey_format {
                key_u64s: 1,
                nr_fields: BKEY_NR_FIELDS,
                bits_per_field: [8, 8, 4, 0, 0, 0],
                field_offset: [1, 1, 0, 0, 0, 0],
            };
            dst.nr_key_bits = crate::btree::bkey::bkey_format_key_bits(&dst.format) as u8;
            dst.nsets = 1;
            dst.set[0] = bset_tree {
                size: 0,
                extra: BSET_NO_AUX_TREE_VAL,
                data_offset: 17,
                aux_data_offset: u16::MAX,
                end_offset: 20,
            };

            bch2_btree_sort_into(core::ptr::null_mut(), &mut dst, &mut src);
            assert_eq!((*crate::btree::types::btree_bset_first(&mut dst)).u64s, 2);
            assert_eq!(dst.set[0].end_offset, 22);
            assert_eq!(dst.nr.live_u64s, 2);
            assert_eq!(dst.nr.bset_u64s, [2, 0, 0]);
            assert_eq!(dst.nr.packed_keys, 2);
            assert_eq!(dst.nr.unpacked_keys, 0);

            for (idx, offset) in [2u64, 4].into_iter().enumerate() {
                let packed = dst_words.as_ptr().add(20 + idx).cast::<bkey_packed_type>();
                let mut unpacked = bkey::default();
                crate::btree::bkey::__bch2_bkey_unpack_key(&dst.format, &mut unpacked, &*packed);
                assert_eq!(unpacked.p, SPOS(1, offset, 0));
                assert_eq!((*packed).format, crate::btree::bkey::KEY_FORMAT_LOCAL_BTREE);
            }
        }
    }

    #[test]
    fn drop_whiteouts_relocates_unwritten_bset_without_dead_keys() {
        unsafe {
            let mut words = vec![0u64; 256];
            let mut aux = vec![0u64; crate::btree::types::__btree_aux_data_bytes(11) / 8];
            let mut b = btree::default();
            b.data = words.as_mut_ptr().cast::<disk_btree_node>();
            b.aux_data = aux.as_mut_ptr().cast();
            b.byte_order = 11;
            b.format = crate::btree::bkey::BKEY_FORMAT_CURRENT;
            b.nr_key_bits = crate::btree::bkey::bkey_format_key_bits(&b.format) as u8;
            bch2_btree_keys_init(&mut b);
            b.nsets = 2;
            b.written = 1;
            (*b.data).min_key = SPOS(1, 0, 0);
            (*b.data).max_key = SPOS(1, 10, 0);

            let first = words.as_mut_ptr().add(17).cast::<disk_bset>();
            (*first).seq = 55;
            (*first).u64s = 5;
            *words.as_mut_ptr().add(20).cast::<bkey>() = bkey {
                u64s: BKEY_U64S,
                format: KEY_FORMAT_CURRENT,
                type_: 1,
                p: SPOS(1, 1, 0),
                ..Default::default()
            };

            let second_entry = words.as_mut_ptr().add(78).cast::<btree_node_entry>();
            (*second_entry).keys.seq = 55;
            (*second_entry).keys.journal_seq = 9;
            (*second_entry).keys.u64s = 5;
            *words.as_mut_ptr().add(83).cast::<bkey>() = bkey {
                u64s: BKEY_U64S,
                format: KEY_FORMAT_CURRENT,
                type_: 2,
                p: SPOS(1, 2, 0),
                ..Default::default()
            };
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
                data_offset: 80,
                aux_data_offset: u16::MAX,
                end_offset: 88,
            };
            b.nr.live_u64s = 10;
            b.nr.bset_u64s = [5, 5, 0];
            b.nr.unpacked_keys = 2;

            assert!(bch2_drop_whiteouts(&mut b, compact_mode::COMPACT_ALL));
            assert_eq!(b.set[1].data_offset, 66);
            assert_eq!(b.set[1].end_offset, 74);
            let moved = words.as_ptr().add(64).cast::<btree_node_entry>();
            assert_eq!((*moved).keys.seq, 55);
            assert_eq!((*moved).keys.journal_seq, 9);
            assert_eq!((*moved).keys.u64s, 5);
            assert_eq!((*(words.as_ptr().add(69).cast::<bkey>())).p, SPOS(1, 2, 0));
            assert_eq!(b.nr.bset_u64s, [5, 5, 0]);
        }
    }

    #[test]
    fn builds_ro_aux_tree_with_live_bfloats() {
        let mut words = vec![0u64; 180];
        let mut aux = vec![0u64; 16];
        let mut b = btree::default();
        b.data = words.as_mut_ptr() as *mut disk_btree_node;
        b.aux_data = aux.as_mut_ptr().cast();
        b.byte_order = 12;
        b.format = bkey_format {
            key_u64s: 1,
            nr_fields: BKEY_NR_FIELDS,
            bits_per_field: [8, 8, 4, 0, 0, 0],
            field_offset: [1, 1, 0, 0, 0, 0],
        };
        b.nr_key_bits = 20;
        b.nsets = 1;

        unsafe {
            (*b.data).min_key = SPOS(1, 1, 0);
            (*b.data).max_key = SPOS(1, 100, 0);
            let disk_set = words.as_mut_ptr().add(17) as *mut disk_bset;
            (*disk_set).u64s = 100;
            for i in 0..100usize {
                let input = bkey {
                    u64s: BKEY_U64S,
                    format: KEY_FORMAT_CURRENT,
                    type_: 1,
                    p: SPOS(1, (i + 1) as u64, 0),
                    ..Default::default()
                };
                let mut packed = bkey_packed_type::default();
                assert!(bch2_bkey_pack_key(&mut packed, &input, &b.format));
                *words.as_mut_ptr().add(20 + i) = *(core::ptr::addr_of!(packed) as *const u64);
            }
            b.set[0] = bset_tree {
                size: 0,
                extra: BSET_NO_AUX_TREE_VAL,
                data_offset: 17,
                aux_data_offset: u16::MAX,
                end_offset: 120,
            };

            let t = b.set.as_mut_ptr();
            bch2_bset_build_aux_tree(&mut b, t, false);
            assert_eq!(bset_aux_tree_type(t), aux_type::BSET_RO_AUX_TREE);
            assert!((*t).size > 2);
            for i in 1..(*t).size as usize {
                assert!((*ro_aux_tree_base(&b, t).add(i)).exponent < u8::MAX);
            }

            let search = SPOS(1, 42, 0);
            let mut iter = btree_node_iter::default();
            bch2_btree_node_iter_init(core::ptr::null_mut(), &mut b, &mut iter, &search);
            let found = bkey_unpack_pos(&b, bch2_btree_node_iter_peek(&mut iter, &mut b));
            let offset = found.offset;
            assert_eq!(offset, 42);
        }
    }
}
