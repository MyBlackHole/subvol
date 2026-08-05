use super::bkey::{bkey_format, bkey_i, bkey_packed, BKEY_NR_FIELDS};
use super::bset::{bset as disk_bset, btree_node as disk_btree_node};
use crate::lock::six::six_lock;
use crate::util::rhashtable::rhashtable;
use std::sync::atomic::{AtomicU32, AtomicUsize};
use std::sync::Mutex;

pub const MAX_BSETS: usize = 3;
pub const BCH_BKEY_PTRS_MAX: usize = 16;
pub const BKEY_BTREE_PTR_VAL_U64S_MAX: usize = 15;
/* Engine-local extension permitted by the storage-core boundary: alloc keeps
 * its bcachefs-derived id 4 and the derived backpointer index is id 8. */
pub const BTREE_ID_NR: usize = 9;
pub const BTREE_ROOT_LEVEL_BITS: usize = 3;
pub const BTREE_ROOT_LEVEL_MASK: usize = (1 << BTREE_ROOT_LEVEL_BITS) - 1;

/*
 * The rewrite keeps the first eight btree ids from the local BCH_BTREE_IDS()
 * table.  Keep the generated property masks in the same form as bcachefs:
 * extents (0, 7) and snapshots (0..3).
 */
pub const BTREE_IS_EXTENTS_MASK: u64 = (1 << 0) | (1 << 7);
pub const BTREE_HAS_SNAPSHOTS_MASK: u64 = (1 << 0) | (1 << 1) | (1 << 2) | (1 << 3);
/* Generated from BCH_BTREE_IDS() and BTREE_IS_write_buffer in the local
 * bcachefs source.  The rewrite's eight retained IDs do not include a
 * write-buffer tree, so the mask is empty. */
pub const BTREE_USES_WRITE_BUFFER_MASK: u64 = 0;

pub const fn btree_id_is_extents(btree: u8) -> bool {
    (BTREE_IS_EXTENTS_MASK & (1u64 << btree)) != 0
}

pub const fn btree_type_has_snapshots(btree: u8) -> bool {
    (BTREE_HAS_SNAPSHOTS_MASK & (1u64 << btree)) != 0
}

pub const fn btree_type_uses_write_buffer(btree: u8) -> bool {
    (BTREE_USES_WRITE_BUFFER_MASK & (1u64 << btree)) != 0
}

pub const fn btree_id_is_extents_snapshots(btree: u8) -> bool {
    btree_id_is_extents(btree) && btree_type_has_snapshots(btree)
}

pub const BCH_VALIDATE_write: u8 = 1 << 0;
pub const BCH_VALIDATE_commit: u8 = 1 << 1;
pub const BCH_VALIDATE_silent: u8 = 1 << 2;

pub const BKEY_VALIDATE_unknown: u8 = 0;
pub const BKEY_VALIDATE_superblock: u8 = 1;
pub const BKEY_VALIDATE_journal: u8 = 2;
pub const BKEY_VALIDATE_btree_root: u8 = 3;
pub const BKEY_VALIDATE_btree_node: u8 = 4;
pub const BKEY_VALIDATE_commit: u8 = 5;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct bkey_validate_context {
    pub from: u8,
    pub flags: u8,
    pub level: u8,
    pub _pad0: u8,
    pub btree: u32,
    pub root: u32,
    pub journal_offset: u32,
    pub journal_seq: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct disk_reservation {
    pub sectors: u64,
    pub gen: u32,
    pub nr_replicas: u32,
}

pub const BCH_DISK_RESERVATION_NOFAIL: u32 = 1 << 0;
pub const BCH_DISK_RESERVATION_PARTIAL: u32 = 1 << 1;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct bch_fs_usage_base {
    pub hidden: u64,
    pub btree: u64,
    pub data: u64,
    pub cached: u64,
    pub reserved: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct bch_fs_usage_short {
    pub capacity: u64,
    pub used: u64,
    pub free: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct bch_fs_capacity_pcpu {
    pub usage: bch_fs_usage_base,
    pub sectors_available: u64,
    pub online_reserved: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct btree_nr_keys {
    pub live_u64s: u16,
    pub bset_u64s: [u16; MAX_BSETS],
    pub packed_keys: u16,
    pub unpacked_keys: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct bset_tree {
    pub size: u16,
    pub extra: u16,
    pub data_offset: u16,
    pub aux_data_offset: u16,
    pub end_offset: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum bset_aux_tree_type {
    BSET_NO_AUX_TREE,
    BSET_RO_AUX_TREE,
    BSET_RW_AUX_TREE,
}

pub const BSET_TREE_NR_TYPES: usize = 3;
pub const BSET_NO_AUX_TREE_VAL: u16 = u16::MAX;
pub const BSET_RW_AUX_TREE_VAL: u16 = u16::MAX - 1;
pub const BSET_CACHELINE: usize = 256;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct btree_node_iter_set {
    pub k: u16,
    pub end: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct btree_node_iter {
    pub data: [btree_node_iter_set; MAX_BSETS],
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct list_head {
    pub next: *mut list_head,
    pub prev: *mut list_head,
}

impl Default for list_head {
    fn default() -> Self {
        Self {
            next: core::ptr::null_mut(),
            prev: core::ptr::null_mut(),
        }
    }
}

pub unsafe fn INIT_LIST_HEAD(head: *mut list_head) {
    (*head).next = head;
    (*head).prev = head;
}

pub unsafe fn list_add(new: *mut list_head, head: *mut list_head) {
    (*(*head).next).prev = new;
    (*new).next = (*head).next;
    (*new).prev = head;
    (*head).next = new;
}

pub unsafe fn list_add_tail(new: *mut list_head, head: *mut list_head) {
    (*(*head).prev).next = new;
    (*new).next = head;
    (*new).prev = (*head).prev;
    (*head).prev = new;
}

pub unsafe fn __list_del(prev: *mut list_head, next: *mut list_head) {
    (*next).prev = prev;
    (*prev).next = next;
}

pub unsafe fn list_del(entry: *mut list_head) {
    __list_del((*entry).prev, (*entry).next);
}

pub unsafe fn list_del_init(entry: *mut list_head) {
    list_del(entry);
    INIT_LIST_HEAD(entry);
}

pub unsafe fn list_replace(old: *const list_head, new: *mut list_head) {
    (*new).next = (*old).next;
    (*new).prev = (*old).prev;
    (*(*new).prev).next = new;
    (*(*new).next).prev = new;
}

pub unsafe fn list_replace_init(old: *mut list_head, new: *mut list_head) {
    let head = (*old).next;
    list_del(old);
    list_add_tail(new, head);
    INIT_LIST_HEAD(old);
}

pub unsafe fn list_move(entry: *mut list_head, head: *mut list_head) {
    __list_del((*entry).prev, (*entry).next);
    list_add(entry, head);
}

pub unsafe fn list_move_tail(entry: *mut list_head, head: *mut list_head) {
    list_del(entry);
    list_add_tail(entry, head);
}

pub unsafe fn list_empty(head: *const list_head) -> bool {
    core::ptr::eq(head, (*head).next)
}

pub unsafe fn list_empty_careful(head: *const list_head) -> bool {
    let next = (*head).next;
    core::ptr::eq(next, head.cast_mut()) && core::ptr::eq(next, (*head).prev)
}

pub unsafe fn list_splice(list: *mut list_head, head: *mut list_head) {
    if !core::ptr::eq(list, (*list).next) {
        (*(*list).next).prev = head;
        (*(*list).prev).next = (*head).next;
        (*(*head).next).prev = (*list).prev;
        (*head).next = (*list).next;
    }
}

pub unsafe fn list_splice_init(list: *mut list_head, head: *mut list_head) {
    list_splice(list, head);
    INIT_LIST_HEAD(list);
}

pub unsafe fn list_splice_tail(list: *mut list_head, head: *mut list_head) {
    if !list_empty(list) {
        let first = (*list).next;
        let last = (*list).prev;
        let prev = (*head).prev;

        (*first).prev = prev;
        (*prev).next = first;
        (*last).next = head;
        (*head).prev = last;
    }
}

pub unsafe fn list_splice_tail_init(list: *mut list_head, head: *mut list_head) {
    list_splice_tail(list, head);
    INIT_LIST_HEAD(list);
}

pub unsafe fn list_count_nodes(head: *mut list_head) -> usize {
    let mut count = 0;
    let mut pos = (*head).next;
    while !core::ptr::eq(pos, head) {
        count += 1;
        pos = (*pos).next;
    }
    count
}

pub unsafe fn list_is_last(list: *const list_head, head: *const list_head) -> bool {
    core::ptr::eq((*list).next, head.cast_mut())
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct rhash_head {
    pub next: *mut rhash_head,
}

impl Default for rhash_head {
    fn default() -> Self {
        Self {
            next: core::ptr::null_mut(),
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct btree_root {
    pub b: *mut btree,
    pub key: bkey_i,
    pub key_pad: [u64; BKEY_BTREE_PTR_VAL_U64S_MAX],
    pub level: u8,
    pub alive: u8,
    pub error: i16,
}

impl Default for btree_root {
    fn default() -> Self {
        Self {
            b: core::ptr::null_mut(),
            key: Default::default(),
            key_pad: [0; BKEY_BTREE_PTR_VAL_U64S_MAX],
            level: 0,
            alive: 0,
            error: 0,
        }
    }
}

#[repr(C)]
pub struct bch_fs_btree_cache {
    pub roots_b: [usize; BTREE_ID_NR],
    pub roots_known: [btree_root; BTREE_ID_NR],
    pub root_lock: std::sync::Mutex<()>,
    pub table: rhashtable,
    pub table_init_done: bool,
    pub lock: std::sync::Mutex<()>,
    pub freeable: list_head,
    pub freed_pcpu: list_head,
    pub freed_nonpcpu: list_head,
    pub live: [btree_cache_list; 2],
    pub nr_freeable: usize,
    pub nr_reserve: usize,
    pub nr_by_btree: [usize; BTREE_ID_NR],
    pub pinned_nodes_mask: [u64; 2],
    pub nr_in_flight: AtomicUsize,
    pub nr_in_flight_inner: AtomicUsize,
    pub allocations: std::sync::Mutex<Vec<usize>>,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct btree_cache_list {
    pub idx: usize,
    pub clean: list_head,
    pub dirty: list_head,
    pub nr_clean: usize,
    pub nr_dirty: usize,
}

impl Default for btree_cache_list {
    fn default() -> Self {
        Self {
            idx: 0,
            clean: list_head::default(),
            dirty: list_head::default(),
            nr_clean: 0,
            nr_dirty: 0,
        }
    }
}

impl Default for bch_fs_btree_cache {
    fn default() -> Self {
        Self {
            roots_b: [0; BTREE_ID_NR],
            roots_known: [btree_root::default(); BTREE_ID_NR],
            root_lock: Default::default(),
            table: rhashtable::default(),
            table_init_done: false,
            lock: Default::default(),
            freeable: list_head::default(),
            freed_pcpu: list_head::default(),
            freed_nonpcpu: list_head::default(),
            live: [btree_cache_list::default(), btree_cache_list::default()],
            nr_freeable: 0,
            nr_reserve: 0,
            nr_by_btree: [0; BTREE_ID_NR],
            pinned_nodes_mask: [0; 2],
            nr_in_flight: AtomicUsize::new(0),
            nr_in_flight_inner: AtomicUsize::new(0),
            allocations: Default::default(),
        }
    }
}

impl Drop for bch_fs_btree_cache {
    fn drop(&mut self) {
        if self.table_init_done {
            unsafe { crate::util::rhashtable::rhashtable_destroy(&mut self.table) };
        }
        let allocations = self.allocations.get_mut().unwrap();
        for node in allocations.drain(..) {
            unsafe {
                super::cache::bch2_btree_node_data_free(node as *mut btree);
                drop(Box::from_raw(node as *mut btree));
            }
        }
    }
}

#[repr(C)]
#[derive(Default)]
pub struct bch_fs_btree {
    pub cache: bch_fs_btree_cache,
    pub evicted_size: btree_evicted_size,
    /* interior.c:3390 async_btree_rewrite + btree.node_rewrites：
     * 读完成（read.c:968）入队的待重写节点。域内差异（AC-1 D1）：
     * 无 async worker，队列保存 key 拷贝（对齐 a->key bkey_buf），
     * 由无锁时机（root_read 末尾 / engine 操作边界）
     * bch2_do_pending_node_rewrites 同步执行 */
    pub node_rewrites: Mutex<Vec<btree_node_rewrite_item>>,
    /* interior_types.h:19-26 bch_fs_btree_reserve_cache：已分配未消费的
     * 节点 key 缓存（bch2_btree_reserve_put 回填 / __bch2_btree_node_alloc
     * 复用）。 */
    pub reserve_cache: crate::btree::alloc::btree_reserve_cache,
}

/* interior.c:3390-3396 async_btree_rewrite 的域内等价：记录
 * btree_id/level + 指针键拷贝（bch2_bkey_buf_copy(&a->key, &b->key)），
 * 不持有节点引用，避免节点被 retire 后悬垂 */
#[repr(C)]
#[derive(Clone)]
pub struct btree_node_rewrite_item {
    pub btree_id: u8,
    pub level: u8,
    pub key: Vec<u64>,
}

#[repr(C)]
pub struct btree_evicted_size {
    pub mask: u64,
    pub entries: Vec<u64>,
}

#[repr(C)]
pub struct journal_key_range_overwritten {
    pub start: usize,
    pub end: usize,
}

#[repr(C)]
pub struct journal_key {
    pub journal_seq_offset: u32,
    pub journal_offset: u32,
    pub btree_id: u8,
    pub level: u8,
    pub allocated: bool,
    pub overwritten: bool,
    pub rewind: bool,
    pub overwritten_range: u32,
    pub allocated_k: *mut bkey_i,
}

impl Default for journal_key {
    fn default() -> Self {
        Self {
            journal_seq_offset: 0,
            journal_offset: 0,
            btree_id: 0,
            level: 0,
            allocated: false,
            overwritten: false,
            rewind: false,
            overwritten_range: 0,
            allocated_k: core::ptr::null_mut(),
        }
    }
}

#[repr(C)]
#[derive(Default)]
pub struct journal_keys {
    pub nr: usize,
    pub size: usize,
    pub data: Vec<journal_key>,
    pub gap: usize,
    pub pre_sort: Vec<journal_key>,
    pub overwrite_lock: std::sync::Mutex<()>,
    pub overwrites: Vec<journal_key_range_overwritten>,
}

impl Drop for journal_keys {
    fn drop(&mut self) {
        for key in self.data.iter_mut().chain(self.pre_sort.iter_mut()) {
            if key.allocated && !key.allocated_k.is_null() {
                unsafe { journal_key_free(key.allocated_k) };
                key.allocated_k = core::ptr::null_mut();
            }
        }
    }
}

/// Rust ownership counterpart of bcachefs' `kfree()` for a journal-overlay
/// key allocated with exactly `bkey_bytes(&k->k)` bytes.
pub unsafe fn journal_key_free(key: *mut bkey_i) {
    if key.is_null() {
        return;
    }
    let bytes = (*key).k.u64s as usize * core::mem::size_of::<u64>();
    if bytes < core::mem::size_of::<bkey_i>() {
        return;
    }
    let Ok(layout) = std::alloc::Layout::from_size_align(bytes, core::mem::align_of::<u64>())
    else {
        return;
    };
    std::alloc::dealloc(key.cast(), layout);
}

impl Default for btree_evicted_size {
    fn default() -> Self {
        Self {
            mask: 0,
            entries: Vec::new(),
        }
    }
}

#[repr(C)]
#[derive(Default)]
pub struct bch_fs {
    pub flags: usize,
    /*
     * Test-only counterpart of iter.h's
     * CONFIG_BCACHEFS_INJECT_TRANSACTION_RESTARTS path.  Keeping the
     * countdown in the filesystem state makes the injection point obey the
     * same transaction-local restart protocol as the normal commit path.
     */
    pub fault_inject_transaction_restarts: AtomicU32,
    /* T0199: per-path restart injection for the discard worker's
     * bucket transactions (discard.c fast_work per-bucket trans,
     * commit.c:1390 injection point).  Kept separate from the
     * generic counter so tests can inject into the discard path
     * without perturbing user transactions. */
    pub fault_inject_discard_restarts: AtomicU32,
    pub devs_online: crate::btree::bset::bch_devs_mask,
    pub disk_sb: crate::sb::bch_sb_handle,
    /* allocator（fs/alloc/types.h 的 bch_fs_allocator 域内子集）：btree
     * 写点 + open_bucket 记账 + reserve_cache。 */
    pub allocator: crate::btree::alloc::bch_fs_allocator,
    pub btree: bch_fs_btree,
    pub journal: crate::journal::journal,
    pub journal_keys: journal_keys,
    pub snapshots: crate::snapshot::bch_fs_snapshots,
}

pub fn bch2_btree_root_pack(b: *mut btree) -> usize {
    if b.is_null() {
        return 0;
    }
    assert_eq!(b as usize & BTREE_ROOT_LEVEL_MASK, 0);
    let level = unsafe { (*b).c.level as usize };
    assert_eq!(level & !BTREE_ROOT_LEVEL_MASK, 0);
    b as usize | level
}

pub const fn bch2_btree_root_unpack_b(v: usize) -> *mut btree {
    (v & !BTREE_ROOT_LEVEL_MASK) as *mut btree
}

pub const fn bch2_btree_root_unpack_level(v: usize) -> u8 {
    (v & BTREE_ROOT_LEVEL_MASK) as u8
}

pub unsafe fn bch2_btree_id_root(c: *mut bch_fs, id: usize) -> *mut btree_root {
    if id < BTREE_ID_NR {
        (*c).btree.cache.roots_known.as_mut_ptr().add(id)
    } else {
        core::ptr::null_mut()
    }
}

pub unsafe fn bch2_btree_id_root_packed(c: *const bch_fs, id: usize) -> usize {
    if id < BTREE_ID_NR {
        (*c).btree.cache.roots_b[id]
    } else {
        0
    }
}

pub unsafe fn bch2_btree_id_root_b(c: *const bch_fs, id: usize) -> *mut btree {
    bch2_btree_root_unpack_b(bch2_btree_id_root_packed(c, id))
}

pub unsafe fn bch2_btree_id_root_set(c: *mut bch_fs, id: usize, b: *mut btree) {
    assert!(id < BTREE_ID_NR);
    (*c).btree.cache.roots_known[id].b = b;
    (*c).btree.cache.roots_known[id].level = if b.is_null() { 0 } else { (*b).c.level };
    (*c).btree.cache.roots_known[id].alive = (!b.is_null()) as u8;
    (*c).btree.cache.roots_b[id] = bch2_btree_root_pack(b);
}

pub type journal_pin_flush_fn =
    unsafe extern "C" fn(*mut crate::journal::journal, *mut journal_entry_pin, u64) -> i32;

#[repr(C)]
#[derive(Default)]
pub struct journal_entry_pin {
    pub list: list_head,
    pub flush: Option<journal_pin_flush_fn>,
    pub seq: u64,
}

#[repr(C)]
#[derive(Default)]
pub struct btree_write {
    pub journal: journal_entry_pin,
}

#[repr(C)]
#[derive(Default)]
pub struct btree_bkey_cached_common {
    pub lock: six_lock,
    pub level: u8,
    pub btree_id: u8,
    pub cached: bool,
}

#[repr(C, packed(4))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct bkey_cached_key {
    pub btree_id: u32,
    pub pos: super::bkey::bpos,
}

#[repr(C)]
pub struct bkey_cached {
    pub c: btree_bkey_cached_common,
    pub flags: usize,
    pub u64s: u16,
    pub key: bkey_cached_key,
    pub hash: rhash_head,
    pub journal: journal_entry_pin,
    pub seq: u64,
    pub k: *mut bkey_i,
    pub rcu: crate::util::rcu::rcu_head,
}

impl Default for bkey_cached {
    fn default() -> Self {
        Self {
            c: btree_bkey_cached_common::default(),
            flags: 0,
            u64s: 0,
            key: bkey_cached_key::default(),
            hash: rhash_head::default(),
            journal: journal_entry_pin::default(),
            seq: 0,
            k: core::ptr::null_mut(),
            rcu: crate::util::rcu::rcu_head::default(),
        }
    }
}

pub unsafe fn btree_node_pos(b: *mut btree_bkey_cached_common) -> super::bkey::bpos {
    assert!(!b.is_null());
    if !(*b).cached {
        (*b.cast::<btree>()).key.k.p
    } else {
        core::ptr::addr_of!((*b.cast::<bkey_cached>()).key.pos).read_unaligned()
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct bkey_unpack_field {
    pub byte_offset: i8,
    pub shift_right: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct open_buckets {
    pub nr: u16,
    pub v: [u16; BCH_BKEY_PTRS_MAX],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum btree_node_cache_state {
    #[default]
    BTREE_NODE_CACHE_NONE,
    BTREE_NODE_CACHE_FREED,
    BTREE_NODE_CACHE_FREEABLE,
    BTREE_NODE_CACHE_CLEAN,
    BTREE_NODE_CACHE_DIRTY,
}

pub const BTREE_NODE_FLAGS_START: usize = 2;
pub const BTREE_NODE_read_in_flight: usize = 3;
pub const BTREE_NODE_read_error: usize = 4;
pub const BTREE_NODE_dirty: usize = 5;
pub const BTREE_NODE_need_write: usize = 6;
pub const BTREE_NODE_write_blocked: usize = 7;
pub const BTREE_NODE_will_make_reachable: usize = 8;
pub const BTREE_NODE_noevict: usize = 9;
pub const BTREE_NODE_write_idx: usize = 10;
pub const BTREE_NODE_accessed: usize = 11;
pub const BTREE_NODE_write_in_flight: usize = 12;
pub const BTREE_NODE_write_in_flight_inner: usize = 13;
pub const BTREE_NODE_just_written: usize = 14;
pub const BTREE_NODE_dying: usize = 15;
pub const BTREE_NODE_fake: usize = 16;
pub const BTREE_NODE_need_rewrite: usize = 17;
pub const BTREE_NODE_need_rewrite_error: usize = 18;
pub const BTREE_NODE_need_rewrite_ptr_written_zero: usize = 19;
pub const BTREE_NODE_never_write: usize = 20;
pub const BTREE_NODE_pinned: usize = 21;
pub const BTREE_NODE_permanent: usize = 22;

macro_rules! btree_node_flag_fns {
    ($test:ident, $set:ident, $clear:ident, $flag:ident) => {
        pub unsafe fn $test(b: *const btree) -> bool {
            (*b).flags & (1usize << $flag) != 0
        }

        pub unsafe fn $set(b: *mut btree) {
            (*b).flags |= 1usize << $flag;
        }

        pub unsafe fn $clear(b: *mut btree) {
            (*b).flags &= !(1usize << $flag);
        }
    };
}

btree_node_flag_fns!(
    btree_node_read_in_flight,
    set_btree_node_read_in_flight,
    clear_btree_node_read_in_flight,
    BTREE_NODE_read_in_flight
);
btree_node_flag_fns!(
    btree_node_read_error,
    set_btree_node_read_error,
    clear_btree_node_read_error,
    BTREE_NODE_read_error
);
btree_node_flag_fns!(
    btree_node_dirty,
    set_btree_node_dirty,
    clear_btree_node_dirty,
    BTREE_NODE_dirty
);
btree_node_flag_fns!(
    btree_node_need_write,
    set_btree_node_need_write,
    clear_btree_node_need_write,
    BTREE_NODE_need_write
);
btree_node_flag_fns!(
    btree_node_write_blocked,
    set_btree_node_write_blocked,
    clear_btree_node_write_blocked,
    BTREE_NODE_write_blocked
);
btree_node_flag_fns!(
    btree_node_will_make_reachable,
    set_btree_node_will_make_reachable,
    clear_btree_node_will_make_reachable,
    BTREE_NODE_will_make_reachable
);
btree_node_flag_fns!(
    btree_node_noevict,
    set_btree_node_noevict,
    clear_btree_node_noevict,
    BTREE_NODE_noevict
);
pub unsafe fn set_btree_node_write_idx(b: *mut btree) {
    (*b).flags |= 1usize << BTREE_NODE_write_idx;
}

pub unsafe fn clear_btree_node_write_idx(b: *mut btree) {
    (*b).flags &= !(1usize << BTREE_NODE_write_idx);
}
btree_node_flag_fns!(
    btree_node_accessed,
    set_btree_node_accessed,
    clear_btree_node_accessed,
    BTREE_NODE_accessed
);
btree_node_flag_fns!(
    btree_node_write_in_flight,
    set_btree_node_write_in_flight,
    clear_btree_node_write_in_flight,
    BTREE_NODE_write_in_flight
);
btree_node_flag_fns!(
    btree_node_write_in_flight_inner,
    set_btree_node_write_in_flight_inner,
    clear_btree_node_write_in_flight_inner,
    BTREE_NODE_write_in_flight_inner
);
btree_node_flag_fns!(
    btree_node_just_written,
    set_btree_node_just_written,
    clear_btree_node_just_written,
    BTREE_NODE_just_written
);
btree_node_flag_fns!(
    btree_node_dying,
    set_btree_node_dying,
    clear_btree_node_dying,
    BTREE_NODE_dying
);
btree_node_flag_fns!(
    btree_node_fake,
    set_btree_node_fake,
    clear_btree_node_fake,
    BTREE_NODE_fake
);
btree_node_flag_fns!(
    btree_node_need_rewrite,
    set_btree_node_need_rewrite,
    clear_btree_node_need_rewrite,
    BTREE_NODE_need_rewrite
);
btree_node_flag_fns!(
    btree_node_need_rewrite_error,
    set_btree_node_need_rewrite_error,
    clear_btree_node_need_rewrite_error,
    BTREE_NODE_need_rewrite_error
);
btree_node_flag_fns!(
    btree_node_need_rewrite_ptr_written_zero,
    set_btree_node_need_rewrite_ptr_written_zero,
    clear_btree_node_need_rewrite_ptr_written_zero,
    BTREE_NODE_need_rewrite_ptr_written_zero
);
btree_node_flag_fns!(
    btree_node_never_write,
    set_btree_node_never_write,
    clear_btree_node_never_write,
    BTREE_NODE_never_write
);
btree_node_flag_fns!(
    btree_node_pinned,
    set_btree_node_pinned,
    clear_btree_node_pinned,
    BTREE_NODE_pinned
);
btree_node_flag_fns!(
    btree_node_permanent,
    set_btree_node_permanent,
    clear_btree_node_permanent,
    BTREE_NODE_permanent
);

#[repr(C)]
pub struct btree {
    pub c: btree_bkey_cached_common,
    pub hash: rhash_head,
    pub hash_val: u64,
    pub flags: usize,
    pub written: u16,
    pub nsets: u8,
    pub nr_key_bits: u8,
    pub version_ondisk: u16,
    pub format: bkey_format,
    pub byte_aligned_fields: bool,
    pub unpack: [bkey_unpack_field; BKEY_NR_FIELDS as usize],
    pub data: *mut disk_btree_node,
    pub aux_data: *mut core::ffi::c_void,
    pub set: [bset_tree; MAX_BSETS],
    pub nr: btree_nr_keys,
    pub sib_u64s: [u16; 2],
    pub whiteout_u64s: u16,
    pub byte_order: u8,
    pub unpack_fn_len: u8,
    pub writes: [btree_write; 2],
    pub key: bkey_i,
    pub key_pad: [u64; BKEY_BTREE_PTR_VAL_U64S_MAX],
    pub write_blocked: list_head,
    pub will_make_reachable: usize,
    pub ob: open_buckets,
    pub list: list_head,
    pub cache_state: btree_node_cache_state,
}

impl Default for btree {
    fn default() -> Self {
        Self {
            c: Default::default(),
            hash: Default::default(),
            hash_val: 0,
            flags: 0,
            written: 0,
            nsets: 0,
            nr_key_bits: 0,
            version_ondisk: 0,
            format: Default::default(),
            byte_aligned_fields: false,
            unpack: [Default::default(); BKEY_NR_FIELDS as usize],
            data: core::ptr::null_mut(),
            aux_data: core::ptr::null_mut(),
            set: [Default::default(); MAX_BSETS],
            nr: Default::default(),
            sib_u64s: [0; 2],
            whiteout_u64s: 0,
            byte_order: 0,
            unpack_fn_len: 0,
            writes: [Default::default(), Default::default()],
            key: Default::default(),
            key_pad: [0; BKEY_BTREE_PTR_VAL_U64S_MAX],
            write_blocked: Default::default(),
            will_make_reachable: 0,
            ob: Default::default(),
            list: Default::default(),
            cache_state: Default::default(),
        }
    }
}

pub unsafe fn btree_node_write_idx(b: *const btree) -> usize {
    ((*b).flags >> super::io::BTREE_NODE_write_idx) & 1
}

pub unsafe fn btree_current_write(b: *mut btree) -> *mut btree_write {
    (*b).writes.as_mut_ptr().add(btree_node_write_idx(b))
}

pub unsafe fn btree_prev_write(b: *mut btree) -> *mut btree_write {
    (*b).writes.as_mut_ptr().add(btree_node_write_idx(b) ^ 1)
}

pub unsafe fn bset_tree_last(b: *mut btree) -> *mut bset_tree {
    assert_ne!((*b).nsets, 0);
    (*b).set.as_mut_ptr().add((*b).nsets as usize - 1)
}

pub unsafe fn __btree_node_offset_to_ptr(b: *const btree, offset: u16) -> *mut core::ffi::c_void {
    ((*b).data as *mut u64).add(offset as usize).cast()
}

pub unsafe fn __btree_node_ptr_to_offset(b: *const btree, p: *const core::ffi::c_void) -> u16 {
    let ret = (p as *const u64).offset_from((*b).data as *const u64);
    let ret = u16::try_from(ret).expect("btree node offset exceeds u16");
    assert_eq!(
        __btree_node_offset_to_ptr(b, ret) as *const core::ffi::c_void,
        p
    );
    ret
}

pub unsafe fn bset(b: *const btree, t: *const bset_tree) -> *mut disk_bset {
    __btree_node_offset_to_ptr(b, (*t).data_offset).cast()
}

pub unsafe fn set_btree_bset_end(b: *mut btree, t: *mut bset_tree) {
    let last = (bset(b, t) as *mut u64).add(3 + (*bset(b, t)).u64s as usize);
    (*t).end_offset = __btree_node_ptr_to_offset(b, last.cast());
}

pub unsafe fn set_btree_bset(b: *mut btree, t: *mut bset_tree, i: *const disk_bset) {
    (*t).data_offset = __btree_node_ptr_to_offset(b, i.cast());
    set_btree_bset_end(b, t);
}

pub unsafe fn btree_bset_first(b: *mut btree) -> *mut disk_bset {
    bset(b, (*b).set.as_ptr())
}
pub unsafe fn btree_bset_last(b: *mut btree) -> *mut disk_bset {
    bset(b, bset_tree_last(b))
}

pub unsafe fn __btree_node_key_to_offset(b: *const btree, k: *const bkey_packed) -> u16 {
    __btree_node_ptr_to_offset(b, k.cast())
}

pub unsafe fn __btree_node_offset_to_key(b: *const btree, k: u16) -> *mut bkey_packed {
    __btree_node_offset_to_ptr(b, k).cast()
}

pub unsafe fn btree_bkey_first_offset(t: *const bset_tree) -> u16 {
    (*t).data_offset + 3
}

pub unsafe fn btree_bkey_first(b: *const btree, t: *const bset_tree) -> *mut bkey_packed {
    __btree_node_offset_to_key(b, btree_bkey_first_offset(t))
}

pub unsafe fn btree_bkey_last(b: *const btree, t: *const bset_tree) -> *mut bkey_packed {
    __btree_node_offset_to_key(b, (*t).end_offset)
}

pub unsafe fn bset_u64s(t: *const bset_tree) -> u32 {
    ((*t).end_offset - (*t).data_offset - 3) as u32
}

pub unsafe fn bset_dead_u64s(b: *const btree, t: *const bset_tree) -> u32 {
    let index = t.offset_from((*b).set.as_ptr()) as usize;
    bset_u64s(t) - (*b).nr.bset_u64s[index] as u32
}

pub unsafe fn bch2_bkey_to_bset_inlined(b: *mut btree, k: *const bkey_packed) -> *mut bset_tree {
    let offset = __btree_node_key_to_offset(b, k);
    for i in 0..(*b).nsets as usize {
        let t = (*b).set.as_mut_ptr().add(i);
        if offset <= (*t).end_offset {
            assert!(offset >= btree_bkey_first_offset(t));
            return t;
        }
    }
    panic!("key is not contained in a bset")
}

pub unsafe fn bset_aux_tree_type(t: *const bset_tree) -> bset_aux_tree_type {
    match (*t).extra {
        BSET_NO_AUX_TREE_VAL => {
            assert_eq!((*t).size, 0);
            bset_aux_tree_type::BSET_NO_AUX_TREE
        }
        BSET_RW_AUX_TREE_VAL => {
            assert_ne!((*t).size, 0);
            bset_aux_tree_type::BSET_RW_AUX_TREE
        }
        _ => {
            assert_ne!((*t).size, 0);
            bset_aux_tree_type::BSET_RO_AUX_TREE
        }
    }
}

pub const fn __btree_keys_cachelines(byte_order: u32) -> usize {
    (1usize << byte_order) / BSET_CACHELINE
}

pub const fn __btree_aux_data_bytes(byte_order: u32) -> usize {
    __btree_keys_cachelines(byte_order) * 8
}

pub unsafe fn bset_has_ro_aux_tree(t: *const bset_tree) -> bool {
    bset_aux_tree_type(t) == bset_aux_tree_type::BSET_RO_AUX_TREE
}

pub unsafe fn bset_has_rw_aux_tree(t: *mut bset_tree) -> bool {
    bset_aux_tree_type(t) == bset_aux_tree_type::BSET_RW_AUX_TREE
}

pub unsafe fn __btree_node_iter_set_end(iter: *const btree_node_iter, i: u32) -> bool {
    (*iter).data[i as usize].k == (*iter).data[i as usize].end
}

pub unsafe fn bch2_btree_node_iter_end(iter: *const btree_node_iter) -> bool {
    __btree_node_iter_set_end(iter, 0)
}

pub unsafe fn __btree_node_iter_used(iter: *const btree_node_iter) -> u32 {
    let mut n = MAX_BSETS as u32;

    while n != 0 && __btree_node_iter_set_end(iter, n - 1) {
        n -= 1;
    }

    n
}

pub unsafe fn btree_node_iter_set_find(
    iter: *mut btree_node_iter,
    end_offset: u32,
) -> *mut btree_node_iter_set {
    let mut set = (*iter).data.as_mut_ptr();
    let end = set.add(MAX_BSETS);

    while set < end && (*set).k != (*set).end {
        if (*set).end == end_offset as u16 {
            return set;
        }
        set = set.add(1);
    }

    core::ptr::null_mut()
}

pub unsafe fn bch2_btree_node_iter_set_drop(
    iter: *mut btree_node_iter,
    set: *mut btree_node_iter_set,
) {
    let last = (*iter).data.as_mut_ptr().add(MAX_BSETS - 1);
    core::ptr::copy(set.add(1), set, last.offset_from(set) as usize);
    *last = btree_node_iter_set { k: 0, end: 0 };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bcachefs_bkey_validate_context_layout_matches_local_header() {
        let context = bkey_validate_context::default();
        let base = &context as *const bkey_validate_context as *const u8;
        assert_eq!(core::mem::size_of::<bkey_validate_context>(), 24);
        unsafe {
            assert_eq!(
                core::ptr::addr_of!(context.level)
                    .cast::<u8>()
                    .offset_from(base),
                2
            );
            assert_eq!(
                core::ptr::addr_of!(context.btree)
                    .cast::<u8>()
                    .offset_from(base),
                4
            );
            assert_eq!(
                core::ptr::addr_of!(context.journal_offset)
                    .cast::<u8>()
                    .offset_from(base),
                12
            );
            assert_eq!(
                core::ptr::addr_of!(context.journal_seq)
                    .cast::<u8>()
                    .offset_from(base),
                16
            );
        }
    }

    #[test]
    fn bcachefs_btree_memory_type_layout() {
        assert_eq!(BCH_DISK_RESERVATION_NOFAIL, 1);
        assert_eq!(BCH_DISK_RESERVATION_PARTIAL, 2);
        assert_eq!(core::mem::size_of::<disk_reservation>(), 16);
        assert_eq!(core::mem::size_of::<bch_fs_usage_base>(), 40);
        assert_eq!(core::mem::size_of::<bch_fs_usage_short>(), 24);
        assert_eq!(core::mem::size_of::<bch_fs_capacity_pcpu>(), 56);
        assert_eq!(core::mem::size_of::<btree_nr_keys>(), 12);
        assert_eq!(core::mem::size_of::<bset_tree>(), 10);
        assert_eq!(core::mem::size_of::<btree_node_iter_set>(), 4);
        assert_eq!(core::mem::size_of::<btree_node_iter>(), 12);
        assert_eq!(core::mem::size_of::<bset_aux_tree_type>(), 4);
    }

    #[test]
    fn bcachefs_bset_aux_tree_encoding() {
        let none = bset_tree {
            extra: BSET_NO_AUX_TREE_VAL,
            ..Default::default()
        };
        let rw = bset_tree {
            size: 1,
            extra: BSET_RW_AUX_TREE_VAL,
            ..Default::default()
        };
        let ro = bset_tree {
            size: 1,
            extra: 0,
            ..Default::default()
        };

        unsafe {
            assert_eq!(
                bset_aux_tree_type(&none),
                bset_aux_tree_type::BSET_NO_AUX_TREE
            );
            assert_eq!(
                bset_aux_tree_type(&rw),
                bset_aux_tree_type::BSET_RW_AUX_TREE
            );
            assert_eq!(
                bset_aux_tree_type(&ro),
                bset_aux_tree_type::BSET_RO_AUX_TREE
            );
        }
        assert_eq!(__btree_keys_cachelines(8), 1);
        assert_eq!(__btree_aux_data_bytes(12), 128);
    }

    #[test]
    fn bcachefs_node_iter_set_drop_matches_memmove() {
        let mut iter = btree_node_iter {
            data: [
                btree_node_iter_set { k: 1, end: 4 },
                btree_node_iter_set { k: 5, end: 8 },
                btree_node_iter_set { k: 9, end: 12 },
            ],
        };

        unsafe {
            assert_eq!(__btree_node_iter_used(&iter), 3);
            let set = btree_node_iter_set_find(&mut iter, 8);
            assert_eq!(set, iter.data.as_mut_ptr().add(1));
            bch2_btree_node_iter_set_drop(&mut iter, set);
        }

        assert_eq!(iter.data[0], btree_node_iter_set { k: 1, end: 4 });
        assert_eq!(iter.data[1], btree_node_iter_set { k: 9, end: 12 });
        assert_eq!(iter.data[2], btree_node_iter_set { k: 0, end: 0 });
    }

    #[test]
    fn local_intrusive_list_operations_preserve_links_and_order() {
        unsafe {
            let mut head = Box::new(list_head::default());
            let mut other = Box::new(list_head::default());
            let mut first = Box::new(list_head::default());
            let mut second = Box::new(list_head::default());
            let mut third = Box::new(list_head::default());
            let mut replacement = Box::new(list_head::default());
            for entry in [
                &mut *head,
                &mut *other,
                &mut *first,
                &mut *second,
                &mut *third,
                &mut *replacement,
            ] {
                INIT_LIST_HEAD(entry);
            }

            assert!(list_empty(&*head));
            assert!(list_empty_careful(&*head));
            list_add(&mut *first, &mut *head);
            list_add_tail(&mut *second, &mut *head);
            assert_eq!(list_count_nodes(&mut *head), 2);
            assert_eq!(head.next, core::ptr::addr_of_mut!(*first));
            assert_eq!(head.prev, core::ptr::addr_of_mut!(*second));

            list_move_tail(&mut *first, &mut *head);
            assert_eq!(head.next, core::ptr::addr_of_mut!(*second));
            assert!(list_is_last(&*first, &*head));
            list_del_init(&mut *second);
            assert!(list_empty(&*second));

            list_add(&mut *third, &mut *other);
            list_splice_tail_init(&mut *other, &mut *head);
            assert!(list_empty(&*other));
            assert_eq!(list_count_nodes(&mut *head), 2);
            assert!(list_is_last(&*third, &*head));

            list_replace_init(&mut *third, &mut *replacement);
            assert!(list_empty(&*third));
            assert!(list_is_last(&*replacement, &*head));
            list_del_init(&mut *first);
            list_del_init(&mut *replacement);
            assert!(list_empty_careful(&*head));
        }
    }

    #[test]
    fn btree_property_masks_match_local_first_eight_ids() {
        assert!(btree_id_is_extents(0));
        assert!(btree_id_is_extents(7));
        assert!(!btree_id_is_extents(1));
        assert!(btree_type_has_snapshots(0));
        assert!(btree_type_has_snapshots(3));
        assert!(!btree_type_has_snapshots(4));
        assert!(btree_id_is_extents_snapshots(0));
        assert!(!btree_id_is_extents_snapshots(7));
        assert!(!btree_type_has_snapshots(BTREE_ID_NR as u8));
        assert!(!btree_type_uses_write_buffer(0));
        assert!(!btree_type_uses_write_buffer((BTREE_ID_NR - 1) as u8));
    }
}
