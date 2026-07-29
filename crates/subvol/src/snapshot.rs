use std::sync::{Mutex, RwLock};
use std::{fs::File, io::Read};

use crate::btree::bkey::bch_val;
use crate::btree::types::bch_fs;

pub const IS_ANCESTOR_BITMAP: u32 = 128;
pub const KEY_TYPE_snapshot: u8 = 22;
pub const BCH_SNAPSHOT_WILL_DELETE: u32 = 1 << 0;
pub const BCH_SNAPSHOT_SUBVOL: u32 = 1 << 1;
pub const BCH_SNAPSHOT_DELETED: u32 = 1 << 2;
pub const BCH_SNAPSHOT_NO_KEYS: u32 = 1 << 3;
pub const BCH_FS_need_delete_dead_snapshots: usize = 19;
pub const BCH_ERR_invalid_snapshot_node: i32 = -2604;

/// Matches the local `DEFINE_DARRAY_NAMED(snapshot_id_list, u32)` layout.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct snapshot_id_list {
    pub nr: usize,
    pub size: usize,
    pub data: *mut u32,
}

pub unsafe fn snapshot_list_has_id(s: *const snapshot_id_list, id: u32) -> bool {
    let mut i = 0usize;
    while i < (*s).nr {
        if *(*s).data.add(i) == id {
            return true;
        }
        i += 1;
    }
    false
}

pub unsafe fn snapshot_list_has_ancestor(
    trans: *mut crate::btree::iter::btree_trans,
    s: *const snapshot_id_list,
    id: u32,
) -> bool {
    let mut i = 0usize;
    while i < (*s).nr {
        if bch2_snapshot_is_ancestor(&*(*trans).c, id, *(*s).data.add(i)) {
            return true;
        }
        i += 1;
    }
    false
}

pub unsafe fn snapshot_list_add_nodup(
    _c: *mut bch_fs,
    s: *mut snapshot_id_list,
    id: u32,
) -> i32 {
    if snapshot_list_has_id(s, id) {
        return 0;
    }
    let new_size = (*s).nr.wrapping_add(1);
    if new_size > (*s).size {
        let Some(capacity) = new_size.checked_next_power_of_two() else {
            crate::rewrite_log_error!(
                "error reallocating snapshot_id_list (size {})",
                (*s).size
            );
            return -12;
        };
        let Ok(layout) = std::alloc::Layout::array::<u32>(capacity) else {
            crate::rewrite_log_error!(
                "error reallocating snapshot_id_list (size {})",
                (*s).size
            );
            return -12;
        };
        let data = std::alloc::alloc(layout).cast::<u32>();
        if data.is_null() {
            crate::rewrite_log_error!(
                "error reallocating snapshot_id_list (size {})",
                (*s).size
            );
            return -12;
        }
        if (*s).size != 0 {
            core::ptr::copy_nonoverlapping((*s).data, data, (*s).size);
            if !(*s).data.is_null() {
                if let Ok(old_layout) = std::alloc::Layout::array::<u32>((*s).size) {
                    std::alloc::dealloc((*s).data.cast::<u8>(), old_layout);
                }
            }
        }
        (*s).data = data;
        (*s).size = capacity;
    }
    *(*s).data.add((*s).nr) = id;
    (*s).nr += 1;
    0
}

pub unsafe fn snapshot_list_add(c: *mut bch_fs, s: *mut snapshot_id_list, id: u32) -> i32 {
    assert!(!snapshot_list_has_id(s, id));
    snapshot_list_add_nodup(c, s, id)
}

pub unsafe fn snapshot_list_merge(
    c: *mut bch_fs,
    dst: *mut snapshot_id_list,
    src: *const snapshot_id_list,
) -> i32 {
    let mut i = 0usize;
    while i < (*src).nr {
        let ret = snapshot_list_add_nodup(c, dst, *(*src).data.add(i));
        if ret != 0 {
            return ret;
        }
        i += 1;
    }
    0
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct bch_snapshot {
    pub v: bch_val,
    pub flags: u32,
    pub parent: u32,
    pub children: [u32; 2],
    pub subvol: u32,
    pub tree: u32,
    pub depth: u32,
    pub skip: [u32; 3],
    pub btime: [u64; 2],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct bch_snapshot_tree {
    pub v: bch_val,
    pub master_subvol: u32,
    pub root_snapshot: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct bkey_i_snapshot {
    pub k: crate::btree::bkey::bkey,
    pub v: bch_snapshot,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum snapshot_id_state {
    #[default]
    SNAPSHOT_ID_empty,
    SNAPSHOT_ID_live,
    SNAPSHOT_ID_deleted,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct snapshot_t {
    pub state: snapshot_id_state,
    pub parent: u32,
    pub skip: [u32; 3],
    pub depth: u32,
    pub children: [u32; 2],
    pub subvol: u32,
    pub tree: u32,
    pub is_ancestor: [usize; 2],
}

#[derive(Default)]
pub struct snapshot_table {
    pub nr: usize,
    pub s: Vec<snapshot_t>,
}

#[derive(Default)]
pub struct bch_fs_snapshots {
    pub table: RwLock<snapshot_table>,
    pub table_lock: Mutex<()>,
}

pub fn __snapshot_t(t: &snapshot_table, id: u32) -> Option<&snapshot_t> {
    let idx = (u32::MAX - id) as usize;
    if idx < t.nr {
        t.s.get(idx)
    } else {
        None
    }
}

pub fn __snapshot_t_mut(t: &mut snapshot_table, id: u32) -> Option<&mut snapshot_t> {
    let idx = (u32::MAX - id) as usize;
    if idx < t.nr {
        t.s.get_mut(idx)
    } else {
        None
    }
}

pub fn bch2_snapshot_parent_early(c: &bch_fs, id: u32) -> u32 {
    let table = c.snapshots.table.read().unwrap();
    __snapshot_t(&table, id).map_or(0, |s| s.parent)
}

pub fn bch2_snapshot_parent(c: &bch_fs, id: u32) -> u32 {
    let table = c.snapshots.table.read().unwrap();
    __snapshot_t(&table, id).map_or(0, |s| s.parent)
}

pub fn bch2_snapshot_tree(c: &bch_fs, id: u32) -> u32 {
    let table = c.snapshots.table.read().unwrap();
    __snapshot_t(&table, id).map_or(0, |s| s.tree)
}

pub fn bch2_snapshots_same_tree(c: &bch_fs, id1: u32, id2: u32) -> bool {
    if id1 == id2 {
        return true;
    }
    let table = c.snapshots.table.read().unwrap();
    match (__snapshot_t(&table, id1), __snapshot_t(&table, id2)) {
        (Some(s1), Some(s2)) => s1.tree == s2.tree,
        _ => false,
    }
}

pub fn bch2_snapshot_nth_parent(c: &bch_fs, mut id: u32, mut n: u32) -> u32 {
    let table = c.snapshots.table.read().unwrap();
    while n != 0 {
        id = __snapshot_t(&table, id).map_or(0, |s| s.parent);
        n -= 1;
    }
    id
}

pub fn bch2_snapshot_skiplist_get(c: &bch_fs, mut id: u32) -> u32 {
    if id == 0 {
        return 0;
    }
    let table = c.snapshots.table.read().unwrap();
    let snapshot = __snapshot_t(&table, id).expect("invalid snapshot node");
    if snapshot.parent == 0 {
        return id;
    }

    let mut random = [0u8; 4];
    File::open("/dev/urandom")
        .expect("get_random_bytes failed")
        .read_exact(&mut random)
        .expect("get_random_bytes failed");
    let mut n = u32::from_ne_bytes(random) % snapshot.depth;
    while n != 0 {
        id = __snapshot_t(&table, id).expect("invalid snapshot node").parent;
        n -= 1;
    }
    id
}

pub fn bch2_snapshot_root(c: &bch_fs, mut id: u32) -> u32 {
    let table = c.snapshots.table.read().unwrap();
    while let Some(parent) = __snapshot_t(&table, id)
        .map(|s| s.parent)
        .filter(|p| *p != 0)
    {
        id = parent;
    }
    id
}

pub fn bch2_snapshot_id_state(c: &bch_fs, id: u32) -> snapshot_id_state {
    let table = c.snapshots.table.read().unwrap();
    __snapshot_t(&table, id).map_or(snapshot_id_state::SNAPSHOT_ID_empty, |s| s.state)
}

pub fn bch2_snapshot_exists(c: &bch_fs, id: u32) -> bool {
    bch2_snapshot_id_state(c, id) == snapshot_id_state::SNAPSHOT_ID_live
}

pub fn bch2_snapshot_is_internal_node(c: &bch_fs, id: u32) -> i32 {
    let table = c.snapshots.table.read().unwrap();
    match __snapshot_t(&table, id) {
        Some(s) => (s.children[0] != 0) as i32,
        None => BCH_ERR_invalid_snapshot_node,
    }
}

pub fn bch2_snapshot_is_leaf(c: &bch_fs, id: u32) -> i32 {
    let ret = bch2_snapshot_is_internal_node(c, id);
    if ret < 0 {
        ret
    } else {
        1 - ret
    }
}

pub fn bch2_snapshot_depth(c: &bch_fs, parent: u32) -> u32 {
    if parent == 0 {
        return 0;
    }
    let table = c.snapshots.table.read().unwrap();
    __snapshot_t(&table, parent)
        .expect("invalid snapshot node")
        .depth
        .wrapping_add(1)
}

pub fn bch2_snapshot_has_children(c: &bch_fs, id: u32) -> bool {
    let table = c.snapshots.table.read().unwrap();
    __snapshot_t(&table, id)
        .map(|s| (s.children[0] | s.children[1]) != 0)
        .unwrap_or(false)
}

pub fn bch2_snapshot_live_descendent(c: &bch_fs, mut id: u32) -> u32 {
    let table = c.snapshots.table.read().unwrap();
    loop {
        let s = __snapshot_t(&table, id).expect("invalid snapshot node");
        if s.state == snapshot_id_state::SNAPSHOT_ID_live {
            return id;
        }
        assert!(s.children[0] != 0 && s.children[1] == 0);
        id = s.children[0];
    }
}

fn get_ancestor_below(t: &snapshot_table, id: u32, ancestor: u32) -> u32 {
    let Some(s) = __snapshot_t(t, id) else {
        return 0;
    };
    if s.skip[2] <= ancestor {
        s.skip[2]
    } else if s.skip[1] <= ancestor {
        s.skip[1]
    } else if s.skip[0] <= ancestor {
        s.skip[0]
    } else {
        s.parent
    }
}

fn test_ancestor_bitmap(t: &snapshot_table, id: u32, ancestor: u32) -> bool {
    let Some(s) = __snapshot_t(t, id) else {
        return false;
    };
    let bit = ancestor - id - 1;
    s.is_ancestor[bit as usize / usize::BITS as usize] & (1usize << (bit % usize::BITS) as usize)
        != 0
}

pub fn bch2_snapshot_is_ancestor_early(c: &bch_fs, mut id: u32, ancestor: u32) -> bool {
    let table = c.snapshots.table.read().unwrap();
    while id != 0 && id < ancestor {
        id = __snapshot_t(&table, id).map_or(0, |s| s.parent);
    }
    id == ancestor
}

pub fn __bch2_snapshot_is_ancestor(c: &bch_fs, mut id: u32, ancestor: u32) -> bool {
    let table = c.snapshots.table.read().unwrap();
    if ancestor >= IS_ANCESTOR_BITMAP {
        while id != 0 && id < ancestor - IS_ANCESTOR_BITMAP {
            id = get_ancestor_below(&table, id, ancestor);
        }
    }
    if id != 0 && id < ancestor {
        test_ancestor_bitmap(&table, id, ancestor)
    } else {
        id == ancestor
    }
}

pub fn bch2_snapshot_is_ancestor(c: &bch_fs, id: u32, ancestor: u32) -> bool {
    assert_ne!(id, 0);
    assert_ne!(ancestor, 0);
    id == ancestor || __bch2_snapshot_is_ancestor(c, id, ancestor)
}

pub unsafe fn __bch2_get_snapshot_overwrites(
    trans: *mut crate::btree::iter::btree_trans,
    btree_id: u8,
    pos: crate::btree::bkey::bpos,
    s: *mut snapshot_id_list,
) -> i32 {
    let mut iter = crate::btree::iter::btree_iter::default();
    crate::btree::iter::bch2_trans_iter_init(
        trans,
        &mut iter,
        btree_id,
        crate::btree::bkey::bpos_predecessor(pos),
        crate::btree::iter::BTREE_ITER_all_snapshots,
    );
    let mut ret = 0;
    loop {
        let k = crate::btree::iter::bch2_btree_iter_peek_prev(&mut iter);
        let err = crate::btree::bkey::bkey_err(k);
        if err != 0 {
            ret = err;
            break;
        }
        if k.k.is_null() || !crate::btree::bkey::bpos_eq((*k.k).p, pos) {
            break;
        }
        let c = (*trans).c;
        if !bch2_snapshot_is_ancestor(&*c, (*k.k).p.snapshot, pos.snapshot)
            || snapshot_list_has_ancestor(trans, s, (*k.k).p.snapshot)
        {
            if !crate::btree::iter::bch2_btree_iter_advance(&mut iter) {
                break;
            }
            continue;
        }
        ret = snapshot_list_add(c, s, (*k.k).p.snapshot);
        if ret != 0 {
            break;
        }
        if !crate::btree::iter::bch2_btree_iter_advance(&mut iter) {
            break;
        }
    }
    crate::btree::iter::bch2_trans_iter_exit(&mut iter);
    ret
}

pub unsafe fn __bch2_key_has_snapshot_overwrites(
    trans: *mut crate::btree::iter::btree_trans,
    btree_id: u8,
    pos: crate::btree::bkey::bpos,
) -> i32 {
    let mut iter = crate::btree::iter::btree_iter::default();
    crate::btree::iter::bch2_trans_iter_init(
        trans,
        &mut iter,
        btree_id,
        crate::btree::bkey::bpos_predecessor(pos),
        crate::btree::iter::BTREE_ITER_not_extents
            | crate::btree::iter::BTREE_ITER_all_snapshots,
    );
    let mut ret = 0;
    loop {
        let k = crate::btree::iter::bch2_btree_iter_peek_prev(&mut iter);
        let err = crate::btree::bkey::bkey_err(k);
        if err != 0 {
            ret = err;
            break;
        }
        if k.k.is_null() {
            break;
        }
        if !crate::btree::bkey::bpos_eq(pos, (*k.k).p) {
            break;
        }
        if bch2_snapshot_is_ancestor(&*(*trans).c, (*k.k).p.snapshot, pos.snapshot) {
            ret = 1;
            break;
        }
        if !crate::btree::iter::bch2_btree_iter_advance(&mut iter) {
            break;
        }
    }
    crate::btree::iter::bch2_trans_iter_exit(&mut iter);
    ret
}

pub fn __bch2_snapshot_tree_next(t: &snapshot_table, mut id: u32, depth: &mut u32) -> u32 {
    let n = __snapshot_t(t, id).map_or(0, |s| s.children[0]);
    if n != 0 {
        *depth += 1;
        return n;
    }
    while let Some(parent) = __snapshot_t(t, id).map(|s| s.parent).filter(|p| *p != 0) {
        *depth -= 1;
        let n = __snapshot_t(t, parent).map_or(0, |s| s.children[1]);
        if n != 0 && n != id {
            *depth += 1;
            return n;
        }
        id = parent;
    }
    0
}

pub fn bch2_snapshot_tree_next(c: &bch_fs, id: u32, depth: &mut u32) -> u32 {
    let table = c.snapshots.table.read().unwrap();
    __bch2_snapshot_tree_next(&table, id, depth)
}

pub unsafe fn bch2_mark_snapshot(
    trans: *mut crate::btree::iter::btree_trans,
    op: crate::btree::update::btree_trigger_op,
) -> i32 {
    let c = (*trans).c;
    let id = (*op.new.k).p.offset as u32;
    let _table_lock = (*c).snapshots.table_lock.lock().unwrap();
    let mut table = (*c).snapshots.table.write().unwrap();
    let idx = (u32::MAX - id) as usize;
    if idx >= table.nr {
        let new_size = (idx + 1).next_power_of_two();
        table.s.resize(new_size, snapshot_t::default());
        table.nr = new_size;
    }

    if (*op.new.k).type_ == KEY_TYPE_snapshot {
        let s = &*(op.new.v.cast::<bch_snapshot>());
        let value_bytes = ((*op.new.k).u64s.saturating_sub(5) as usize) * 8;
        let mut entry = snapshot_t {
            state: if s.flags & (BCH_SNAPSHOT_DELETED | BCH_SNAPSHOT_NO_KEYS) == 0 {
                snapshot_id_state::SNAPSHOT_ID_live
            } else {
                snapshot_id_state::SNAPSHOT_ID_deleted
            },
            parent: s.parent,
            children: s.children,
            subvol: if s.flags & BCH_SNAPSHOT_SUBVOL != 0 {
                s.subvol
            } else {
                0
            },
            tree: s.tree,
            ..Default::default()
        };
        if value_bytes > core::mem::offset_of!(bch_snapshot, depth) {
            entry.depth = s.depth;
            entry.skip = s.skip;
        }
        let mut parent = entry.parent;
        while parent != 0 && parent - id - 1 < IS_ANCESTOR_BITMAP {
            let bit = parent - id - 1;
            entry.is_ancestor[bit as usize / usize::BITS as usize] |=
                1usize << (bit % usize::BITS) as usize;
            parent = __snapshot_t(&table, parent).map_or(0, |v| v.parent);
        }
        table.s[idx] = entry;
        if s.flags & BCH_SNAPSHOT_WILL_DELETE != 0 {
            (*c).flags |= 1usize << BCH_FS_need_delete_dead_snapshots;
        }
    } else {
        table.s[idx] = snapshot_t::default();
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set_ancestor_bit(s: &mut snapshot_t, id: u32, ancestor: u32) {
        let bit = ancestor - id - 1;
        s.is_ancestor[bit as usize / usize::BITS as usize] |=
            1usize << (bit % usize::BITS) as usize;
    }

    #[test]
    fn snapshot_layout_matches_local_format() {
        assert_eq!(core::mem::size_of::<bch_snapshot>(), 56);
        assert_eq!(core::mem::size_of::<bch_snapshot_tree>(), 8);
        assert_eq!(core::mem::size_of::<snapshot_t>(), 56);
        assert_eq!(core::mem::size_of::<bkey_i_snapshot>(), 96);
        assert_eq!(core::mem::size_of::<snapshot_id_list>(), 24);
    }

    #[test]
    fn snapshot_id_list_lookup_matches_local_darray_find() {
        let mut ids = [u32::MAX, u32::MAX - 2, 7];
        let list = snapshot_id_list {
            nr: ids.len(),
            size: ids.len(),
            data: ids.as_mut_ptr(),
        };
        assert!(unsafe { snapshot_list_has_id(&list, u32::MAX - 2) });
        assert!(!unsafe { snapshot_list_has_id(&list, 8) });
        let empty = snapshot_id_list::default();
        assert!(!unsafe { snapshot_list_has_ancestor(core::ptr::null_mut(), &empty, 1) });
    }

    #[test]
    fn snapshot_id_list_push_and_merge_follow_darray_growth() {
        unsafe {
            let mut dst = snapshot_id_list::default();
            assert_eq!(snapshot_list_add_nodup(core::ptr::null_mut(), &mut dst, 3), 0);
            assert_eq!(snapshot_list_add_nodup(core::ptr::null_mut(), &mut dst, 3), 0);
            let mut source_ids = [5u32, 3u32, 8u32];
            let source = snapshot_id_list {
                nr: source_ids.len(),
                size: source_ids.len(),
                data: source_ids.as_mut_ptr(),
            };
            assert_eq!(snapshot_list_merge(core::ptr::null_mut(), &mut dst, &source), 0);
            assert_eq!(dst.nr, 3);
            assert_eq!(*dst.data.add(0), 3);
            assert_eq!(*dst.data.add(1), 5);
            assert_eq!(*dst.data.add(2), 8);
            std::alloc::dealloc(
                dst.data.cast::<u8>(),
                std::alloc::Layout::array::<u32>(dst.size).unwrap(),
            );
        }
    }

    #[test]
    fn snapshot_parent_bitmap_skip_and_tree_walk() {
        let c = bch_fs::default();
        let root = u32::MAX;
        let left = root - 1;
        let leaf = root - 2;
        let right = root - 3;
        {
            let mut t = c.snapshots.table.write().unwrap();
            t.nr = 4;
            t.s.resize(4, snapshot_t::default());
            t.s[0] = snapshot_t {
                state: snapshot_id_state::SNAPSHOT_ID_live,
                tree: 10,
                children: [left, right],
                ..Default::default()
            };
            t.s[1] = snapshot_t {
                state: snapshot_id_state::SNAPSHOT_ID_live,
                parent: root,
                children: [leaf, 0],
                tree: 10,
                skip: [root, root, root],
                depth: 1,
                ..Default::default()
            };
            set_ancestor_bit(&mut t.s[1], left, root);
            t.s[2] = snapshot_t {
                state: snapshot_id_state::SNAPSHOT_ID_live,
                parent: left,
                tree: 10,
                skip: [left, root, root],
                depth: 2,
                ..Default::default()
            };
            set_ancestor_bit(&mut t.s[2], leaf, left);
            set_ancestor_bit(&mut t.s[2], leaf, root);
            t.s[3] = snapshot_t {
                state: snapshot_id_state::SNAPSHOT_ID_live,
                parent: root,
                tree: 20,
                skip: [root, root, root],
                depth: 1,
                ..Default::default()
            };
            set_ancestor_bit(&mut t.s[3], right, root);
        }

        assert_eq!(bch2_snapshot_parent_early(&c, leaf), left);
        assert_eq!(bch2_snapshot_parent(&c, leaf), left);
        assert_eq!(bch2_snapshot_tree(&c, leaf), 10);
        assert!(bch2_snapshots_same_tree(&c, root, leaf));
        assert!(!bch2_snapshots_same_tree(&c, root, right));
        assert_eq!(bch2_snapshot_depth(&c, left), 2);
        assert!(bch2_snapshot_has_children(&c, root));
        assert!(!bch2_snapshot_has_children(&c, leaf));
        assert_eq!(bch2_snapshot_live_descendent(&c, leaf), leaf);
        assert_eq!(bch2_snapshot_is_internal_node(&c, root), 1);
        assert_eq!(bch2_snapshot_is_leaf(&c, leaf), 1);
        assert_eq!(
            bch2_snapshot_is_internal_node(&c, 123),
            BCH_ERR_invalid_snapshot_node
        );
        assert_eq!(bch2_snapshot_nth_parent(&c, leaf, 2), root);
        assert_eq!(bch2_snapshot_root(&c, leaf), root);
        let skip = bch2_snapshot_skiplist_get(&c, leaf);
        assert!(skip == leaf || skip == left);
        assert_eq!(bch2_snapshot_skiplist_get(&c, root), root);
        assert_eq!(bch2_snapshot_skiplist_get(&c, 0), 0);
        assert!(bch2_snapshot_is_ancestor(&c, leaf, root));
        assert!(!bch2_snapshot_is_ancestor(&c, right, left));

        let mut depth = 0;
        assert_eq!(bch2_snapshot_tree_next(&c, root, &mut depth), left);
        assert_eq!(bch2_snapshot_tree_next(&c, left, &mut depth), leaf);
        assert_eq!(bch2_snapshot_tree_next(&c, leaf, &mut depth), right);
        assert_eq!(bch2_snapshot_tree_next(&c, right, &mut depth), 0);
    }

    #[test]
    fn atomic_snapshot_trigger_updates_memory_table() {
        use crate::btree::bkey::{
            bkey_format_key_bits, BKEY_FORMAT_CURRENT, KEY_FORMAT_CURRENT, POS_MIN, SPOS, SPOS_MAX,
        };
        use crate::btree::bset::{bset as disk_bset, btree_node as disk_btree_node};
        use crate::btree::iter::{
            bch2_btree_iter_peek, bch2_trans_init, bch2_trans_iter_exit, bch2_trans_iter_init,
            btree_iter, btree_trans, BTREE_ITER_intent,
        };
        use crate::btree::types::{bch2_btree_id_root_set, bset_tree, btree, BSET_NO_AUX_TREE_VAL};
        use crate::btree::update::{bch2_trans_commit, bch2_trans_update};

        unsafe {
            let mut words = vec![0u64; 100];
            let mut node = Box::new(btree::default());
            node.data = words.as_mut_ptr().cast::<disk_btree_node>();
            node.format = BKEY_FORMAT_CURRENT;
            node.nr_key_bits = bkey_format_key_bits(&node.format) as u8;
            node.nsets = 1;
            node.byte_order = 9;
            (*node.data).min_key = POS_MIN;
            (*node.data).max_key = SPOS_MAX;
            let set = words.as_mut_ptr().add(17).cast::<disk_bset>();
            node.set[0] = bset_tree {
                size: 0,
                extra: BSET_NO_AUX_TREE_VAL,
                data_offset: 17,
                aux_data_offset: u16::MAX,
                end_offset: 20,
            };

            let mut c = bch_fs::default();
            bch2_btree_id_root_set(&mut c, 0, &mut *node);
            let id = u32::MAX;
            let mut key = bkey_i_snapshot {
                k: crate::btree::bkey::bkey {
                    u64s: 12,
                    format: KEY_FORMAT_CURRENT,
                    type_: KEY_TYPE_snapshot,
                    p: SPOS(0, id as u64, 0),
                    ..Default::default()
                },
                v: bch_snapshot {
                    flags: BCH_SNAPSHOT_SUBVOL,
                    subvol: 9,
                    tree: 4,
                    ..Default::default()
                },
            };
            let mut trans = btree_trans::default();
            bch2_trans_init(&mut trans, &mut c);
            let mut iter = btree_iter::default();
            bch2_trans_iter_init(&mut trans, &mut iter, 0, key.k.p, BTREE_ITER_intent);
            assert!(bch2_btree_iter_peek(&mut iter).k.is_null());
            assert_eq!(
                bch2_trans_update(
                    &mut trans,
                    &mut iter,
                    (&mut key as *mut bkey_i_snapshot).cast(),
                    0,
                ),
                0
            );
            assert_eq!(bch2_trans_commit(&mut trans), 0);
            bch2_trans_iter_exit(&mut iter);

            assert!(bch2_snapshot_exists(&c, id));
            let table = c.snapshots.table.read().unwrap();
            let entry = __snapshot_t(&table, id).unwrap();
            assert_eq!((entry.subvol, entry.tree), (9, 4));
            assert_eq!((*set).journal_seq, 1);
        }
    }
}
