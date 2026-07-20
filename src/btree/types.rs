use crate::bcachefs::*;
use crate::bcachefs_format::*;

pub const MAX_BSETS: u32 = 3;

#[derive(Clone, Debug)]
pub struct BtreeNrKeys {
    pub live_u64s: u16,
    pub bset_u64s: [u16; MAX_BSETS as usize],
    pub packed_keys: u16,
    pub unpacked_keys: u16,
}

impl BtreeNrKeys {
    pub fn new() -> Self {
        BtreeNrKeys {
            live_u64s: 0,
            bset_u64s: [0; 3],
            packed_keys: 0,
            unpacked_keys: 0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct BsetTree {
    pub size: u16,
    pub extra: u16,
    pub data_offset: u16,
    pub aux_data_offset: u16,
    pub end_offset: u16,
}

impl BsetTree {
    pub fn new() -> Self {
        BsetTree {
            size: 0,
            extra: 0,
            data_offset: 0,
            aux_data_offset: 0,
            end_offset: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BtreeNodeCacheState {
    None,
    Freed,
    Freeable,
    Clean,
    Dirty,
}

#[derive(Clone, Debug)]
pub struct BtreeNode {
    pub c: BtreeBkeyCachedCommon,
    pub hash_val: u64,
    pub flags: u64,
    pub written: u16,
    pub nsets: u8,
    pub nr_key_bits: u8,
    pub version_ondisk: u16,
    pub format: BkeyFormat,
    pub unpack_fn_used: u64,
    pub nr: BtreeNrKeys,
    pub data: Vec<u8>,
    pub set: Vec<BsetTree>,
    pub writes: Vec<BtreeWrite>,
    pub sequence: u64,
    pub list: *mut std::ffi::c_void,
    pub key: BkeyI,
    pub key_pad: [u64; 8],
}

impl BtreeNode {
    pub fn new() -> Self {
        BtreeNode {
            c: BtreeBkeyCachedCommon::new(),
            hash_val: 0,
            flags: 0,
            written: 0,
            nsets: 0,
            nr_key_bits: 0,
            version_ondisk: 0,
            format: BkeyFormat {
                key_u64s: 0,
                nr_fields: 0,
                bits_per_field: [0; 6],
                field_offset: [0; 6],
            },
            unpack_fn_used: 0,
            nr: BtreeNrKeys::new(),
            data: Vec::new(),
            set: Vec::new(),
            writes: Vec::new(),
            sequence: 0,
            list: std::ptr::null_mut(),
            key: BkeyI { k: Bkey::init() },
            key_pad: [0u64; 8],
        }
    }

    pub fn btree_id(&self) -> u8 { self.c.btree_id }
    pub fn level(&self) -> u8 { self.c.level }
}

#[derive(Clone, Debug)]
pub struct BtreeWrite {
    pub journal: JournalEntryPin,
}

#[derive(Clone, Debug)]
pub struct BtreeBkeyCachedCommon {
    pub lock: SixLock,
    pub level: u8,
    pub btree_id: u8,
    pub cached: bool,
}

impl BtreeBkeyCachedCommon {
    pub fn new() -> Self {
        BtreeBkeyCachedCommon {
            lock: SixLock::new(),
            level: 0,
            btree_id: 0,
            cached: false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct BtreePath {
    pub pos: Bpos,
    pub idx: u16,
    pub sorted_idx: u16,
    pub ref_count: u16,
    pub preserve: bool,
    pub should_be_locked: bool,
    pub level: u8,
    pub locks_want: u8,
    pub nodes_locked: u8,
    pub intent_lock_pass: u8,
    pub cached: bool,
    pub uptodate: u8,
    pub b: Vec<Option<BtreePathLevel>>,
    pub l: Vec<BtreePathLevel>,
}

impl BtreePath {
    pub fn new() -> Self {
        BtreePath {
            pos: Bpos::ZERO,
            idx: 0,
            sorted_idx: 0,
            ref_count: 1,
            preserve: false,
            should_be_locked: false,
            level: 0,
            locks_want: 0,
            nodes_locked: 0,
            intent_lock_pass: 0,
            cached: false,
            uptodate: 0,
            b: Vec::new(),
            l: Vec::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct BtreePathLevel {
    pub b: *mut BtreeNode,
    pub iter: BtreePathIter,
}

#[derive(Clone, Debug)]
pub struct BtreePathIter {
    pub bset: u32,
    pub bkey: u32,
}

#[derive(Clone, Debug)]
pub struct BtreeIter {
    pub path: Vec<BtreePath>,
    pub transactions: Vec<BtreeTrans>,
    pub pos: Bpos,
    pub level: u8,
    pub flags: u32,
}

impl BtreeIter {
    pub fn new() -> Self {
        BtreeIter {
            path: Vec::new(),
            transactions: Vec::new(),
            pos: Bpos::ZERO,
            level: 0,
            flags: 0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct BtreeTrans {
    pub paths: Vec<BtreePath>,
    pub ip: usize,
    pub iter_count: u32,
    pub iter_ticket_count: u32,
    pub nr_updates: u32,
    pub nr_updates_before_replay: u32,
    pub updating: bool,
    pub memory_allocation_failure: bool,
    pub fn_name: Option<&'static str>,
}

impl BtreeTrans {
    pub fn new() -> Self {
        BtreeTrans {
            paths: Vec::new(),
            ip: 0,
            iter_count: 0,
            iter_ticket_count: 0,
            nr_updates: 0,
            nr_updates_before_replay: 0,
            updating: false,
            memory_allocation_failure: false,
            fn_name: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct BtreeUpdate {
    pub k: BkeyI,
    pub ip: usize,
    pub level: u8,
    pub btree_id: u8,
    pub btree_level: u8,
    pub path: u16,
    pub cached: bool,
}

#[derive(Clone, Debug)]
pub struct BtreeInsertEntry {
    pub btree_id: u8,
    pub level: u8,
    pub k: Bkey,
}

#[derive(Clone, Debug)]
pub struct JournalEntryPin {
    pub seq: u64,
    pub list: *mut std::ffi::c_void,
}

#[derive(Clone, Debug)]
pub struct BtreeCache {
    pub used: u64,
    pub max_size: u64,
    pub nr_nodes: u64,
    pub shrinker_wait: bool,
}

impl BtreeCache {
    pub fn new() -> Self {
        BtreeCache {
            used: 0,
            max_size: 0,
            nr_nodes: 0,
            shrinker_wait: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BtreeNodeState {
    Read,
    ReadDone,
    Write,
    WriteDone,
    Compact,
    Merge,
    Split,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BtreeIterFlags {
    Intents = 1,
    Prelock = 2,
    Slots = 4,
    Cached = 8,
    NoCheckPtGuard = 16,
    AllSnapshots = 32,
    FilterSnapshots = 64,
    Nopreserve = 128,
    PeekSlots = 256,
    NotExtents = 512,
    IsExtents = 1024,
    SrchDeletingSnapshots = 2048,
    SrchSnapshotInit = 4096,
    Recheck = 8192,
    Uptodate = 16384,
    Error = 32768,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BtreeNodeFlags {
    WriteStarted = 1,
    NeedsRewrite = 2,
    ReadInFlight = 4,
    WriteInFlight = 8,
    WriteLockHeld = 16,
    DirtyJournal = 32,
    ReadError = 64,
    WriteError = 128,
    FormatUpdate = 256,
    BtreeNodeAccounting = 512,
}

pub struct SixLock {
    pub state: u64,
    pub waiters: Vec<*mut std::ffi::c_void>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SixLockType {
    Read,
    Intent,
    Write,
}
