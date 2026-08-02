//! Safe, single-format storage-engine API over the bcachefs-style btree,
//! transaction and journal core.
//!
//! The raw port remains internal: every mutation below is staged through an
//! intent iterator, committed in a transaction, and made recoverable only by
//! a successfully flushed journal record.  This is deliberately an engine
//! core, not a filesystem-compatibility layer.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fmt,
    fs::OpenOptions,
    io,
    ops::{Deref, DerefMut},
    path::Path,
    sync::{atomic::Ordering, Arc, Condvar, Mutex, MutexGuard, Weak},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use urcu::{Rcu, RcuThread};

use crate::{
    btree::{
        bkey::{
            bkey, bkey_err, bkey_i, bkey_s_c, bkey_val_u64s, bpos, bpos_eq, BKEY_U64S,
            BKEY_VAL_U64S_MAX, KEY_FORMAT_CURRENT, POS_MIN,
        },
        bset::{
            bch2_bkey_ptrs_c, bch_alloc_v4, bch_backpointer, extent_entry_is_ptr,
            KEY_TYPE_alloc_v4, KEY_TYPE_backpointer, KEY_TYPE_btree_ptr, KEY_TYPE_btree_ptr_v2,
            KEY_TYPE_cookie, KEY_TYPE_deleted, KEY_TYPE_extent, BCH_EXTENT_PTR_DEV,
            BCH_EXTENT_PTR_GEN, BCH_EXTENT_PTR_OFFSET,
        },
        cache::bch2_fs_btree_cache_init,
        interior::{bch2_btree_node_check_topology, bch2_btree_root_alloc_fake},
        iter::{
            bch2_btree_iter_next, bch2_btree_iter_peek, bch2_btree_iter_peek_node,
            bch2_btree_iter_traverse, bch2_trans_begin, bch2_trans_init, bch2_trans_iter_exit,
            bch2_trans_iter_init, bch2_trans_iter_init_common, bch2_trans_put, btree_iter,
            btree_trans, BTREE_ITER_all_snapshots, BTREE_ITER_intent, BTREE_ITER_not_extents,
            BTREE_ITER_snapshot_field,
        },
        types::{
            bch2_btree_id_root_b, bch_fs, clear_btree_node_fake, clear_btree_node_need_rewrite,
            BTREE_ID_NR,
        },
        update::{
            bch2_btree_bit_mod, bch2_clear_derived_tree, bch2_rebuild_derived_for_key,
            bch2_trans_commit, bch2_trans_update, trigger_update_value,
        },
    },
    journal::{
        bch2_journal_flush, bch2_journal_flush_pins, bch2_journal_read, bch2_journal_replay,
        bch2_journal_restore_for_replay, bch2_journal_update_last_seq,
        bch2_journal_update_last_seq_ondisk, journal_low_on_space, journal_med_on_space,
        journal_start_info, journal_state_offset,
    },
    sb::{
        bcachefs_metadata_version_current, bch_member, bch_sb_field_journal_v2,
        bch_sb_field_journal_v2_entry, bch_sb_field_members_v2, BCH_SB_FIELD_journal_v2,
        BCH_SB_FIELD_members_v2, BCHFS_MAGIC,
    },
};

/// The only durable engine data format accepted by this crate.
pub const STORAGE_FORMAT_VERSION: u32 = 2;

/*
 * Fixed single-version engine layout:
 *
 *   [reserved][four journal buckets]
 *
 * Durability follows bcachefs' journal semantics: every written jset repeats
 * the full btree-root set (write.c's bch2_journal_write_prep()), recovery
 * replays the retained window from last_seq (recovery.c), and reclaim
 * advances last_seq only after node pins have been flushed.
 */
const JOURNAL_FILE_SECTORS: u64 = 16_384;
const JOURNAL_BUCKET_START: u64 = 1;
const JOURNAL_BUCKETS: u64 = 4;
const JOURNAL_BUCKET_SIZE: u16 = 2_048;
const ENGINE_JOURNAL_UUID: [u8; 16] = [0x53; 16];
const RECLAIM_WORKER_DELAY: Duration = Duration::from_millis(25);
const BCH_DATA_FREE: u8 = 0;
const BCH_DATA_BTREE: u8 = 3;
const BCH_DATA_NEED_DISCARD: u8 = 9;
const BTREE_ID_FREESPACE: u8 = 5;
const BTREE_ID_NEED_DISCARD: u8 = 6;

fn alloc_freespace_pos(position: bpos, alloc: &bch_alloc_v4) -> bpos {
    let gc_gen = alloc.gen.wrapping_sub(alloc.oldest_gen);
    bpos {
        offset: position.offset | (((gc_gen as u64) >> 4) << 56),
        ..position
    }
}

/// A logical btree identifier.  The IDs are engine-local and need not expose
/// the filesystem-specific `BCH_BTREE_IDS()` namespace.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BtreeId(u8);

impl BtreeId {
    pub const DEFAULT: Self = Self(0);

    pub fn new(id: u8) -> Result<Self, EngineError> {
        if id as usize >= BTREE_ID_NR {
            return Err(EngineError::InvalidBtreeId(id));
        }
        Ok(Self(id))
    }

    pub const fn as_u8(self) -> u8 {
        self.0
    }
}

/// Full btree search position.  It maps directly to the bcachefs `bpos`
/// carried by an iterator path.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct KeyPosition {
    pub inode: u64,
    pub offset: u64,
    pub snapshot: u32,
}

impl KeyPosition {
    pub const fn new(inode: u64, offset: u64, snapshot: u32) -> Self {
        Self {
            inode,
            offset,
            snapshot,
        }
    }

    const fn raw(self) -> bpos {
        bpos {
            inode: self.inode,
            offset: self.offset,
            snapshot: self.snapshot,
        }
    }
}

/// An owned logical key.  Values are stored as native-endian u64 words so
/// their variable-size bkey representation remains exact and single-version.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BtreeKey {
    position: KeyPosition,
    value: Vec<u64>,
}

/// Deterministic test fault locations with bcachefs-equivalent retry/write
/// boundaries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FaultPoint {
    /// `trans_maybe_inject_restart()` before commit side effects.
    TransactionRestart,
    /// A journal write failure before record publication or sequence advance.
    JournalWrite,
    /// A restart injected at the discard worker's per-bucket transaction
    /// commit boundary (discard.c:598-657 fast_work: every bucket is its
    /// own btree transaction, commit.c:1390 injection point).
    DiscardCommitRestart,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryFaultPoint {
    AfterJournalReplay,
    DuringDerivedRebuild,
    BeforePublication,
}

/// A fault injected into the fsck repair path (T0200), mirroring the
/// `RecoveryFaultPoint` one-shot injection model: `fsck_image_with_fault`
/// passes a single point, the first repair transaction consumes it.
/// The points ride the existing error propagation paths, adding no new
/// control flow: `DuringRepairRestart` injects -4 at the repair commit
/// boundary, which the bch2_trans_begin retry loop resolves
/// (trans_maybe_inject_restart, commit.c:1390; lockrestart_do,
/// iter.h:1115-1127); `DuringRepairOom` injects a hard -12 with no
/// realloc requirement (restarted == 0), which aborts the repair with
/// the transaction error; `AfterRepairBeforeFlush` fails the journal
/// flush that makes repairs durable, mirroring a failed fs.exit()
/// shutdown (fsck.rs:457-460).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FsckFaultPoint {
    DuringRepairRestart,
    DuringRepairOom,
    AfterRepairBeforeFlush,
}

/// A durable journal image captured after successful flushes.  It models the
/// state a fresh engine receives after a crash; unflushed transaction updates
/// are intentionally absent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JournalSnapshot {
    format_version: u32,
    records: Vec<Vec<u64>>,
    next_sequence: u64,
}

impl JournalSnapshot {
    pub const fn format_version(&self) -> u32 {
        self.format_version
    }

    pub fn record_count(&self) -> usize {
        self.records.len()
    }

    pub const fn next_sequence(&self) -> u64 {
        self.next_sequence
    }
}

/// Durable boundary returned by `sync()` and `Transaction::commit_sync()`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DurabilityPoint {
    pub journal_sequence: u64,
    pub journal_sequence_ondisk: u64,
}

impl BtreeKey {
    pub fn new(position: KeyPosition, value: Vec<u64>) -> Result<Self, EngineError> {
        if value.len() > BKEY_VAL_U64S_MAX as usize {
            return Err(EngineError::ValueTooLarge(value.len()));
        }
        Ok(Self { position, value })
    }

    pub const fn position(&self) -> KeyPosition {
        self.position
    }

    pub fn value(&self) -> &[u64] {
        &self.value
    }
}

/// Observable state of the single-consumer background journal reclaimer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReclaimStatus {
    pub requested: u64,
    pub completed: u64,
    pub running: bool,
    pub last_error: Option<i32>,
}

/// Stable operational counters for the engine core.  The fields intentionally
/// describe durability and reclaim progress rather than filesystem policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EngineMetrics {
    pub journal_sequence: u64,
    pub journal_sequence_ondisk: u64,
    pub journal_last_sequence: u64,
    pub journal_last_sequence_ondisk: u64,
    pub journal_records: usize,
    pub reclaim: ReclaimStatus,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DerivedStateMismatch {
    InvalidPointer,
    Generation,
    DuplicateBackpointer,
    AllocSet,
    BackpointerSet,
    FreespaceSet,
    NeedDiscardSet,
    OpenBucketFree,
    NotRwBucketFree,
}

#[derive(Debug)]
pub enum EngineError {
    InvalidBtreeId(u8),
    ValueTooLarge(usize),
    UnsupportedFormatVersion(u32),
    Transaction(i32),
    DerivedState(DerivedStateMismatch),
    Journal(i32),
    ReclaimTimeout,
    Io(io::Error),
    Poisoned,
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidBtreeId(id) => write!(f, "invalid btree id {id}"),
            Self::ValueTooLarge(words) => write!(f, "bkey value has {words} words, exceeds format"),
            Self::UnsupportedFormatVersion(version) => {
                write!(f, "unsupported storage format version {version}")
            }
            Self::Transaction(error) => write!(f, "btree transaction failed: {error}"),
            Self::DerivedState(mismatch) => write!(f, "derived state mismatch: {mismatch:?}"),
            Self::Journal(error) => write!(f, "journal operation failed: {error}"),
            Self::ReclaimTimeout => {
                f.write_str("background journal reclaim did not finish in time")
            }
            Self::Io(error) => write!(f, "journal device I/O failed: {error}"),
            Self::Poisoned => f.write_str("storage engine mutex is poisoned"),
        }
    }
}

impl std::error::Error for EngineError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for EngineError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

enum TransactionOperation {
    Put {
        btree: BtreeId,
        key: BtreeKey,
    },
    Delete {
        btree: BtreeId,
        position: KeyPosition,
    },
}

/// Buffered transaction.  It has no mutation method outside its iterator
/// staging/commit implementation in `StorageEngine::commit_operations()`.
pub struct Transaction<'engine> {
    engine: &'engine StorageEngine,
    operations: Vec<TransactionOperation>,
}

impl<'engine> Transaction<'engine> {
    pub fn put(&mut self, btree: BtreeId, key: BtreeKey) -> &mut Self {
        self.operations
            .push(TransactionOperation::Put { btree, key });
        self
    }

    pub fn delete(&mut self, btree: BtreeId, position: KeyPosition) -> &mut Self {
        self.operations
            .push(TransactionOperation::Delete { btree, position });
        self
    }

    pub fn commit(self) -> Result<(), EngineError> {
        self.engine.commit_operations(&self.operations)
    }

    /// Commits through the iterator/transaction path and waits until the
    /// resulting journal record is durable.
    pub fn commit_sync(self) -> Result<DurabilityPoint, EngineError> {
        let engine = self.engine;
        self.commit()?;
        engine.sync()
    }
}

/// A read-side transaction.  The userland RCU registration remains tied to
/// the calling thread, and each btree lookup/scan still constructs the raw
/// bcachefs-style iterator path while the engine mutex is held.
pub struct ReadTransaction<'engine> {
    engine: &'engine StorageEngine,
    rcu_thread: RcuThread,
    external_registration: bool,
}

impl ReadTransaction<'_> {
    pub fn get(
        &self,
        btree: BtreeId,
        position: KeyPosition,
    ) -> Result<Option<BtreeKey>, EngineError> {
        let mut fs = self.engine.lock_fs()?;
        self.rcu_thread.rscs(|_| ());
        unsafe { get_locked(&mut **fs, btree, position) }
    }

    pub fn scan(&self, btree: BtreeId) -> Result<Vec<BtreeKey>, EngineError> {
        let mut fs = self.engine.lock_fs()?;
        self.rcu_thread.rscs(|_| ());
        unsafe { scan_locked(&mut **fs, btree) }
    }
}

impl Drop for ReadTransaction<'_> {
    fn drop(&mut self) {
        if self.external_registration {
            crate::util::rcu::rcu_external_registration_exit();
        }
    }
}

/* The raw port stores self-references and C-shaped pointers in bch_fs.  The
 * Box address is fixed before initialization and every access is serialized
 * by the enclosing Mutex; that is the ownership boundary which makes the
 * background reclaim thread safe to move between Rust threads. */
struct EngineFs(Box<bch_fs>);

unsafe impl Send for EngineFs {}

impl Deref for EngineFs {
    type Target = bch_fs;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for EngineFs {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

#[derive(Default)]
struct ReclaimWorkerState {
    requested: u64,
    completed: u64,
    running: bool,
    stopping: bool,
    last_error: Option<i32>,
}

struct ReclaimControl {
    state: Mutex<ReclaimWorkerState>,
    wake: Condvar,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl Default for ReclaimControl {
    fn default() -> Self {
        Self {
            state: Mutex::new(ReclaimWorkerState::default()),
            wake: Condvar::new(),
            worker: Mutex::new(None),
        }
    }
}

struct EngineState {
    /*
     * btree nodes and journal state retain raw references to their owning
     * bch_fs, just as the C implementation obtains it with container_of().
     * Keep that owner at one stable heap address before any such references
     * are initialized; moving StorageEngine must never invalidate them.
     */
    fs: Mutex<EngineFs>,
    rcu: Rcu,
    reclaim: Arc<ReclaimControl>,
    discard_inflight: Mutex<(VecDeque<(u64, u64)>, BTreeSet<(u64, u64)>)>,
    open_buckets: Mutex<BTreeSet<(u64, u64)>>,
    rw_devs: Mutex<BTreeSet<u64>>,
}

/// A self-contained btree/transaction/journal storage engine.  Clones share
/// the same durable state and the same single-consumer reclaim worker.
#[derive(Clone)]
pub struct StorageEngine {
    inner: Arc<EngineState>,
}

impl StorageEngine {
    /// Creates a new in-memory engine with a single supported data format.
    pub fn new() -> Result<Self, EngineError> {
        let mut fs = Box::new(bch_fs::default());
        unsafe {
            let ret = crate::sb::io::bch2_sb_realloc(&mut fs.disk_sb, 0);
            if ret != 0 {
                return Err(EngineError::Transaction(ret));
            }
            (*fs.disk_sb.sb).block_size = 1;
            /* 4KB node buffer: BCH_SB_BTREE_NODE_SIZE occupies flags[0]
             * bits 12-27 in units of sectors (bcachefs_format.h:1223,
             * sb/io.rs:256), and the port's btree cache and fake-root
             * recovery tests use the same 8-sector geometry. */
            (*fs.disk_sb.sb).flags[0] = 8 << 12;

            let ret = bch2_fs_btree_cache_init(&mut *fs);
            if ret != 0 {
                crate::sb::io::bch2_free_super(&mut fs.disk_sb);
                return Err(EngineError::Transaction(ret));
            }

            for id in 0..BTREE_ID_NR {
                bch2_btree_root_alloc_fake(&mut *fs, id as u8, 0);
                let root = bch2_btree_id_root_b(&*fs, id);
                if root.is_null() {
                    crate::sb::io::bch2_free_super(&mut fs.disk_sb);
                    return Err(EngineError::Transaction(-12));
                }
                /* The fake allocation is recovery.c's bootstrap mechanism.
                 * A fresh independent engine has no disk root to rewrite, so
                 * it becomes an ordinary writable leaf immediately. */
                clear_btree_node_fake(root);
                clear_btree_node_need_rewrite(root);
            }

            let ret = bch2_journal_replay(&mut *fs);
            if ret != 0 {
                crate::sb::io::bch2_free_super(&mut fs.disk_sb);
                return Err(EngineError::Journal(ret));
            }
        }
        let rcu = Rcu::init();
        let inner = Arc::new(EngineState {
            fs: Mutex::new(EngineFs(fs)),
            rcu,
            reclaim: Arc::new(ReclaimControl::default()),
            discard_inflight: Mutex::new((VecDeque::new(), BTreeSet::new())),
            open_buckets: Mutex::new(BTreeSet::new()),
            rw_devs: Mutex::new(BTreeSet::new()),
        });
        start_reclaim_worker(&inner)?;
        Ok(Self { inner })
    }

    /// Creates a persistent journal/checkpoint device using the engine's
    /// single fixed layout.
    pub fn create_persistent(path: impl AsRef<Path>) -> Result<Self, EngineError> {
        let engine = Self::new()?;
        engine.attach_persistent_journal(path.as_ref(), true)?;
        Ok(engine)
    }

    /// Opens a persistent engine, installs its durable checkpoint base, and
    /// then replays the remaining journal window.
    pub fn open_persistent(path: impl AsRef<Path>) -> Result<Self, EngineError> {
        let engine = Self::new()?;
        engine.attach_persistent_journal(path.as_ref(), false)?;
        Ok(engine)
    }

    pub fn transaction(&self) -> Transaction<'_> {
        Transaction {
            engine: self,
            operations: Vec::new(),
        }
    }

    /// Starts a read-side transaction.  Its operations retain a userland RCU
    /// registration and derive their btree search path through normal raw
    /// iterators; callers must use this object rather than direct mutation
    /// while a read-side context is desired.
    pub fn read_transaction(&self) -> ReadTransaction<'_> {
        let rcu_thread = RcuThread::register(&self.inner.rcu);
        crate::util::rcu::rcu_external_registration_enter();
        ReadTransaction {
            engine: self,
            rcu_thread,
            external_registration: true,
        }
    }

    pub fn put(&self, btree: BtreeId, key: BtreeKey) -> Result<(), EngineError> {
        let mut transaction = self.transaction();
        transaction.put(btree, key);
        transaction.commit()
    }

    /// Commits one put transaction and waits for its journal record to become
    /// durable on the configured journal device.
    pub fn put_sync(&self, btree: BtreeId, key: BtreeKey) -> Result<DurabilityPoint, EngineError> {
        let mut transaction = self.transaction();
        transaction.put(btree, key);
        transaction.commit_sync()
    }

    pub fn delete(&self, btree: BtreeId, position: KeyPosition) -> Result<(), EngineError> {
        let mut transaction = self.transaction();
        transaction.delete(btree, position);
        transaction.commit()
    }

    /// Commits one delete transaction and waits for its journal record to
    /// become durable on the configured journal device.
    pub fn delete_sync(
        &self,
        btree: BtreeId,
        position: KeyPosition,
    ) -> Result<DurabilityPoint, EngineError> {
        let mut transaction = self.transaction();
        transaction.delete(btree, position);
        transaction.commit_sync()
    }

    pub fn get(
        &self,
        btree: BtreeId,
        position: KeyPosition,
    ) -> Result<Option<BtreeKey>, EngineError> {
        self.read_transaction().get(btree, position)
    }

    /// Returns all live keys ordered by their iterator search position.
    pub fn scan(&self, btree: BtreeId) -> Result<Vec<BtreeKey>, EngineError> {
        self.read_transaction().scan(btree)
    }

    /// Checks the root topology and the iterator-visible key order.
    pub fn verify(&self, btree: BtreeId) -> Result<(), EngineError> {
        let keys = self.scan(btree)?;
        if keys
            .windows(2)
            .any(|pair| pair[0].position() >= pair[1].position())
        {
            return Err(EngineError::Transaction(-1));
        }

        let mut fs = self.lock_fs()?;
        unsafe {
            let mut trans = btree_trans::default();
            bch2_trans_init(&mut trans, &mut **fs);
            bch2_trans_begin(&mut trans);
            let root = bch2_btree_id_root_b(&**fs, btree.as_u8() as usize);
            let result = if root.is_null() {
                Err(EngineError::Transaction(-1))
            } else {
                let ret = bch2_btree_node_check_topology(&mut trans, root);
                if ret == 0 {
                    Ok(())
                } else {
                    Err(EngineError::Transaction(ret))
                }
            };
            bch2_trans_put(&mut trans);
            result
        }
    }

    /// Validates the primary physical-pointer set against alloc/backpointer
    /// derived indexes without repairing either tree.
    pub fn verify_derived_state(&self) -> Result<(), EngineError> {
        let mut fs = self.lock_fs()?;
        unsafe { check_extents_to_backpointers(&mut **fs) }
    }

    /// Checks the free alloc buckets against the freespace btree index.
    pub fn verify_bucket_indexes(&self) -> Result<(), EngineError> {
        let mut fs = self.lock_fs()?;
        unsafe {
            let mut alloc_free = BTreeSet::new();
            let mut expected_index = BTreeSet::new();
            let mut expected_need_discard = BTreeSet::new();
            for raw in scan_raw_locked(&mut **fs, 4)? {
                let key = raw.words.as_ptr().cast::<bkey_i>();
                if (*key).k.type_ != KEY_TYPE_alloc_v4 {
                    continue;
                }
                let value = (key as *const u8)
                    .add(core::mem::size_of::<bkey>())
                    .cast::<bch_alloc_v4>();
                let alloc = core::ptr::read_unaligned(value);
                if alloc.data_type == BCH_DATA_FREE {
                    alloc_free.insert(((*key).k.p.inode, (*key).k.p.offset));
                    let indexed = alloc_freespace_pos((*key).k.p, &alloc);
                    expected_index.insert((indexed.inode, indexed.offset));
                } else if alloc.data_type == BCH_DATA_NEED_DISCARD {
                    expected_need_discard.insert(((*key).k.p.inode, (*key).k.p.offset));
                }
            }
            let mut indexed = BTreeSet::new();
            for raw in scan_raw_locked(&mut **fs, BTREE_ID_FREESPACE)? {
                let key = raw.words.as_ptr().cast::<bkey_i>();
                if (*key).k.type_ == crate::btree::bset::KEY_TYPE_set {
                    indexed.insert(((*key).k.p.inode, (*key).k.p.offset));
                }
            }
            if alloc_free.len() != indexed.len()
                || expected_index != indexed
                || indexed
                    .iter()
                    .any(|(dev, offset)| !alloc_free.contains(&(*dev, offset & ((1u64 << 56) - 1))))
            {
                return Err(EngineError::DerivedState(
                    DerivedStateMismatch::FreespaceSet,
                ));
            }
            let mut actual_need_discard = BTreeSet::new();
            for raw in scan_raw_locked(&mut **fs, BTREE_ID_NEED_DISCARD)? {
                let key = raw.words.as_ptr().cast::<bkey_i>();
                if (*key).k.type_ == crate::btree::bset::KEY_TYPE_set {
                    actual_need_discard.insert(((*key).k.p.inode, (*key).k.p.offset));
                }
            }
            if expected_need_discard != actual_need_discard {
                return Err(EngineError::DerivedState(
                    DerivedStateMismatch::NeedDiscardSet,
                ));
            }
            Ok(())
        }
    }

    /// Verifies the guard invariants as one aggregated pass, mirroring
    /// bch2_check_allocations() re-deriving and checking allocation state
    /// in a single recovery pass (check.c:1097-1160).  Two invariants are
    /// enforced, both expressed upstream as skip guards in the discard path:
    /// - no bucket in open_buckets may be FREE: bch2_bucket_is_open_safe()
    ///   skips open buckets (discard.c:344-347, 433-436, 743)
    /// - no FREE bucket may live on a non-rw device: bch2_dev_get_ioref()
    ///   WRITE failing skips the bucket (discard.c:349-357, 654, 871)
    pub fn verify_guard_invariants(&self) -> Result<(), EngineError> {
        let mut fs = self.lock_fs()?;
        unsafe {
            if fs.disk_sb.sb.is_null() {
                return Err(EngineError::Transaction(-1));
            }
            let open_buckets = self
                .inner
                .open_buckets
                .lock()
                .map_err(|_| EngineError::Poisoned)?;
            let rw_devs = self
                .inner
                .rw_devs
                .lock()
                .map_err(|_| EngineError::Poisoned)?;
            for raw in scan_raw_locked(&mut **fs, 4)? {
                let key = raw.words.as_ptr().cast::<bkey_i>();
                if (*key).k.type_ != KEY_TYPE_alloc_v4 {
                    continue;
                }
                let value = (key as *const u8)
                    .add(core::mem::size_of::<bkey>())
                    .cast::<bch_alloc_v4>();
                let alloc = core::ptr::read_unaligned(value);
                if alloc.data_type != BCH_DATA_FREE {
                    continue;
                }
                let pos = ((*key).k.p.inode, (*key).k.p.offset);
                if open_buckets.contains(&pos) {
                    return Err(EngineError::DerivedState(
                        DerivedStateMismatch::OpenBucketFree,
                    ));
                }
                if !rw_devs.contains(&pos.0) {
                    return Err(EngineError::DerivedState(
                        DerivedStateMismatch::NotRwBucketFree,
                    ));
                }
            }
            Ok(())
        }
    }

    /// Runs every consistency check in dependency order, mirroring the
    /// recovery pass driver executing each pass in sequence while keeping
    /// the first error (`__bch2_run_explicit_recovery_pass(...) ?: ret`,
    /// recovery.c:68-98): every check runs, the first error wins.  Order:
    /// topology (verify) -> derived pointers (verify_derived_state) ->
    /// bucket indexes (verify_bucket_indexes) -> guard invariants
    /// (verify_guard_invariants).
    pub fn verify_all(&self) -> Result<(), EngineError> {
        let live_btrees = {
            let fs = self.lock_fs()?;
            let mut ids = Vec::new();
            unsafe {
                for id in 0..BTREE_ID_NR {
                    if !bch2_btree_id_root_b(&**fs, id).is_null() {
                        ids.push(id);
                    }
                }
            }
            ids
        };
        let mut first_err = None;
        for id in live_btrees {
            if let Err(err) = self.verify(BtreeId::new(id as u8).expect("id in BTREE_ID_NR")) {
                first_err.get_or_insert(err);
            }
        }
        for check in [
            Self::verify_derived_state,
            Self::verify_bucket_indexes,
            Self::verify_guard_invariants,
        ] {
            if let Err(err) = check(self) {
                first_err.get_or_insert(err);
            }
        }
        match first_err {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    /// Queries the number of currently open buckets, the engine-local
    /// counterpart of the bch2_open_buckets_stop() close-all-on-umount
    /// invariant (fs.c:324, foreground.c:1171-1230): a caller may check
    /// before dropping the engine that no open bucket would leak.
    pub fn open_bucket_count(&self) -> Result<usize, EngineError> {
        Ok(self
            .inner
            .open_buckets
            .lock()
            .map_err(|_| EngineError::Poisoned)?
            .len())
    }

    /// Queries whether the discard queue is empty.  The worker returns Ok
    /// only when it has drained the queue, matching the fast_work while
    /// loop (discard.c:605-633); a queue left non-empty after an EAGAIN
    /// rotation is legal (T0191), so this is a query, not an auto-check.
    pub fn discard_queue_empty(&self) -> Result<bool, EngineError> {
        Ok(self
            .inner
            .discard_inflight
            .lock()
            .map_err(|_| EngineError::Poisoned)?
            .0
            .is_empty())
    }

    /// 测试设施：将 bucket_offset 初始化为 free 桶（alloc 记录 + freespace
    /// 位）。语义锚点：bcachefs 的 alloc 键更新与 freespace 位维护
    /// （fs/alloc/background.c:1113 alloc_freespace_pos +
    /// bch2_btree_bit_mod）——先写 alloc_v4 记录（trigger_update_value
    /// 触发 alloc 触发器），再置 freespace 位。仅供属性测试初始化桶
    /// 状态，非运行时路径（T0202 组合测试从 mod tests 提升，逻辑零变化）。
    pub fn add_free_bucket(&self, bucket_offset: u64) {
        unsafe {
            let mut fs = self.lock_fs().unwrap();
            let position = crate::btree::bkey::POS(0, bucket_offset);
            let alloc = bch_alloc_v4::default();
            let mut trans = btree_trans::default();
            bch2_trans_init(&mut trans, &mut **fs);
            loop {
                bch2_trans_begin(&mut trans);
                let ret = trigger_update_value(
                    &mut trans,
                    4,
                    position,
                    KEY_TYPE_alloc_v4,
                    (&alloc as *const bch_alloc_v4).cast(),
                    core::mem::size_of::<bch_alloc_v4>(),
                );
                let ret = if ret == 0 {
                    bch2_btree_bit_mod(
                        &mut trans,
                        BTREE_ID_FREESPACE,
                        alloc_freespace_pos(position, &alloc),
                        true,
                    )
                } else {
                    ret
                };
                let ret = if ret == 0 {
                    bch2_trans_commit(&mut trans)
                } else {
                    ret
                };
                if ret == -12 && trans.realloc_bytes_required != 0 {
                    continue;
                }
                assert_eq!(ret, 0);
                break;
            }
            bch2_trans_put(&mut trans);
        }
    }

    /// Selects the first free alloc bucket for a device and atomically marks
    /// it as btree-owned, matching foreground.c's free-bucket candidate rule.
    pub fn allocate_bucket(&self, dev: u64) -> Result<KeyPosition, EngineError> {
        let mut fs = self.lock_fs()?;
        unsafe {
            if fs.disk_sb.sb.is_null() || dev >= (*fs.disk_sb.sb).nr_devices as u64 {
                return Err(EngineError::Transaction(-1));
            }
            let member = crate::sb::io::bch2_sb_member_get(fs.disk_sb.sb, dev as usize);
            if member.bucket_size == 0 || !crate::sb::bch2_member_alive(&member) {
                return Err(EngineError::Transaction(-1));
            }
            if !self
                .inner
                .rw_devs
                .lock()
                .map_err(|_| EngineError::Poisoned)?
                .contains(&dev)
            {
                return Err(EngineError::Transaction(-1));
            }
            let mut freespace_candidates = BTreeSet::new();
            for raw in scan_raw_locked(&mut **fs, BTREE_ID_FREESPACE)? {
                let key = raw.words.as_ptr().cast::<bkey_i>();
                if (*key).k.type_ == crate::btree::bset::KEY_TYPE_set && (*key).k.p.inode == dev {
                    freespace_candidates.insert((*key).k.p.offset & ((1u64 << 56) - 1));
                }
            }
            for raw in scan_raw_locked(&mut **fs, 4)? {
                let key = raw.words.as_ptr().cast::<bkey_i>();
                if (*key).k.type_ != KEY_TYPE_alloc_v4
                    || (*key).k.p.inode != dev
                    || raw.words.len() < BKEY_U64S as usize + 1
                {
                    continue;
                }
                let bucket_offset = (*key).k.p.offset;
                if bucket_offset < member.first_bucket as u64 || bucket_offset >= member.nbuckets {
                    continue;
                }
                if !freespace_candidates.is_empty()
                    && !freespace_candidates.contains(&bucket_offset)
                {
                    continue;
                }
                let value = (key as *mut u8)
                    .add(core::mem::size_of::<bkey>())
                    .cast::<bch_alloc_v4>();
                let mut alloc = core::ptr::read_unaligned(value);
                if alloc.data_type != BCH_DATA_FREE {
                    continue;
                }
                let old_alloc = alloc;
                alloc.data_type = BCH_DATA_BTREE;
                let mut trans = btree_trans::default();
                bch2_trans_init(&mut trans, &mut **fs);
                let ret = loop {
                    bch2_trans_begin(&mut trans);
                    let ret = trigger_update_value(
                        &mut trans,
                        4,
                        (*key).k.p,
                        KEY_TYPE_alloc_v4,
                        (&alloc as *const bch_alloc_v4).cast(),
                        core::mem::size_of::<bch_alloc_v4>(),
                    );
                    let ret = if ret == 0 {
                        bch2_btree_bit_mod(
                            &mut trans,
                            BTREE_ID_FREESPACE,
                            alloc_freespace_pos((*key).k.p, &old_alloc),
                            false,
                        )
                    } else {
                        ret
                    };
                    let ret = if ret == 0 {
                        bch2_trans_commit(&mut trans)
                    } else {
                        ret
                    };
                    if ret == -4 || (ret == -12 && trans.realloc_bytes_required != 0) {
                        continue;
                    }
                    break ret;
                };
                bch2_trans_put(&mut trans);
                if ret != 0 {
                    return Err(EngineError::Transaction(ret));
                }
                return Ok(KeyPosition::new((*key).k.p.inode, (*key).k.p.offset, 0));
            }
            Err(EngineError::Transaction(-28))
        }
    }

    /// Marks a bucket as open, mirroring an in-progress write claim in the
    /// open_buckets hash (foreground.h:274-296).  While open, the bucket is
    /// protected from reclamation: reclaim and discard both refuse it, like
    /// bch2_bucket_is_open_safe() skipping open buckets in the discard path
    /// (discard.c:344-347, 433-436).
    pub fn open_bucket(&self, position: KeyPosition) -> Result<(), EngineError> {
        self.inner
            .open_buckets
            .lock()
            .map_err(|_| EngineError::Poisoned)?
            .insert((position.inode, position.offset));
        Ok(())
    }

    /// Releases the open claim on a bucket, mirroring bch2_open_bucket_put().
    pub fn close_open_bucket(&self, position: KeyPosition) -> Result<(), EngineError> {
        self.inner
            .open_buckets
            .lock()
            .map_err(|_| EngineError::Poisoned)?
            .remove(&(position.inode, position.offset));
        Ok(())
    }

    /// Sets the writable state of a device, mirroring
    /// bch2_dev_allocator_set_rw()'s rw_devs bitmap (background.c:1650-1667).
    /// A non-rw device refuses allocation and free transitions, like
    /// bch2_dev_get_ioref(WRITE) failing in the discard path (discard.c:357-365).
    pub fn set_device_rw(&self, dev: u64, rw: bool) -> Result<(), EngineError> {
        /* lock order open_buckets -> rw_devs, matching reclaim_bucket and
         * discard_bucket, so a concurrent reclaim can never deadlock */
        let open_buckets = self
            .inner
            .open_buckets
            .lock()
            .map_err(|_| EngineError::Poisoned)?;
        let mut rw_devs = self
            .inner
            .rw_devs
            .lock()
            .map_err(|_| EngineError::Poisoned)?;
        if rw {
            rw_devs.insert(dev);
            return Ok(());
        }
        /* bch2_dev_allocator_remove() first marks the device ro, then stops
         * its open buckets and waits for open write points to drain
         * (background.c:1690-1722, bch2_dev_has_open_write_point
         * background.c:1650-1662).  Without concurrent I/O the wait is
         * expressed as a refusal while any open bucket remains. */
        if open_buckets.iter().any(|&(d, _)| d == dev) {
            return Err(EngineError::Transaction(-16));
        }
        rw_devs.remove(&dev);
        Ok(())
    }

    /// Releases a bucket only after its reverse-reference btree has no live
    /// entries; the transition first records need_discard and then free.
    pub fn reclaim_bucket(&self, position: KeyPosition) -> Result<(), EngineError> {
        let mut fs = self.lock_fs()?;
        unsafe {
            if fs.disk_sb.sb.is_null() || position.inode >= (*fs.disk_sb.sb).nr_devices as u64 {
                return Err(EngineError::Transaction(-1));
            }
            let backpointers = scan_raw_locked(&mut **fs, 8)?;
            let member = crate::sb::io::bch2_sb_member_get(fs.disk_sb.sb, position.inode as usize);
            if member.bucket_size == 0 {
                return Err(EngineError::Transaction(-1));
            }
            if position.offset < member.first_bucket as u64 || position.offset >= member.nbuckets {
                return Err(EngineError::Transaction(-1));
            }
            if self
                .inner
                .open_buckets
                .lock()
                .map_err(|_| EngineError::Poisoned)?
                .contains(&(position.inode, position.offset))
            {
                return Err(EngineError::Transaction(-16));
            }
            if !self
                .inner
                .rw_devs
                .lock()
                .map_err(|_| EngineError::Poisoned)?
                .contains(&position.inode)
            {
                return Err(EngineError::Transaction(-16));
            }
            let start = position.offset.saturating_mul(member.bucket_size as u64);
            let end = start.saturating_add(member.bucket_size as u64);
            for raw in backpointers {
                let key = raw.words.as_ptr().cast::<bkey_i>();
                if (*key).k.type_ == KEY_TYPE_backpointer
                    && (*key).k.p.inode == position.inode
                    && (*key).k.p.offset >= start
                    && (*key).k.p.offset < end
                {
                    return Err(EngineError::Transaction(-16));
                }
            }
            let alloc_keys = scan_raw_locked(&mut **fs, 4)?;
            for raw in alloc_keys {
                let key = raw.words.as_ptr().cast::<bkey_i>();
                if (*key).k.type_ != KEY_TYPE_alloc_v4 || (*key).k.p != position.raw() {
                    continue;
                }
                let value = (key as *mut u8)
                    .add(core::mem::size_of::<bkey>())
                    .cast::<bch_alloc_v4>();
                let mut alloc = core::ptr::read_unaligned(value);
                if alloc.dirty_sectors != 0 || alloc.cached_sectors != 0 {
                    return Err(EngineError::Transaction(-16));
                }
                let old_alloc = alloc;
                if alloc.data_type == BCH_DATA_NEED_DISCARD
                    && alloc.journal_seq_empty > fs.journal.last_seq_ondisk.load(Ordering::Acquire)
                {
                    return Err(EngineError::Transaction(-11));
                }
                if alloc.data_type != BCH_DATA_NEED_DISCARD {
                    /* background.c first moves an empty bucket into
                     * need_discard; discard.c performs the later free
                     * transition after the device-side discard boundary. */
                    alloc.data_type = BCH_DATA_NEED_DISCARD;
                    if alloc.oldest_gen == alloc.gen {
                        alloc.oldest_gen = alloc.oldest_gen.wrapping_add(1);
                    }
                    alloc.gen = alloc.gen.wrapping_add(1);
                } else {
                    alloc.data_type = BCH_DATA_FREE;
                }
                let mut trans = btree_trans::default();
                bch2_trans_init(&mut trans, &mut **fs);
                let ret = loop {
                    bch2_trans_begin(&mut trans);
                    let ret = trigger_update_value(
                        &mut trans,
                        4,
                        (*key).k.p,
                        KEY_TYPE_alloc_v4,
                        (&alloc as *const bch_alloc_v4).cast(),
                        core::mem::size_of::<bch_alloc_v4>(),
                    );
                    let ret = if ret == 0 {
                        if alloc.data_type == BCH_DATA_FREE {
                            let ret = bch2_btree_bit_mod(
                                &mut trans,
                                BTREE_ID_FREESPACE,
                                alloc_freespace_pos((*key).k.p, &alloc),
                                true,
                            );
                            if ret == 0 {
                                bch2_btree_bit_mod(
                                    &mut trans,
                                    BTREE_ID_NEED_DISCARD,
                                    (*key).k.p,
                                    false,
                                )
                            } else {
                                ret
                            }
                        } else {
                            let ret = bch2_btree_bit_mod(
                                &mut trans,
                                BTREE_ID_FREESPACE,
                                alloc_freespace_pos((*key).k.p, &old_alloc),
                                false,
                            );
                            if ret == 0 {
                                bch2_btree_bit_mod(
                                    &mut trans,
                                    BTREE_ID_NEED_DISCARD,
                                    (*key).k.p,
                                    true,
                                )
                            } else {
                                ret
                            }
                        }
                    } else {
                        ret
                    };
                    let ret = if ret == 0 {
                        if fs
                            .fault_inject_discard_restarts
                            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                                count.checked_sub(1)
                            })
                            .is_ok()
                        {
                            /* T0199: per-bucket restart injection at the
                             * discard worker transaction commit boundary,
                             * the trans_maybe_inject_restart position
                             * (commit.c:1390); -4 rides the existing
                             * bch2_trans_begin retry loop below. */
                            -4
                        } else {
                            bch2_trans_commit(&mut trans)
                        }
                    } else {
                        ret
                    };
                    if ret == -4 || (ret == -12 && trans.realloc_bytes_required != 0) {
                        continue;
                    }
                    break ret;
                };
                bch2_trans_put(&mut trans);
                return if ret == 0 {
                    Ok(())
                } else {
                    Err(EngineError::Transaction(ret))
                };
            }
            Err(EngineError::Transaction(-2))
        }
    }

    /// Completes the controlled discard boundary for a bucket already marked
    /// need_discard.  The caller-facing error is EAGAIN-like until the
    /// journal boundary is durable; the actual state/index transition remains
    /// the single transaction performed by reclaim_bucket().
    pub fn discard_bucket(&self, position: KeyPosition) -> Result<(), EngineError> {
        let mut fs = self.lock_fs()?;
        unsafe {
            if fs.disk_sb.sb.is_null() || position.inode >= (*fs.disk_sb.sb).nr_devices as u64 {
                return Err(EngineError::Transaction(-1));
            }
            let mut alloc_state = None;
            for raw in scan_raw_locked(&mut **fs, 4)? {
                let key = raw.words.as_ptr().cast::<bkey_i>();
                if (*key).k.type_ != KEY_TYPE_alloc_v4 || (*key).k.p != position.raw() {
                    continue;
                }
                let value = (key as *const u8)
                    .add(core::mem::size_of::<bkey>())
                    .cast::<bch_alloc_v4>();
                alloc_state = Some(core::ptr::read_unaligned(value));
                break;
            }
            let alloc = alloc_state.ok_or(EngineError::Transaction(-2))?;
            if alloc.data_type != BCH_DATA_NEED_DISCARD
                || alloc.journal_seq_empty > fs.journal.last_seq_ondisk.load(Ordering::Acquire)
            {
                return Err(EngineError::Transaction(-11));
            }
        }
        if self
            .inner
            .open_buckets
            .lock()
            .map_err(|_| EngineError::Poisoned)?
            .contains(&(position.inode, position.offset))
        {
            return Err(EngineError::Transaction(-11));
        }
        if !self
            .inner
            .rw_devs
            .lock()
            .map_err(|_| EngineError::Poisoned)?
            .contains(&position.inode)
        {
            return Err(EngineError::Transaction(-11));
        }
        drop(fs);
        self.reclaim_bucket(position)
    }

    /// Queues one bucket for the engine-local discard worker.  A duplicate
    /// in-flight request is rejected with the discard.c EEXIST boundary.
    /// The FIFO order of the queue is the submission order, matching the
    /// per-device darray in bch2_fast_discard_bucket_add (discard.c:643).
    pub fn queue_discard_bucket(&self, position: KeyPosition) -> Result<(), EngineError> {
        let mut inflight = self
            .inner
            .discard_inflight
            .lock()
            .map_err(|_| EngineError::Poisoned)?;
        if !inflight.1.insert((position.inode, position.offset)) {
            return Err(EngineError::Transaction(-17));
        }
        inflight.0.push_back((position.inode, position.offset));
        Ok(())
    }

    /// Runs one queued discard.  EAGAIN keeps the request queued for a later
    pub fn run_discard_worker_once(&self) -> Result<(), EngineError> {
        let position = {
            let mut inflight = self
                .inner
                .discard_inflight
                .lock()
                .map_err(|_| EngineError::Poisoned)?;
            inflight
                .0
                .pop_front()
                .map(|(inode, offset)| KeyPosition::new(inode, offset, 0))
                .ok_or(EngineError::Transaction(-11))?
        };
        match self.discard_bucket(position) {
            Err(EngineError::Transaction(-11)) => {
                self.inner
                    .discard_inflight
                    .lock()
                    .map_err(|_| EngineError::Poisoned)?
                    .0
                    .push_back((position.inode, position.offset));
                Err(EngineError::Transaction(-11))
            }
            result => {
                self.inner
                    .discard_inflight
                    .lock()
                    .map_err(|_| EngineError::Poisoned)?
                    .1
                    .remove(&(position.inode, position.offset));
                result
            }
        }
    }

    /// Runs one worker pass over the whole discard queue: every queued bucket
    /// is attempted once in FIFO order.  A bucket not yet ready (EAGAIN) is
    /// rotated to the queue tail instead of blocking the pass, mirroring the
    /// main-path advance-and-continue semantics for a bucket that cannot be
    /// completed right now (discard.c:488-491); a terminal error aborts the
    /// pass like the fastpath break (discard.c:631-633).  The pass keeps
    /// draining until the queue is empty, matching the fast_work while loop
    /// (discard.c:605-633), so buckets queued by a concurrent producer while
    /// the pass runs are picked up in the same pass.  Returns EAGAIN when
    /// buckets remain queued, Ok(()) once the queue is fully drained.
    pub fn run_discard_worker(&self) -> Result<(), EngineError> {
        loop {
            let round = {
                let inflight = self
                    .inner
                    .discard_inflight
                    .lock()
                    .map_err(|_| EngineError::Poisoned)?;
                inflight.0.len()
            };
            if round == 0 {
                return Ok(());
            }
            let mut deferred = false;
            for _ in 0..round {
                let position = {
                    let mut inflight = self
                        .inner
                        .discard_inflight
                        .lock()
                        .map_err(|_| EngineError::Poisoned)?;
                    match inflight.0.pop_front() {
                        Some((inode, offset)) => KeyPosition::new(inode, offset, 0),
                        None => break,
                    }
                };
                match self.discard_bucket(position) {
                    Err(EngineError::Transaction(-11)) => {
                        self.inner
                            .discard_inflight
                            .lock()
                            .map_err(|_| EngineError::Poisoned)?
                            .0
                            .push_back((position.inode, position.offset));
                        deferred = true;
                    }
                    result => {
                        self.inner
                            .discard_inflight
                            .lock()
                            .map_err(|_| EngineError::Poisoned)?
                            .1
                            .remove(&(position.inode, position.offset));
                        result?;
                    }
                }
            }
            if deferred {
                return Err(EngineError::Transaction(-11));
            }
        }
    }

    /// Re-discovers persisted need_discard entries after a process-style
    /// restart and queues them without duplicating already discovered work.
    pub fn discover_discard_buckets(&self) -> Result<usize, EngineError> {
        let mut fs = self.lock_fs()?;
        let mut positions = Vec::new();
        unsafe {
            for raw in scan_raw_locked(&mut **fs, BTREE_ID_NEED_DISCARD)? {
                let key = raw.words.as_ptr().cast::<bkey_i>();
                if (*key).k.type_ == crate::btree::bset::KEY_TYPE_set {
                    positions.push(((*key).k.p.inode, (*key).k.p.offset));
                }
            }
        }
        drop(fs);
        let mut inflight = self
            .inner
            .discard_inflight
            .lock()
            .map_err(|_| EngineError::Poisoned)?;
        let mut inserted = 0;
        for position in positions {
            if inflight.1.insert(position) {
                inflight.0.push_back(position);
                inserted += 1;
            }
        }
        Ok(inserted)
    }

    /// Publishes the current journal buffer.  Only records returned by
    /// `durable_journal()` after this succeeds can survive a crash.
    pub fn flush_journal(&self) -> Result<(), EngineError> {
        let result = {
            let fs = self.lock_fs()?;
            let ret = bch2_journal_flush(&fs.journal);
            if ret == 0 {
                Ok(())
            } else {
                Err(EngineError::Journal(ret))
            }
        };
        if result.is_ok() {
            self.schedule_reclaim_if_needed()?;
        }
        result
    }

    /// Flushes the journal and reports the exact persistence boundary reached
    /// by this call.  It does not force checkpoint compaction.
    pub fn sync(&self) -> Result<DurabilityPoint, EngineError> {
        self.flush_journal()?;
        self.durability_point()
    }

    /// Rewrites the btree node at `position`/`level` with a fresh
    /// allocation: format recomputation (falling back to the old format
    /// when the new one does not fit), key sequence +1, full key relocation,
    /// and a parent pivot replacement (or root replacement).  This is the
    /// subvol counterpart of `bch2_btree_node_rewrite_pos`
    /// (interior.c:3373): `level == 0` is a caller bug (BUG_ON(!level)).
    pub fn rewrite_node(
        &self,
        btree: BtreeId,
        level: u8,
        position: KeyPosition,
    ) -> Result<(), EngineError> {
        assert!(level != 0, "rewrite_node requires level >= 1");
        let mut fs = self.lock_fs()?;
        unsafe { rewrite_node_locked(&mut **fs, btree, level, position) }
    }

    /// Rewrites the btree node whose pointer key hash matches `key`
    /// (interior.c:3345 `bch2_btree_node_rewrite_key`): the node is located
    /// at `key.position` and rewritten only when its `btree_ptr_v2.seq`
    /// matches, otherwise `Transaction(-2)` (ENOENT) is returned.
    /// `level` is the target node's level (leaf == 0, unlike `rewrite_node`
    /// whose level counts the pointer key level).
    pub fn rewrite_node_key(
        &self,
        btree: BtreeId,
        level: u8,
        key: &BtreeKey,
    ) -> Result<(), EngineError> {
        let mut fs = self.lock_fs()?;
        unsafe { rewrite_node_key_locked(&mut **fs, btree, level, key) }
    }

    /// Reclaims the journal through the durable flush path: flush any pending
    /// journal entry, force node pins to flush, then advance `last_seq` once
    /// the records they cover are durable.  This is the engine counterpart to
    /// bcachefs journal reclaim (reclaim.c): data is durable before
    /// `last_seq` advances and makes earlier journal entries reclaimable.
    /// This is the direct-reclaim path used when a caller cannot wait for the
    /// background single consumer.
    pub fn reclaim_journal(&self) -> Result<(), EngineError> {
        let mut fs = self.lock_fs()?;
        unsafe { self.checkpoint_locked(&mut **fs) }
    }

    /// Kicks the background single-consumer reclaimer and returns its request
    /// sequence.  Multiple callers are coalesced by that worker.
    pub fn request_reclaim(&self) -> Result<u64, EngineError> {
        self.request_reclaim_inner()
    }

    /// Waits for all reclaim work requested before this call to complete.
    /// Completion includes an error result in `last_error`, which callers can
    /// inspect without losing worker liveness.
    pub fn wait_for_reclaim(&self, timeout: Duration) -> Result<ReclaimStatus, EngineError> {
        let control = &self.inner.reclaim;
        let started = Instant::now();
        let mut state = control.state.lock().map_err(|_| EngineError::Poisoned)?;
        let target = state.requested;
        while state.completed < target {
            let Some(remaining) = timeout.checked_sub(started.elapsed()) else {
                return Err(EngineError::ReclaimTimeout);
            };
            let (next, timed_out) = control
                .wake
                .wait_timeout(state, remaining)
                .map_err(|_| EngineError::Poisoned)?;
            state = next;
            if timed_out.timed_out() && state.completed < target {
                return Err(EngineError::ReclaimTimeout);
            }
        }
        Ok(reclaim_status(&state))
    }

    pub fn reclaim_status(&self) -> Result<ReclaimStatus, EngineError> {
        let state = self
            .inner
            .reclaim
            .state
            .lock()
            .map_err(|_| EngineError::Poisoned)?;
        Ok(reclaim_status(&state))
    }

    /// Reports the durable journal boundary without issuing I/O.
    pub fn durability_point(&self) -> Result<DurabilityPoint, EngineError> {
        let fs = self.lock_fs()?;
        Ok(DurabilityPoint {
            journal_sequence: fs.journal.seq.load(Ordering::Acquire),
            journal_sequence_ondisk: fs.journal.seq_ondisk.load(Ordering::Acquire),
        })
    }

    pub fn metrics(&self) -> Result<EngineMetrics, EngineError> {
        let fs = self.lock_fs()?;
        let journal_records = fs
            .journal
            .closed
            .lock()
            .map_err(|_| EngineError::Poisoned)?
            .len();
        let reclaim = self.reclaim_status()?;
        Ok(EngineMetrics {
            journal_sequence: fs.journal.seq.load(Ordering::Acquire),
            journal_sequence_ondisk: fs.journal.seq_ondisk.load(Ordering::Acquire),
            journal_last_sequence: fs.journal.last_seq.load(Ordering::Acquire),
            journal_last_sequence_ondisk: fs.journal.last_seq_ondisk.load(Ordering::Acquire),
            journal_records,
            reclaim,
        })
    }

    pub fn durable_journal(&self) -> Result<JournalSnapshot, EngineError> {
        let fs = self.lock_fs()?;
        let records = fs.journal.closed.lock().unwrap().clone();
        let next_sequence = fs.journal.seq.load(Ordering::Acquire);
        Ok(JournalSnapshot {
            format_version: STORAGE_FORMAT_VERSION,
            records,
            next_sequence,
        })
    }

    /// Reconstructs an engine from a crash image captured by
    /// `durable_journal()`.
    pub fn recover(snapshot: &JournalSnapshot) -> Result<Self, EngineError> {
        Self::recover_with_fault(snapshot, None)
    }

    pub fn recover_with_fault(
        snapshot: &JournalSnapshot,
        fault: Option<RecoveryFaultPoint>,
    ) -> Result<Self, EngineError> {
        if snapshot.format_version != STORAGE_FORMAT_VERSION {
            return Err(EngineError::UnsupportedFormatVersion(
                snapshot.format_version,
            ));
        }

        let engine = Self::new()?;
        let mut fs = engine.lock_fs()?;
        unsafe {
            let ret = bch2_journal_restore_for_replay(
                &fs.journal,
                snapshot.records.clone(),
                snapshot.next_sequence,
            );
            if ret != 0 {
                return Err(EngineError::Journal(ret));
            }
            let ret = bch2_journal_replay(&mut **fs);
            if ret != 0 {
                return Err(EngineError::Journal(ret));
            }
            if fault == Some(RecoveryFaultPoint::AfterJournalReplay) {
                return Err(EngineError::Journal(-4));
            }
            if fault == Some(RecoveryFaultPoint::DuringDerivedRebuild) {
                return Err(EngineError::Journal(-4));
            }
            rebuild_derived_state(&mut **fs, fault)?;
            if fault == Some(RecoveryFaultPoint::BeforePublication) {
                return Err(EngineError::Journal(-4));
            }
            check_extents_to_backpointers(&mut **fs)?;
        }
        drop(fs);
        Ok(engine)
    }

    pub fn inject_fault(&self, point: FaultPoint, count: u32) -> Result<(), EngineError> {
        let fs = self.lock_fs()?;
        match point {
            FaultPoint::TransactionRestart => fs
                .fault_inject_transaction_restarts
                .store(count, Ordering::Release),
            FaultPoint::JournalWrite => fs
                .journal
                .fault_inject_write_error
                .store(count, Ordering::Release),
            FaultPoint::DiscardCommitRestart => fs
                .fault_inject_discard_restarts
                .store(count, Ordering::Release),
        }
        Ok(())
    }

    fn lock_fs(&self) -> Result<MutexGuard<'_, EngineFs>, EngineError> {
        self.inner.fs.lock().map_err(|_| EngineError::Poisoned)
    }

    unsafe fn checkpoint_locked(&self, fs: &mut bch_fs) -> Result<(), EngineError> {
        /* The journal record that precedes the reclaim boundary is the
         * write-ahead guarantee: do not advance last_seq past updates that
         * have not reached this successful flush.  When the current entry is
         * already empty, avoid consuming the last physical journal slot:
         * reclaim.c may first free the stable prefix and then write the
         * following entry. */
        if journal_state_offset(fs.journal.reservations.load(Ordering::Acquire)) != 0 {
            let ret = bch2_journal_flush(&fs.journal);
            if ret != 0 {
                return Err(EngineError::Journal(ret));
            }
        }
        let sequence = fs.journal.seq_ondisk.load(Ordering::Acquire);
        if sequence == 0 {
            return Err(EngineError::Journal(-1));
        }

        /* This is reclaim.c's journal_flush_pins() pass: every node pin up to
         * the flushed sequence is written back (the pin callbacks take the
         * node write locks), and bch2_journal_update_last_seq() then releases
         * the covered records.  The `last_seq` bound below is that completed-
         * write boundary; advancing last_seq_ondisk past it is what reclaims
         * the old journal window.  In-memory engines have no device, so node
         * writeback cannot complete and the boundary never advances. */
        if !fs.disk_sb.s_bdev_file.is_null() {
            bch2_journal_flush_pins(&fs.journal, sequence + 1);
            bch2_journal_update_last_seq(&fs.journal);
            let last_seq = fs.journal.last_seq.load(Ordering::Acquire);
            let ret = bch2_journal_update_last_seq_ondisk(&fs.journal, last_seq);
            if ret != 0 {
                return Err(EngineError::Journal(ret));
            }
            /* Keep the in-memory record mirror equal to the retained
             * on-disk window: records at or below the advanced boundary are
             * reclaimed.  A device-less engine keeps every record instead,
             * because its journal mirror is the only durable source from
             * which recovery can rebuild the btree. */
            fs.journal
                .closed
                .lock()
                .unwrap()
                .retain(|record| record.get(3).is_some_and(|seq| *seq > sequence));
        }

        /* Publish the advanced last_seq and the current root set in a
         * following empty jset (journal.c's __bch2_journal_meta()): every
         * written entry repeats all btree roots, so recovery binds to the
         * newest root set before replaying the still-retained window. */
        let ret = bch2_journal_flush(&fs.journal);
        if ret != 0 {
            return Err(EngineError::Journal(ret));
        }
        Ok(())
    }

    fn commit_operations(&self, operations: &[TransactionOperation]) -> Result<(), EngineError> {
        if operations.is_empty() {
            return Ok(());
        }

        /* bch2_journal_res_get() enters direct reclaim when a reservation
         * cannot proceed.  The engine's reclaim implementation is a complete
         * checkpoint, so retry the untouched transaction only after that
         * durable base/pin-release cycle succeeds. */
        loop {
            match self.commit_operations_once(operations) {
                Err(EngineError::Transaction(-9)) => self.reclaim_journal()?,
                Ok(()) => {
                    self.schedule_reclaim_if_needed()?;
                    return Ok(());
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn commit_operations_once(
        &self,
        operations: &[TransactionOperation],
    ) -> Result<(), EngineError> {
        let mut fs = self.lock_fs()?;
        unsafe {
            let mut trans = btree_trans::default();
            bch2_trans_init(&mut trans, &mut **fs);
            bch2_trans_begin(&mut trans);

            let result = loop {
                /* Key buffers must remain valid until raw commit consumes the
                 * staged update entries.  Vec relocation moves only each Vec
                 * header, never its separately allocated key buffer. */
                let mut staged_keys = Vec::with_capacity(operations.len());
                let mut ret = 0;
                crate::rewrite_log_debug!(
                    "transaction round begin ops={} restarted={}",
                    operations.len(),
                    trans.restarted
                );
                for operation in operations {
                    let (btree, position, deleted, value) = match operation {
                        TransactionOperation::Put { btree, key } => {
                            (*btree, key.position(), false, key.value())
                        }
                        TransactionOperation::Delete { btree, position } => {
                            (*btree, *position, true, &[] as &[u64])
                        }
                    };
                    crate::rewrite_log_debug!(
                        "transaction op inode={} offset={} snap={} deleted={}",
                        position.inode,
                        position.offset,
                        position.snapshot,
                        deleted
                    );
                    staged_keys.push(encode_key(position, value, deleted));
                    let raw = staged_keys
                        .last_mut()
                        .expect("staged key was just pushed")
                        .as_mut_ptr()
                        .cast::<bkey_i>();

                    let mut iter = btree_iter::default();
                    bch2_trans_iter_init(
                        &mut trans,
                        &mut iter,
                        btree.as_u8(),
                        position.raw(),
                        BTREE_ITER_intent | BTREE_ITER_not_extents,
                    );
                    ret = bch2_btree_iter_traverse(&mut iter);
                    if ret == 0 {
                        ret = bch2_trans_update(&mut trans, &mut iter, raw, 0);
                    }
                    bch2_trans_iter_exit(&mut iter);
                    crate::rewrite_log_debug!("transaction op staged ret={ret}");
                    if ret != 0 {
                        break;
                    }
                }
                if ret == 0 {
                    ret = bch2_trans_commit(&mut trans);
                    crate::rewrite_log_debug!("transaction commit ret={ret}");
                }
                if ret != 0 && ret != -4 {
                    crate::rewrite_log_error!(
                        "transaction failed ret={ret} restarted={} req={} mem_bytes={} nr_updates={}",
                        trans.restarted,
                        trans.realloc_bytes_required,
                        trans.mem_bytes,
                        trans.nr_updates
                    );
                }

                /* 对齐 commit.c:1319-1320：ENOMEM 与 transaction_restart 同级
                 * 均纳入重试。restart 必须由 trans_begin 消费
                 * realloc_bytes_required 扩容后才能成功；真 OOM（首次分配
                 * 失败，restarted 未设置）保持 -12 硬失败，避免无限重试。 */
                if ret == -4 || (ret == -12 && trans.restarted != 0) {
                    crate::rewrite_log_debug!(
                        "transaction restart nr_updates={} restarted={} req={}",
                        trans.nr_updates,
                        trans.restarted,
                        trans.realloc_bytes_required
                    );
                    /* The local commit/replay loops begin a fresh transaction
                     * before retraversing every iterator path.  Do this while
                     * the old key buffers are still alive. */
                    bch2_trans_begin(&mut trans);
                    continue;
                }
                break ret;
            };

            bch2_trans_put(&mut trans);
            if result == 0 {
                Ok(())
            } else {
                Err(EngineError::Transaction(result))
            }
        }
    }

    fn attach_persistent_journal(&self, path: &Path, truncate: bool) -> Result<(), EngineError> {
        let file = if truncate {
            let file = OpenOptions::new()
                .create(true)
                .read(true)
                .truncate(true)
                .write(true)
                .open(path)?;
            file.set_len(JOURNAL_FILE_SECTORS * 512)?;
            file
        } else {
            let file = OpenOptions::new().read(true).write(true).open(path)?;
            if file.metadata()?.len() < JOURNAL_FILE_SECTORS * 512 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "journal device is shorter than its fixed layout",
                )
                .into());
            }
            file
        };

        let mut fs = self.lock_fs()?;
        unsafe {
            configure_persistent_journal(&mut fs, file)?;
            /* Devices come online here: bch2_dev_allocator_add() marks them
             * rw (background.c:1723-1728), so derive the initial rw_devs set
             * from devs_online instead of a hardcoded device 0. */
            let mut rw_devs = self
                .inner
                .rw_devs
                .lock()
                .map_err(|_| EngineError::Poisoned)?;
            rw_devs.clear();
            for word in 0..fs.devs_online.d.len() {
                let mut bits = fs.devs_online.d[word];
                let mut bit = 0;
                while bits != 0 {
                    if bits & 1 != 0 {
                        rw_devs.insert((word * usize::BITS as usize + bit) as u64);
                    }
                    bits >>= 1;
                    bit += 1;
                }
            }
            drop(rw_devs);
            let mut info = journal_start_info::default();
            let ret = bch2_journal_read(&mut **fs, &mut info);
            if ret != 0 {
                return Err(EngineError::Journal(ret));
            }
            let ret = bch2_journal_replay(&mut **fs);
            if ret != 0 {
                return Err(EngineError::Journal(ret));
            }
            rebuild_derived_state(&mut **fs, None)?;
            check_extents_to_backpointers(&mut **fs)?;
        }
        drop(fs);
        Ok(())
    }

    fn request_reclaim_inner(&self) -> Result<u64, EngineError> {
        let control = &self.inner.reclaim;
        let mut state = control.state.lock().map_err(|_| EngineError::Poisoned)?;
        if state.stopping {
            return Err(EngineError::Transaction(-1));
        }
        state.requested = state.requested.saturating_add(1);
        let requested = state.requested;
        drop(state);
        control.wake.notify_one();
        Ok(requested)
    }

    fn schedule_reclaim_if_needed(&self) -> Result<(), EngineError> {
        let should_reclaim = {
            let fs = self.lock_fs()?;
            journal_med_on_space(&fs.journal)
                || journal_low_on_space(&fs.journal)
                || fs.journal.reclaim_kicked.load(Ordering::Acquire)
        };
        if should_reclaim {
            let _ = self.request_reclaim_inner()?;
        }
        Ok(())
    }

    fn reclaim_background_once(&self) -> Result<(), EngineError> {
        let mut fs = self.lock_fs()?;
        if journal_state_offset(fs.journal.reservations.load(Ordering::Acquire)) == 0
            && fs.journal.seq_ondisk.load(Ordering::Acquire) == 0
        {
            return Ok(());
        }
        unsafe { self.checkpoint_locked(&mut **fs) }
    }
}

impl Drop for EngineState {
    fn drop(&mut self) {
        {
            let mut state = match self.reclaim.state.lock() {
                Ok(state) => state,
                Err(poisoned) => poisoned.into_inner(),
            };
            state.stopping = true;
        }
        self.reclaim.wake.notify_all();
        let worker = match self.reclaim.worker.lock() {
            Ok(mut worker) => worker.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        if let Some(worker) = worker {
            if worker.thread().id() != thread::current().id() {
                let _ = worker.join();
            }
        }
        self.rcu.barrier();
        /* umount semantics: bch2_open_buckets_stop(c, NULL, true) closes all
         * open buckets when the fs goes read-only (fs.c:324).  An engine
         * dropping with unpaired open buckets is a caller leak. */
        {
            let open = match self.open_buckets.lock() {
                Ok(open) => open,
                Err(poisoned) => poisoned.into_inner(),
            };
            assert!(
                open.is_empty(),
                "open bucket leak: {} bucket(s) never closed",
                open.len()
            );
        }
        let fs = match self.fs.get_mut() {
            Ok(fs) => fs,
            Err(poisoned) => poisoned.into_inner(),
        };
        unsafe { crate::sb::io::bch2_free_super(&mut (**fs).disk_sb) };
    }
}

fn start_reclaim_worker(inner: &Arc<EngineState>) -> Result<(), EngineError> {
    let control = Arc::clone(&inner.reclaim);
    let worker_control = Arc::clone(&control);
    let weak = Arc::downgrade(inner);
    let worker = thread::Builder::new()
        .name("subvol-journal-reclaim".to_owned())
        .spawn(move || reclaim_worker_loop(weak, worker_control))?;
    let mut slot = control.worker.lock().map_err(|_| EngineError::Poisoned)?;
    *slot = Some(worker);
    Ok(())
}

fn reclaim_worker_loop(engine: Weak<EngineState>, control: Arc<ReclaimControl>) {
    loop {
        let mut state = match control.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        if state.stopping {
            return;
        }
        if state.requested <= state.completed {
            let (next, timeout) = match control.wake.wait_timeout(state, RECLAIM_WORKER_DELAY) {
                Ok(result) => result,
                Err(poisoned) => poisoned.into_inner(),
            };
            state = next;
            if state.stopping {
                return;
            }
            if state.requested <= state.completed {
                let timed_out = timeout.timed_out();
                drop(state);
                if !timed_out {
                    continue;
                }
                let Some(inner) = engine.upgrade() else {
                    return;
                };
                if !background_reclaim_needed(&inner) {
                    continue;
                }
                let mut state = match control.state.lock() {
                    Ok(state) => state,
                    Err(poisoned) => poisoned.into_inner(),
                };
                if state.stopping {
                    return;
                }
                if state.requested <= state.completed {
                    state.requested = state.requested.saturating_add(1);
                }
                drop(state);
                control.wake.notify_one();
                continue;
            }
        }

        state.running = true;
        let request = state.requested;
        drop(state);

        let result = match engine.upgrade() {
            Some(inner) => StorageEngine { inner }.reclaim_background_once(),
            None => return,
        };
        let error = result.as_ref().err().map(engine_error_code);

        let mut state = match control.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.running = false;
        state.completed = state.completed.max(request);
        state.last_error = error;
        drop(state);
        control.wake.notify_all();
    }
}

fn background_reclaim_needed(engine: &EngineState) -> bool {
    let fs = match engine.fs.lock() {
        Ok(fs) => fs,
        Err(poisoned) => poisoned.into_inner(),
    };
    journal_med_on_space(&fs.journal)
        || journal_low_on_space(&fs.journal)
        || fs.journal.reclaim_kicked.load(Ordering::Acquire)
}

fn engine_error_code(error: &EngineError) -> i32 {
    match error {
        EngineError::Transaction(error) | EngineError::Journal(error) => *error,
        EngineError::Io(_) => -5,
        EngineError::ReclaimTimeout => -110,
        EngineError::InvalidBtreeId(_)
        | EngineError::ValueTooLarge(_)
        | EngineError::UnsupportedFormatVersion(_)
        | EngineError::DerivedState(_)
        | EngineError::Poisoned => -1,
    }
}

fn reclaim_status(state: &ReclaimWorkerState) -> ReclaimStatus {
    ReclaimStatus {
        requested: state.requested,
        completed: state.completed,
        running: state.running,
        last_error: state.last_error,
    }
}

unsafe fn decode_key(raw: bkey_s_c) -> Result<BtreeKey, EngineError> {
    let header = &*raw.k;
    if header.u64s < BKEY_U64S || header.format != KEY_FORMAT_CURRENT {
        return Err(EngineError::Transaction(-1));
    }
    let value_u64s = bkey_val_u64s(header) as usize;
    if value_u64s > BKEY_VAL_U64S_MAX as usize || (value_u64s != 0 && raw.v.is_null()) {
        return Err(EngineError::Transaction(-1));
    }

    let mut value = vec![0; value_u64s];
    if value_u64s != 0 {
        core::ptr::copy_nonoverlapping(raw.v.cast::<u64>(), value.as_mut_ptr(), value_u64s);
    }
    BtreeKey::new(
        KeyPosition::new(header.p.inode, header.p.offset, header.p.snapshot),
        value,
    )
}

fn encode_key(position: KeyPosition, value: &[u64], deleted: bool) -> Vec<u64> {
    let value_u64s = if deleted { 0 } else { value.len() };
    let mut words = vec![0u64; BKEY_U64S as usize + value_u64s];
    unsafe {
        let raw = words.as_mut_ptr().cast::<bkey_i>();
        (*raw).k = bkey {
            u64s: words.len() as u8,
            format: KEY_FORMAT_CURRENT,
            type_: if deleted {
                KEY_TYPE_deleted
            } else {
                KEY_TYPE_cookie
            },
            p: position.raw(),
            ..Default::default()
        };
        if !deleted {
            words[BKEY_U64S as usize..].copy_from_slice(value);
        }
    }
    words
}

unsafe fn rewrite_node_locked(
    fs: &mut bch_fs,
    btree: BtreeId,
    level: u8,
    position: KeyPosition,
) -> Result<(), EngineError> {
    /* interior.c:3373 bch2_btree_node_rewrite_pos() 语义：定位到
     * position 处的节点并重写。调用层的 level 表示"指针键所在层"
     * （目标节点层 + 1，move.c:321 同款），内部路径停在 level-1 层
     * （bcachefs 的 CLASS depth=level-1 遍历约定），因此 level == 0
     * 是调用方错误（BUG_ON(!level)，由公共入口保证）。 */
    let mut trans = btree_trans::default();
    bch2_trans_init(&mut trans, fs);
    bch2_trans_begin(&mut trans);

    let mut iter = btree_iter::default();
    bch2_trans_iter_init_common(
        &mut trans,
        &mut iter,
        btree.as_u8(),
        position.raw(),
        crate::btree::bset::BTREE_MAX_DEPTH,
        level - 1,
        crate::btree::iter::BTREE_ITER_intent,
    );
    let b = crate::btree::iter::bch2_btree_iter_peek_node(&mut iter);
    let ret = if b.is_null() || (*b).data.is_null() {
        -5
    } else {
        crate::btree::interior::bch2_btree_node_rewrite(&mut trans, iter.path)
    };
    bch2_trans_iter_exit(&mut iter);
    bch2_trans_put(&mut trans);
    if ret != 0 {
        Err(EngineError::Transaction(ret))
    } else {
        Ok(())
    }
}

unsafe fn rewrite_node_key_locked(
    fs: &mut bch_fs,
    btree: BtreeId,
    level: u8,
    key: &BtreeKey,
) -> Result<(), EngineError> {
    /* interior.c:3345 bch2_btree_node_rewrite_key() 语义：仅当
     * 定位节点的指针键 hash 与给定键匹配时才重写，否则 -ENOENT。
     * 注意与 rewrite_node_locked 的 level 语义不同：rewrite_key
     * 的 level 即"目标节点层数"（async 传 b->c.level，read.c:1243
     * 传 scrub->level - 1），CLASS depth=level；因此叶节点
     * level == 0 合法，无 BUG_ON(!level)。 */
    let mut trans = btree_trans::default();
    bch2_trans_init(&mut trans, fs);
    bch2_trans_begin(&mut trans);

    let mut iter = btree_iter::default();
    bch2_trans_iter_init_common(
        &mut trans,
        &mut iter,
        btree.as_u8(),
        key.position().raw(),
        crate::btree::bset::BTREE_MAX_DEPTH,
        level,
        crate::btree::iter::BTREE_ITER_intent,
    );
    let b = crate::btree::iter::bch2_btree_iter_peek_node(&mut iter);
    /* 指针键必须为 btree_ptr_v2 类型（btree_ptr_hash_val 只对
     * btree_ptr_v2 取 seq）；encode_key 的通用路径编码为 cookie。 */
    let mut words = vec![0u64; BKEY_U64S as usize + key.value().len()];
    let raw_key = words.as_mut_ptr().cast::<bkey_i>();
    (*raw_key).k = bkey {
        u64s: words.len() as u8,
        format: KEY_FORMAT_CURRENT,
        type_: KEY_TYPE_btree_ptr_v2,
        p: key.position().raw(),
        ..Default::default()
    };
    words[BKEY_U64S as usize..].copy_from_slice(key.value());
    let raw_key = words.as_ptr().cast::<bkey_i>();
    let found = !b.is_null()
        && !(*b).data.is_null()
        && crate::btree::cache::btree_ptr_hash_val(&(*b).key)
            == crate::btree::cache::btree_ptr_hash_val(raw_key);
    let ret = if found {
        crate::btree::interior::bch2_btree_node_rewrite(&mut trans, iter.path)
    } else {
        -2
    };
    bch2_trans_iter_exit(&mut iter);
    bch2_trans_put(&mut trans);
    if ret != 0 {
        Err(EngineError::Transaction(ret))
    } else {
        Ok(())
    }
}

unsafe fn get_locked(
    fs: &mut bch_fs,
    btree: BtreeId,
    position: KeyPosition,
) -> Result<Option<BtreeKey>, EngineError> {
    let mut trans = btree_trans::default();
    bch2_trans_init(&mut trans, fs);
    bch2_trans_begin(&mut trans);

    let mut iter = btree_iter::default();
    bch2_trans_iter_init(
        &mut trans,
        &mut iter,
        btree.as_u8(),
        position.raw(),
        BTREE_ITER_not_extents,
    );
    let found = bch2_btree_iter_peek(&mut iter);
    let result = if bkey_err(found) != 0 {
        Err(EngineError::Transaction(bkey_err(found)))
    } else if found.k.is_null()
        || !bpos_eq((*found.k).p, position.raw())
        || (*found.k).type_ == KEY_TYPE_deleted
    {
        Ok(None)
    } else {
        decode_key(found).map(Some)
    };
    bch2_trans_iter_exit(&mut iter);
    bch2_trans_put(&mut trans);
    result
}

unsafe fn rebuild_derived_state(
    fs: &mut bch_fs,
    fault: Option<RecoveryFaultPoint>,
) -> Result<(), EngineError> {
    let sb = fs.disk_sb.sb;
    if sb.is_null() || crate::sb::io::bch2_sb_field_get_id(sb, BCH_SB_FIELD_members_v2).is_null() {
        return Ok(());
    }

    let mut preserved_alloc = BTreeMap::new();
    for raw in scan_raw_locked(fs, 4)? {
        let key = raw.words.as_ptr().cast::<bkey_i>();
        if (*key).k.type_ == KEY_TYPE_alloc_v4 {
            let value = (key as *const u8)
                .add(core::mem::size_of::<bkey>())
                .cast::<bch_alloc_v4>();
            let position = (*key).k.p;
            preserved_alloc.insert(
                (position.inode, position.offset),
                core::ptr::read_unaligned(value),
            );
        }
    }

    for id in [4u8, BTREE_ID_FREESPACE, 8u8] {
        let ret = bch2_clear_derived_tree(fs, id);
        if ret != 0 {
            return Err(EngineError::Transaction(ret));
        }
    }

    if fault == Some(RecoveryFaultPoint::DuringDerivedRebuild) {
        return Err(EngineError::Journal(-4));
    }

    /* recovery.c's replay has already installed all primary keys with norun.
     * Copy each visible primary key before dropping its read iterator, then
     * feed it to the explicit derived-state reconstruction transaction. */
    for id in 0..BTREE_ID_NR as u8 {
        if id == 4 || id == 8 {
            continue;
        }
        let mut trans = btree_trans::default();
        bch2_trans_init(&mut trans, fs);
        bch2_trans_begin(&mut trans);
        let mut iter = btree_iter::default();
        bch2_trans_iter_init(
            &mut trans,
            &mut iter,
            id,
            POS_MIN,
            BTREE_ITER_not_extents | BTREE_ITER_snapshot_field | BTREE_ITER_all_snapshots,
        );
        let mut keys = Vec::new();
        let mut current = bch2_btree_iter_peek(&mut iter);
        while !current.k.is_null() {
            let error = bkey_err(current);
            if error != 0 {
                bch2_trans_iter_exit(&mut iter);
                bch2_trans_put(&mut trans);
                return Err(EngineError::Transaction(error));
            }
            if (*current.k).type_ != KEY_TYPE_deleted {
                let u64s = (*current.k).u64s as usize;
                if u64s < BKEY_U64S as usize {
                    bch2_trans_iter_exit(&mut iter);
                    bch2_trans_put(&mut trans);
                    return Err(EngineError::Transaction(-1));
                }
                let mut copied = vec![0u64; u64s];
                let key = copied.as_mut_ptr().cast::<bkey_i>();
                (*key).k = *current.k;
                core::ptr::copy_nonoverlapping(
                    current.v.cast::<u64>(),
                    (key as *mut u64).add(BKEY_U64S as usize),
                    u64s - BKEY_U64S as usize,
                );
                keys.push(copied);
            }
            current = bch2_btree_iter_next(&mut iter);
        }
        bch2_trans_iter_exit(&mut iter);
        bch2_trans_put(&mut trans);

        for mut key in keys {
            if fault == Some(RecoveryFaultPoint::DuringDerivedRebuild) {
                return Err(EngineError::Journal(-4));
            }
            let ret = bch2_rebuild_derived_for_key(fs, id, 0, &mut key);
            if ret != 0 {
                return Err(EngineError::Transaction(ret));
            }
        }
    }

    let mut rebuilt_alloc = BTreeMap::new();
    for raw in scan_raw_locked(fs, 4)? {
        let key = raw.words.as_ptr().cast::<bkey_i>();
        if (*key).k.type_ == KEY_TYPE_alloc_v4 {
            let value = (key as *const u8)
                .add(core::mem::size_of::<bkey>())
                .cast::<bch_alloc_v4>();
            rebuilt_alloc.insert(
                ((*key).k.p.inode, (*key).k.p.offset),
                core::ptr::read_unaligned(value),
            );
        }
    }
    for ((dev, bucket), old) in preserved_alloc {
        let mut alloc = rebuilt_alloc.get(&(dev, bucket)).copied().unwrap_or(old);
        alloc.data_type = old.data_type;
        alloc.gen = old.gen;
        alloc.oldest_gen = old.oldest_gen;
        let position = crate::btree::bkey::POS(dev, bucket);
        let mut trans = btree_trans::default();
        bch2_trans_init(&mut trans, fs);
        loop {
            bch2_trans_begin(&mut trans);
            let ret = trigger_update_value(
                &mut trans,
                4,
                position,
                KEY_TYPE_alloc_v4,
                (&alloc as *const bch_alloc_v4).cast(),
                core::mem::size_of::<bch_alloc_v4>(),
            );
            let ret = if ret == 0 {
                bch2_trans_commit(&mut trans)
            } else {
                ret
            };
            if ret == -12 && trans.realloc_bytes_required != 0 {
                continue;
            }
            bch2_trans_put(&mut trans);
            if ret != 0 {
                return Err(EngineError::Transaction(ret));
            }
            break;
        }
    }

    /* Rebuild the freespace candidate index from the now-restored alloc
     * primary records.  bcachefs keeps this as a derived tree and only
     * indexes BCH_DATA_free buckets, with the generation delta encoded in
     * the high position bits. */
    let mut free_positions = Vec::new();
    for raw in scan_raw_locked(fs, 4)? {
        let key = raw.words.as_ptr().cast::<bkey_i>();
        if (*key).k.type_ != KEY_TYPE_alloc_v4 {
            continue;
        }
        let value = (key as *const u8)
            .add(core::mem::size_of::<bkey>())
            .cast::<bch_alloc_v4>();
        let alloc = core::ptr::read_unaligned(value);
        if alloc.data_type == BCH_DATA_FREE {
            free_positions.push(((*key).k.p, alloc));
        }
    }
    for (position, alloc) in free_positions {
        let mut trans = btree_trans::default();
        bch2_trans_init(&mut trans, fs);
        loop {
            bch2_trans_begin(&mut trans);
            let ret = bch2_btree_bit_mod(
                &mut trans,
                BTREE_ID_FREESPACE,
                alloc_freespace_pos(position, &alloc),
                true,
            );
            let ret = if ret == 0 {
                bch2_trans_commit(&mut trans)
            } else {
                ret
            };
            if ret == -12 && trans.realloc_bytes_required != 0 {
                continue;
            }
            bch2_trans_put(&mut trans);
            if ret != 0 {
                return Err(EngineError::Transaction(ret));
            }
            break;
        }
    }
    Ok(())
}

unsafe fn scan_locked(fs: &mut bch_fs, btree: BtreeId) -> Result<Vec<BtreeKey>, EngineError> {
    let mut trans = btree_trans::default();
    bch2_trans_init(&mut trans, fs);
    bch2_trans_begin(&mut trans);

    let mut iter = btree_iter::default();
    bch2_trans_iter_init(
        &mut trans,
        &mut iter,
        btree.as_u8(),
        POS_MIN,
        /* Full-tree scan: every key carries an explicit snapshot field
         * (KeyPosition), so traversal must enumerate all snapshots.
         * Without all_snapshots the iterator's advance() jumps to the
         * next nosnap position (bpos_nosnap_successor) and skips keys
         * that share (inode, offset) across snapshots — matching
         * bcachefs's filtered traversal.  The explicit snapshot_field
         * flag keeps all_snapshots from being normalized away in
         * bch2_btree_iter_flags() when the btree id reports no
         * snapshot field. */
        BTREE_ITER_not_extents | BTREE_ITER_snapshot_field | BTREE_ITER_all_snapshots,
    );

    let mut output = Vec::new();
    let mut current = bch2_btree_iter_peek(&mut iter);
    let result = loop {
        let error = bkey_err(current);
        if error != 0 {
            break Err(EngineError::Transaction(error));
        }
        if current.k.is_null() {
            break Ok(());
        }
        if (*current.k).type_ != KEY_TYPE_deleted {
            let key = match decode_key(current) {
                Ok(key) => key,
                Err(error) => break Err(error),
            };
            crate::rewrite_log_debug!(
                "scan visit ({},{},{})",
                key.position().inode,
                key.position().offset,
                key.position().snapshot,
            );
            if output
                .last()
                .is_some_and(|previous: &BtreeKey| previous.position() >= key.position())
            {
                break Err(EngineError::Transaction(-1));
            }
            output.push(key);
        }
        current = bch2_btree_iter_next(&mut iter);
    };

    bch2_trans_iter_exit(&mut iter);
    bch2_trans_put(&mut trans);
    result.map(|()| output)
}

struct RawScannedKey {
    btree: u8,
    words: Vec<u64>,
}

/* recovery.c's explicit allocation/backpointer checks walk the primary
 * btrees independently of the derived indexes.  Keep that same separation
 * here: the validator receives an owned copy of each primary key and never
 * mutates the tree while it compares the derived state. */
unsafe fn scan_raw_locked(fs: &mut bch_fs, btree: u8) -> Result<Vec<RawScannedKey>, EngineError> {
    let mut trans = btree_trans::default();
    bch2_trans_init(&mut trans, fs);
    bch2_trans_begin(&mut trans);
    let mut iter = btree_iter::default();
    bch2_trans_iter_init(&mut trans, &mut iter, btree, POS_MIN, BTREE_ITER_intent);
    let mut output = Vec::new();
    let mut current = bch2_btree_iter_peek(&mut iter);
    let result = loop {
        let error = bkey_err(current);
        if error != 0 {
            break Err(EngineError::Transaction(error));
        }
        if current.k.is_null() {
            break Ok(());
        }
        if (*current.k).type_ != KEY_TYPE_deleted {
            let u64s = (*current.k).u64s as usize;
            if u64s < BKEY_U64S as usize {
                break Err(EngineError::Transaction(-1));
            }
            let mut words = vec![0u64; u64s];
            core::ptr::copy_nonoverlapping(
                current.k.cast::<u64>(),
                words.as_mut_ptr(),
                BKEY_U64S as usize,
            );
            if u64s > BKEY_U64S as usize {
                core::ptr::copy_nonoverlapping(
                    current.v.cast::<u64>(),
                    words.as_mut_ptr().add(BKEY_U64S as usize),
                    u64s - BKEY_U64S as usize,
                );
            }
            output.push(RawScannedKey { btree, words });
        }
        current = bch2_btree_iter_next(&mut iter);
    };
    bch2_trans_iter_exit(&mut iter);
    bch2_trans_put(&mut trans);
    result.map(|()| output)
}

pub(crate) unsafe fn check_extents_to_backpointers(fs: &mut bch_fs) -> Result<(), EngineError> {
    let mut primary = Vec::new();
    for id in 0..BTREE_ID_NR as u8 {
        if id != 4 && id != 8 {
            primary.extend(scan_raw_locked(fs, id)?);
        }
    }

    let mut expected_alloc: BTreeMap<(u64, u64), (u8, u32)> = BTreeMap::new();
    let mut expected_bp: BTreeMap<(u64, u64), (u8, u8, u8, u8, u32, bpos)> = BTreeMap::new();
    for raw in primary {
        let key = raw.words.as_ptr().cast::<bkey_i>();
        let type_ = (*key).k.type_;
        if type_ != KEY_TYPE_extent && type_ != KEY_TYPE_btree_ptr && type_ != KEY_TYPE_btree_ptr_v2
        {
            continue;
        }
        let ptrs = bch2_bkey_ptrs_c(bkey_s_c {
            k: &(*key).k,
            v: (key as *const u64).add(BKEY_U64S as usize).cast(),
        });
        let mut entry = ptrs.start;
        while !entry.is_null() && (entry as usize) < (ptrs.end as usize) {
            if extent_entry_is_ptr(entry) {
                let ptr = (*entry).ptr;
                let dev = BCH_EXTENT_PTR_DEV(&ptr);
                let offset = BCH_EXTENT_PTR_OFFSET(&ptr);
                let generation = BCH_EXTENT_PTR_GEN(&ptr) as u8;
                let member = crate::sb::io::bch2_sb_member_get(fs.disk_sb.sb, dev as usize);
                if member.bucket_size == 0 {
                    crate::rewrite_log_error!("derived validator: zero bucket size for dev {dev}");
                    return Err(EngineError::DerivedState(
                        DerivedStateMismatch::InvalidPointer,
                    ));
                }
                let bucket = offset / member.bucket_size as u64;
                let sectors = (*key).k.size;
                let alloc = expected_alloc
                    .entry((dev, bucket))
                    .or_insert((generation, 0));
                if alloc.0 != generation {
                    crate::rewrite_log_error!(
                        "derived validator: generation mismatch dev={dev} bucket={bucket}"
                    );
                    return Err(EngineError::DerivedState(DerivedStateMismatch::Generation));
                }
                alloc.1 = alloc
                    .1
                    .checked_add(sectors)
                    .ok_or(EngineError::Transaction(-1))?;
                let bp = (
                    raw.btree,
                    0,
                    if type_ == KEY_TYPE_extent { 0 } else { 1 },
                    generation,
                    sectors,
                    (*key).k.p,
                );
                if expected_bp.insert((dev, offset), bp).is_some() {
                    crate::rewrite_log_error!(
                        "derived validator: duplicate backpointer dev={dev} offset={offset}"
                    );
                    return Err(EngineError::DerivedState(
                        DerivedStateMismatch::DuplicateBackpointer,
                    ));
                }
            }
            entry = crate::btree::bset::extent_entry_next_safe(fs, entry, ptrs.end);
        }
    }

    let alloc_keys = scan_raw_locked(fs, 4)?;
    let mut actual_alloc = BTreeMap::new();
    for raw in alloc_keys {
        let key = raw.words.as_ptr().cast::<bkey_i>();
        if (*key).k.type_ != KEY_TYPE_alloc_v4 || raw.words.len() < BKEY_U64S as usize + 1 {
            continue;
        }
        let value = (key as *const u8)
            .add(core::mem::size_of::<bkey>())
            .cast::<bch_alloc_v4>();
        let alloc = core::ptr::read_unaligned(value);
        if alloc.dirty_sectors != 0 {
            actual_alloc.insert(
                ((*key).k.p.inode, (*key).k.p.offset),
                (alloc.gen, alloc.dirty_sectors),
            );
        }
    }
    if actual_alloc != expected_alloc {
        crate::rewrite_log_error!("derived validator: alloc set mismatch");
        return Err(EngineError::DerivedState(DerivedStateMismatch::AllocSet));
    }

    let bp_keys = scan_raw_locked(fs, 8)?;
    let mut actual_bp = BTreeMap::new();
    for raw in bp_keys {
        let key = raw.words.as_ptr().cast::<bkey_i>();
        if (*key).k.type_ != KEY_TYPE_backpointer {
            continue;
        }
        let value = (key as *const u8)
            .add(core::mem::size_of::<bkey>())
            .cast::<bch_backpointer>();
        let bp = core::ptr::read_unaligned(value);
        actual_bp.insert(
            ((*key).k.p.inode, (*key).k.p.offset),
            (
                bp.btree_id,
                bp.level,
                bp.data_type,
                bp.bucket_gen,
                bp.bucket_len,
                bp.pos,
            ),
        );
    }
    if actual_bp != expected_bp {
        crate::rewrite_log_error!("derived validator: backpointer set mismatch");
        return Err(EngineError::DerivedState(
            DerivedStateMismatch::BackpointerSet,
        ));
    }
    Ok(())
}

/// Opens a persistent engine image and runs every consistency check,
/// fix_errors mode for `fsck_image`, mirroring the upstream fsck option
/// values FSCK_FIX_no / FSCK_FIX_yes (opts.h:132, init/error.c:437-449):
/// `No` reports the first verifying error without touching the image
/// (upstream `-n` -> nochanges + fix_errors=no, fsck.rs:266-269), `Yes`
/// repairs the alloc<->derived-index inconsistencies it knows how to fix
/// before re-verifying (upstream `-y` -> fix_errors=yes, fsck.rs:248-250).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FixErrors {
    No,
    Yes,
}

/// Runs the fsck flow over a persistent image, mirroring the fsck command
/// flow: open the device, run all recovery passes, report the first error
/// (fsck.rs:419-447).  The engine has no interactive repair path, so only
/// the no-repair and the automatic-repair modes exist, matching upstream
/// `-n/--no_repair` ("Don't repair, only check for errors", fsck.rs:60-61)
/// and `-y/--yes` (auto-repair).  An open failure surfaces as an Io error;
/// a failed check surfaces as the verifying error (e.g. a
/// DerivedStateMismatch variant).  In `Yes` mode the alloc<->derived-index
/// inconsistencies are repaired first (T0198, see repair_derived_indexes);
/// guard-verdict errors (OpenBucketFree / NotRwBucketFree) are never
/// repaired, matching the upstream skip semantics.
pub fn fsck_image(path: impl AsRef<Path>, fix: FixErrors) -> Result<(), EngineError> {
    fsck_image_with_fault(path, fix, None)
}

/// `fsck_image` with a one-shot repair-path fault injection (T0200).
/// The public entry point passes `None`; the fault matrix tests pass a
/// `FsckFaultPoint` and assert the image never falsely reports success
/// (the `recovery_fault_matrix_never_publishes_success` pattern).
fn fsck_image_with_fault(
    path: impl AsRef<Path>,
    fix: FixErrors,
    fault: Option<FsckFaultPoint>,
) -> Result<(), EngineError> {
    let engine = StorageEngine::open_persistent(path)?;
    if fix == FixErrors::Yes {
        repair_derived_indexes(&engine, fault)?;
        if fault == Some(FsckFaultPoint::AfterRepairBeforeFlush) {
            /* mirror a failed fs.exit() shutdown: the repairs were
             * committed but never made durable (fsck.rs:457-460) */
            return Err(EngineError::Journal(-5));
        }
        /* make the repairs durable before reporting success, like the
         * upstream fsck flow's fs.exit() shutdown (fsck.rs:457-460) */
        engine.flush_journal()?;
    }
    engine.verify_all()
}

/// Repairs the alloc<->derived-index inconsistencies reported by
/// `verify_bucket_indexes` (FreespaceSet / NeedDiscardSet).  The repair is
/// two-directional, mirroring `bch2_check_alloc_key` (alloc/check.c:175-188):
/// an index entry whose alloc bucket is no longer in the indexed state is
/// deleted (`delete_freespace_key` alloc/check.c:352-386, and
/// `bch2_check_discard_key`'s `bch2_btree_bit_mod_buffered(..., false)`
/// alloc/check.c:411-416), and an alloc bucket missing its index entry gets
/// one inserted.  Each entry is repaired in its own transaction, matching
/// `delete_freespace_key`'s single-transaction commit (alloc/check.c:366-371);
/// a non-index failure aborts the repair (upstream ret propagation) instead
/// of pretending success.
fn repair_derived_indexes(
    engine: &StorageEngine,
    fault: Option<FsckFaultPoint>,
) -> Result<(), EngineError> {
    let mut fs = engine.lock_fs()?;
    unsafe {
        if fs.disk_sb.sb.is_null() {
            return Err(EngineError::Transaction(-1));
        }
        /* Derive the expected index sets from the alloc tree, the same
         * projection verify_bucket_indexes computes (engine.rs:618-668):
         * FREE buckets must carry a freespace entry at
         * alloc_freespace_pos(), NEED_DISCARD buckets a need_discard
         * entry at the bucket position. */
        let mut expected_index = BTreeSet::new();
        let mut expected_need_discard = BTreeSet::new();
        for raw in scan_raw_locked(&mut **fs, 4)? {
            let key = raw.words.as_ptr().cast::<bkey_i>();
            if (*key).k.type_ != KEY_TYPE_alloc_v4 {
                continue;
            }
            let value = (key as *const u8)
                .add(core::mem::size_of::<bkey>())
                .cast::<bch_alloc_v4>();
            let alloc = core::ptr::read_unaligned(value);
            if alloc.data_type == BCH_DATA_FREE {
                let indexed = alloc_freespace_pos((*key).k.p, &alloc);
                expected_index.insert((indexed.inode, indexed.offset));
            } else if alloc.data_type == BCH_DATA_NEED_DISCARD {
                expected_need_discard.insert(((*key).k.p.inode, (*key).k.p.offset));
            }
        }
        /* Delete stale freespace entries, then insert missing ones. */
        let mut stale = Vec::new();
        let mut missing = expected_index.clone();
        for raw in scan_raw_locked(&mut **fs, BTREE_ID_FREESPACE)? {
            let key = raw.words.as_ptr().cast::<bkey_i>();
            if (*key).k.type_ == crate::btree::bset::KEY_TYPE_set {
                let pos = ((*key).k.p.inode, (*key).k.p.offset);
                if !expected_index.contains(&pos) {
                    stale.push((*key).k.p);
                }
                missing.remove(&pos);
            }
        }
        let mut fault_rest = fault;
        for position in stale {
            bit_mod_sync(
                &mut **fs,
                BTREE_ID_FREESPACE,
                position,
                false,
                &mut fault_rest,
            )?;
        }
        for (inode, offset) in missing {
            bit_mod_sync(
                &mut **fs,
                BTREE_ID_FREESPACE,
                crate::btree::bkey::POS(inode, offset),
                true,
                &mut fault_rest,
            )?;
        }
        /* Delete stale need_discard entries, then insert missing ones. */
        let mut stale = Vec::new();
        let mut missing = expected_need_discard.clone();
        for raw in scan_raw_locked(&mut **fs, BTREE_ID_NEED_DISCARD)? {
            let key = raw.words.as_ptr().cast::<bkey_i>();
            if (*key).k.type_ == crate::btree::bset::KEY_TYPE_set {
                let pos = ((*key).k.p.inode, (*key).k.p.offset);
                if !expected_need_discard.contains(&pos) {
                    stale.push((*key).k.p);
                }
                missing.remove(&pos);
            }
        }
        for position in stale {
            bit_mod_sync(
                &mut **fs,
                BTREE_ID_NEED_DISCARD,
                position,
                false,
                &mut fault_rest,
            )?;
        }
        for (inode, offset) in missing {
            bit_mod_sync(
                &mut **fs,
                BTREE_ID_NEED_DISCARD,
                crate::btree::bkey::POS(inode, offset),
                true,
                &mut fault_rest,
            )?;
        }
    }
    Ok(())
}

/// One-transaction bit-map entry modification, mirroring
/// `delete_freespace_key`'s commit flow (alloc/check.c:366-371): a -12
/// (ENOMEM) restart grows the transaction and retries, a -4 restart
/// (trans restart) rides the bch2_trans_begin retry loop like
/// lockrestart_do (iter.h:1115-1127), any other failure aborts the
/// repair with the transaction error.  A one-shot `fault` (T0200) is
/// consumed at the commit boundary (the trans_maybe_inject_restart
/// position, commit.c:1390).
unsafe fn bit_mod_sync(
    fs: &mut bch_fs,
    btree: u8,
    position: bpos,
    set: bool,
    fault: &mut Option<FsckFaultPoint>,
) -> Result<(), EngineError> {
    let mut trans = btree_trans::default();
    bch2_trans_init(&mut trans, fs);
    loop {
        bch2_trans_begin(&mut trans);
        let ret = bch2_btree_bit_mod(&mut trans, btree, position, set);
        let ret = if ret == 0 {
            match *fault {
                Some(FsckFaultPoint::DuringRepairRestart) => {
                    *fault = None;
                    -4
                }
                Some(FsckFaultPoint::DuringRepairOom) => {
                    *fault = None;
                    /* hard ENOMEM with no realloc requirement: the -12
                     * fails the realloc retry condition below and aborts
                     * the repair (restarted == 0 semantics) */
                    -12
                }
                Some(FsckFaultPoint::AfterRepairBeforeFlush) | None => {
                    bch2_trans_commit(&mut trans)
                }
            }
        } else {
            ret
        };
        if ret == -4 || (ret == -12 && trans.realloc_bytes_required != 0) {
            continue;
        }
        bch2_trans_put(&mut trans);
        if ret != 0 {
            return Err(EngineError::Transaction(ret));
        }
        break;
    }
    Ok(())
}

unsafe fn configure_persistent_journal(
    fs: &mut bch_fs,
    file: std::fs::File,
) -> Result<(), EngineError> {
    if fs.disk_sb.sb.is_null() || !fs.disk_sb.s_bdev_file.is_null() {
        return Err(EngineError::Transaction(-1));
    }

    let file_sectors = file.metadata()?.len().div_ceil(512);
    let sb = fs.disk_sb.sb;
    (*sb).version = bcachefs_metadata_version_current;
    (*sb).version_min = bcachefs_metadata_version_current;
    (*sb).magic = BCHFS_MAGIC;
    (*sb).uuid = ENGINE_JOURNAL_UUID;
    (*sb).dev_idx = 0;
    (*sb).nr_devices = 1;
    (*sb).block_size = 1;

    let members_u64s = (core::mem::size_of::<bch_sb_field_members_v2>()
        + core::mem::size_of::<bch_member>())
    .div_ceil(core::mem::size_of::<u64>()) as u32;
    let members = crate::sb::io::bch2_sb_field_resize_id(
        &mut fs.disk_sb,
        BCH_SB_FIELD_members_v2,
        members_u64s,
    )
    .cast::<bch_sb_field_members_v2>();
    if members.is_null() {
        return Err(EngineError::Transaction(-12));
    }
    (*members).member_bytes = core::mem::size_of::<bch_member>() as u16;
    *members
        .cast::<u8>()
        .add(core::mem::size_of::<bch_sb_field_members_v2>())
        .cast::<bch_member>() = bch_member {
        uuid: ENGINE_JOURNAL_UUID,
        /* The fixed journal occupies the initial range, but btree-node
         * slots are appended after it.  Reopening must reconstruct geometry
         * from the whole device or replay rejects those physical pointers as
         * out of bounds. */
        nbuckets: file_sectors.div_ceil(JOURNAL_BUCKET_SIZE as u64),
        first_bucket: 0,
        bucket_size: JOURNAL_BUCKET_SIZE,
        ..Default::default()
    };

    /* members-v2 is the persistent geometry authority.  Attach the only
     * configured device before any future physical-pointer trigger is allowed
     * to consume that geometry, matching members.h's devs_online predicate. */
    fs.devs_online.d[0] |= 1;

    let journal_u64s = (core::mem::size_of::<bch_sb_field_journal_v2>()
        + core::mem::size_of::<bch_sb_field_journal_v2_entry>())
    .div_ceil(core::mem::size_of::<u64>()) as u32;
    let journal = crate::sb::io::bch2_sb_field_resize_id(
        &mut fs.disk_sb,
        BCH_SB_FIELD_journal_v2,
        journal_u64s,
    )
    .cast::<bch_sb_field_journal_v2>();
    if journal.is_null() {
        return Err(EngineError::Transaction(-12));
    }
    *journal
        .cast::<u8>()
        .add(core::mem::size_of::<bch_sb_field_journal_v2>())
        .cast::<bch_sb_field_journal_v2_entry>() = bch_sb_field_journal_v2_entry {
        start: JOURNAL_BUCKET_START,
        nr: JOURNAL_BUCKETS,
    };

    fs.disk_sb.s_bdev_file = Box::into_raw(Box::new(file)).cast();
    Ok(())
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use std::{
        collections::BTreeMap,
        fs,
        os::unix::fs::FileExt,
        path::{Path, PathBuf},
        process::Command,
        sync::{atomic::AtomicU64, Arc},
        time::Duration,
    };

    use super::*;

    fn key(offset: u64, value: &[u64]) -> BtreeKey {
        BtreeKey::new(KeyPosition::new(1, offset, 0), value.to_vec()).unwrap()
    }

    static TEST_FILE_NONCE: AtomicU64 = AtomicU64::new(0);

    fn persistent_test_path(label: &str) -> PathBuf {
        let nonce = TEST_FILE_NONCE.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        std::env::temp_dir().join(format!("subvol-{label}-{}-{nonce}", std::process::id(),))
    }

    fn clear_journal_region(path: &Path) {
        let file = OpenOptions::new().write(true).open(path).unwrap();
        let zeros = vec![0; JOURNAL_BUCKETS as usize * JOURNAL_BUCKET_SIZE as usize * 512];
        let offset = JOURNAL_BUCKET_START * JOURNAL_BUCKET_SIZE as u64 * 512;
        assert_eq!(file.write_at(&zeros, offset).unwrap(), zeros.len());
        file.sync_all().unwrap();
    }

    fn prepared_bucket_engine(label: &str, bucket: u64) -> (StorageEngine, PathBuf) {
        let path = persistent_test_path(label);
        let file = fs::File::create(&path).unwrap();
        file.set_len(32 * 1024 * 1024).unwrap();
        drop(file);
        let engine = StorageEngine::create_persistent(&path).unwrap();
        unsafe {
            let mut fs = engine.lock_fs().unwrap();
            let position = crate::btree::bkey::POS(0, bucket);
            let alloc = bch_alloc_v4::default();
            let mut trans = btree_trans::default();
            bch2_trans_init(&mut trans, &mut **fs);
            loop {
                bch2_trans_begin(&mut trans);
                let ret = trigger_update_value(
                    &mut trans,
                    4,
                    position,
                    KEY_TYPE_alloc_v4,
                    (&alloc as *const bch_alloc_v4).cast(),
                    core::mem::size_of::<bch_alloc_v4>(),
                );
                let ret = if ret == 0 {
                    bch2_btree_bit_mod(
                        &mut trans,
                        BTREE_ID_FREESPACE,
                        alloc_freespace_pos(position, &alloc),
                        true,
                    )
                } else {
                    ret
                };
                let ret = if ret == 0 {
                    bch2_trans_commit(&mut trans)
                } else {
                    ret
                };
                if ret == -12 && trans.realloc_bytes_required != 0 {
                    continue;
                }
                assert_eq!(ret, 0);
                break;
            }
            bch2_trans_put(&mut trans);
        }
        (engine, path)
    }

    fn set_bucket_journal_seq(engine: &StorageEngine, position: bpos, seq: u64) {
        unsafe {
            let mut fs = engine.lock_fs().unwrap();
            let mut alloc = None;
            for raw in scan_raw_locked(&mut **fs, 4).unwrap() {
                let key = raw.words.as_ptr().cast::<bkey_i>();
                if (*key).k.type_ == KEY_TYPE_alloc_v4 && (*key).k.p == position {
                    let value = (key as *const u8)
                        .add(core::mem::size_of::<bkey>())
                        .cast::<bch_alloc_v4>();
                    alloc = Some(core::ptr::read_unaligned(value));
                    break;
                }
            }
            let mut alloc = alloc.expect("test bucket alloc exists");
            alloc.journal_seq_empty = seq;
            let mut trans = btree_trans::default();
            bch2_trans_init(&mut trans, &mut **fs);
            loop {
                bch2_trans_begin(&mut trans);
                let ret = trigger_update_value(
                    &mut trans,
                    4,
                    position,
                    KEY_TYPE_alloc_v4,
                    (&alloc as *const bch_alloc_v4).cast(),
                    core::mem::size_of::<bch_alloc_v4>(),
                );
                let ret = if ret == 0 {
                    bch2_trans_commit(&mut trans)
                } else {
                    ret
                };
                if ret == -12 && trans.realloc_bytes_required != 0 {
                    continue;
                }
                assert_eq!(ret, 0);
                break;
            }
            bch2_trans_put(&mut trans);
        }
    }

    fn set_bucket_sectors(engine: &StorageEngine, position: bpos, dirty: u32, cached: u32) {
        unsafe {
            let mut fs = engine.lock_fs().unwrap();
            let mut alloc = None;
            for raw in scan_raw_locked(&mut **fs, 4).unwrap() {
                let key = raw.words.as_ptr().cast::<bkey_i>();
                if (*key).k.type_ == KEY_TYPE_alloc_v4 && (*key).k.p == position {
                    let value = (key as *const u8)
                        .add(core::mem::size_of::<bkey>())
                        .cast::<bch_alloc_v4>();
                    alloc = Some(core::ptr::read_unaligned(value));
                    break;
                }
            }
            let mut alloc = alloc.expect("test bucket alloc exists");
            alloc.dirty_sectors = dirty;
            alloc.cached_sectors = cached;
            let mut trans = btree_trans::default();
            bch2_trans_init(&mut trans, &mut **fs);
            loop {
                bch2_trans_begin(&mut trans);
                let ret = trigger_update_value(
                    &mut trans,
                    4,
                    position,
                    KEY_TYPE_alloc_v4,
                    (&alloc as *const bch_alloc_v4).cast(),
                    core::mem::size_of::<bch_alloc_v4>(),
                );
                let ret = if ret == 0 {
                    bch2_trans_commit(&mut trans)
                } else {
                    ret
                };
                if ret == -12 && trans.realloc_bytes_required != 0 {
                    continue;
                }
                assert_eq!(ret, 0);
                break;
            }
            bch2_trans_put(&mut trans);
        }
    }

    fn set_need_discard_index(engine: &StorageEngine, position: bpos, set: bool) {
        unsafe {
            let mut fs = engine.lock_fs().unwrap();
            let mut trans = btree_trans::default();
            bch2_trans_init(&mut trans, &mut **fs);
            loop {
                bch2_trans_begin(&mut trans);
                let ret = bch2_btree_bit_mod(&mut trans, BTREE_ID_NEED_DISCARD, position, set);
                let ret = if ret == 0 {
                    bch2_trans_commit(&mut trans)
                } else {
                    ret
                };
                if ret == -12 && trans.realloc_bytes_required != 0 {
                    continue;
                }
                assert_eq!(ret, 0);
                break;
            }
            bch2_trans_put(&mut trans);
        }
    }

    #[test]
    fn transaction_restart_retraverses_before_committing_once() {
        let engine = StorageEngine::new().unwrap();
        engine
            .inject_fault(FaultPoint::TransactionRestart, 1)
            .unwrap();
        engine.put(BtreeId::DEFAULT, key(10, &[1, 2])).unwrap();
        assert_eq!(
            engine
                .get(BtreeId::DEFAULT, KeyPosition::new(1, 10, 0))
                .unwrap(),
            Some(key(10, &[1, 2]))
        );

        engine.flush_journal().unwrap();
        let image = engine.durable_journal().unwrap();
        assert_eq!(image.record_count(), 1);
        let recovered = StorageEngine::recover(&image).unwrap();
        assert_eq!(
            recovered.scan(BtreeId::DEFAULT).unwrap(),
            vec![key(10, &[1, 2])]
        );
    }

    #[test]
    fn dropped_transaction_never_changes_the_tree_or_journal() {
        let engine = StorageEngine::new().unwrap();
        {
            let mut transaction = engine.transaction();
            transaction.put(BtreeId::DEFAULT, key(13, &[3]));
        }

        assert!(engine
            .get(BtreeId::DEFAULT, KeyPosition::new(1, 13, 0))
            .unwrap()
            .is_none());
        assert_eq!(engine.durable_journal().unwrap().record_count(), 0);
    }

    #[test]
    fn one_transaction_replays_all_of_its_btree_updates() {
        let engine = StorageEngine::new().unwrap();
        let secondary = BtreeId::new(1).unwrap();
        let mut transaction = engine.transaction();
        transaction.put(BtreeId::DEFAULT, key(14, &[1]));
        transaction.put(secondary, key(15, &[2, 3]));
        transaction.commit().unwrap();
        engine.flush_journal().unwrap();

        let recovered = StorageEngine::recover(&engine.durable_journal().unwrap()).unwrap();
        assert_eq!(
            recovered
                .get(BtreeId::DEFAULT, KeyPosition::new(1, 14, 0))
                .unwrap(),
            Some(key(14, &[1]))
        );
        assert_eq!(
            recovered
                .get(secondary, KeyPosition::new(1, 15, 0))
                .unwrap(),
            Some(key(15, &[2, 3]))
        );
    }

    #[test]
    fn failed_flush_does_not_make_a_transaction_recoverable() {
        let engine = StorageEngine::new().unwrap();
        engine.put(BtreeId::DEFAULT, key(11, &[7])).unwrap();
        engine.inject_fault(FaultPoint::JournalWrite, 1).unwrap();
        assert!(matches!(
            engine.flush_journal(),
            Err(EngineError::Journal(-5))
        ));

        let not_durable = engine.durable_journal().unwrap();
        assert_eq!(not_durable.record_count(), 0);
        assert!(StorageEngine::recover(&not_durable)
            .unwrap()
            .get(BtreeId::DEFAULT, KeyPosition::new(1, 11, 0))
            .unwrap()
            .is_none());

        engine.flush_journal().unwrap();
        let durable = engine.durable_journal().unwrap();
        assert_eq!(
            StorageEngine::recover(&durable)
                .unwrap()
                .get(BtreeId::DEFAULT, KeyPosition::new(1, 11, 0))
                .unwrap(),
            Some(key(11, &[7]))
        );
    }

    #[test]
    fn reclaim_releases_old_records_and_replays_the_tail() {
        let engine = StorageEngine::new().unwrap();
        let secondary = BtreeId::new(1).unwrap();
        engine.put(BtreeId::DEFAULT, key(21, &[1, 2])).unwrap();
        engine.put(secondary, key(22, &[3])).unwrap();
        engine.flush_journal().unwrap();

        engine.reclaim_journal().unwrap();
        let reclaimed = engine.durable_journal().unwrap();
        /* A device-less engine keeps every record: its journal mirror is the
         * only durable source from which recovery can rebuild the btree, so
         * reclaim publishes the anchor without discarding the window. */
        assert_eq!(reclaimed.record_count(), 2);

        engine.put(BtreeId::DEFAULT, key(23, &[4, 5, 6])).unwrap();
        engine.flush_journal().unwrap();
        let recovered = StorageEngine::recover(&engine.durable_journal().unwrap()).unwrap();
        assert_eq!(
            recovered.scan(BtreeId::DEFAULT).unwrap(),
            vec![key(21, &[1, 2]), key(23, &[4, 5, 6])]
        );
        assert_eq!(recovered.scan(secondary).unwrap(), vec![key(22, &[3])]);
        recovered.verify(BtreeId::DEFAULT).unwrap();
        recovered.verify(secondary).unwrap();
    }

    #[test]
    fn persistent_journal_reopens_after_process_style_drop() {
        let path = std::env::temp_dir().join(format!(
            "subvol-engine-journal-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        {
            let engine = StorageEngine::create_persistent(&path).unwrap();
            engine.put(BtreeId::DEFAULT, key(12, &[9, 10, 11])).unwrap();
            engine.flush_journal().unwrap();
        }

        let recovered = StorageEngine::open_persistent(&path).unwrap();
        assert_eq!(
            recovered
                .get(BtreeId::DEFAULT, KeyPosition::new(1, 12, 0))
                .unwrap(),
            Some(key(12, &[9, 10, 11]))
        );
        drop(recovered);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn public_bucket_api_runs_allocate_reclaim_and_reuse_sequence() {
        let (engine, path) = prepared_bucket_engine("bucket-api", 4);

        assert!(matches!(
            engine.allocate_bucket(1),
            Err(EngineError::Transaction(-1))
        ));
        assert!(matches!(
            engine.reclaim_bucket(KeyPosition::new(0, 8, 0)),
            Err(EngineError::Transaction(-1))
        ));
        engine
            .inject_fault(FaultPoint::TransactionRestart, 1)
            .unwrap();
        assert_eq!(
            engine.allocate_bucket(0).unwrap(),
            KeyPosition::new(0, 4, 0)
        );
        assert!(matches!(
            engine.discard_bucket(KeyPosition::new(0, 4, 0)),
            Err(EngineError::Transaction(-11))
        ));
        assert!(engine.verify_all().is_ok());
        engine.reclaim_bucket(KeyPosition::new(0, 4, 0)).unwrap();
        assert!(engine.verify_all().is_ok());
        set_need_discard_index(&engine, crate::btree::bkey::POS(0, 4), false);
        assert!(matches!(
            engine.verify_bucket_indexes(),
            Err(EngineError::DerivedState(
                DerivedStateMismatch::NeedDiscardSet
            ))
        ));
        set_need_discard_index(&engine, crate::btree::bkey::POS(0, 4), true);
        assert!(engine.verify_all().is_ok());
        set_bucket_sectors(&engine, crate::btree::bkey::POS(0, 4), 1, 0);
        assert!(matches!(
            engine.reclaim_bucket(KeyPosition::new(0, 4, 0)),
            Err(EngineError::Transaction(-16))
        ));
        set_bucket_sectors(&engine, crate::btree::bkey::POS(0, 4), 0, 0);
        set_bucket_journal_seq(&engine, crate::btree::bkey::POS(0, 4), 2);
        {
            let fs = engine.lock_fs().unwrap();
            fs.journal.last_seq_ondisk.store(1, Ordering::Release);
        }
        assert!(matches!(
            engine.discard_bucket(KeyPosition::new(0, 4, 0)),
            Err(EngineError::Transaction(-11))
        ));
        assert!(engine.verify_all().is_ok());
        {
            let fs = engine.lock_fs().unwrap();
            fs.journal.last_seq_ondisk.store(2, Ordering::Release);
        }
        engine.discard_bucket(KeyPosition::new(0, 4, 0)).unwrap();
        assert!(engine.verify_all().is_ok());
        assert_eq!(
            engine.allocate_bucket(0).unwrap(),
            KeyPosition::new(0, 4, 0)
        );

        engine.inject_fault(FaultPoint::JournalWrite, 1).unwrap();
        assert!(engine.flush_journal().is_err());
        assert!(engine.verify_all().is_ok());
        engine.flush_journal().unwrap();
        drop(engine);
        let recovered = StorageEngine::open_persistent(&path).unwrap();
        assert!(recovered.verify_all().is_ok());

        drop(recovered);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn discard_worker_deduplicates_and_retries_eagain() {
        let (engine, path) = prepared_bucket_engine("discard-worker", 4);
        let position = KeyPosition::new(0, 4, 0);
        engine.queue_discard_bucket(position).unwrap();
        assert!(matches!(
            engine.queue_discard_bucket(position),
            Err(EngineError::Transaction(-17))
        ));
        assert!(matches!(
            engine.run_discard_worker_once(),
            Err(EngineError::Transaction(-11))
        ));
        assert!(matches!(
            engine.queue_discard_bucket(position),
            Err(EngineError::Transaction(-17))
        ));
        engine.allocate_bucket(0).unwrap();
        engine.reclaim_bucket(position).unwrap();
        engine.run_discard_worker_once().unwrap();
        assert!(engine.verify_all().is_ok());
        drop(engine);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn discard_worker_concurrent_queue_single_worker_drains_all() {
        let (engine, path) = prepared_bucket_engine("discard-concurrent", 4);
        engine.add_free_bucket(5);
        engine.add_free_bucket(6);
        engine.add_free_bucket(7);
        let mut positions = Vec::new();
        for _ in 0..4 {
            let position = engine.allocate_bucket(0).unwrap();
            engine.reclaim_bucket(position).unwrap();
            positions.push(position);
        }
        let engine = Arc::new(engine);
        let barrier = Arc::new(std::sync::Barrier::new(positions.len()));
        let mut workers = Vec::new();
        for position in &positions {
            let engine = Arc::clone(&engine);
            let barrier = Arc::clone(&barrier);
            let position = *position;
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                engine.queue_discard_bucket(position).unwrap();
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
        engine.run_discard_worker().unwrap();
        engine.verify_all().unwrap();
        for position in &positions {
            assert!(
                engine.queue_discard_bucket(*position).is_ok(),
                "queue should be drained: {position:?}"
            );
        }
        drop(engine);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn fsck_image_passes_on_healthy_image() {
        let (engine, path) = prepared_bucket_engine("fsck-healthy", 4);
        engine.add_free_bucket(5);
        let position = engine.allocate_bucket(0).unwrap();
        engine.reclaim_bucket(position).unwrap();
        drop(engine);
        assert!(fsck_image(&path, FixErrors::No).is_ok());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn fsck_image_io_error_on_unreadable_image() {
        let path = persistent_test_path("fsck-io");
        assert!(matches!(
            fsck_image(&path, FixErrors::No),
            Err(EngineError::Io(_))
        ));
    }

    #[test]
    fn fsck_image_no_mode_reports_stale_need_discard_key() {
        let (engine, path) = prepared_bucket_engine("fsck-nd-stale", 4);
        engine.add_free_bucket(5);
        /* a need_discard index entry for a bucket the alloc tree does not
         * know: rebuild_derived_state clears freespace/alloc but not the
         * need_discard tree (engine.rs:2014-2019), so the stale entry
         * survives open_persistent and must be reported as NeedDiscardSet
         * in no-repair mode (T0198). */
        set_need_discard_index(&engine, crate::btree::bkey::POS(0, 9), true);
        engine.flush_journal().unwrap();
        drop(engine);
        assert!(matches!(
            fsck_image(&path, FixErrors::No),
            Err(EngineError::DerivedState(
                DerivedStateMismatch::NeedDiscardSet
            ))
        ));
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn fsck_image_yes_mode_deletes_stale_need_discard_key() {
        let (engine, path) = prepared_bucket_engine("fsck-nd-fix", 4);
        engine.add_free_bucket(5);
        set_need_discard_index(&engine, crate::btree::bkey::POS(0, 9), true);
        engine.flush_journal().unwrap();
        drop(engine);
        assert!(fsck_image(&path, FixErrors::Yes).is_ok());
        /* the repaired image reopens verified: the stale entry is gone and
         * no index inconsistency remains (T0198 AC-2/AC-4) */
        let reopened = StorageEngine::open_persistent(&path).unwrap();
        assert!(reopened.verify_all().is_ok());
        let mut fs = reopened.lock_fs().unwrap();
        let mut stale = false;
        unsafe {
            for raw in scan_raw_locked(&mut **fs, BTREE_ID_NEED_DISCARD).unwrap() {
                let key = raw.words.as_ptr().cast::<bkey_i>();
                if (*key).k.type_ == crate::btree::bset::KEY_TYPE_set
                    && (*key).k.p == crate::btree::bkey::POS(0, 9)
                {
                    stale = true;
                }
            }
        }
        drop(fs);
        drop(reopened);
        assert!(!stale, "stale need_discard entry must be deleted");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn fsck_image_yes_mode_restores_missing_need_discard_entry() {
        let (engine, path) = prepared_bucket_engine("fsck-nd-missing", 4);
        engine.add_free_bucket(5);
        let position = engine.allocate_bucket(0).unwrap();
        engine.reclaim_bucket(position).unwrap();
        /* drop the need_discard index entry the reclaim wrote: the alloc
         * tree still says NEED_DISCARD, so the entry must be re-inserted
         * (upstream bch2_check_alloc_key's bidirectional repair,
         * alloc/check.c:175-179) */
        set_need_discard_index(&engine, position.raw(), false);
        engine.flush_journal().unwrap();
        drop(engine);
        assert!(fsck_image(&path, FixErrors::Yes).is_ok());
        let reopened = StorageEngine::open_persistent(&path).unwrap();
        assert!(reopened.verify_all().is_ok());
        let mut fs = reopened.lock_fs().unwrap();
        let mut present = false;
        unsafe {
            for raw in scan_raw_locked(&mut **fs, BTREE_ID_NEED_DISCARD).unwrap() {
                let key = raw.words.as_ptr().cast::<bkey_i>();
                if (*key).k.type_ == crate::btree::bset::KEY_TYPE_set
                    && (*key).k.p == position.raw()
                {
                    present = true;
                }
            }
        }
        drop(fs);
        drop(reopened);
        assert!(present, "missing need_discard entry must be re-inserted");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn fsck_repair_restart_injected_retries_and_succeeds() {
        /* T0200: a -4 restart injected at the first repair commit rides
         * the bch2_trans_begin retry loop (lockrestart_do, iter.h:1115-1127)
         * and the repair converges; the injected point is consumed once. */
        let (engine, path) = prepared_bucket_engine("fsck-rt-inject", 4);
        engine.add_free_bucket(5);
        set_need_discard_index(&engine, crate::btree::bkey::POS(0, 9), true);
        engine.flush_journal().unwrap();
        drop(engine);
        assert!(fsck_image_with_fault(
            &path,
            FixErrors::Yes,
            Some(FsckFaultPoint::DuringRepairRestart)
        )
        .is_ok());
        let reopened = StorageEngine::open_persistent(&path).unwrap();
        assert!(reopened.verify_all().is_ok());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn fsck_repair_oom_injected_aborts_and_rerun_recovers() {
        /* T0200: a hard -12 (restarted == 0, no realloc requirement)
         * aborts the repair with the transaction error; the image keeps
         * its inconsistency, so a clean rerun completes the repair. */
        let (engine, path) = prepared_bucket_engine("fsck-oom-inject", 4);
        engine.add_free_bucket(5);
        set_need_discard_index(&engine, crate::btree::bkey::POS(0, 9), true);
        engine.flush_journal().unwrap();
        drop(engine);
        assert!(matches!(
            fsck_image_with_fault(&path, FixErrors::Yes, Some(FsckFaultPoint::DuringRepairOom)),
            Err(EngineError::Transaction(-12))
        ));
        assert!(fsck_image(&path, FixErrors::Yes).is_ok());
        let reopened = StorageEngine::open_persistent(&path).unwrap();
        assert!(reopened.verify_all().is_ok());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn fsck_repair_flush_failure_injected_aborts_and_rerun_recovers() {
        /* T0200: a flush failure after the repairs were committed (the
         * fs.exit() shutdown point, fsck.rs:457-460) surfaces as a
         * Journal error without falsely reporting success; the unflushed
         * repairs are dropped by the reopen (journal replay only re-applies
         * durable records), so a clean rerun repairs and verifies. */
        let (engine, path) = prepared_bucket_engine("fsck-flush-inject", 4);
        engine.add_free_bucket(5);
        set_need_discard_index(&engine, crate::btree::bkey::POS(0, 9), true);
        engine.flush_journal().unwrap();
        drop(engine);
        assert!(matches!(
            fsck_image_with_fault(
                &path,
                FixErrors::Yes,
                Some(FsckFaultPoint::AfterRepairBeforeFlush)
            ),
            Err(EngineError::Journal(-5))
        ));
        assert!(fsck_image(&path, FixErrors::Yes).is_ok());
        let reopened = StorageEngine::open_persistent(&path).unwrap();
        assert!(reopened.verify_all().is_ok());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn fsck_repair_fault_matrix_never_falsely_reports_success() {
        /* T0200: every repair-path fault point must fail the repair
         * instead of publishing success, mirroring the recovery fault
         * matrix pattern (recovery_fault_matrix_never_publishes_success).
         * DuringRepairRestart is excluded: a -4 restart is retried by the
         * transaction loop, not a failure. */
        for fault in [
            FsckFaultPoint::DuringRepairOom,
            FsckFaultPoint::AfterRepairBeforeFlush,
        ] {
            let (engine, path) = prepared_bucket_engine("fsck-fault-matrix", 4);
            engine.add_free_bucket(5);
            set_need_discard_index(&engine, crate::btree::bkey::POS(0, 9), true);
            engine.flush_journal().unwrap();
            drop(engine);
            assert!(
                fsck_image_with_fault(&path, FixErrors::Yes, Some(fault)).is_err(),
                "fault {fault:?} unexpectedly reported repair success"
            );
            fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn fsck_image_no_mode_leaves_image_unchanged() {
        let (engine, path) = prepared_bucket_engine("fsck-nd-unchanged", 4);
        engine.add_free_bucket(5);
        set_need_discard_index(&engine, crate::btree::bkey::POS(0, 9), true);
        engine.flush_journal().unwrap();
        drop(engine);
        /* no-repair must not touch the image: the same inconsistency
         * reports again, and yes-mode can still repair afterwards */
        assert!(matches!(
            fsck_image(&path, FixErrors::No),
            Err(EngineError::DerivedState(
                DerivedStateMismatch::NeedDiscardSet
            ))
        ));
        assert!(matches!(
            fsck_image(&path, FixErrors::No),
            Err(EngineError::DerivedState(
                DerivedStateMismatch::NeedDiscardSet
            ))
        ));
        assert!(fsck_image(&path, FixErrors::Yes).is_ok());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn verify_all_returns_first_error_and_runs_every_check() {
        let (engine, path) = prepared_bucket_engine("verify-all", 4);
        engine.add_free_bucket(5);
        assert!(engine.verify_all().is_ok());
        let position = KeyPosition::new(0, 5, 0);
        engine.open_bucket(position).unwrap();
        assert!(matches!(
            engine.verify_all(),
            Err(EngineError::DerivedState(
                DerivedStateMismatch::OpenBucketFree
            ))
        ));
        engine.close_open_bucket(position).unwrap();
        assert!(engine.verify_all().is_ok());
        drop(engine);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn verify_all_keeps_first_error_when_multiple_checks_fail() {
        let (engine, path) = prepared_bucket_engine("verify-all-first", 4);
        engine.add_free_bucket(5);
        let position = KeyPosition::new(0, 5, 0);
        set_need_discard_index(&engine, position.raw(), true);
        engine.open_bucket(position).unwrap();
        assert!(matches!(
            engine.verify_all(),
            Err(EngineError::DerivedState(
                DerivedStateMismatch::NeedDiscardSet
            ))
        ));
        engine.close_open_bucket(position).unwrap();
        set_need_discard_index(&engine, position.raw(), false);
        assert!(engine.verify_all().is_ok());
        drop(engine);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn verify_all_runs_later_checks_after_an_early_failure() {
        let (engine, path) = prepared_bucket_engine("verify-all-continue", 4);
        engine.add_free_bucket(5);
        let position = KeyPosition::new(0, 5, 0);
        set_need_discard_index(&engine, position.raw(), true);
        engine.open_bucket(position).unwrap();
        let result = engine.verify_all();
        assert!(
            matches!(
                result,
                Err(EngineError::DerivedState(
                    DerivedStateMismatch::NeedDiscardSet
                ))
            ),
            "bucket index check precedes the guard check and must win"
        );
        engine.close_open_bucket(position).unwrap();
        set_need_discard_index(&engine, position.raw(), false);
        assert!(engine.verify_all().is_ok());
        drop(engine);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn verify_guard_invariants_rejects_open_free_bucket() {
        let (engine, path) = prepared_bucket_engine("guard-open-free", 4);
        engine.add_free_bucket(5);
        let position = KeyPosition::new(0, 5, 0);
        assert!(engine.verify_all().is_ok());
        engine.open_bucket(position).unwrap();
        assert!(matches!(
            engine.verify_guard_invariants(),
            Err(EngineError::DerivedState(
                DerivedStateMismatch::OpenBucketFree
            ))
        ));
        engine.close_open_bucket(position).unwrap();
        assert!(engine.verify_all().is_ok());
        drop(engine);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn verify_guard_invariants_rejects_notrw_free_bucket() {
        let (engine, path) = prepared_bucket_engine("guard-notrw-free", 4);
        engine.add_free_bucket(5);
        assert!(engine.verify_all().is_ok());
        engine.set_device_rw(0, false).unwrap();
        assert!(matches!(
            engine.verify_guard_invariants(),
            Err(EngineError::DerivedState(
                DerivedStateMismatch::NotRwBucketFree
            ))
        ));
        engine.set_device_rw(0, true).unwrap();
        assert!(engine.verify_all().is_ok());
        drop(engine);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn guard_query_open_bucket_count_and_queue_empty() {
        let (engine, path) = prepared_bucket_engine("guard-query", 4);
        engine.add_free_bucket(5);
        let position = engine.allocate_bucket(0).unwrap();
        assert_eq!(engine.open_bucket_count().unwrap(), 0);
        assert!(engine.discard_queue_empty().unwrap());
        engine.open_bucket(position).unwrap();
        assert_eq!(engine.open_bucket_count().unwrap(), 1);
        engine.close_open_bucket(position).unwrap();
        assert_eq!(engine.open_bucket_count().unwrap(), 0);
        engine.reclaim_bucket(position).unwrap();
        engine.queue_discard_bucket(position).unwrap();
        assert!(!engine.discard_queue_empty().unwrap());
        engine.run_discard_worker().unwrap();
        assert!(engine.discard_queue_empty().unwrap());
        assert!(engine.verify_all().is_ok());
        drop(engine);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn discard_worker_rejects_open_bucket_until_closed() {
        let (engine, path) = prepared_bucket_engine("discard-open", 4);
        engine.add_free_bucket(5);
        let position = engine.allocate_bucket(0).unwrap();
        engine.open_bucket(position).unwrap();
        assert!(matches!(
            engine.reclaim_bucket(position),
            Err(EngineError::Transaction(-16))
        ));
        assert!(matches!(
            engine.discard_bucket(position),
            Err(EngineError::Transaction(-11))
        ));
        assert!(engine.verify_all().is_ok());
        engine.close_open_bucket(position).unwrap();
        engine.reclaim_bucket(position).unwrap();
        assert!(engine.verify_all().is_ok());
        drop(engine);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn set_device_rw_false_refuses_open_bucket_on_device() {
        let (engine, path) = prepared_bucket_engine("rw-open-guard", 4);
        engine.add_free_bucket(5);
        let position = engine.allocate_bucket(0).unwrap();
        engine.open_bucket(position).unwrap();
        assert!(matches!(
            engine.set_device_rw(0, false),
            Err(EngineError::Transaction(-16))
        ));
        assert!(engine.verify_all().is_ok());
        engine.close_open_bucket(position).unwrap();
        engine.set_device_rw(0, false).unwrap();
        assert!(matches!(
            engine.allocate_bucket(0),
            Err(EngineError::Transaction(-1))
        ));
        engine.set_device_rw(0, true).unwrap();
        engine.reclaim_bucket(position).unwrap();
        engine.discard_bucket(position).unwrap();
        assert!(engine.verify_all().is_ok());
        drop(engine);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn rw_devs_initialized_from_devs_online() {
        let engine = StorageEngine::new().unwrap();
        assert!(
            engine.inner.rw_devs.lock().unwrap().is_empty(),
            "memory engine has no online devices, rw_devs must derive from devs_online"
        );
        drop(engine);
        let path = persistent_test_path("rw-init");
        let file = fs::File::create(&path).unwrap();
        file.set_len(32 * 1024 * 1024).unwrap();
        drop(file);
        let engine = StorageEngine::create_persistent(&path).unwrap();
        assert_eq!(
            *engine.inner.rw_devs.lock().unwrap(),
            BTreeSet::from([0]),
            "create_persistent puts dev 0 online (devs_online.d[0]), rw_devs must follow"
        );
        drop(engine);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn persistent_engine_derives_rw_devs_from_devs_online() {
        let (engine, path) = prepared_bucket_engine("rw-derive", 4);
        engine.add_free_bucket(5);
        let position = engine.allocate_bucket(0).unwrap();
        assert_eq!(position.inode, 0);
        engine.set_device_rw(0, false).unwrap();
        assert!(matches!(
            engine.allocate_bucket(0),
            Err(EngineError::Transaction(-1))
        ));
        engine.set_device_rw(0, true).unwrap();
        let position = engine.allocate_bucket(0).unwrap();
        assert_eq!(position.inode, 0);
        engine.close_open_bucket(position).unwrap();
        engine.reclaim_bucket(position).unwrap();
        assert!(engine.verify_all().is_ok());
        drop(engine);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn drop_detects_unclosed_open_bucket_leak() {
        let (engine, path) = prepared_bucket_engine("drop-leak", 4);
        engine.add_free_bucket(5);
        let position = engine.allocate_bucket(0).unwrap();
        engine.open_bucket(position).unwrap();
        assert_eq!(
            engine.open_bucket_count().unwrap(),
            1,
            "query API must report the leak before drop"
        );
        /* The reclaim worker periodically upgrades its Weak handle and holds
         * an Arc for the duration of one poll; dropping the engine inside
         * that window would merely decrement the count and skip the leak
         * assertion entirely.  Stop and join the worker first so this drop
         * is the one that triggers EngineState::drop. */
        {
            let mut state = engine.inner.reclaim.state.lock().unwrap();
            state.stopping = true;
        }
        engine.inner.reclaim.wake.notify_all();
        {
            let mut worker = engine.inner.reclaim.worker.lock().unwrap();
            if let Some(worker) = worker.take() {
                worker.join().unwrap();
            }
        }
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(engine);
        }));
        let message = caught
            .err()
            .expect("drop with unclosed open bucket must panic");
        let message = message
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| message.downcast_ref::<&str>().copied())
            .unwrap_or_default();
        assert!(
            message.contains("open bucket leak"),
            "unexpected panic message: {message}"
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn close_open_bucket_then_drop_is_clean() {
        let (engine, path) = prepared_bucket_engine("drop-clean", 4);
        engine.add_free_bucket(5);
        let position = engine.allocate_bucket(0).unwrap();
        engine.open_bucket(position).unwrap();
        engine.close_open_bucket(position).unwrap();
        drop(engine);
        let recovered = StorageEngine::open_persistent(&path).unwrap();
        assert!(recovered.verify_all().is_ok());
        drop(recovered);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn discard_reclaim_transaction_fault_leaves_no_half_state() {
        let (engine, path) = prepared_bucket_engine("discard-fault", 4);
        engine.add_free_bucket(5);
        let position = engine.allocate_bucket(0).unwrap();
        engine
            .inject_fault(FaultPoint::TransactionRestart, 1)
            .unwrap();
        assert!(engine.reclaim_bucket(position).is_ok());
        assert!(engine.verify_all().is_ok());
        engine.inject_fault(FaultPoint::JournalWrite, 1).unwrap();
        assert!(engine.flush_journal().is_err());
        assert!(engine.verify_all().is_ok());
        engine.flush_journal().unwrap();
        assert!(engine.discard_bucket(position).is_ok());
        assert!(engine.verify_all().is_ok());
        drop(engine);
        let recovered = StorageEngine::open_persistent(&path).unwrap();
        assert!(recovered.verify_all().is_ok());
        drop(recovered);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn discard_worker_skips_open_and_notrw_but_drains_ready_buckets() {
        let (engine, path) = prepared_bucket_engine("discard-guard", 4);
        engine.add_free_bucket(5);
        engine.add_free_bucket(6);
        let open = engine.allocate_bucket(0).unwrap();
        let ready = engine.allocate_bucket(0).unwrap();
        engine.reclaim_bucket(open).unwrap();
        engine.reclaim_bucket(ready).unwrap();
        engine.open_bucket(open).unwrap();
        engine.queue_discard_bucket(open).unwrap();
        engine.queue_discard_bucket(ready).unwrap();
        assert!(matches!(
            engine.run_discard_worker(),
            Err(EngineError::Transaction(-11))
        ));
        assert!(
            matches!(
                engine.queue_discard_bucket(open),
                Err(EngineError::Transaction(-17))
            ),
            "open bucket should stay queued"
        );
        engine.close_open_bucket(open).unwrap();
        assert!(engine.run_discard_worker().is_ok());
        assert!(engine.verify_all().is_ok());
        assert!(
            engine.discard_queue_empty().unwrap(),
            "worker Ok implies drained queue"
        );
        assert!(engine.queue_discard_bucket(open).is_ok());
        drop(engine);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn discard_worker_rotates_notrw_device_buckets_until_rw_restored() {
        let (engine, path) = prepared_bucket_engine("discard-notrw-rotate", 4);
        engine.add_free_bucket(5);
        let position = engine.allocate_bucket(0).unwrap();
        engine.reclaim_bucket(position).unwrap();
        engine.set_device_rw(0, false).unwrap();
        engine.queue_discard_bucket(position).unwrap();
        assert!(matches!(
            engine.run_discard_worker(),
            Err(EngineError::Transaction(-11))
        ));
        assert!(matches!(
            engine.queue_discard_bucket(position),
            Err(EngineError::Transaction(-17))
        ));
        engine.set_device_rw(0, true).unwrap();
        assert!(engine.run_discard_worker().is_ok());
        assert!(engine.verify_all().is_ok());
        assert!(
            engine.discard_queue_empty().unwrap(),
            "worker Ok implies drained queue"
        );
        drop(engine);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn discard_worker_requires_rw_device() {
        let (engine, path) = prepared_bucket_engine("discard-notrw", 4);
        engine.add_free_bucket(5);
        let position = engine.allocate_bucket(0).unwrap();
        engine.reclaim_bucket(position).unwrap();
        engine.set_device_rw(0, false).unwrap();
        assert!(matches!(
            engine.discard_bucket(position),
            Err(EngineError::Transaction(-11))
        ));
        assert!(matches!(
            engine.reclaim_bucket(position),
            Err(EngineError::Transaction(-16))
        ));
        assert!(matches!(
            engine.allocate_bucket(0),
            Err(EngineError::Transaction(-1))
        ));
        assert!(engine.verify_bucket_indexes().is_ok());
        engine.set_device_rw(0, true).unwrap();
        engine.discard_bucket(position).unwrap();
        assert!(engine.verify_all().is_ok());
        drop(engine);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn discard_worker_fifo_pass_drains_entire_queue() {
        let (engine, path) = prepared_bucket_engine("discard-fifo", 4);
        engine.add_free_bucket(5);
        engine.add_free_bucket(6);
        engine.add_free_bucket(7);
        let mut positions = Vec::new();
        for _ in 0..3 {
            let position = engine.allocate_bucket(0).unwrap();
            engine.reclaim_bucket(position).unwrap();
            engine.queue_discard_bucket(position).unwrap();
            positions.push(position);
        }
        engine.run_discard_worker().unwrap();
        engine.verify_all().unwrap();
        for position in &positions {
            assert!(
                engine.queue_discard_bucket(*position).is_ok(),
                "queue should be drained after one pass: {position:?}"
            );
        }
        drop(engine);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn discard_worker_eagain_rotates_to_tail_without_blocking_ready_buckets() {
        let (engine, path) = prepared_bucket_engine("discard-eagain", 4);
        engine.add_free_bucket(5);
        let ready = engine.allocate_bucket(0).unwrap();
        let not_ready = KeyPosition::new(0, if ready.offset == 4 { 5 } else { 4 }, 0);
        engine.reclaim_bucket(ready).unwrap();
        engine.queue_discard_bucket(ready).unwrap();
        engine.queue_discard_bucket(not_ready).unwrap();
        assert!(matches!(
            engine.run_discard_worker(),
            Err(EngineError::Transaction(-11))
        ));
        assert!(
            matches!(
                engine.queue_discard_bucket(not_ready),
                Err(EngineError::Transaction(-17))
            ),
            "not-ready bucket should stay queued"
        );
        engine.allocate_bucket(0).unwrap();
        engine.reclaim_bucket(not_ready).unwrap();
        assert!(engine.run_discard_worker().is_ok());
        assert!(engine.verify_all().is_ok());
        assert!(
            engine.discard_queue_empty().unwrap(),
            "worker Ok implies drained queue"
        );
        for position in [ready, not_ready] {
            assert!(
                engine.queue_discard_bucket(position).is_ok(),
                "queue should be drained: {position:?}"
            );
        }
        drop(engine);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn discard_worker_rediscovers_need_discard_after_restart() {
        let (engine, path) = prepared_bucket_engine("discard-restart", 4);
        let position = KeyPosition::new(0, 4, 0);
        engine.allocate_bucket(0).unwrap();
        engine.reclaim_bucket(position).unwrap();
        engine.flush_journal().unwrap();
        drop(engine);

        let recovered = StorageEngine::open_persistent(&path).unwrap();
        assert_eq!(recovered.discover_discard_buckets().unwrap(), 1);
        recovered.run_discard_worker_once().unwrap();
        assert!(recovered.verify_all().is_ok());
        drop(recovered);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn discard_worker_drained_persistent_image_reopens_verified() {
        let (engine, path) = prepared_bucket_engine("discard-reopen-verify", 4);
        let position = KeyPosition::new(0, 4, 0);
        engine.allocate_bucket(0).unwrap();
        engine.reclaim_bucket(position).unwrap();
        engine.queue_discard_bucket(position).unwrap();
        engine.run_discard_worker().unwrap();
        assert!(engine.discard_queue_empty().unwrap());
        assert!(engine.verify_all().is_ok());
        engine.flush_journal().unwrap();
        drop(engine);

        let reopened = StorageEngine::open_persistent(&path).unwrap();
        assert!(reopened.verify_all().is_ok());
        assert!(reopened.discard_queue_empty().unwrap());
        drop(reopened);
        fs::remove_file(path).unwrap();
    }

    /// Matches a model-derived guard verdict against the implementation's
    /// verification result.  A mismatch closes every open bucket before
    /// panicking so the engine drop never aborts with an open-bucket-leak
    /// assertion that masks the real failure message (T0197).
    fn expect_verdict<F>(
        engine: &StorageEngine,
        open: &[bool; 4],
        buckets: &[KeyPosition; 4],
        expected: Option<DerivedStateMismatch>,
        check: F,
        context: &str,
    ) where
        F: Fn(&StorageEngine) -> Result<(), EngineError>,
    {
        let ok = match expected {
            None => check(engine).is_ok(),
            Some(expected) => matches!(
                check(engine),
                Err(EngineError::DerivedState(actual)) if actual == expected
            ),
        };
        if !ok {
            for index in 0..4 {
                if open[index] {
                    let _ = engine.close_open_bucket(buckets[index]);
                }
            }
            panic!(
                "{context}: model expected {expected:?}, engine reported {:?}",
                check(engine)
            );
        }
    }

    /// Panic-safe model engine: closes every open bucket on drop so a
    /// proptest failure mid-sequence can never abort with the engine's
    /// open-bucket-leak assertion masking the real failure message (T0197).
    struct ModelEngine {
        engine: Option<StorageEngine>,
        open: [bool; 4],
    }

    impl ModelEngine {
        fn new(engine: StorageEngine) -> Self {
            Self {
                engine: Some(engine),
                open: [false; 4],
            }
        }

        fn open_bucket(&mut self, index: usize) {
            self.engine
                .as_ref()
                .unwrap()
                .open_bucket(KeyPosition::new(0, 4 + index as u64, 0))
                .unwrap();
            self.open[index] = true;
        }

        fn close_open_bucket(&mut self, index: usize) {
            self.engine
                .as_ref()
                .unwrap()
                .close_open_bucket(KeyPosition::new(0, 4 + index as u64, 0))
                .unwrap();
            self.open[index] = false;
        }

        fn reopen(&mut self, path: &Path) {
            for index in 0..4 {
                if self.open[index] {
                    self.engine
                        .as_ref()
                        .unwrap()
                        .close_open_bucket(KeyPosition::new(0, 4 + index as u64, 0))
                        .unwrap();
                    self.open[index] = false;
                }
            }
            let old = self.engine.take().unwrap();
            old.flush_journal().unwrap();
            drop(old);
            self.engine = Some(StorageEngine::open_persistent(path).unwrap());
        }
    }

    impl Drop for ModelEngine {
        fn drop(&mut self) {
            if let Some(engine) = self.engine.as_ref() {
                for index in 0..4 {
                    if self.open[index] {
                        let _ = engine.close_open_bucket(KeyPosition::new(0, 4 + index as u64, 0));
                    }
                }
            }
        }
    }

    impl std::ops::Deref for ModelEngine {
        type Target = StorageEngine;
        fn deref(&self) -> &StorageEngine {
            self.engine.as_ref().unwrap()
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 16,
            max_shrink_iters: 64,
            ..ProptestConfig::default()
        })]

        #[test]
        fn open_bucket_discard_model_protects_open_from_reuse(
            operations in prop::collection::vec((0u8..8, 0u8..4), 1..=40),
        ) {            let (engine, path) = prepared_bucket_engine("discard-open-model", 4);
            engine.add_free_bucket( 5);
            engine.add_free_bucket( 6);
            engine.add_free_bucket( 7);
            let buckets = [
                KeyPosition::new(0, 4, 0),
                KeyPosition::new(0, 5, 0),
                KeyPosition::new(0, 6, 0),
                KeyPosition::new(0, 7, 0),
            ];
            let mut engine = ModelEngine::new(engine);
            let mut state = [0u8; 4]; // 0=free, 1=btree-owned, 2=need-discard
            let mut open = [false; 4];
            let mut queued = [false; 4];
            let mut device_rw = true;
            let mut shadow_queue: VecDeque<usize> = VecDeque::new();
            for (kind, bucket) in operations {
                let index = bucket as usize;
                match kind {
                    0 => {
                        let result = engine.queue_discard_bucket(buckets[index]);
                        if queued[index] {
                            prop_assert!(matches!(
                                result,
                                Err(EngineError::Transaction(-17))
                            ));
                        } else {
                            prop_assert!(result.is_ok());
                            queued[index] = true;
                            shadow_queue.push_back(index);
                        }
                    }
                    1 => {
                        let mut deferred = false;
                        let round = shadow_queue.len();
                        for _ in 0..round {
                            if let Some(head) = shadow_queue.pop_front() {
                                if state[head] == 2 && !open[head] && device_rw {
                                    queued[head] = false;
                                    state[head] = 0;
                                } else {
                                    shadow_queue.push_back(head);
                                    deferred = true;
                                }
                            }
                        }
                        let result = engine.run_discard_worker();
                        if deferred {
                            prop_assert!(matches!(
                                result,
                                Err(EngineError::Transaction(-11))
                            ));
                        } else {
                            prop_assert!(result.is_ok());
                        }
                    }
                    2 => {
                        let result = engine.reclaim_bucket(buckets[index]);
                        if open[index] || !device_rw {
                            prop_assert!(matches!(
                                result,
                                Err(EngineError::Transaction(-16))
                            ));
                        } else {
                            prop_assert!(result.is_ok());
                            state[index] = if state[index] == 2 { 0 } else { 2 };
                        }
                    }
                    3 => {
                        let result = engine.allocate_bucket(0);
                        let free_count = state.iter().filter(|&&s| s == 0).count();
                        if !device_rw || free_count == 0 {
                            prop_assert!(result.is_err());
                        } else {
                            let position = result.unwrap();
                            let allocated = buckets
                                .iter()
                                .position(|b| *b == position)
                                .expect("allocated bucket is in the model");
                            prop_assert_eq!(state[allocated], 0);
                            state[allocated] = 1;
                        }
                    }
                    4 => {
                        engine.reopen(&path);
                        /* open_persistent re-derives rw_devs from
                         * devs_online (engine.rs:1687-1700), so a
                         * process-style restart puts dev 0 back rw. */
                        device_rw = true;
                        let discovered = engine.discover_discard_buckets().unwrap();
                        let expected = state.iter().filter(|&&s| s == 2).count();
                        prop_assert_eq!(discovered, expected);
                        state = [0u8; 4];
                        queued = [false; 4];
                        shadow_queue.clear();
                        for index in 0..4 {
                            let bucket = buckets[index];
                            let mut fs = engine.lock_fs().unwrap();
                            let mut alloc = None;
                            unsafe {
                                for raw in scan_raw_locked(&mut **fs, 4).unwrap() {
                                    let key = raw.words.as_ptr().cast::<bkey_i>();
                                    if (*key).k.type_ == KEY_TYPE_alloc_v4
                                        && (*key).k.p == bucket.raw()
                                    {
                                        let value = (key as *const u8)
                                            .add(core::mem::size_of::<bkey>())
                                            .cast::<bch_alloc_v4>();
                                        alloc = Some(core::ptr::read_unaligned(value));
                                        break;
                                    }
                                }
                            }
                            drop(fs);
                            let alloc = alloc.expect("model bucket alloc exists");
                            state[index] = if alloc.data_type == BCH_DATA_FREE {
                                0
                            } else if alloc.data_type == BCH_DATA_NEED_DISCARD {
                                queued[index] = true;
                                shadow_queue.push_back(index);
                                2
                            } else {
                                1
                            };
                            open[index] = false;
                        }
                    }
                    5 => {
                        /* Guard decision injection (T0197): the model no
                         * longer pre-judges openability from its shadow
                         * state; it unconditionally opens and lets the
                         * implementation's guard invariants adjudicate
                         * below (open_bucket is an unguarded insert,
                         * engine.rs:901). */
                        engine.open_bucket(index);
                        open[index] = true;
                    }
                    6 => {
                        if open[index] {
                            engine.close_open_bucket(index);
                            open[index] = false;
                        }
                    }
                    7 => {
                        /* not_rw dimension (T0197 AC-3): toggle dev 0
                         * writability and let set_device_rw's open-bucket
                         * refusal (engine.rs:944-949) drive the model. */
                        let rw = index & 1 == 0;
                        let result = engine.set_device_rw(0, rw);
                        if rw {
                            prop_assert!(result.is_ok());
                            device_rw = true;
                        } else if open.iter().any(|&o| o) {
                            prop_assert!(matches!(
                                result,
                                Err(EngineError::Transaction(-16))
                            ));
                        } else {
                            prop_assert!(result.is_ok());
                            device_rw = false;
                        }
                    }
                    _ => {}
                }
                /* Expectation-driven adjudication: the model derives the
                 * implementation's guard verdict (first violating free
                 * bucket in tree order: open wins over not_rw, guard
                 * invariants, engine.rs:713-722) and matches it against
                 * verify_all's first error. */
                let expected_error = {
                    let mut expected = None;
                    for index in 0..4 {
                        if state[index] == 0 {
                            if open[index] {
                                expected =
                                    Some(DerivedStateMismatch::OpenBucketFree);
                                break;
                            }
                            if !device_rw {
                                expected =
                                    Some(DerivedStateMismatch::NotRwBucketFree);
                                break;
                            }
                        }
                    }
                    expected
                };
                expect_verdict(
                    &engine,
                    &open,
                    &buckets,
                    expected_error,
                    |e| e.verify_all(),
                    "verify_all",
                );
                let mut fs = engine.lock_fs().unwrap();
                for index in 0..4 {
                    let mut alloc = None;
                    unsafe {
                        for raw in scan_raw_locked(&mut **fs, 4).unwrap() {
                            let key = raw.words.as_ptr().cast::<bkey_i>();
                            if (*key).k.type_ == KEY_TYPE_alloc_v4
                                && (*key).k.p == buckets[index].raw()
                            {
                                let value = (key as *const u8)
                                    .add(core::mem::size_of::<bkey>())
                                    .cast::<bch_alloc_v4>();
                                alloc = Some(core::ptr::read_unaligned(value));
                                break;
                            }
                        }
                    }
                    let alloc = alloc.expect("model bucket alloc exists");
                    if state[index] == 2 {
                        prop_assert_eq!(alloc.data_type, BCH_DATA_NEED_DISCARD);
                    }
                }
                drop(fs);
                expect_verdict(
                    &engine,
                    &open,
                    &buckets,
                    expected_error,
                    |e| e.verify_guard_invariants(),
                    "verify_guard_invariants",
                );
            }
            drop(engine);
            fs::remove_file(path).unwrap();
        }
    }

    /// Deterministic not_rw-dimension companion to the proptest above
    /// (T0197 AC-3): every guard verdict in this scenario is expected by
    /// the model and produced by the implementation's own adjudication.
    #[test]
    fn not_rw_dimension_guard_verdicts_are_implementation_adjudicated() {
        let (engine, path) = prepared_bucket_engine("discard-notrw-model", 4);
        engine.add_free_bucket(5);
        let mut engine = ModelEngine::new(engine);
        let bucket5 = KeyPosition::new(0, 5, 0);
        engine.set_device_rw(0, false).unwrap();
        assert!(matches!(
            engine.verify_all(),
            Err(EngineError::DerivedState(
                DerivedStateMismatch::NotRwBucketFree
            ))
        ));
        /* open wins over not_rw in tree-order verdict (engine.rs:713-722) */
        engine.open_bucket(0);
        assert!(matches!(
            engine.verify_all(),
            Err(EngineError::DerivedState(
                DerivedStateMismatch::OpenBucketFree
            ))
        ));
        engine.close_open_bucket(0);
        /* an open bucket refuses dev removal, like bch2_dev_allocator_remove()
         * waiting for open write points to drain */
        engine.open_bucket(1);
        assert!(matches!(
            engine.set_device_rw(0, false),
            Err(EngineError::Transaction(-16))
        ));
        engine.close_open_bucket(1);
        engine.set_device_rw(0, false).unwrap();
        /* not_rw preserves the failure semantics of allocate (-1) and
         * reclaim (-16), and the discard worker keeps rotating (EAGAIN) */
        assert!(engine.allocate_bucket(0).is_err());
        assert!(matches!(
            engine.reclaim_bucket(bucket5),
            Err(EngineError::Transaction(-16))
        ));
        engine.queue_discard_bucket(bucket5).unwrap();
        assert!(matches!(
            engine.run_discard_worker(),
            Err(EngineError::Transaction(-11))
        ));
        /* a process-style restart re-derives rw_devs from devs_online
         * (engine.rs:1687-1700): dev 0 is rw again */
        engine.reopen(&path);
        assert!(engine.allocate_bucket(0).is_ok());
        drop(engine);
        fs::remove_file(path).unwrap();
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 16,
            max_shrink_iters: 64,
            ..ProptestConfig::default()
        })]

        #[test]
        fn multi_bucket_discard_worker_model_converges(
            operations in prop::collection::vec((0u8..6, 0u8..4), 1..=40),
        ) {
            let (engine, path) = prepared_bucket_engine("discard-model", 4);
            engine.add_free_bucket( 5);
            engine.add_free_bucket( 6);
            engine.add_free_bucket( 7);
            let buckets = [
                KeyPosition::new(0, 4, 0),
                KeyPosition::new(0, 5, 0),
                KeyPosition::new(0, 6, 0),
                KeyPosition::new(0, 7, 0),
            ];
            let mut engine = engine;
            let mut state = [0u8; 4]; // 0=free, 1=btree-owned, 2=need-discard
            let mut queued = [false; 4];
            let mut shadow_queue: VecDeque<usize> = VecDeque::new();
            for (kind, bucket) in operations {
                match kind {
                    0 => {
                        let result = engine.queue_discard_bucket(buckets[bucket as usize]);
                        if queued[bucket as usize] {
                            prop_assert!(matches!(
                                result,
                                Err(EngineError::Transaction(-17))
                            ));
                        } else {
                            prop_assert!(result.is_ok());
                            queued[bucket as usize] = true;
                            shadow_queue.push_back(bucket as usize);
                        }
                    }
                    1 => {
                        let mut deferred = false;
                        let round = shadow_queue.len();
                        for _ in 0..round {
                            if let Some(head) = shadow_queue.pop_front() {
                                if state[head] == 2 {
                                    queued[head] = false;
                                    state[head] = 0;
                                } else {
                                    shadow_queue.push_back(head);
                                    deferred = true;
                                }
                            }
                        }
                        let result = engine.run_discard_worker();
                        if deferred {
                            prop_assert!(matches!(
                                result,
                                Err(EngineError::Transaction(-11))
                            ));
                        } else {
                            prop_assert!(result.is_ok());
                        }
                    }
                    2 => {
                        let index = bucket as usize;
                        let result = engine.reclaim_bucket(buckets[index]);
                        prop_assert!(result.is_ok());
                        state[index] = if state[index] == 2 { 0 } else { 2 };
                    }
                    3 => {
                        let result = engine.allocate_bucket(0);
                        let free_count = state.iter().filter(|&&s| s == 0).count();
                        if free_count == 0 {
                            prop_assert!(result.is_err());
                        } else {
                            let position = result.unwrap();
                            let index = buckets
                                .iter()
                                .position(|b| *b == position)
                                .expect("allocated bucket is in the model");
                            prop_assert_eq!(state[index], 0);
                            state[index] = 1;
                        }
                    }
                    4 => {
                        engine.flush_journal().unwrap();
                        drop(engine);
                        let recovered = StorageEngine::open_persistent(&path).unwrap();
                        let discovered = recovered.discover_discard_buckets().unwrap();
                        let expected = state.iter().filter(|&&s| s == 2).count();
                        prop_assert_eq!(discovered, expected);
                        state = [0u8; 4];
                        queued = [false; 4];
                        shadow_queue.clear();
                        for index in 0..4 {
                            let bucket = buckets[index];
                            let mut fs = recovered.lock_fs().unwrap();
                            let mut alloc = None;
                            unsafe {
                                for raw in scan_raw_locked(&mut **fs, 4).unwrap() {
                                    let key = raw.words.as_ptr().cast::<bkey_i>();
                                    if (*key).k.type_ == KEY_TYPE_alloc_v4
                                        && (*key).k.p == bucket.raw()
                                    {
                                        let value = (key as *const u8)
                                            .add(core::mem::size_of::<bkey>())
                                            .cast::<bch_alloc_v4>();
                                        alloc = Some(core::ptr::read_unaligned(value));
                                        break;
                                    }
                                }
                            }
                            drop(fs);
                            let alloc = alloc.expect("model bucket alloc exists");
                            state[index] = if alloc.data_type == BCH_DATA_FREE {
                                0
                            } else if alloc.data_type == BCH_DATA_NEED_DISCARD {
                                queued[index] = true;
                                shadow_queue.push_back(index);
                                2
                            } else {
                                1
                            };
                        }
                        engine = recovered;
                    }
                    _ => {}
                }
                prop_assert!(engine.verify_all().is_ok());
            }
            drop(engine);
            fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn recovery_fault_matrix_never_publishes_success() {
        let engine = StorageEngine::new().unwrap();
        let mut tx = engine.transaction();
        tx.put(BtreeId::DEFAULT, key(41, &[7, 8, 9]));
        tx.commit_sync().unwrap();
        let snapshot = engine.durable_journal().unwrap();
        StorageEngine::recover(&snapshot)
            .unwrap()
            .verify_derived_state()
            .unwrap();
        for fault in [
            RecoveryFaultPoint::AfterJournalReplay,
            RecoveryFaultPoint::DuringDerivedRebuild,
            RecoveryFaultPoint::BeforePublication,
        ] {
            assert!(
                StorageEngine::recover_with_fault(&snapshot, Some(fault)).is_err(),
                "fault {fault:?} unexpectedly published"
            );
        }
    }

    #[test]
    fn freespace_index_position_round_trips_bucket_and_generation_bits() {
        let alloc = bch_alloc_v4 {
            gen: 0x31,
            oldest_gen: 0x11,
            ..Default::default()
        };
        let position = bpos {
            inode: 2,
            offset: 37,
            snapshot: 0,
        };
        let indexed = alloc_freespace_pos(position, &alloc);
        let position_inode = position.inode;
        let position_offset = position.offset;
        let indexed_inode = indexed.inode;
        let indexed_offset = indexed.offset;
        assert_eq!(indexed_inode, position_inode);
        assert_eq!(indexed_offset & ((1u64 << 56) - 1), position_offset);
        assert_eq!(indexed_offset >> 56, 2);
    }

    #[test]
    fn generated_transaction_journal_recovery_matches_the_model() {
        for seed in 1..=12u64 {
            let engine = StorageEngine::new().unwrap();
            let mut model = BTreeMap::<KeyPosition, BtreeKey>::new();
            let mut state = seed;

            for step in 0..80u64 {
                state ^= state << 7;
                state ^= state >> 9;
                state ^= state << 8;
                let position = KeyPosition::new(1, state % 48 + 1, 0);
                if step % 17 == 0 {
                    engine
                        .inject_fault(FaultPoint::TransactionRestart, 1)
                        .unwrap();
                }

                let mut transaction = engine.transaction();
                if state & 1 == 0 {
                    let expected = key(position.offset, &[seed, step, state]);
                    transaction.put(BtreeId::DEFAULT, expected.clone());
                    model.insert(position, expected);
                } else {
                    transaction.delete(BtreeId::DEFAULT, position);
                    model.remove(&position);
                }
                transaction.commit().unwrap();

                if step % 8 == 7 {
                    engine.flush_journal().unwrap();
                    let image = engine.durable_journal().unwrap();
                    let recovered = StorageEngine::recover(&image).unwrap();
                    let expected = model.values().cloned().collect::<Vec<_>>();
                    assert_eq!(recovered.scan(BtreeId::DEFAULT).unwrap(), expected);
                    recovered.verify(BtreeId::DEFAULT).unwrap();
                }
            }

            engine.flush_journal().unwrap();
            let recovered = StorageEngine::recover(&engine.durable_journal().unwrap()).unwrap();
            assert_eq!(
                recovered.scan(BtreeId::DEFAULT).unwrap(),
                model.values().cloned().collect::<Vec<_>>()
            );
            recovered.verify(BtreeId::DEFAULT).unwrap();
        }
    }

    #[test]
    fn generated_reclaim_recovery_matches_the_model() {
        for seed in 1..=6u64 {
            let engine = StorageEngine::new().unwrap();
            let mut model = BTreeMap::<KeyPosition, BtreeKey>::new();
            let mut state = seed ^ 0x9e37_79b9;

            for step in 0..64u64 {
                state ^= state << 7;
                state ^= state >> 11;
                state ^= state << 9;
                let position = KeyPosition::new(2, state % 40 + 1, 0);
                let mut transaction = engine.transaction();
                if state & 1 == 0 {
                    let expected = key(position.offset, &[seed, step, state]);
                    transaction.put(BtreeId::DEFAULT, expected.clone());
                    model.insert(position, expected);
                } else {
                    transaction.delete(BtreeId::DEFAULT, position);
                    model.remove(&position);
                }
                transaction.commit().unwrap();

                if step % 8 == 7 {
                    engine.flush_journal().unwrap();
                }
                if step % 16 == 15 {
                    engine.reclaim_journal().unwrap();
                    let image = engine.durable_journal().unwrap();
                    let recovered = StorageEngine::recover(&image).unwrap();
                    assert_eq!(
                        recovered.scan(BtreeId::DEFAULT).unwrap(),
                        model.values().cloned().collect::<Vec<_>>()
                    );
                    recovered.verify(BtreeId::DEFAULT).unwrap();
                }
            }

            engine.flush_journal().unwrap();
            let recovered = StorageEngine::recover(&engine.durable_journal().unwrap()).unwrap();
            assert_eq!(
                recovered.scan(BtreeId::DEFAULT).unwrap(),
                model.values().cloned().collect::<Vec<_>>()
            );
            recovered.verify(BtreeId::DEFAULT).unwrap();
        }
    }

    #[test]
    fn durability_api_and_metrics_report_the_committed_boundary() {
        let path = persistent_test_path("durability-reclaim");
        {
            let engine = StorageEngine::create_persistent(&path).unwrap();
            let mut transaction = engine.transaction();
            transaction.put(BtreeId::DEFAULT, key(401, &[1, 2, 3]));
            let durable = transaction.commit_sync().unwrap();
            assert_ne!(durable.journal_sequence_ondisk, 0);
            /* A synchronous flush writes the committed entry, then rotates
             * the ring: seq names the next slot, one past the durable
             * record. */
            assert_eq!(
                durable.journal_sequence,
                durable.journal_sequence_ondisk + 1
            );

            let before = engine.metrics().unwrap();
            assert_ne!(before.journal_records, 0);
            assert_eq!(before.journal_sequence, durable.journal_sequence);

            engine.reclaim_journal().unwrap();
            let after = engine.metrics().unwrap();
            assert_ne!(after.journal_last_sequence, 0);
            assert_eq!(engine.durable_journal().unwrap().record_count(), 1);
        }

        let recovered = StorageEngine::open_persistent(&path).unwrap();
        assert_eq!(
            recovered.scan(BtreeId::DEFAULT).unwrap(),
            vec![key(401, &[1, 2, 3])]
        );
        recovered.verify(BtreeId::DEFAULT).unwrap();
        drop(recovered);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn high_watermark_kicks_background_reclaim_and_preserves_the_tail() {
        let engine = StorageEngine::new().unwrap();
        engine.put_sync(BtreeId::DEFAULT, key(410, &[1])).unwrap();
        let before = engine.reclaim_status().unwrap();
        {
            let fs = engine.lock_fs().unwrap();
            fs.journal.flags.fetch_or(
                1usize << crate::journal::JOURNAL_med_on_space,
                std::sync::atomic::Ordering::AcqRel,
            );
        }

        /* commit_operations() observes the watermark after its transaction
         * path completes, just as a journal producer kicks the C reclaimer. */
        engine.put(BtreeId::DEFAULT, key(411, &[2, 3])).unwrap();
        let status = engine.wait_for_reclaim(Duration::from_secs(1)).unwrap();
        assert!(status.requested > before.requested);
        assert!(status.completed >= status.requested);
        assert_eq!(status.last_error, None);
        /* The reclaim pass published an empty anchor that repeats the root
         * set; a device-less engine keeps both original records too, since
         * its journal mirror is the only durable recovery source. */
        assert_eq!(engine.durable_journal().unwrap().record_count(), 3);
        assert_eq!(
            engine.scan(BtreeId::DEFAULT).unwrap(),
            vec![key(410, &[1]), key(411, &[2, 3])]
        );
    }

    #[test]
    fn background_reclaim_checkpoint_preserves_verified_state() {
        let (engine, path) = prepared_bucket_engine("reclaim-verify", 4);
        engine.put_sync(BtreeId::DEFAULT, key(420, &[1])).unwrap();
        let requested = engine.request_reclaim().unwrap();
        let status = engine.wait_for_reclaim(Duration::from_secs(1)).unwrap();
        assert!(status.completed >= requested);
        assert_eq!(status.last_error, None);
        assert!(engine.verify_all().is_ok());
        engine.flush_journal().unwrap();
        drop(engine);

        let reopened = StorageEngine::open_persistent(&path).unwrap();
        assert!(reopened.verify_all().is_ok());
        assert_eq!(
            reopened.scan(BtreeId::DEFAULT).unwrap(),
            vec![key(420, &[1])]
        );
        drop(reopened);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn corrupt_journal_tail_never_survives_recovery() {
        let path = persistent_test_path("corrupt-journal-tail");
        {
            let engine = StorageEngine::create_persistent(&path).unwrap();
            engine.put_sync(BtreeId::DEFAULT, key(600, &[1])).unwrap();
            engine.reclaim_journal().unwrap();
            engine.put_sync(BtreeId::DEFAULT, key(601, &[2])).unwrap();
        }

        clear_journal_region(&path);
        let file = OpenOptions::new().write(true).open(&path).unwrap();
        let journal_start = JOURNAL_BUCKET_START * JOURNAL_BUCKET_SIZE as u64 * 512;
        assert_eq!(file.write_at(&[0xa5; 64], journal_start).unwrap(), 64);
        file.sync_all().unwrap();
        drop(file);

        /* With the journal storage region erased there is no longer any
         * durable root set or replay window: the corrupt journal must be
         * rejected, and an empty journal must not fabricate data. */
        match StorageEngine::open_persistent(&path) {
            Ok(recovered) => {
                assert!(recovered.scan(BtreeId::DEFAULT).unwrap().is_empty());
            }
            Err(EngineError::Journal(_)) => {}
            Err(error) => panic!("unexpected corrupted-journal result: {error}"),
        }
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn concurrent_rcu_read_transactions_and_writers_keep_iterator_order() {
        let engine = Arc::new(StorageEngine::new().unwrap());
        let mut workers = Vec::new();

        for writer in 0..4u64 {
            let engine = Arc::clone(&engine);
            workers.push(std::thread::spawn(move || {
                for offset in 0..24u64 {
                    engine
                        .put(
                            BtreeId::DEFAULT,
                            BtreeKey::new(
                                KeyPosition::new(writer + 1, offset + 1, 0),
                                vec![writer, offset],
                            )
                            .unwrap(),
                        )
                        .unwrap();
                }
                engine.sync().unwrap();
            }));
        }
        for _ in 0..3 {
            let engine = Arc::clone(&engine);
            workers.push(std::thread::spawn(move || {
                for _ in 0..32 {
                    let reader = engine.read_transaction();
                    let keys = reader.scan(BtreeId::DEFAULT).unwrap();
                    assert!(keys
                        .windows(2)
                        .all(|pair| pair[0].position() < pair[1].position()));
                }
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }

        engine.sync().unwrap();
        assert_eq!(engine.scan(BtreeId::DEFAULT).unwrap().len(), 96);
        engine.verify(BtreeId::DEFAULT).unwrap();
    }

    #[test]
    fn discard_commit_restart_injected_worker_retries_and_drains() {
        /* T0199: DiscardCommitRestart 注入命中 discard 路径的每桶
         * 事务提交边界（discard.c:598-657 fast_work 每桶一事务，
         * commit.c:1390 注入位置），-4 走既有 bch2_trans_begin 重试
         * 循环，最终队列排空且桶全部 freed。 */
        let (engine, path) = prepared_bucket_engine("interleave-inject", 4);
        for offset in 5..=7 {
            engine.add_free_bucket(offset);
        }
        let mut positions = Vec::new();
        for _ in 0..4 {
            let position = engine.allocate_bucket(0).unwrap();
            engine.reclaim_bucket(position).unwrap();
            engine.queue_discard_bucket(position).unwrap();
            positions.push(position);
        }
        engine
            .inject_fault(FaultPoint::DiscardCommitRestart, 6)
            .unwrap();
        engine.run_discard_worker().unwrap();
        engine.verify_all().unwrap();
        for position in &positions {
            assert!(
                engine.queue_discard_bucket(*position).is_ok(),
                "queue should be drained: {position:?}"
            );
        }
        drop(engine);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn concurrent_writers_with_restart_injection_converge() {
        /* T0199 写者×写者：4 线程 Barrier 起跑并发 allocate/reclaim
         * 争用全局 fs 锁 + 共享 TransactionRestart 注入计数（谁先消费
         * 谁重启），断言只依赖最终一致：全部成功 + 派生树一致 + 无
         * 桶泄漏（对齐上游：并发下只保证最终一致，不保证到达顺序）。
         * 持久化几何固定 8 桶（8MB / 1MB 桶），可用 free 桶 4..=7；每轮
         * allocate 后二次 reclaim 归还 freespace（NEED_DISCARD→FREE +
         * freespace 补键，engine.rs:1043-1049），4 桶循环复用。 */
        let (engine, path) = prepared_bucket_engine("interleave-writers", 4);
        for offset in 5..=7 {
            engine.add_free_bucket(offset);
        }
        let engine = Arc::new(engine);
        let barrier = Arc::new(std::sync::Barrier::new(4));
        let mut workers = Vec::new();
        for _ in 0..4 {
            let engine = Arc::clone(&engine);
            let barrier = Arc::clone(&barrier);
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..6 {
                    let position = engine.allocate_bucket(0).unwrap();
                    engine.reclaim_bucket(position).unwrap();
                    engine.reclaim_bucket(position).unwrap();
                }
            }));
        }
        engine
            .inject_fault(FaultPoint::TransactionRestart, 12)
            .unwrap();
        for worker in workers {
            worker.join().unwrap();
        }
        engine.verify_all().unwrap();
        engine
            .inject_fault(FaultPoint::TransactionRestart, 0)
            .unwrap();
        drop(engine);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn concurrent_producers_and_discard_worker_with_injection_drain() {
        /* T0199 写者×worker：生产者线程并发入队（FIFO，对齐
         * bch2_fast_discard_bucket_add discard.c:643），主线程
         * run_discard_worker 并发排空 + DiscardCommitRestart 注入；
         * 断言只依赖最终一致：队列最终空 + 树一致 + 桶可重新入队。
         * 桶 4..=7 循环：discard 归还 freespace 后下一轮重新 reclaim。 */
        let (engine, path) = prepared_bucket_engine("interleave-producer", 4);
        for offset in 5..=7 {
            engine.add_free_bucket(offset);
        }
        let buckets = [4u64, 5, 6, 7];
        let engine = Arc::new(engine);
        for _round in 0..3 {
            for bucket in buckets {
                let position = engine.allocate_bucket(0).unwrap();
                engine.reclaim_bucket(position).unwrap();
                assert_eq!(position.offset, bucket);
            }
            let barrier = Arc::new(std::sync::Barrier::new(5));
            let mut workers = Vec::new();
            for bucket in buckets {
                let engine = Arc::clone(&engine);
                let barrier = Arc::clone(&barrier);
                workers.push(std::thread::spawn(move || {
                    barrier.wait();
                    engine
                        .queue_discard_bucket(KeyPosition::new(0, bucket, 0))
                        .unwrap();
                }));
            }
            barrier.wait();
            engine
                .inject_fault(FaultPoint::DiscardCommitRestart, 2)
                .unwrap();
            engine.run_discard_worker().unwrap();
            for worker in workers {
                worker.join().unwrap();
            }
            engine.run_discard_worker().unwrap();
            let pending = engine.inner.discard_inflight.lock().unwrap().0.len();
            assert_eq!(pending, 0, "queue should be fully drained");
        }
        engine.verify_all().unwrap();
        drop(engine);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn rcu_readers_with_writer_restart_injection_keep_order() {
        /* T0199 RCU 读者×写者：读者（RCU read guard，不持 fs 锁）与
         * 写者并发，写者事务注入 TransactionRestart；读者每次 scan
         * 必须有序（读一致快照），最终 96 键全部落盘且 verify 通过。 */
        let engine = Arc::new(StorageEngine::new().unwrap());
        let mut workers = Vec::new();
        for writer in 0..4u64 {
            let engine = Arc::clone(&engine);
            workers.push(std::thread::spawn(move || {
                for offset in 0..24u64 {
                    engine
                        .put(
                            BtreeId::DEFAULT,
                            BtreeKey::new(
                                KeyPosition::new(writer + 1, offset + 1, 0),
                                vec![writer, offset],
                            )
                            .unwrap(),
                        )
                        .unwrap();
                }
                engine.sync().unwrap();
            }));
        }
        for _ in 0..3 {
            let engine = Arc::clone(&engine);
            workers.push(std::thread::spawn(move || {
                for _ in 0..32 {
                    let reader = engine.read_transaction();
                    let keys = reader.scan(BtreeId::DEFAULT).unwrap();
                    assert!(keys
                        .windows(2)
                        .all(|pair| pair[0].position() < pair[1].position()));
                }
            }));
        }
        engine
            .inject_fault(FaultPoint::TransactionRestart, 24)
            .unwrap();
        for worker in workers {
            worker.join().unwrap();
        }

        engine.sync().unwrap();
        assert_eq!(engine.scan(BtreeId::DEFAULT).unwrap().len(), 96);
        engine.verify(BtreeId::DEFAULT).unwrap();
    }

    #[test]
    fn single_transaction_many_keys_into_one_leaf_splits_without_overflowing() {
        /* D1 回归：同 leaf 多 update 的空间占用未累加（bcachefs
         * commit.c:1083-1097 有 `u64s += i->k->k.u64s`），单事务内连续
         * 键写满 512B 初始节点时 bch2_bset_insert 的 copy_nonoverlapping
         * 会越过 bset 尾部写坏堆（ASAN 可复现）；修复后按 acc_u64s
         * 检查并触发 split/grow。 */
        let engine = StorageEngine::new().unwrap();
        let mut transaction = engine.transaction();
        for offset in 1..=32u64 {
            transaction.put(
                BtreeId::DEFAULT,
                BtreeKey::new(KeyPosition::new(1, offset, 0), vec![offset; 4]).unwrap(),
            );
        }
        transaction.commit().unwrap();
        assert_eq!(engine.scan(BtreeId::DEFAULT).unwrap().len(), 32);
        engine.verify(BtreeId::DEFAULT).unwrap();
    }

    #[test]
    fn process_crash_child() {
        let Ok(path) = std::env::var("SUBVOL_ENGINE_CRASH_PATH") else {
            return;
        };
        let ready = std::env::var("SUBVOL_ENGINE_CRASH_READY").unwrap();
        let phase = std::env::var("SUBVOL_ENGINE_CRASH_PHASE").unwrap();
        let engine = StorageEngine::create_persistent(&path).unwrap();
        match phase.as_str() {
            "journal" => {
                engine.put_sync(BtreeId::DEFAULT, key(701, &[1])).unwrap();
            }
            "checkpoint" => {
                engine.put_sync(BtreeId::DEFAULT, key(702, &[2])).unwrap();
                engine.reclaim_journal().unwrap();
            }
            "tail" => {
                engine.put_sync(BtreeId::DEFAULT, key(703, &[3])).unwrap();
                engine.reclaim_journal().unwrap();
                engine.put_sync(BtreeId::DEFAULT, key(704, &[4])).unwrap();
            }
            "cc-flush-before" => {
                concurrent_crash_child(Arc::new(engine.clone()), "cc-flush-before");
            }
            "cc-flush-after" => {
                concurrent_crash_child(Arc::new(engine.clone()), "cc-flush-after");
            }
            "cc-mid-write" => {
                concurrent_crash_child(Arc::new(engine.clone()), "cc-mid-write");
            }
            "cc-single-put" => {
                /* deterministic unflushed crash point: a JournalWrite
                 * fault is armed so even a background-reclaim flush fails
                 * before any write; the record then provably never
                 * reaches disk (T0196 fault-matrix pattern, engine-local
                 * — the recovered image opens without the fault) */
                engine.inject_fault(FaultPoint::JournalWrite, 20).unwrap();
                let k = BtreeKey::new(KeyPosition::new(9, 1, 0), vec![9]);
                engine.put(BtreeId::DEFAULT, k.unwrap()).unwrap();
            }
            _ => panic!("unknown crash phase {phase}"),
        }
        let journal_diag = {
            let fs = engine.inner.fs.lock().unwrap();
            let j = &fs.journal;
            let space = j.space.lock().unwrap();
            let ja = j.device.lock().unwrap();
            /* 崩溃点诊断：SUBVOL_LOG=info 时打印 abort 前的 journal 状态，
             * 供人工审计。cc-single-put/cc-flush-before 注入 JournalWrite
             * 故障后 seq_ondisk 应保持 0（无任何记录落盘），flush-after
             * 应等于已提交 seq；cc-mid-write 无注入，后台 reclaim 可能已
             * 落盘部分记录，seq_ondisk 介于 0 与已提交 seq 之间。空间
             * 字段用裸索引（space[2]=journal_space_clean、
             * space[3]=journal_space_total，见 journal.rs
             * journal_space_from）。 */
            format!(
                "seq_ondisk={} last_seq_ondisk={} closed={} watermark={} cur_entry_u64s={} pin={} med={} low={} seq={} clean_total={} clean_next={} total_total={} nr_direct_reclaim={} ja_nr={} ja_sectors_free={} ja_cur={} ja_dirty_idx={} ja_dirty_idx_ondisk={}",
                j.seq_ondisk.load(Ordering::Acquire),
                j.last_seq_ondisk.load(Ordering::Acquire),
                j.closed.lock().unwrap().len(),
                j.watermark.load(Ordering::Acquire),
                j.cur_entry_u64s.load(Ordering::Acquire),
                j.pin.lock().unwrap().1.len(),
                journal_med_on_space(j),
                journal_low_on_space(j),
                j.seq.load(Ordering::Acquire),
                space[2].total,
                space[2].next_entry,
                space[3].total,
                j.nr_direct_reclaim.load(Ordering::Acquire),
                ja.nr, ja.sectors_free, ja.cur_idx, ja.dirty_idx, ja.dirty_idx_ondisk,
            )
        };
        crate::rewrite_log_info!("[crash-child {phase}] journal: {journal_diag}");
        fs::write(ready, b"durable-before-abort").unwrap();
        std::process::abort();
    }

    /// T0201: concurrent writers (btree put + alloc mix) with a one-shot
    /// restart injection, then the caller aborts at the chosen crash
    /// point.  The crash point is selected by `mode`:
    /// - cc-flush-before: a JournalWrite fault is armed (every flush
    ///   fails before any write, exactly like T0196's fault matrix), then
    ///   all writers finish their rounds and the process aborts before
    ///   any flush — the journal provably holds only in-memory
    ///   transaction records that recovery must drop (journal replay only
    ///   re-applies durable records).  The fault is engine-local, so the
    ///   recovered image opens without it.
    /// - cc-flush-after: all writers finish and the journal is flushed
    ///   before the abort — every committed transaction is durable and
    ///   must survive recovery.
    /// - cc-mid-write: no fault is armed (real crash timing), the main
    ///   thread returns after the first round's barrier slack without
    ///   joining the writers, so the abort lands while writers are still
    ///   mid-round; some transactions are unflushed and must be dropped,
    ///   the rest may have been flushed by background reclaim and
    ///   survive.
    fn concurrent_crash_child(engine: Arc<StorageEngine>, mode: &str) {
        if mode == "cc-flush-before" {
            /* every background-reclaim flush attempt fails before any
             * write lands, so no committed transaction can become durable
             * between the writers' commits and the abort */
            engine.inject_fault(FaultPoint::JournalWrite, 20).unwrap();
        }
        /* 4 free buckets (4..=7) so the 4 writers' start barrier allocates
         * one bucket each without contention, then recycles (T0199 pattern) */
        for offset in 4..=7 {
            engine.add_free_bucket(offset);
        }
        /* Barrier(5): 4 writers + the main thread, which participates in
         * every barrier so the round boundaries are deterministic (the
         * T0199 rule: barrier participants must match waiters). */
        let barrier = Arc::new(std::sync::Barrier::new(5));
        let mut workers = Vec::new();
        for writer in 0..4u64 {
            let engine = Arc::clone(&engine);
            let barrier = Arc::clone(&barrier);
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                for round in 0..3u64 {
                    /* btree writer: journal transaction on the default tree */
                    engine
                        .put(
                            BtreeId::DEFAULT,
                            BtreeKey::new(
                                KeyPosition::new(writer + 1, round + 1, 0),
                                vec![writer, round],
                            )
                            .unwrap(),
                        )
                        .unwrap();
                    /* alloc writer: allocate/reclaim round on the fixed
                     * 8-bucket geometry (free buckets 4..=7); the second
                     * reclaim returns the freespace key so the 3 free
                     * buckets cycle across all 4 writers x 3 rounds
                     * without exhausting (same pattern as T0199
                     * concurrent_writers_with_restart_injection_converge) */
                    let position = engine.allocate_bucket(0).unwrap();
                    engine.reclaim_bucket(position).unwrap();
                    engine.reclaim_bucket(position).unwrap();
                    barrier.wait();
                }
            }));
        }
        engine
            .inject_fault(FaultPoint::TransactionRestart, 8)
            .unwrap();
        match mode {
            "cc-flush-after" => {
                for _ in 0..4 {
                    barrier.wait();
                }
                for worker in workers {
                    worker.join().unwrap();
                }
                engine.flush_journal().unwrap();
            }
            "cc-mid-write" => {
                /* wait through the start barrier and the first round's
                 * end barrier: all writers are then provably inside the
                 * second round; return without joining so the abort
                 * terminates the process mid-write */
                for _ in 0..2 {
                    barrier.wait();
                }
            }
            "cc-flush-before" => {
                for _ in 0..4 {
                    barrier.wait();
                }
                for worker in workers {
                    worker.join().unwrap();
                }
            }
            _ => panic!("unknown concurrent crash mode {mode}"),
        }
    }

    fn run_crash_child(path: &Path, phase: &str) {
        let ready = path.with_extension("ready");
        let _ = fs::remove_file(&ready);
        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "engine::tests::process_crash_child",
                "--nocapture",
            ])
            .env("SUBVOL_ENGINE_CRASH_PATH", path)
            .env("SUBVOL_ENGINE_CRASH_READY", &ready)
            .env("SUBVOL_ENGINE_CRASH_PHASE", phase)
            .status()
            .unwrap();
        assert!(!status.success());
        assert!(ready.exists());
        fs::remove_file(ready).unwrap();
    }

    #[test]
    fn process_abort_recovery_observes_only_durable_boundaries() {
        for (phase, expected) in [
            ("journal", vec![key(701, &[1])]),
            ("checkpoint", vec![key(702, &[2])]),
            ("tail", vec![key(703, &[3]), key(704, &[4])]),
        ] {
            let path = persistent_test_path(&format!("process-crash-{phase}"));
            run_crash_child(&path, phase);
            let recovered = StorageEngine::open_persistent(&path).unwrap();
            assert_eq!(recovered.scan(BtreeId::DEFAULT).unwrap(), expected);
            recovered.verify(BtreeId::DEFAULT).unwrap();
            drop(recovered);
            fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn persistent_concurrent_crash_recovery_converges() {
        /* T0201: concurrent writers + deterministic crash point + recovery.
         * Every crash point must recover to a consistent image: replay only
         * re-applies durable journal records (unflushed transactions are
         * dropped), derived state rebuilds from the alloc tree, verify_all
         * passes, no open-bucket leak, and the btree keys are readable.
         *
         * Deterministic durability boundaries:
         * - cc-single-put: a JournalWrite fault is armed (every flush
         *   fails before any write, T0196 pattern), one non-sync put
         *   commits, then an immediate abort — the journal provably holds
         *   only an in-memory record (never flushed), so recovery must
         *   drop it.
         * - cc-flush-before: the JournalWrite fault is armed before the
         *   writers start, all 4 writers finish their 3 rounds (12
         *   committed transactions), then an immediate abort with no
         *   flush — every committed transaction is unflushed, so recovery
         *   must drop all 12.
         * - cc-flush-after: all writers finish and the journal is flushed
         *   before the abort — every committed transaction is durable, so
         *   all 12 writer keys survive.
         *
         * cc-mid-write arms no fault (real crash timing): the abort lands
         * mid-round and background reclaim may have flushed any subset of
         * the committed transactions.  Exactly like bcachefs's background
         * journal reclaim, which records survive is nondeterministic —
         * assert only final consistency (T0199 principle: never assert
         * arrival order or a specific survivor set). */
        for phase in [
            "cc-single-put",
            "cc-flush-before",
            "cc-flush-after",
            "cc-mid-write",
        ] {
            let path = persistent_test_path(&format!("concurrent-crash-{phase}"));
            run_crash_child(&path, phase);
            let recovered = StorageEngine::open_persistent(&path).unwrap();
            recovered.verify_all().unwrap();
            assert_eq!(recovered.open_bucket_count().unwrap(), 0);
            let keys = recovered.scan(BtreeId::DEFAULT).unwrap();
            assert!(keys
                .windows(2)
                .all(|pair| pair[0].position() < pair[1].position()));
            match phase {
                "cc-single-put" => {
                    /* JournalWrite fault armed: the journal record was
                     * never written to disk, so recovery drops the
                     * transaction — journal replay re-applies only durable
                     * records */
                    assert!(
                        keys.is_empty(),
                        "cc-single-put: {} keys survived (expected 0): {:?}",
                        keys.len(),
                        keys.iter().map(|k| k.position()).collect::<Vec<_>>()
                    );
                }
                "cc-flush-before" => {
                    /* JournalWrite fault armed: all 12 committed
                     * transactions stayed in-memory, so recovery must
                     * drop all of them (journal replay re-applies only
                     * durable records) */
                    assert!(
                        keys.is_empty(),
                        "cc-flush-before: {} keys survived (expected 0): {:?}",
                        keys.len(),
                        keys.iter().map(|k| k.position()).collect::<Vec<_>>()
                    );
                }
                "cc-flush-after" => {
                    /* flush-after: every committed transaction is durable,
                     * so all 12 writer keys survive */
                    assert_eq!(keys.len(), 12);
                }
                "cc-mid-write" => {
                    /* background reclaim may have flushed any subset of
                     * the committed transactions before the abort; assert
                     * only that the survivor set is a subset of the 12
                     * writer keys (positions writer+1/round+1 for writer
                     * in 0..4, round in 0..3) and the image is consistent */
                    for key in &keys {
                        let pos = key.position();
                        assert!(
                            pos.inode >= 1 && pos.inode <= 4 && pos.offset >= 1 && pos.offset <= 3,
                            "unexpected key outside writer rounds: {pos:?}"
                        );
                    }
                }
                _ => unreachable!("unknown phase {phase}"),
            }
            drop(recovered);
            fs::remove_file(path).unwrap();
        }
    }

    #[derive(Default, Debug)]
    struct TreeStats {
        nodes: usize,
        leaves: usize,
        max_depth: usize,
    }

    /// 物理层统计：沿 root 的 child 指针遍历所有节点，逐节点做
    /// bch2_btree_node_check_topology 校验（AC-1 §5 的物理层断言）。
    unsafe fn tree_stats(fs: &mut bch_fs) -> TreeStats {
        let mut trans = crate::btree::iter::btree_trans::default();
        crate::btree::iter::bch2_trans_init(&mut trans, fs);
        let mut stats = TreeStats::default();
        let root = crate::btree::types::bch2_btree_id_root_b(fs, 0);
        assert!(!root.is_null(), "tree must have a root");
        unsafe fn walk(
            trans: *mut crate::btree::iter::btree_trans,
            b: *mut crate::btree::types::btree,
            depth: usize,
            stats: &mut TreeStats,
        ) {
            stats.nodes += 1;
            stats.max_depth = stats.max_depth.max(depth);
            assert_eq!(
                crate::btree::interior::bch2_btree_node_check_topology(trans, b),
                0,
                "topology broken at depth {depth}"
            );
            if (*b).c.level == 0 {
                stats.leaves += 1;
                return;
            }
            let mut iter = crate::btree::types::btree_node_iter::default();
            crate::btree::node_iter::bch2_btree_node_iter_init_from_start(&mut iter, b);
            loop {
                let ptr = crate::btree::node_iter::bch2_btree_node_iter_peek(&mut iter, b);
                if ptr.is_null() {
                    break;
                }
                let key_u64s = crate::btree::bkey::bkeyp_key_u64s(&(*b).format, &*ptr);
                let child = *ptr.cast::<u64>().add(key_u64s as usize)
                    as usize as *mut crate::btree::types::btree;
                assert!(!child.is_null(), "interior key without child at depth {depth}");
                walk(trans, child, depth + 1, stats);
                crate::btree::node_iter::bch2_btree_node_iter_next_all(&mut iter, b);
            }
        }
        walk(&mut trans, root, 0, &mut stats);
        crate::btree::iter::bch2_trans_put(&mut trans);
        stats
    }

    #[test]
    fn merge_bulk_delete_shrinks_tree_and_preserves_keyset() {
        /* T0204 delete_stress（AC-1 §5）：批量 put 撑起多层树 → 交错
         * 批量 delete（3/4）收缩；断言：键集与 BTreeMap 模型一致、
         * verify_all 通过、深度不增、叶/节点数减少（前台合并把半空
         * 兄弟打包），全部物理节点拓扑有效。 */
        let path = persistent_test_path("merge-delete-stress");
        let file = fs::File::create(&path).unwrap();
        file.set_len(32 * 1024 * 1024).unwrap();
        drop(file);
        let engine = StorageEngine::create_persistent(&path).unwrap();
        let mut model = BTreeMap::new();
        /* 单事务 update 数受路径池约束（BTREE_ITER_INITIAL=64，每
         * update 持一条路径引用），批量取 16 键/批；批大小还需避开
         * 叶容量谐振（叶容量 64 键，批 32 键 + split 后半叶 32 键
         * = 恰好填满，导致无限 split 重放） */
        for offset in (0..768u64).collect::<Vec<_>>().chunks(16) {
            let mut txn = engine.transaction();
            for &o in offset {
                let k = key(o, &[o, o + 1]);
                txn.put(BtreeId::DEFAULT, k.clone());
                model.insert(KeyPosition::new(1, o, 0), k);
            }
            txn.commit().unwrap();
        }
        let before = unsafe { tree_stats(&mut *engine.lock_fs().unwrap()) };
        assert!(
            before.max_depth >= 1,
            "stress must build a multi-level tree (root internal + leaves), got depth {}",
            before.max_depth
        );
        for chunk in (0..768u64)
            .filter(|o| o % 4 != 0)
            .collect::<Vec<_>>()
            .chunks(16)
        {
            let mut txn = engine.transaction();
            for &o in chunk {
                txn.delete(BtreeId::DEFAULT, KeyPosition::new(1, o, 0));
                model.remove(&KeyPosition::new(1, o, 0));
            }
            txn.commit().unwrap();
        }
        let after = unsafe { tree_stats(&mut *engine.lock_fs().unwrap()) };
        engine.verify_all().unwrap();
        assert_eq!(
            engine.scan(BtreeId::DEFAULT).unwrap(),
            model.values().cloned().collect::<Vec<_>>()
        );
        assert!(
            after.max_depth <= before.max_depth,
            "merge must not deepen the tree: {before:?} -> {after:?}"
        );
        assert!(
            after.leaves < before.leaves,
            "merge should shrink leaf count: {before:?} -> {after:?}"
        );
        assert!(
            after.nodes < before.nodes,
            "merge should shrink node count: {before:?} -> {after:?}"
        );
        drop(engine);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn merge_delete_stress_survives_replay() {
        /* T0204 崩溃恢复（AC-1 §5）：delete 压力后 sync 落盘，drop
         * （不 flush）后 open_persistent，replay 必须恢复出精确键集
         * 且拓扑有效。 */
        let path = persistent_test_path("merge-delete-recovery");
        let file = fs::File::create(&path).unwrap();
        file.set_len(32 * 1024 * 1024).unwrap();
        drop(file);
        let mut model = BTreeMap::new();
        {
            let engine = StorageEngine::create_persistent(&path).unwrap();
            for offset in (0..512u64).collect::<Vec<_>>().chunks(16) {
                let mut txn = engine.transaction();
                for &o in offset {
                    let k = key(o, &[o]);
                    txn.put(BtreeId::DEFAULT, k.clone());
                    model.insert(KeyPosition::new(1, o, 0), k);
                }
                txn.commit().unwrap();
            }
            engine.sync().unwrap();
            for chunk in (0..512u64)
                .filter(|o| o % 4 != 0)
                .collect::<Vec<_>>()
                .chunks(16)
            {
                let mut txn = engine.transaction();
                for &o in chunk {
                    txn.delete(BtreeId::DEFAULT, KeyPosition::new(1, o, 0));
                    model.remove(&KeyPosition::new(1, o, 0));
                }
                txn.commit().unwrap();
            }
            engine.sync().unwrap();
        }
        let recovered = StorageEngine::open_persistent(&path).unwrap();
        recovered.verify_all().unwrap();
        assert_eq!(
            recovered.scan(BtreeId::DEFAULT).unwrap(),
            model.values().cloned().collect::<Vec<_>>()
        );
        drop(recovered);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn merge_random_operations_preserve_keyset_model() {
        /* T0204 属性测试（AC-1 §5）：确定性伪随机 put/delete 序列，
         * 键级模型（BTreeMap）对比；merge 的 restart（-4）由引擎
         * commit 循环透明处理，物理布局对逻辑模型不可见。 */
        for seed in 1..=4u64 {
            let engine = StorageEngine::new().unwrap();
            let mut model = BTreeMap::<KeyPosition, BtreeKey>::new();
            let mut state = seed ^ 0x9e37_79b9;
            for _step in 0..256u64 {
                state ^= state << 7;
                state ^= state >> 11;
                state ^= state << 9;
                let position = KeyPosition::new(1, state % 96, 0);
                let mut txn = engine.transaction();
                if state & 1 == 0 {
                    let k = key(position.offset, &[seed, _step, state]);
                    txn.put(BtreeId::DEFAULT, k.clone());
                    model.insert(position, k);
                } else if model.remove(&position).is_some() {
                    txn.delete(BtreeId::DEFAULT, position);
                } else {
                    continue;
                }
                txn.commit().unwrap();
            }
            engine.verify_all().unwrap();
            assert_eq!(
                engine.scan(BtreeId::DEFAULT).unwrap(),
                model.values().cloned().collect::<Vec<_>>(),
                "seed {seed}"
            );
        }
    }

    /// 沿 root 的 child 指针下行到指定 level，返回包含 pos 的节点。
    unsafe fn find_node_at_level(
        fs: &mut bch_fs,
        level: u8,
        pos: bpos,
    ) -> *mut crate::btree::types::btree {
        let c = fs as *mut bch_fs;
        let mut b = crate::btree::types::bch2_btree_id_root_b(fs, 0);
        assert!(!b.is_null(), "tree must have a root");
        while (*b).c.level > level {
            let mut iter = crate::btree::types::btree_node_iter::default();
            crate::btree::node_iter::bch2_btree_node_iter_init(c, b, &mut iter, &pos);
            let ptr = crate::btree::node_iter::bch2_btree_node_iter_peek(&mut iter, b);
            assert!(!ptr.is_null(), "no child key covers pos at level {}", (*b).c.level);
            let key_u64s = crate::btree::bkey::bkeyp_key_u64s(&(*b).format, &*ptr);
            let child = *ptr.cast::<u64>().add(key_u64s as usize)
                as usize as *mut crate::btree::types::btree;
            assert!(!child.is_null(), "interior key without child");
            b = child;
        }
        assert_eq!((*b).c.level, level);
        b
    }

    unsafe fn node_value_words(b: *mut crate::btree::types::btree) -> Vec<u64> {
        let n = bkey_val_u64s(&(*b).key.k) as usize;
        (0..n)
            .map(|i| *((&(*b).key.v as *const crate::btree::bkey::bch_val).cast::<u64>()).add(i))
            .collect()
    }

    #[test]
    fn rewrite_leaf_node_pos_preserves_keyset_and_bumps_seq() {
        /* T0205 T1（AC-1 §4）：rewrite_pos 重写叶节点。断言：键集
         * 不变、scan 与重写前一致、seq+1、min/max 继承、parent
         * pivot（level 1）指向新节点且 max_key 相同。 */
        let path = persistent_test_path("rewrite-leaf");
        let file = fs::File::create(&path).unwrap();
        file.set_len(32 * 1024 * 1024).unwrap();
        drop(file);
        let engine = StorageEngine::create_persistent(&path).unwrap();
        for offset in (0..768u64).collect::<Vec<_>>().chunks(16) {
            let mut txn = engine.transaction();
            for &o in offset {
                txn.put(BtreeId::DEFAULT, key(o, &[o, o + 1]));
            }
            txn.commit().unwrap();
        }
        let before_scan = engine.scan(BtreeId::DEFAULT).unwrap();
        let before_stats = unsafe { tree_stats(&mut *engine.lock_fs().unwrap()) };
        assert!(
            before_stats.max_depth >= 1,
            "must have an internal root over leaves"
        );
        let pos = KeyPosition::new(1, 20, 0);
        let before = unsafe {
            let fs = &mut *engine.lock_fs().unwrap();
            let leaf = find_node_at_level(fs, 0, pos.raw());
            assert_eq!((*leaf).c.level, 0);
            (
                (*(*leaf).data).keys.seq,
                (*(*leaf).data).min_key,
                (*(*leaf).data).max_key,
                (*leaf).key.k.p,
            )
        };
        engine.rewrite_node(BtreeId::DEFAULT, 1, pos).unwrap();
        let after = unsafe {
            let fs = &mut *engine.lock_fs().unwrap();
            let leaf = find_node_at_level(fs, 0, pos.raw());
            assert_eq!((*leaf).c.level, 0);
            let parent = find_node_at_level(fs, 1, pos.raw());
            let seq = (*(*leaf).data).keys.seq;
            let pivot_ok = {
                let mut iter = crate::btree::types::btree_node_iter::default();
                crate::btree::node_iter::bch2_btree_node_iter_init(
                    (&mut **fs) as *mut bch_fs,
                    parent,
                    &mut iter,
                    &pos.raw(),
                );
                let ptr = crate::btree::node_iter::bch2_btree_node_iter_peek(&mut iter, parent);
                let key_u64s = crate::btree::bkey::bkeyp_key_u64s(&(*parent).format, &*ptr);
                let child = *ptr.cast::<u64>().add(key_u64s as usize)
                    as usize as *mut crate::btree::types::btree;
                child == leaf && bpos_eq(crate::btree::node_iter::bkey_unpack_pos(parent, ptr), (*leaf).key.k.p)
            };
            (seq, pivot_ok, (*(*leaf).data).min_key, (*(*leaf).data).max_key)
        };
        assert_eq!(
            after.0,
            before.0 + 1,
            "rewrite must bump the node key sequence"
        );
        assert!(after.1, "parent pivot must point at the rewritten node");
        assert_eq!(after.2, before.1, "min_key must be inherited");
        assert_eq!(after.3, before.2, "max_key must be inherited");
        assert_eq!(
            engine.scan(BtreeId::DEFAULT).unwrap(),
            before_scan,
            "keyset must be preserved"
        );
        engine.verify_all().unwrap();
        drop(engine);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn rewrite_internal_node_keeps_subtree_visible() {
        /* T0205 T2（AC-1 §4）：rewrite_pos 重写带 parent 的内部节点
         * （root level >= 2）。断言：子树全键可遍历、topology 校验
         * 通过（tree_stats 遍历物理树）、键集一致。 */
        let path = persistent_test_path("rewrite-internal");
        let file = fs::File::create(&path).unwrap();
        file.set_len(32 * 1024 * 1024).unwrap();
        drop(file);
        let engine = StorageEngine::create_persistent(&path).unwrap();
        for offset in (0..8192u64).collect::<Vec<_>>().chunks(16) {
            let mut txn = engine.transaction();
            for &o in offset {
                txn.put(BtreeId::DEFAULT, key(o, &[o, o + 1]));
            }
            txn.commit().unwrap();
        }
        let before_scan = engine.scan(BtreeId::DEFAULT).unwrap();
        let before_stats = unsafe { tree_stats(&mut *engine.lock_fs().unwrap()) };
        assert!(
            before_stats.max_depth >= 2,
            "must have an internal level above the leaves, got {before_stats:?}"
        );
        let pos = KeyPosition::new(1, 600, 0);
        engine.rewrite_node(BtreeId::DEFAULT, 2, pos).unwrap();
        let after_stats = unsafe { tree_stats(&mut *engine.lock_fs().unwrap()) };
        assert!(
            after_stats.max_depth == before_stats.max_depth,
            "rewrite must not change tree depth"
        );
        assert_eq!(
            engine.scan(BtreeId::DEFAULT).unwrap(),
            before_scan,
            "keyset must be preserved"
        );
        engine.verify_all().unwrap();
        drop(engine);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn rewrite_root_self_pointing_key_and_deep_scan() {
        /* T0205 T3（AC-1 §4）：root 重写（parent == null 分支）。
         * 断言：set_root 生效（root 指针更新）、root.key 为自身
         * 指针（mem_ptr == root）、level 不变、深遍历键集一致。 */
        let path = persistent_test_path("rewrite-root");
        let file = fs::File::create(&path).unwrap();
        file.set_len(32 * 1024 * 1024).unwrap();
        drop(file);
        let engine = StorageEngine::create_persistent(&path).unwrap();
        for offset in (0..768u64).collect::<Vec<_>>().chunks(16) {
            let mut txn = engine.transaction();
            for &o in offset {
                txn.put(BtreeId::DEFAULT, key(o, &[o, o + 1]));
            }
            txn.commit().unwrap();
        }
        let before_scan = engine.scan(BtreeId::DEFAULT).unwrap();
        let root_level = unsafe {
            let fs = &mut *engine.lock_fs().unwrap();
            (*crate::btree::types::bch2_btree_id_root_b(&**fs, 0)).c.level
        };
        assert!(root_level >= 1, "root must be internal, got level {root_level}");
        let pos = KeyPosition::new(1, 20, 0);
        engine
            .rewrite_node(BtreeId::DEFAULT, root_level + 1, pos)
            .unwrap();
        let (root_level_after, self_pointing) = unsafe {
            let fs = &mut *engine.lock_fs().unwrap();
            let root = crate::btree::types::bch2_btree_id_root_b(&**fs, 0);
            let self_ptr = (*(core::ptr::addr_of!((*root).key.v).cast::<crate::btree::bset::bch_btree_ptr_v2>())).mem_ptr
                == root as usize as u64;
            ((*root).c.level, self_ptr)
        };
        assert_eq!(root_level_after, root_level, "root level must not change");
        assert!(self_pointing, "root key must point at the root itself");
        assert_eq!(
            engine.scan(BtreeId::DEFAULT).unwrap(),
            before_scan,
            "deep traversal must be consistent"
        );
        engine.verify_all().unwrap();
        drop(engine);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn rewrite_key_hash_mismatch_returns_enoent() {
        /* T0205 T4（AC-1 §4）：rewrite_key 的 hash 匹配语义
         * （interior.c:3345-3359）。断言：seq 不匹配 → Transaction(-2)
         * 且原节点不动；seq 匹配 → 重写成功且 seq+1。 */
        let path = persistent_test_path("rewrite-key");
        let file = fs::File::create(&path).unwrap();
        file.set_len(32 * 1024 * 1024).unwrap();
        drop(file);
        let engine = StorageEngine::create_persistent(&path).unwrap();
        for offset in (0..768u64).collect::<Vec<_>>().chunks(16) {
            let mut txn = engine.transaction();
            for &o in offset {
                txn.put(BtreeId::DEFAULT, key(o, &[o, o + 1]));
            }
            txn.commit().unwrap();
        }
        let (stale_key, live_key, target_pos) = unsafe {
            let fs = &mut *engine.lock_fs().unwrap();
            let leaf = find_node_at_level(fs, 0, KeyPosition::new(1, 20, 0).raw());
            let target_pos = KeyPosition::new(
                (*leaf).key.k.p.inode,
                (*leaf).key.k.p.offset,
                (*leaf).key.k.p.snapshot,
            );
            let value = node_value_words(leaf);
            let mut stale_value = value.clone();
            /* btree_ptr_v2 布局：mem_ptr(0) seq(1) min_key(2,3,4)… */
            stale_value[1] += 1;
            (
                BtreeKey::new(target_pos, stale_value).unwrap(),
                BtreeKey::new(target_pos, value).unwrap(),
                target_pos,
            )
        };
        assert!(matches!(
            engine.rewrite_node_key(BtreeId::DEFAULT, 0, &stale_key),
            Err(EngineError::Transaction(-2))
        ));
        let seq_before = unsafe {
            let fs = &mut *engine.lock_fs().unwrap();
            (*(*find_node_at_level(fs, 0, target_pos.raw())).data).keys.seq
        };
        engine
            .rewrite_node_key(BtreeId::DEFAULT, 0, &live_key)
            .unwrap();
        let seq_after = unsafe {
            let fs = &mut *engine.lock_fs().unwrap();
            (*(*find_node_at_level(fs, 0, target_pos.raw())).data).keys.seq
        };
        assert_eq!(seq_before, seq_after - 1, "matched key must rewrite");
        engine.verify_all().unwrap();
        drop(engine);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn rewrite_invalid_path_and_double_rewrite_no_orphan_paths() {
        /* T0205 T5（AC-1 §4）：失败注入。a) 无效路径（path 0 未分配）
         * → Transaction(-5)，树不动；b) 连续两次重写同一叶 → 两次
         * 成功、seq 单调、scan 一致、verify_all 通过（无悬挂路径/
         * 悬挂引用）。 */
        let path = persistent_test_path("rewrite-fail");
        let file = fs::File::create(&path).unwrap();
        file.set_len(32 * 1024 * 1024).unwrap();
        drop(file);
        let engine = StorageEngine::create_persistent(&path).unwrap();
        for offset in (0..768u64).collect::<Vec<_>>().chunks(16) {
            let mut txn = engine.transaction();
            for &o in offset {
                txn.put(BtreeId::DEFAULT, key(o, &[o, o + 1]));
            }
            txn.commit().unwrap();
        }
        let before_scan = engine.scan(BtreeId::DEFAULT).unwrap();
        let invalid = unsafe {
            let fs = &mut *engine.lock_fs().unwrap();
            let mut trans = btree_trans::default();
            bch2_trans_init(&mut trans, &mut **fs);
            let ret = crate::btree::interior::bch2_btree_node_rewrite(&mut trans, 0);
            bch2_trans_put(&mut trans);
            ret
        };
        assert_eq!(invalid, -5, "unallocated path must reject");
        assert_eq!(
            engine.scan(BtreeId::DEFAULT).unwrap(),
            before_scan,
            "rejected rewrite must not touch the tree"
        );
        let pos = KeyPosition::new(1, 20, 0);
        let seq_before = unsafe {
            let fs: &mut bch_fs = &mut *engine.lock_fs().unwrap();
            (*(*find_node_at_level(fs, 0, pos.raw())).data).keys.seq
        };
        engine.rewrite_node(BtreeId::DEFAULT, 1, pos).unwrap();
        engine.rewrite_node(BtreeId::DEFAULT, 1, pos).unwrap();
        let seq_after = unsafe {
            let fs: &mut bch_fs = &mut *engine.lock_fs().unwrap();
            (*(*find_node_at_level(fs, 0, pos.raw())).data).keys.seq
        };
        assert_eq!(
            seq_after, seq_before + 2,
            "double rewrite must bump seq once per call"
        );
        assert_eq!(
            engine.scan(BtreeId::DEFAULT).unwrap(),
            before_scan,
            "keyset must be preserved"
        );
        engine.verify_all().unwrap();
        drop(engine);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn rewrite_survives_crash_and_flush_reopen() {
        /* T0205 T6（AC-1 §4）：崩溃恢复。a) 重写提交后 drop（不
         * flush）→ 重开：journal replay 恢复精确键集、verify_all
         * 通过；b) 重写 + sync（journal-first 持久化落盘）→ 重开：
         * 键集一致、拓扑有效。
         *
         * 崩溃语义对齐 cc-flush-before（T0199）：只有已写盘的 journal
         * 记录在崩溃后存活。因此 a) 中 rewrite 前必须先 sync() 把
         * 512 键全部持久化（drop 不 flush 只保证"已持久化部分"不丢，
         * 未 flush 的事务允许丢失，与并发崩溃测试同款原则）。 */
        let path = persistent_test_path("rewrite-crash");
        let file = fs::File::create(&path).unwrap();
        file.set_len(32 * 1024 * 1024).unwrap();
        drop(file);
        let mut model = BTreeMap::new();
        {
            let engine = StorageEngine::create_persistent(&path).unwrap();
            for offset in (0..512u64).collect::<Vec<_>>().chunks(16) {
                let mut txn = engine.transaction();
                for &o in offset {
                    let k = key(o, &[o, o + 1]);
                    txn.put(BtreeId::DEFAULT, k.clone());
                    model.insert(KeyPosition::new(1, o, 0), k);
                }
                txn.commit().unwrap();
            }
            engine.sync().unwrap();
            engine.rewrite_node(BtreeId::DEFAULT, 1, KeyPosition::new(1, 20, 0)).unwrap();
        }
        let recovered = StorageEngine::open_persistent(&path).unwrap();
        recovered.verify_all().unwrap();
        assert_eq!(
            recovered.scan(BtreeId::DEFAULT).unwrap(),
            model.values().cloned().collect::<Vec<_>>(),
            "crash after rewrite of synced keys must replay the exact keyset"
        );
        drop(recovered);

        let recovered = StorageEngine::open_persistent(&path).unwrap();
        {
            let mut txn = recovered.transaction();
            for o in 1000..1016u64 {
                txn.put(BtreeId::DEFAULT, key(o, &[o]));
            }
            txn.commit().unwrap();
        }
        for o in 1000..1016u64 {
            model.insert(KeyPosition::new(1, o, 0), key(o, &[o]));
        }
        recovered.sync().unwrap();
        drop(recovered);

        let recovered = StorageEngine::open_persistent(&path).unwrap();
        recovered.verify_all().unwrap();
        assert_eq!(
            recovered.scan(BtreeId::DEFAULT).unwrap(),
            model.values().cloned().collect::<Vec<_>>(),
            "flush + reopen must preserve the keyset"
        );
        drop(recovered);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn rewrite_random_operations_preserve_keyset_model() {
        /* T0205 T7（AC-1 §4）：属性测试。确定性伪随机 put/delete +
         * 随机节点重写（叶/内部/root 层），键级模型对比；重写只
         * 改变物理布局，逻辑键集必须与模型完全一致。 */
        for seed in 1..=4u64 {
            let engine = StorageEngine::new().unwrap();
            let mut model = BTreeMap::<KeyPosition, BtreeKey>::new();
            let mut state = seed ^ 0x9e37_79b9;
            for step in 0..256u64 {
                state ^= state << 7;
                state ^= state >> 11;
                state ^= state << 9;
                let position = KeyPosition::new(1, state % 200, 0);
                let mut txn = engine.transaction();
                if state & 1 == 0 {
                    let k = key(position.offset, &[seed, step, state]);
                    txn.put(BtreeId::DEFAULT, k.clone());
                    model.insert(position, k);
                } else if model.remove(&position).is_some() {
                    txn.delete(BtreeId::DEFAULT, position);
                } else {
                    continue;
                }
                txn.commit().unwrap();
                if step % 16 == 0 && step >= 32 {
                    state ^= state >> 5;
                    let target = KeyPosition::new(1, state % 200, 0);
                    let level = 1 + ((state >> 8) % 2) as u8;
                    let result = engine.rewrite_node(BtreeId::DEFAULT, level, target);
                    assert!(
                        result.is_ok()
                            || matches!(result, Err(EngineError::Transaction(-5))),
                        "rewrite must succeed or report no node, got {result:?}"
                    );
                }
            }
            engine.verify_all().unwrap();
            assert_eq!(
                engine.scan(BtreeId::DEFAULT).unwrap(),
                model.values().cloned().collect::<Vec<_>>(),
                "seed {seed}"
            );
        }
    }
}
