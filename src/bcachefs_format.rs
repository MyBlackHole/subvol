use crate::opts::BchOpts;

pub const BCH_SB_SECTOR: u64 = 8;
pub const BCH_SB_LAYOUT_SECTOR: u64 = 7;
pub const BCH_SB_LABEL_SIZE: usize = 64;
pub const BCH_SB_MEMBERS_MAX: usize = 64;
pub const BCH_REPLICAS_MAX: u32 = 4;
pub const BCH_BKEY_PTRS_MAX: u32 = 16;
pub const BKEY_U64S: usize = 4;
pub const BKEY_U64S_MAX: u8 = u8::MAX;
pub const BKEY_VAL_U64S_MAX: u8 = BKEY_U64S_MAX - BKEY_U64S as u8;
pub const KEY_PACKED_BITS_START: u8 = 24;
pub const KEY_FORMAT_LOCAL_BTREE: u8 = 0;
pub const KEY_FORMAT_CURRENT: u8 = 1;
pub const KEY_INODE_MAX: u64 = !0u64;
pub const KEY_OFFSET_MAX: u64 = !0u64;
pub const KEY_SNAPSHOT_MAX: u32 = !0u32;
pub const KEY_SIZE_MAX: u32 = !0u32;
pub const BTREE_MAX_DEPTH: u32 = 4;
pub const BTREE_ID_NR_MAX: u8 = 63;
pub const BCH_JOURNAL_BUCKETS_MIN: u32 = 8;
pub const BCH_SB_LAYOUT_SIZE_BITS_MAX: u8 = 16;
pub const BCH_SB_EXTENT_BP_SHIFT_DEFAULT: u64 = 10;
pub const BCH_KEY_MAGIC: u64 = 0x6263682a2a6b6579u64;
pub const JSET_KEYS_U64S: usize = 2;
pub const BCACHEFS_STATFS_MAGIC: u64 = 0xca6fa0cb;

pub type Uuid = [u8; 16];

pub const BCACHE_MAGIC: Uuid = [
    0xc6, 0x85, 0x73, 0xf6, 0x4e, 0x1a, 0x45, 0xca,
    0x82, 0x65, 0xf5, 0x7f, 0x48, 0xba, 0x6d, 0x81,
];

pub const BCHFS_MAGIC: Uuid = [
    0xc6, 0x85, 0x73, 0xf6, 0x66, 0xce, 0x90, 0xa9,
    0xd9, 0x6a, 0x60, 0xcf, 0x80, 0x3d, 0xf7, 0xef,
];

pub const JSET_MAGIC: u64 = 0x245235c1a3625032;
pub const BSET_MAGIC: u64 = 0x90135c78b99e07f5;

pub const GC_MERGE_NODES: u32 = 4;
pub const BTREE_NODE_OPEN_BUCKET_RESERVE: u32 = 8;

pub const BCH_VERSION_MAJOR: fn(u16) -> u16 = |v: u16| v >> 10;
pub const BCH_VERSION_MINOR: fn(u16) -> u16 = |v: u16| v & !(!0u16 << 10);
pub const BCH_VERSION: fn(u16, u16) -> u16 = |major: u16, minor: u16| (major << 10) | minor;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BchBkeyType {
    Deleted,
    Whiteout,
    Error,
    Cookie,
    HashWhiteout,
    BtreePtr,
    Extent,
    Reservation,
    Inode,
    InodeGeneration,
    Dirent,
    Xattr,
    Alloc,
    Quota,
    Stripe,
    ReflinkP,
    ReflinkV,
    InlineData,
    BtreePtrV2,
    IndirectInlineData,
    AllocV2,
    Subvolume,
    Snapshot,
    InodeV2,
    AllocV3,
    Set,
    Lru,
    AllocV4,
    Backpointer,
    InodeV3,
    BucketGens,
    SnapshotTree,
    LoggedOpTruncate,
    LoggedOpFinsert,
    Accounting,
    InodeAllocCursor,
    ExtentWhiteout,
    LoggedOpStripeUpdate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BtreeId {
    Extents,
    Inodes,
    Dirents,
    Xattrs,
    Alloc,
    Quotas,
    Stripes,
    Reflink,
    Subvolumes,
    Snapshots,
    Lru,
    Freespace,
    NeedDiscard,
    Backpointers,
    BucketGens,
    SnapshotTrees,
    DeletedInodes,
    LoggedOps,
    ReconcileWork,
    SubvolumeChildren,
    Accounting,
    ReconcileHipri,
    ReconcilePending,
    ReconcileScan,
    ReconcileWorkPhys,
    ReconcileHipriPhys,
    BucketToStripe,
    StripeBackpointers,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BchSbFieldType {
    Journal,
    MembersV1,
    Crypt,
    ReplicasV0,
    Quota,
    DiskGroups,
    Clean,
    Replicas,
    JournalSeqBlacklist,
    JournalV2,
    Counters,
    MembersV2,
    Errors,
    Ext,
    Downgrade,
    RecoveryPasses,
    ExtentTypeU64s,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BcachefsMetadataVersion {
    BkeyRenumber = 0x000a,
    InodeBtreeChange = 0x000b,
    Snapshot = 0x000c,
    InodeBackpointers = 0x000d,
    BtreePtrSectorsWritten = 0x000e,
    Snapshot2 = 0x000f,
    ReflinkPFix = 0x0010,
    SubvolDirent = 0x0011,
    InodeV2 = 0x0012,
    Freespace = 0x0013,
    AllocV4 = 0x0014,
    NewDataTypes = 0x0015,
    Backpointers = 0x0016,
    InodeV3 = 0x0017,
    UnwrittenExtents = 0x0018,
    BucketGens = 0x0019,
    LruV2 = 0x001a,
    FragmentationLru = 0x001b,
    NoBpsInAllocKeys = 0x001c,
    SnapshotTrees = 0x001d,
    MajorMinor = 0x0400,
    SnapshotSkiplists = 0x0401,
    DeletedInodes = 0x0402,
    RebalanceWork = 0x0403,
    MemberSeq = 0x0404,
    SubvolumeFsParent = 0x0405,
    BtreeSubvolumeChildren = 0x0406,
    MiBtreeBitmap = 0x0407,
    BucketStripeSectors = 0x0408,
    DiskAccountingV2 = 0x0409,
    DiskAccountingV3 = 0x040a,
    DiskAccountingInum = 0x040b,
    RebalanceWorkAcctFix = 0x040c,
    InodeHasChildSnapshots = 0x040d,
    BackpointerBucketGen = 0x040e,
    DiskAccountingBigEndian = 0x040f,
    ReflinkPMayUpdateOpts = 0x0410,
    InodeDepth = 0x0411,
    PersistentInodeCursors = 0x0412,
    AutofixErrors = 0x0413,
    DirectorySize = 0x0414,
    CachedBackpointers = 0x0415,
    StripeBackpointers = 0x0416,
    StripeLru = 0x0417,
    Casefolding = 0x0418,
    ExtentFlags = 0x0419,
    SnapshotDeletionV2 = 0x041a,
    FastDeviceRemoval = 0x041b,
    InodeHasCaseInsensitive = 0x041c,
    ExtentSnapshotWhiteouts = 0x041d,
    BitDirentOffset = 0x041e,
    BtreeNodeAccounting = 0x041f,
    SbFieldExtentTypeU64s = 0x0420,
    Reconcile = 0x0421,
    ExtentedKeyTypeError = 0x0422,
    BucketStripeIndex = 0x0423,
    NoSbUserDataReplicas = 0x0424,
    ErasureCoding = 0x0425,
    NeedDiscardByJournalSeq = 0x0426,
}

pub const bcachefs_metadata_version_min: u32 = 9;
pub const bcachefs_metadata_version_current: u32 = 0x0426;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BtreeIdFlags {
    Extents = 1,
    Snapshots = 2,
    SnapshotField = 4,
    Data = 8,
    WriteBuffer = 16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BchJsetEntryType {
    BtreeKeys,
    BtreeRoot,
    PrioPtrs,
    Blacklist,
    BlacklistV2,
    Usage,
    DataUsage,
    Clock,
    DevUsage,
    Log,
    Overwrite,
    WriteBufferKeys,
    Datetime,
    LogBkey,
    RewindLimit,
    Rewind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BchCsumType {
    None,
    Crc32cNonzero,
    Crc64Nonzero,
    Chacha20Poly1305_80,
    Chacha20Poly1305_128,
    Crc32c,
    Crc64,
    Xxhash,
}

pub static BCH_CRC_BYTES: [u8; 8] = [0, 4, 8, 10, 16, 4, 8, 8];

impl BchCsumType {
    pub fn is_encryption(self) -> bool {
        matches!(self, BchCsumType::Chacha20Poly1305_80 | BchCsumType::Chacha20Poly1305_128)
    }
    pub fn crc_bytes(self) -> u8 {
        BCH_CRC_BYTES[self as usize]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BchCompressionType {
    None,
    Lz4Old,
    Gzip,
    Lz4,
    Zstd,
    Incompressible,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BchStrHashType {
    Crc32c,
    Crc64,
    SiphashOld,
    Siphash,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BchSbFeature {
    Lz4,
    Gzip,
    Zstd,
    AtomicNlink,
    Ec,
    JournalSeqBlacklistV3,
    Reflink,
    NewSiphash,
    InlineData,
    NewExtentOverwrite,
    Incompressible,
    BtreePtrV2,
    ExtentsAboveBtreeUpdates,
    BtreeUpdatesJournalled,
    ReflinkInlineData,
    NewVarint,
    JournalNoFlush,
    AllocV2,
    ExtentsAcrossBtreeNodes,
    IncompatVersionField,
    Casefolding,
    NoAllocInfo,
    SmallImage,
    NoDefaultSb,
}

pub const BCH_SB_FEATURES_ALWAYS: u64 = 0x0006_2000;
pub const BCH_SB_FEATURES_ALL: u64 = 0x000F_BC00;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BchDataOpType {
    Unknown,
    Auto,
}

pub const BCH_SINGLE_DEVICE_SB_FIELDS: u32 = 0x0000_0003;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BchFsFlags {
    NewFs,
    Started,
    CleanRecovery,
    BtreeRunning,
    AccountingReplayDone,
    MayGoRw,
    ScrubJournal,
    MayUpgradeDowngrade,
    Rw,
    RwInitDone,
    WasRw,
    Stopping,
    EmergencyRo,
    GoingRo,
    WriteDisableComplete,
    CleanShutdown,
    InRecovery,
    InFsck,
    InitialGcUnfixed,
    NeedDeleteDeadSnapshots,
    Error,
    TopologyError,
    ErrorsFixed,
    ErrorsFixedSilent,
    ErrorsNotFixed,
    NoInvalidChecks,
    DiscardMountOptSet,
    SbDirty,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BchErrorActions {
    Continue,
    FixSafe,
    Panic,
    Ro,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BchFsUsageType {
    Reserved,
    Inodes,
    KeyVersion,
}

pub const BCH_DATA_NR: usize = 11;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum BchDataType {
    Free = 0,
    Sb = 1,
    Journal = 2,
    Btree = 3,
    User = 4,
    Cached = 5,
    Parity = 6,
    Stripe = 7,
    NeedGcGens = 8,
    NeedDiscard = 9,
    Unstriped = 10,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BchMemberError {
    No,
    Corruption,
    Read,
    Write,
    Checksum,
    NoRepair,
    Stale,
}

pub const BCH_MEMBER_ERROR_NR: usize = 7;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BchFsckRet {
    Ignore,
    Fix,
    CannotFix,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BchValidateFlags {
    NoEarly,
    SkipSort,
    SkipPossiblyWrong,
    NoJournalSeq,
    NoRoot,
    ReadDone,
    WriteCommit,
    WriteFlush,
}

#[derive(Clone, Copy, Debug)]
pub struct Bpos {
    pub inode: u64,
    pub offset: u64,
    pub snapshot: u32,
}

impl Bpos {
    pub const ZERO: Bpos = Bpos { inode: 0, offset: 0, snapshot: 0 };
    pub const MIN: Bpos = Bpos { inode: 0, offset: 0, snapshot: 0 };
    pub const MAX: Bpos = Bpos { inode: KEY_INODE_MAX, offset: KEY_OFFSET_MAX, snapshot: 0 };
    pub const SPOS_MAX: Bpos = Bpos { inode: KEY_INODE_MAX, offset: KEY_OFFSET_MAX, snapshot: KEY_SNAPSHOT_MAX };

    pub fn pos(inode: u64, offset: u64) -> Self {
        Bpos { inode, offset, snapshot: 0 }
    }

    pub fn spos(inode: u64, offset: u64, snapshot: u32) -> Self {
        Bpos { inode, offset, snapshot }
    }
}

impl Ord for Bpos {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.inode.cmp(&other.inode)
            .then(self.offset.cmp(&other.offset))
            .then(self.snapshot.cmp(&other.snapshot))
    }
}

impl PartialOrd for Bpos {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Bpos {
    fn eq(&self, other: &Self) -> bool {
        self.inode == other.inode && self.offset == other.offset && self.snapshot == other.snapshot
    }
}

impl Eq for Bpos {}

#[derive(Clone, Copy, Debug)]
pub struct Bversion {
    pub lo: u64,
    pub hi: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct BchCsum {
    pub lo: u64,
    pub hi: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct Bkey {
    pub u64s: u8,
    pub format: u8,
    pub needs_whiteout: bool,
    pub type_: BchBkeyType,
    pub bversion: Bversion,
    pub size: u32,
    pub p: Bpos,
}

#[derive(Clone, Copy, Debug)]
#[repr(C, packed)]
pub struct BkeyPacked {
    pub _data: [u64; 0],
    pub u64s: u8,
    pub format_needs_whiteout: u8,
    pub type_: u8,
    pub key_start: [u8; 0],
    pub pad: [u8; 37],
}

impl BkeyPacked {
    pub fn u64s(&self) -> u8 { self.u64s }
    pub fn type_(&self) -> u8 { self.type_ }
    pub fn format(&self) -> u8 { self.format_needs_whiteout & 0x7f }
    pub fn needs_whiteout(&self) -> bool { self.format_needs_whiteout & 0x80 != 0 }
    pub fn is_packed(&self) -> bool { self.format() != 0 }
    pub fn bytes(&self) -> usize { self.u64s as usize * 8 }
}

impl Bkey {
    pub fn init() -> Self {
        Bkey {
            u64s: BKEY_U64S as u8,
            format: KEY_FORMAT_CURRENT,
            needs_whiteout: false,
            type_: BchBkeyType::Deleted,
            bversion: Bversion { lo: 0, hi: 0 },
            size: 0,
            p: Bpos::ZERO,
        }
    }

    pub fn key(inode: u64, offset: u64, size: u32) -> Self {
        Bkey {
            u64s: BKEY_U64S as u8,
            format: KEY_FORMAT_CURRENT,
            needs_whiteout: false,
            type_: BchBkeyType::Deleted,
            bversion: Bversion { lo: 0, hi: 0 },
            size,
            p: Bpos::pos(inode, offset),
        }
    }

    pub fn pos_key(pos: Bpos) -> Self {
        Bkey {
            u64s: BKEY_U64S as u8,
            format: KEY_FORMAT_CURRENT,
            needs_whiteout: false,
            type_: BchBkeyType::Deleted,
            bversion: Bversion { lo: 0, hi: 0 },
            size: 0,
            p: pos,
        }
    }

    pub fn bytes(&self) -> usize {
        self.u64s as usize * 8
    }
}

#[derive(Clone, Copy, Debug)]
pub struct BkeyI {
    pub k: Bkey,
}

#[derive(Clone, Copy, Debug)]
pub struct BkeyFormat {
    pub key_u64s: u8,
    pub nr_fields: u8,
    pub bits_per_field: [u8; 6],
    pub field_offset: [u64; 6],
}

#[derive(Clone, Copy, Debug)]
pub struct BchExtentPtr {
    pub dev: u32,
    pub gen: u32,
    pub offset: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct Bset {
    pub seq: u64,
    pub journal_seq: u64,
    pub flags: u32,
    pub version: u16,
    pub u64s: u16,
}

#[derive(Clone, Copy, Debug)]
pub struct BchSbField {
    pub u64s: u32,
    pub type_: BchSbFieldType,
}

#[derive(Clone, Copy, Debug)]
pub struct BchSbLayout {
    pub magic: Uuid,
    pub layout_type: u8,
    pub sb_max_size_bits: u8,
    pub nr_superblocks: u8,
    pub sb_offset: [u64; 61],
}

#[derive(Clone, Debug)]
pub struct BchSb {
    pub csum: BchCsum,
    pub version: u16,
    pub version_min: u16,
    pub magic: Uuid,
    pub uuid: Uuid,
    pub user_uuid: Uuid,
    pub label: [u8; BCH_SB_LABEL_SIZE],
    pub offset: u64,
    pub seq: u64,
    pub block_size: u16,
    pub dev_idx: u8,
    pub nr_devices: u8,
    pub u64s: u32,
    pub time_base_lo: u64,
    pub time_base_hi: u32,
    pub time_precision: u32,
    pub flags: [u64; 7],
    pub write_time: u64,
    pub features: [u64; 2],
    pub compat: [u64; 2],
    pub layout: BchSbLayout,
    pub fields: Vec<BchSbField>,
}

impl BchSb {
    pub fn new() -> Self {
        BchSb {
            csum: BchCsum { lo: 0, hi: 0 },
            version: 0,
            version_min: 0,
            magic: [0u8; 16],
            uuid: [0u8; 16],
            user_uuid: [0u8; 16],
            label: [0u8; BCH_SB_LABEL_SIZE],
            offset: 0,
            seq: 0,
            block_size: 0,
            dev_idx: 0,
            nr_devices: 0,
            u64s: 0,
            time_base_lo: 0,
            time_base_hi: 0,
            time_precision: 0,
            flags: [0u64; 7],
            write_time: 0,
            features: [0u64; 2],
            compat: [0u64; 2],
            layout: BchSbLayout {
                magic: [0u8; 16],
                layout_type: 0,
                sb_max_size_bits: 0,
                nr_superblocks: 0,
                sb_offset: [0u64; 61],
            },
            fields: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct JsetEntry {
    pub u64s: u16,
    pub btree_id: u8,
    pub level: u8,
    pub type_: BchJsetEntryType,
}

#[derive(Clone, Debug)]
pub struct Jset {
    pub csum: BchCsum,
    pub magic: u64,
    pub seq: u64,
    pub version: u32,
    pub flags: u32,
    pub u64s: u32,
    pub last_seq: u64,
    pub entries: Vec<JsetEntry>,
}

#[derive(Clone, Copy, Debug)]
pub struct BtreeNodeHeader {
    pub csum: BchCsum,
    pub magic: u64,
    pub flags: u64,
    pub min_key: Bpos,
    pub max_key: Bpos,
    pub format: BkeyFormat,
    pub keys: Bset,
    pub u64s: u16,
}

#[derive(Clone, Copy, Debug)]
pub struct BchMember {
    pub uuid: Uuid,
    pub label: [u8; BCH_SB_LABEL_SIZE],
    pub btree_bitmap: [u64; 1],
    pub seq: u64,
    pub bucket_size: u16,
    pub group: u8,
    pub pad: [u8; 5],
    pub njournal_buckets: u32,
    pub guage: u32,
    pub state: u32,
    pub discard: u8,
    pub data_allowed: u8,
    pub has_data: u64,
    pub capabilities: u64,
    pub flags: u64,
    pub io_opt: u16,
    pub max_extent: u16,
    pub bucket_deleted: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct BchMemberCpu {
    pub nbuckets: u64,
    pub first_bucket: u64,
    pub bucket_size: u16,
    pub group: u8,
    pub state: u32,
    pub discard: bool,
    pub data_allowed: u8,
    pub has_data: u64,
    pub capabilities: u64,
    pub io_opt: u16,
    pub max_extent: u16,
}

#[derive(Clone, Copy, Debug)]
pub struct Nonce {
    pub d: [u32; 4],
}

#[derive(Clone, Copy, Debug)]
pub struct BchKey_(pub [u64; 4]);

#[derive(Clone, Copy, Debug)]
pub struct BchEncryptedKey {
    pub magic: u64,
    pub key: BchKey_,
}

#[derive(Clone, Copy, Debug)]
pub struct BchSbFieldCrypt {
    pub field: BchSbField,
    pub flags: u64,
    pub kdf_flags: u64,
    pub key: BchEncryptedKey,
}

#[derive(Clone, Copy, Debug)]
pub struct Bk {
    pub k: Bkey,
    pub v: [u64; 0],
}

pub const BKEY_PADDED_ONSTACK: fn(BkeyI, usize) -> Vec<u64> = |_key: BkeyI, _pad: usize| Vec::new();

/* Alloc v4 format (per-bucket allocation state) */

pub const BCH_ALLOC_V4_U64S_V0: u32 = 6;
pub const BCH_ALLOC_V4_U64S: u32 = std::mem::size_of::<BchAllocV4>() as u32 / 8;

#[derive(Clone, Copy, Debug)]
pub struct BchAllocV4 {
    pub journal_seq_nonempty: u64,
    pub flags: u32,
    pub gen: u8,
    pub oldest_gen: u8,
    pub data_type: u8,
    pub stripe_redundancy_obsolete: u8,
    pub dirty_sectors: u32,
    pub cached_sectors: u32,
    pub io_time: [u64; 2],
    pub stripe_refcount: u32,
    pub nr_external_backpointers: u32,
    pub journal_seq_empty: u64,
    pub stripe_sectors: u32,
}

impl BchAllocV4 {
    pub fn default() -> Self {
        BchAllocV4 {
            journal_seq_nonempty: 0,
            flags: 0,
            gen: 0,
            oldest_gen: 0,
            data_type: BchDataType::Free as u8,
            stripe_redundancy_obsolete: 0,
            dirty_sectors: 0,
            cached_sectors: 0,
            io_time: [0; 2],
            stripe_refcount: 0,
            nr_external_backpointers: 0,
            journal_seq_empty: 0,
            stripe_sectors: 0,
        }
    }

    pub fn data_type(&self) -> BchDataType {
        match self.data_type {
            0 => BchDataType::Free,
            1 => BchDataType::Sb,
            2 => BchDataType::Journal,
            3 => BchDataType::Btree,
            4 => BchDataType::User,
            5 => BchDataType::Cached,
            6 => BchDataType::Parity,
            7 => BchDataType::Stripe,
            8 => BchDataType::NeedGcGens,
            9 => BchDataType::NeedDiscard,
            10 => BchDataType::Unstriped,
            _ => BchDataType::Free,
        }
    }

    pub fn backpointers_start(&self) -> u32 {
        ((self.flags >> 2) & 0x3f)
    }

    pub fn nr_backpointers(&self) -> u32 {
        ((self.flags >> 8) & 0x3f)
    }
}

/* Backpointer format */

#[derive(Clone, Copy, Debug)]
pub struct BchBackpointer {
    pub btree_id: u8,
    pub level: u8,
    pub data_type: u8,
    pub bucket_gen: u8,
    pub bucket_len: u64,
    pub pos: Bpos,
}

impl BchBackpointer {
    pub fn default() -> Self {
        BchBackpointer {
            btree_id: 0,
            level: 0,
            data_type: 0,
            bucket_gen: 0,
            bucket_len: 0,
            pos: Bpos::ZERO,
        }
    }
}

/* LRU format */

pub const LRU_TIME_BITS: u64 = 48;
pub const LRU_TIME_MAX: u64 = (1u64 << LRU_TIME_BITS) - 1;

#[derive(Clone, Copy, Debug)]
pub struct BchLru {
    pub idx: u64,
}

/* Replicas format */

#[derive(Clone, Copy, Debug)]
pub struct BchReplicasEntryV1 {
    pub data_type: u8,
    pub nr_devs: u8,
    pub nr_required: u8,
    pub devs: [u8; BCH_REPLICAS_MAX as usize],
}

impl BchReplicasEntryV1 {
    pub fn entry_bytes(&self) -> usize {
        3 + self.nr_devs as usize
    }
}

#[derive(Clone, Copy, Debug)]
pub struct BchReplicasEntryV0 {
    pub data_type: u8,
    pub nr_devs: u8,
    pub devs: [u8; BCH_REPLICAS_MAX as usize],
}

/* Accounting format */

pub const BCH_ACCOUNTING_MAX_COUNTERS: usize = 3;

#[derive(Clone, Copy, Debug)]
pub struct BchAccounting {
    pub d: [u64; BCH_ACCOUNTING_MAX_COUNTERS],
}

#[derive(Clone, Copy, Debug)]
pub struct BchAcctNrInodes;

#[derive(Clone, Copy, Debug)]
pub struct BchAcctPersistentReserved {
    pub nr_replicas: u8,
}

#[derive(Clone, Copy, Debug)]
pub struct BchAcctReplicas {
    pub data_type: u8,
    pub nr_devs: u8,
    pub nr_required: u8,
    pub devs: [u8; BCH_REPLICAS_MAX as usize],
}

#[derive(Clone, Copy, Debug)]
pub struct BchAcctDevDataType {
    pub dev: u8,
    pub data_type: u8,
}

#[derive(Clone, Copy, Debug)]
pub struct BchAcctCompression {
    pub type_: u8,
}

#[derive(Clone, Copy, Debug)]
pub struct DiskAccountingPos {
    pub type_: u8,
    pub data: [u8; 19],
}

/* Disk group format */

pub const BCH_SB_LABEL_SIZE_32: usize = 32;

#[derive(Clone, Copy, Debug)]
pub struct BchDiskGroup {
    pub label: [u8; BCH_SB_LABEL_SIZE_32],
    pub flags: [u64; 2],
}

#[derive(Clone, Copy, Debug)]
pub struct BchDiskGroupCpu {
    pub deleted: bool,
    pub parent: u16,
    pub label: [u8; BCH_SB_LABEL_SIZE_32],
    pub devs: [u64; 1],
}

#[derive(Clone, Copy, Debug)]
pub struct BchSbFieldDiskGroups {
    pub entries: [BchDiskGroup; 0],
}

/* Bucket gens format */

pub const KEY_TYPE_BUCKET_GENS_BITS: u32 = 8;
pub const KEY_TYPE_BUCKET_GENS_NR: u32 = 1u32 << KEY_TYPE_BUCKET_GENS_BITS;
pub const KEY_TYPE_BUCKET_GENS_MASK: u32 = KEY_TYPE_BUCKET_GENS_NR - 1;

#[derive(Clone, Copy, Debug)]
pub struct BchBucketGens {
    pub gens: [u8; KEY_TYPE_BUCKET_GENS_NR as usize],
}
