//! Safe, single-format storage-engine API over the bcachefs-style btree,
//! transaction and journal core.
//!
//! The raw port remains internal: every mutation below is staged through an
//! intent iterator, committed in a transaction, and made recoverable only by
//! a successfully flushed journal record.  This is deliberately an engine
//! core, not a filesystem-compatibility layer.

use std::{
    collections::{BTreeMap, BTreeSet},
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
            bch2_btree_iter_next, bch2_btree_iter_peek, bch2_btree_iter_traverse, bch2_trans_begin,
            bch2_trans_init, bch2_trans_iter_exit, bch2_trans_iter_init, bch2_trans_put,
            btree_iter, btree_trans, BTREE_ITER_all_snapshots, BTREE_ITER_intent,
            BTREE_ITER_not_extents, BTREE_ITER_snapshot_field,
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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryFaultPoint {
    AfterJournalReplay,
    DuringDerivedRebuild,
    BeforePublication,
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
            Ok(())
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
                            bch2_btree_bit_mod(
                                &mut trans,
                                BTREE_ID_FREESPACE,
                                alloc_freespace_pos((*key).k.p, &alloc),
                                true,
                            )
                        } else {
                            bch2_btree_bit_mod(
                                &mut trans,
                                BTREE_ID_FREESPACE,
                                alloc_freespace_pos((*key).k.p, &old_alloc),
                                false,
                            )
                        }
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
                return if ret == 0 {
                    Ok(())
                } else {
                    Err(EngineError::Transaction(ret))
                };
            }
            Err(EngineError::Transaction(-2))
        }
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
        actual_alloc.insert(
            ((*key).k.p.inode, (*key).k.p.offset),
            (alloc.gen, alloc.dirty_sectors),
        );
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
        assert_eq!(
            engine.allocate_bucket(0).unwrap(),
            KeyPosition::new(0, 4, 0)
        );
        assert!(engine.verify_bucket_indexes().is_ok());
        engine.reclaim_bucket(KeyPosition::new(0, 4, 0)).unwrap();
        assert!(engine.verify_bucket_indexes().is_ok());
        engine.reclaim_bucket(KeyPosition::new(0, 4, 0)).unwrap();
        assert!(engine.verify_bucket_indexes().is_ok());
        assert_eq!(
            engine.allocate_bucket(0).unwrap(),
            KeyPosition::new(0, 4, 0)
        );

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
        fn public_bucket_api_operation_model_stays_consistent(
            operations in prop::collection::vec(0u8..3, 1..=30),
        ) {
            let (engine, path) = prepared_bucket_engine("bucket-api-prop", 4);
            let position = KeyPosition::new(0, 4, 0);
            let mut state = 0u8; // free, btree-owned, need-discard
            for operation in operations {
                match operation {
                    0 => {
                        let result = engine.allocate_bucket(0);
                        if state == 0 {
                            prop_assert_eq!(result.unwrap(), position);
                            state = 1;
                        } else {
                            prop_assert!(result.is_err());
                        }
                    }
                    1 => {
                        prop_assert!(engine.reclaim_bucket(position).is_ok());
                        state = if state == 2 { 0 } else { 2 };
                    }
                    _ => {
                        prop_assert!(engine.reclaim_bucket(KeyPosition::new(0, 8, 0)).is_err());
                    }
                }
                prop_assert!(engine.verify_bucket_indexes().is_ok());
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
            _ => panic!("unknown crash phase {phase}"),
        }
        fs::write(ready, b"durable-before-abort").unwrap();
        std::process::abort();
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
}
