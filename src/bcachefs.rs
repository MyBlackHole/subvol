use crate::bcachefs_format::*;
use crate::opts::BchOpts;

pub const BCH_TIME_STAT_NR: usize = 46;

#[derive(Clone, Debug)]
pub struct BchFsCounters {
    pub cells: Vec<u64>,
}

#[derive(Clone, Debug)]
pub struct BchFsErrors {
    pub nr: u64,
}

#[derive(Clone, Debug)]
pub struct BchFsRecovery {
    pub passes_done: [u64; 4],
    pub passes_run: [u64; 4],
}

#[derive(Clone, Debug)]
pub struct BchFsGc {
    pub rewrites: u64,
    pub nodes_visited: u64,
}

#[derive(Clone, Debug)]
pub struct BchFsGcGens {
    pub need: bool,
}

#[derive(Clone, Debug)]
pub struct BchFsBtree {
    pub nodes: u64,
    pub cache_size: u64,
}

#[derive(Clone, Debug)]
pub struct BchFsAllocator {
    pub need_inc: bool,
    pub need_lru: bool,
    pub need_reclaim: bool,
    pub rw_devs: [BchDevsMask; BCH_DATA_NR],
    pub rw_devs_change_count: u64,
    pub open_buckets: Vec<OpenBucket>,
    pub open_buckets_hash: Vec<OpenBucketIdx>,
    pub open_buckets_nr_free: OpenBucketIdx,
    pub open_buckets_partial: Vec<OpenBucketIdx>,
    pub open_buckets_partial_nr: OpenBucketIdx,
    pub write_points: Vec<WritePoint>,
}

#[derive(Clone, Debug)]
pub struct BchFsDiscards {
    pub nr: u64,
}

#[derive(Clone, Debug)]
pub struct BchFsCapacity {
    pub capacity: u64,
    pub reserved: u64,
    pub nr_free: u64,
    pub nr_dirty: u64,
    pub nr_cached: u64,
}

pub type OpenBucketIdx = u16;

#[derive(Clone, Copy, Debug, Default)]
pub struct BchDevUsage {
    pub buckets: [u64; BCH_DATA_NR],
}

#[derive(Clone, Copy, Debug, Default)]
pub struct BchDevUsageType {
    pub buckets: u64,
    pub sectors: u64,
    pub fragmented: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct BchDevUsageFull {
    pub d: [BchDevUsageType; BCH_DATA_NR],
}

impl Default for BchDevUsageFull {
    fn default() -> Self {
        BchDevUsageFull {
            d: [Default::default(); BCH_DATA_NR],
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct BchFsUsageBase {
    pub hidden: u64,
    pub btree: u64,
    pub data: u64,
    pub cached: u64,
    pub reserved: u64,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct BchFsUsageShort {
    pub capacity: u64,
    pub used: u64,
    pub free: u64,
}

#[derive(Clone, Debug)]
pub struct OpenBucket {
    pub freelist: OpenBucketIdx,
    pub hash: OpenBucketIdx,
    pub ec_idx: u8,
    pub data_type: BchDataType,
    pub valid: bool,
    pub on_partial_list: bool,
    pub do_discards_fast: bool,
    pub dev: u8,
    pub gen: u8,
    pub sectors_free: u32,
    pub bucket: u64,
}

#[derive(Clone, Debug)]
pub enum WritePointState {
    Stopped,
    WaitingIo,
    WaitingWork,
    Runnable,
    Running,
}

#[derive(Clone, Debug)]
pub struct WritePoint {
    pub last_used: u64,
    pub write_point_val: u64,
    pub data_type: BchDataType,
    pub sectors_free: u32,
    pub prev_sectors_free: u32,
    pub sectors_allocated: u64,
    pub state: WritePointState,
    pub last_state_change: u64,
}

#[derive(Clone, Debug)]
pub struct BchFsCompress {
    pub compress: BchCompressionType,
    pub background_compress: BchCompressionType,
}

#[derive(Clone, Debug)]
pub struct BchFsReconcile {
    pub nr_work: u64,
    pub nr_pending: u64,
}

#[derive(Clone, Debug)]
pub struct BchFsCopygc {
    pub nr_running: u32,
    pub threshold: u32,
}

#[derive(Clone, Debug)]
pub struct BchFsEc {
    pub nr_stripes: u64,
    pub nr_active: u32,
}

#[derive(Clone, Debug)]
pub struct BchFsSnapshots {
    pub nr: u64,
    pub skiplists: bool,
}

#[derive(Clone, Debug)]
pub struct BchFsVfs {
    pub nr_inodes: u64,
    pub nr_dentries: u64,
}

#[derive(Clone, Debug)]
pub struct BchFs {
    pub flags: u64,
    pub name: String,
    pub devs: Vec<Option<BchDev>>,
    pub opts: BchOpts,
    pub sb: BchSb,
    pub block_bits: u16,
    pub journal: Journal,
    pub btree: BchFsBtree,
    pub allocator: BchFsAllocator,
    pub gc: BchFsGc,
    pub gc_gens: BchFsGcGens,
    pub recovery: BchFsRecovery,
    pub counters: BchFsCounters,
    pub errors: BchFsErrors,
    pub capacity: BchFsCapacity,
    pub compress: BchFsCompress,
    pub reconcile: BchFsReconcile,
    pub copygc: BchFsCopygc,
    pub ec: BchFsEc,
    pub snapshots: BchFsSnapshots,
    pub key_version: u64,
    pub discard: bool,
}

impl BchFs {
    pub fn new(name: &str) -> Self {
        BchFs {
            flags: 0,
            name: name.to_string(),
            devs: Vec::new(),
            opts: BchOpts::empty(),
            sb: BchSb::new(),
            block_bits: 9,
            journal: Journal::new(),
            btree: BchFsBtree { nodes: 0, cache_size: 0 },
            allocator: BchFsAllocator {
                need_inc: false,
                need_lru: false,
                need_reclaim: false,
                rw_devs: [BchDevsMask::new(); BCH_DATA_NR],
                rw_devs_change_count: 0,
                open_buckets: Vec::new(),
                open_buckets_hash: Vec::new(),
                open_buckets_nr_free: 0,
                open_buckets_partial: Vec::new(),
                open_buckets_partial_nr: 0,
                write_points: Vec::new(),
            },
            gc: BchFsGc { rewrites: 0, nodes_visited: 0 },
            gc_gens: BchFsGcGens { need: false },
            recovery: BchFsRecovery { passes_done: [0; 4], passes_run: [0; 4] },
            counters: BchFsCounters { cells: Vec::new() },
            errors: BchFsErrors { nr: 0 },
            capacity: BchFsCapacity { capacity: 0, reserved: 0, nr_free: 0, nr_dirty: 0, nr_cached: 0 },
            compress: BchFsCompress { compress: BchCompressionType::None, background_compress: BchCompressionType::None },
            reconcile: BchFsReconcile { nr_work: 0, nr_pending: 0 },
            copygc: BchFsCopygc { nr_running: 0, threshold: 0 },
            ec: BchFsEc { nr_stripes: 0, nr_active: 0 },
            snapshots: BchFsSnapshots { nr: 0, skiplists: false },
            key_version: 0,
            discard: false,
        }
    }

    pub fn block_bytes(&self) -> u32 {
        self.opts.block_size
    }

    pub fn block_sectors(&self) -> u32 {
        self.opts.block_size >> 9
    }

    pub fn bucket_bytes(&self, ca: &BchDev) -> u32 {
        ca.mi.bucket_size as u32 * 512
    }
}

#[derive(Clone, Debug)]
pub struct BchDev {
    pub fs: *mut BchFs,
    pub dev_idx: u8,
    pub removing: bool,
    pub mi: BchMemberCpu,
    pub uuid: Uuid,
    pub name: String,
    pub nbuckets: u64,
    pub first_bucket: u64,
    pub bucket_size: u16,
    pub nr_open_buckets: u32,
    pub nr_partial_buckets: u32,
    pub nr_btree_reserve: u32,
    pub alloc_cursor: [u64; 3],
    pub journal: JournalDevice,
}

impl BchDev {
    pub fn new(fs: *mut BchFs, dev_idx: u8) -> Self {
        BchDev {
            fs,
            dev_idx,
            removing: false,
            mi: BchMemberCpu {
                nbuckets: 0,
                first_bucket: 0,
                bucket_size: 0,
                group: 0,
                state: 0,
                discard: false,
                data_allowed: 0,
                has_data: 0,
                capabilities: 0,
                io_opt: 0,
                max_extent: 0,
            },
            uuid: [0; 16],
            name: String::new(),
            nbuckets: 0,
            first_bucket: 0,
            bucket_size: 0,
            nr_open_buckets: 0,
            nr_partial_buckets: 0,
            nr_btree_reserve: 0,
            alloc_cursor: [0; 3],
            journal: JournalDevice::new(),
        }
    }

    pub fn bucket_bytes(&self) -> u32 {
        self.bucket_size as u32 * 512
    }

    pub fn fs(&self) -> &BchFs {
        unsafe { &*self.fs }
    }

    pub fn fs_mut(&mut self) -> &mut BchFs {
        unsafe { &mut *self.fs }
    }
}

#[derive(Clone, Debug)]
pub struct Journal {
    pub buf: Vec<u8>,
    pub devices: Vec<JournalDevice>,
    pub seq: u64,
    pub last_seq: u64,
    pub nr_entries: u32,
    pub flushed_seq_ondisk: u64,
    pub seq_ondisk: u64,
    pub cur_seq: u64,
    pub pin: Vec<u8>,
    pub reservations: u32,
    pub buf_size_want: u32,
    pub cur_entry_u64s: u32,
    pub blocked: u64,
    pub fs: *mut BchFs,
    pub entry_list: Vec<u8>,
}

impl Journal {
    pub fn new() -> Self {
        Journal {
            buf: Vec::new(),
            devices: Vec::new(),
            seq: 0,
            last_seq: 0,
            nr_entries: 0,
            flushed_seq_ondisk: 0,
            seq_ondisk: 0,
            cur_seq: 0,
            pin: Vec::new(),
            reservations: 0,
            buf_size_want: 0,
            cur_entry_u64s: 0,
            blocked: 0,
            fs: std::ptr::null_mut(),
            entry_list: Vec::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct JournalDevice {
    pub buckets: Vec<u64>,
    pub bucket_seq: Vec<u64>,
    pub nr_buckets: u32,
    pub bucket_size: u32,
    pub sectors_free: u32,
    pub nr: u32,
    pub cur_idx: u32,
    pub next_bucket: u32,
    pub seq: u64,
    pub highest_seq_found: u64,
}

impl JournalDevice {
    pub fn new() -> Self {
        JournalDevice {
            buckets: Vec::new(),
            bucket_seq: Vec::new(),
            nr_buckets: 0,
            bucket_size: 0,
            sectors_free: 0,
            nr: 0,
            cur_idx: 0,
            next_bucket: 0,
            seq: 0,
            highest_seq_found: 0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Bucket {
    pub marker: u64,
    pub gen: u8,
    pub data_type: u8,
    pub dirty_sectors: u16,
    pub cached_sectors: u16,
    pub stripe: u64,
    pub nr_extents: u32,
}

#[derive(Clone, Debug)]
pub struct BchReplicasEntry {
    pub data_type: u8,
    pub nr_devs: u8,
    pub devs: [u8; BCH_REPLICAS_MAX as usize],
    pub sectors: u64,
}

#[derive(Clone, Debug)]
pub struct BchReplicasCpu {
    pub entries: Vec<BchReplicasEntry>,
    pub nr: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct BchDevsMask {
    pub d: [u64; 1],
}

impl BchDevsMask {
    pub fn new() -> Self {
        BchDevsMask { d: [0] }
    }

    pub fn set(&mut self, dev: u32) {
        self.d[0] |= 1u64 << dev;
    }

    pub fn test(&self, dev: u32) -> bool {
        (self.d[0] >> dev) & 1 != 0
    }
}

pub struct IoClock {
    pub now: u64,
    pub max_time: u64,
}

pub struct BchMemquotaType {
    pub enabled: bool,
}
