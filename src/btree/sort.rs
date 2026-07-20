use crate::bcachefs::*;
use crate::bcachefs_format::*;
use crate::btree::bkey::*;
use crate::btree::bset::*;
use crate::btree::types::*;
use crate::errcode::*;

/// Sort iter — merges keys from multiple bsets
pub struct SortIter {
    pub b: *mut BtreeNode,
    pub used: u32,
    pub size: u32,
    pub data: Vec<SortIterEntry>,
}

pub struct SortIterEntry {
    pub k: *const u8,
    pub end: *const u8,
    pub pos: Bpos,
}

/// Initialize sort iterator
pub fn sort_iter_init(b: &mut BtreeNode, size: u32) -> SortIter {
    SortIter {
        b: b as *mut BtreeNode,
        used: 0,
        size,
        data: Vec::with_capacity(size as usize),
    }
}

/// Add a bset to the sort iter
pub fn sort_iter_add(iter: &mut SortIter, b: &BtreeNode, i: &Bset) {
    if i.u64s() == 0 {
        return;
    }
    let entry = SortIterEntry {
        k: i.start().as_ptr(),
        end: vstruct_last(i).as_ptr(),
        pos: Bpos::default(),
    };
    iter.data.push(entry);
    iter.used += 1;
}

/// Peek next key from the sort heap
pub fn sort_iter_peek(iter: &mut SortIter) -> Option<&BkeyPacked> {
    // Find the smallest key among all entries
    let mut best_idx = None;
    let mut best_k: Option<&BkeyPacked> = None;

    for (idx, entry) in iter.data.iter().enumerate() {
        if entry.k >= entry.end {
            continue;
        }
        let k = unsafe { &*(entry.k as *const BkeyPacked) };
        match best_k {
            None => {
                best_idx = Some(idx);
                best_k = Some(k);
            }
            Some(bk) => {
                let cmp = bkey_cmp_packed(&entry.pos, &k.p);
                if cmp < 0 {
                    best_idx = Some(idx);
                    best_k = Some(k);
                }
            }
        }
    }

    let idx = best_idx?;
    let k = best_k?;
    // Update position
    let p = unsafe { &*k };
    iter.data[idx].pos = p.p;
    Some(k)
}

/// Advance the sort iterator
pub fn sort_iter_advance(iter: &mut SortIter, key: &BkeyPacked) {
    for entry in iter.data.iter_mut() {
        if entry.k == key as *const _ as *const u8 {
            entry.k = unsafe { entry.k.add(4 * key.k.u64s as usize) }; // u64s * 8 bytes
            break;
        }
    }
}

/// Sort keys into a destination buffer
pub fn bch2_sort_keys_into(
    dst: &mut Vec<u64>,
    iter: &mut SortIter,
    filter_whiteouts: bool,
) -> usize {
    let mut written = 0usize;
    loop {
        let k = match sort_iter_peek(iter) {
            Some(k) => k,
            None => break,
        };

        if !filter_whiteouts || !bkey_packed_whiteout(k) {
            let u64s = k.u64s() as usize;
            let src = k as *const _ as *const u64;
            dst.extend_from_slice(unsafe { core::slice::from_raw_parts(src, u64s) });
            written += u64s;
        }

        sort_iter_advance(iter, k);
    }
    written
}

/// Compare two packed keys
pub fn bkey_cmp_packed(pos: &Bpos, other: &Bpos) -> i32 {
    bkey_cmp(pos, other)
}
