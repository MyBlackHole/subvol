//! Safe, single-format storage-engine API over the bcachefs-style btree,
//! transaction and journal core.
//!
//! The raw port remains internal: every mutation below is staged through an
//! intent iterator, committed in a transaction, and made recoverable only by
//! a successfully flushed journal record.  This is deliberately an engine
//! core, not a filesystem-compatibility layer.

use std::{
    fmt,
    fs::{File, OpenOptions},
    io,
    ops::{Deref, DerefMut},
    path::Path,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc, Condvar, Mutex, MutexGuard, Weak,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use urcu::{boxed::RcuBox, Rcu, RcuThread};

use crate::{
    btree::{
        bkey::{
            bkey, bkey_err, bkey_i, bkey_s_c, bkey_val_u64s, bpos, bpos_eq, BKEY_U64S,
            BKEY_VAL_U64S_MAX, KEY_FORMAT_CURRENT, POS_MIN,
        },
        bset::{KEY_TYPE_cookie, KEY_TYPE_deleted},
        cache::{bch2_btree_node_write_done_clean, bch2_fs_btree_cache_init},
        interior::{bch2_btree_node_check_topology, bch2_btree_root_alloc_fake},
        iter::{
            bch2_btree_iter_next, bch2_btree_iter_peek, bch2_btree_iter_traverse, bch2_trans_begin,
            bch2_trans_init, bch2_trans_iter_exit, bch2_trans_iter_init, bch2_trans_put,
            btree_iter, btree_trans, BTREE_ITER_intent, BTREE_ITER_not_extents,
        },
        types::{
            bch2_btree_id_root_b, bch_fs, btree, clear_btree_node_dirty, clear_btree_node_fake,
            clear_btree_node_just_written, clear_btree_node_need_rewrite,
            clear_btree_node_need_write, BTREE_ID_NR,
        },
        update::{bch2_trans_commit, bch2_trans_update},
    },
    journal::{
        bch2_journal_flush, bch2_journal_pin_drop, bch2_journal_read, bch2_journal_replay,
        bch2_journal_replay_key, bch2_journal_restore_for_replay,
        bch2_journal_update_last_seq_ondisk, journal_checkpoint_markers, journal_low_on_space,
        journal_med_on_space, journal_start_info, journal_state_offset,
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
 *   [two checkpoint headers][reserved][four journal buckets][checkpoint data]
 *
 * The journal range is intentionally independent of the checkpoint arena.
 * A checkpoint payload is made durable before its alternate header is
 * published; only then may a following journal record advance last_seq.
 */
const JOURNAL_FILE_SECTORS: u64 = 16_384;
const JOURNAL_BUCKET_START: u64 = 1;
const JOURNAL_BUCKETS: u64 = 4;
const JOURNAL_BUCKET_SIZE: u16 = 2_048;
const ENGINE_JOURNAL_UUID: [u8; 16] = [0x53; 16];
const CHECKPOINT_HEADER_BYTES: usize = 4 << 10;
const CHECKPOINT_HEADER_WORDS: usize = 11;
const CHECKPOINT_HEADER_SLOTS: usize = 2;
const CHECKPOINT_DATA_START: u64 = JOURNAL_FILE_SECTORS * 512;
const CHECKPOINT_ALIGN: u64 = CHECKPOINT_HEADER_BYTES as u64;
const ENGINE_CHECKPOINT_MAGIC: u64 = 0x5355_4256_4f4c_4350;
const ENGINE_CHECKPOINT_PAYLOAD_MAGIC: u64 = 0x5355_4256_4f4c_4450;
const ENGINE_CHECKPOINT_NODE_MAGIC: u64 = 0x5355_4256_4f4c_4e44;
const CHECKPOINT_PAYLOAD_HEADER_WORDS: usize = 6;
const CHECKPOINT_ROOT_WORDS: usize = 4;
const CHECKPOINT_NODE_HEADER_WORDS: usize = 8;
const CHECKPOINT_NODE_KEY_WORDS: usize = 128;
const RECLAIM_WORKER_DELAY: Duration = Duration::from_millis(25);

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

/*
 * A checkpoint is the engine's durable btree-base image.  Each root names a
 * contiguous immutable run of leaf pages.  The next checkpoint allocates a
 * new page image and publishes it through the alternate header only after all
 * page checksums are stable, matching bcachefs' root publication after node
 * write completion.
 */
#[derive(Clone, Debug, Eq, PartialEq)]
struct CheckpointRoot {
    btree: BtreeId,
    level: u8,
    first_node: u32,
    node_count: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CheckpointNode {
    btree: BtreeId,
    level: u8,
    page: u32,
    entries: Vec<BtreeKey>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct CheckpointImage {
    sequence: u64,
    generation: u64,
    roots: Vec<CheckpointRoot>,
    nodes: Vec<CheckpointNode>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct CheckpointSlot {
    generation: u64,
    sequence: u64,
    offset: u64,
    bytes: u64,
    capacity: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct CheckpointState {
    image: CheckpointImage,
    slots: [CheckpointSlot; CHECKPOINT_HEADER_SLOTS],
    active_slot: Option<usize>,
}

impl CheckpointState {
    fn next_image(
        &self,
        sequence: u64,
        entries: Vec<(BtreeId, BtreeKey)>,
    ) -> Result<CheckpointImage, EngineError> {
        CheckpointImage::from_entries(
            sequence,
            self.image.generation.saturating_add(1).max(1),
            entries,
        )
    }
}

impl CheckpointImage {
    fn key_count(&self) -> usize {
        self.nodes.iter().map(|node| node.entries.len()).sum()
    }

    fn from_entries(
        sequence: u64,
        generation: u64,
        entries: Vec<(BtreeId, BtreeKey)>,
    ) -> Result<Self, EngineError> {
        if sequence == 0 || generation == 0 {
            return Err(EngineError::Checkpoint(-1));
        }

        let mut roots = Vec::new();
        let mut nodes = Vec::new();
        let mut cursor = 0usize;
        while cursor < entries.len() {
            let btree = entries[cursor].0;
            let first_node =
                u32::try_from(nodes.len()).map_err(|_| EngineError::Checkpoint(-12))?;
            let mut node_entries = Vec::new();
            let mut node_words = 0usize;

            while cursor < entries.len() && entries[cursor].0 == btree {
                let key = &entries[cursor].1;
                let key_words = (BKEY_U64S as usize)
                    .checked_add(key.value().len())
                    .ok_or(EngineError::Checkpoint(-12))?;
                if !node_entries.is_empty()
                    && node_words
                        .checked_add(key_words)
                        .ok_or(EngineError::Checkpoint(-12))?
                        > CHECKPOINT_NODE_KEY_WORDS
                {
                    let page =
                        u32::try_from(nodes.len()).map_err(|_| EngineError::Checkpoint(-12))?;
                    nodes.push(CheckpointNode {
                        btree,
                        level: 0,
                        page,
                        entries: core::mem::take(&mut node_entries),
                    });
                    node_words = 0;
                }
                node_words = node_words
                    .checked_add(key_words)
                    .ok_or(EngineError::Checkpoint(-12))?;
                node_entries.push(key.clone());
                cursor += 1;
            }

            if !node_entries.is_empty() {
                let page = u32::try_from(nodes.len()).map_err(|_| EngineError::Checkpoint(-12))?;
                nodes.push(CheckpointNode {
                    btree,
                    level: 0,
                    page,
                    entries: node_entries,
                });
            }
            let node_count = nodes
                .len()
                .checked_sub(first_node as usize)
                .and_then(|count| u32::try_from(count).ok())
                .ok_or(EngineError::Checkpoint(-12))?;
            roots.push(CheckpointRoot {
                btree,
                level: 0,
                first_node,
                node_count,
            });
        }

        let image = Self {
            sequence,
            generation,
            roots,
            nodes,
        };
        validate_checkpoint_image(&image)?;
        Ok(image)
    }
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

/// Deterministic test fault locations with bcachefs-equivalent retry/write
/// boundaries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FaultPoint {
    /// `trans_maybe_inject_restart()` before commit side effects.
    TransactionRestart,
    /// A journal write failure before record publication or sequence advance.
    JournalWrite,
    /// Checkpoint payload is durable but its alternate header is not yet
    /// published, so recovery must remain on the old journal window.
    CheckpointWrite,
    /// The checkpoint header is durable, but the following journal anchor
    /// has not yet been published.
    CheckpointBarrier,
}

/// A durable journal image captured after successful flushes.  It models the
/// state a fresh engine receives after a crash; unflushed transaction updates
/// are intentionally absent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JournalSnapshot {
    format_version: u32,
    checkpoint: CheckpointImage,
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

    /// Sequence covered by the durable btree-base checkpoint, or zero when
    /// this image still requires replay from the first journal record.
    pub const fn checkpoint_sequence(&self) -> u64 {
        self.checkpoint.sequence
    }

    pub const fn checkpoint_generation(&self) -> u64 {
        self.checkpoint.generation
    }

    pub fn checkpoint_key_count(&self) -> usize {
        self.checkpoint.key_count()
    }

    pub fn checkpoint_node_count(&self) -> usize {
        self.checkpoint.nodes.len()
    }

    pub const fn next_sequence(&self) -> u64 {
        self.next_sequence
    }
}

/// Immutable root/page descriptor copied out while an RCU read-side section
/// is active.  It describes the checkpoint generation that a reader observed,
/// without exposing mutable storage-engine internals.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CheckpointSummary {
    pub sequence: u64,
    pub generation: u64,
    pub root_count: usize,
    pub node_count: usize,
    pub key_count: usize,
}

/// Durable boundary returned by `sync()` and `Transaction::commit_sync()`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DurabilityPoint {
    pub journal_sequence: u64,
    pub journal_sequence_ondisk: u64,
    pub checkpoint_sequence: u64,
    pub checkpoint_generation: u64,
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
    pub checkpoint: CheckpointSummary,
    pub reclaim: ReclaimStatus,
}

#[derive(Debug)]
pub enum EngineError {
    InvalidBtreeId(u8),
    ValueTooLarge(usize),
    UnsupportedFormatVersion(u32),
    Transaction(i32),
    Journal(i32),
    Checkpoint(i32),
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
            Self::Journal(error) => write!(f, "journal operation failed: {error}"),
            Self::Checkpoint(error) => write!(f, "checkpoint operation failed: {error}"),
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
    pub fn checkpoint(&self) -> CheckpointSummary {
        self.rcu_thread.rscs(|rscs| {
            let image = self.engine.inner.checkpoint_view.read(rscs);
            checkpoint_summary(&image)
        })
    }

    pub fn get(
        &self,
        btree: BtreeId,
        position: KeyPosition,
    ) -> Result<Option<BtreeKey>, EngineError> {
        let mut fs = self.engine.lock_fs()?;
        self.rcu_thread.rscs(|rscs| {
            let _ = self.engine.inner.checkpoint_view.read(rscs);
        });
        unsafe { get_locked(&mut **fs, btree, position) }
    }

    pub fn scan(&self, btree: BtreeId) -> Result<Vec<BtreeKey>, EngineError> {
        let mut fs = self.engine.lock_fs()?;
        self.rcu_thread.rscs(|rscs| {
            let _ = self.engine.inner.checkpoint_view.read(rscs);
        });
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
    checkpoint: Mutex<CheckpointState>,
    checkpoint_view: RcuBox<CheckpointImage>,
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
            /* This is the same node-buffer geometry used by the port's btree
             * cache and fake-root recovery tests. */
            (*fs.disk_sb.sb).flags[0] = 1 << 12;

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
            checkpoint: Mutex::new(CheckpointState::default()),
            checkpoint_view: RcuBox::new(&rcu, CheckpointImage::default()),
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

    /// Writes the current btree state as the durable base, then publishes an
    /// empty journal anchor after its pins have been released.  This is the
    /// engine counterpart to bcachefs journal reclaim: data is durable before
    /// `last_seq` may advance and make earlier journal entries reclaimable.
    pub fn checkpoint(&self) -> Result<(), EngineError> {
        let mut fs = self.lock_fs()?;
        unsafe { self.checkpoint_locked(&mut **fs) }
    }

    /// Runs the durable checkpoint path and returns the resulting boundary.
    pub fn checkpoint_sync(&self) -> Result<DurabilityPoint, EngineError> {
        self.checkpoint()?;
        self.durability_point()
    }

    /// Reclaims the journal through a complete checkpoint.  This is the
    /// direct-reclaim path used when a caller cannot wait for the background
    /// single consumer.
    pub fn reclaim_journal(&self) -> Result<(), EngineError> {
        self.checkpoint()
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

    /// Reports the durable journal/checkpoint boundary without issuing I/O.
    pub fn durability_point(&self) -> Result<DurabilityPoint, EngineError> {
        let fs = self.lock_fs()?;
        let checkpoint = self.lock_checkpoint()?.image.clone();
        Ok(DurabilityPoint {
            journal_sequence: fs.journal.seq.load(Ordering::Acquire),
            journal_sequence_ondisk: fs.journal.seq_ondisk.load(Ordering::Acquire),
            checkpoint_sequence: checkpoint.sequence,
            checkpoint_generation: checkpoint.generation,
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
        let checkpoint = self.lock_checkpoint()?.image.clone();
        let reclaim = self.reclaim_status()?;
        Ok(EngineMetrics {
            journal_sequence: fs.journal.seq.load(Ordering::Acquire),
            journal_sequence_ondisk: fs.journal.seq_ondisk.load(Ordering::Acquire),
            journal_last_sequence: fs.journal.last_seq.load(Ordering::Acquire),
            journal_last_sequence_ondisk: fs.journal.last_seq_ondisk.load(Ordering::Acquire),
            journal_records,
            checkpoint: checkpoint_summary(&checkpoint),
            reclaim,
        })
    }

    pub fn durable_journal(&self) -> Result<JournalSnapshot, EngineError> {
        let fs = self.lock_fs()?;
        let records = fs.journal.closed.lock().unwrap().clone();
        let next_sequence = fs.journal.seq.load(Ordering::Acquire);
        let checkpoint = self.lock_checkpoint()?.image.clone();
        Ok(JournalSnapshot {
            format_version: STORAGE_FORMAT_VERSION,
            checkpoint,
            records,
            next_sequence,
        })
    }

    /// Reconstructs an engine from a crash image captured by
    /// `durable_journal()`.
    pub fn recover(snapshot: &JournalSnapshot) -> Result<Self, EngineError> {
        if snapshot.format_version != STORAGE_FORMAT_VERSION {
            return Err(EngineError::UnsupportedFormatVersion(
                snapshot.format_version,
            ));
        }

        let engine = Self::new()?;
        let mut fs = engine.lock_fs()?;
        let checkpoint = CheckpointState {
            image: snapshot.checkpoint.clone(),
            ..Default::default()
        };
        validate_checkpoint_recovery(&checkpoint.image, &snapshot.records)?;
        unsafe {
            let ret = bch2_journal_restore_for_replay(
                &fs.journal,
                snapshot.records.clone(),
                snapshot.next_sequence,
            );
            if ret != 0 {
                return Err(EngineError::Journal(ret));
            }
            load_checkpoint_base(&mut **fs, &checkpoint.image)?;
            fs.journal
                .checkpoint_seq
                .store(checkpoint.image.sequence, Ordering::Release);
            fs.journal
                .checkpoint_generation
                .store(checkpoint.image.generation, Ordering::Release);
            let ret = bch2_journal_replay(&mut **fs);
            if ret != 0 {
                return Err(EngineError::Journal(ret));
            }
        }
        drop(fs);
        *engine.lock_checkpoint()? = checkpoint.clone();
        engine.publish_checkpoint_view(&checkpoint.image);
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
            FaultPoint::CheckpointWrite => fs
                .journal
                .fault_inject_checkpoint_write_error
                .store(count, Ordering::Release),
            FaultPoint::CheckpointBarrier => fs
                .journal
                .fault_inject_checkpoint_barrier_error
                .store(count, Ordering::Release),
        }
        Ok(())
    }

    fn lock_fs(&self) -> Result<MutexGuard<'_, EngineFs>, EngineError> {
        self.inner.fs.lock().map_err(|_| EngineError::Poisoned)
    }

    fn lock_checkpoint(&self) -> Result<MutexGuard<'_, CheckpointState>, EngineError> {
        self.inner
            .checkpoint
            .lock()
            .map_err(|_| EngineError::Poisoned)
    }

    unsafe fn checkpoint_locked(&self, fs: &mut bch_fs) -> Result<(), EngineError> {
        /* The journal record that precedes the checkpoint is the write-ahead
         * boundary.  Do not make a base image from updates that have not
         * reached this successful flush.  When the current entry is already
         * empty, avoid consuming the last physical journal slot: reclaim.c
         * may first free the stable prefix and then write the next anchor. */
        if journal_state_offset(fs.journal.reservations.load(Ordering::Acquire)) != 0 {
            let ret = bch2_journal_flush(&fs.journal);
            if ret != 0 {
                return Err(EngineError::Journal(ret));
            }
        }
        let sequence = fs.journal.seq_ondisk.load(Ordering::Acquire);
        if sequence == 0 {
            return Err(EngineError::Checkpoint(-1));
        }

        let entries = collect_checkpoint_entries(fs)?;
        let current = self.lock_checkpoint()?.clone();
        let mut next = CheckpointState {
            image: current.next_image(sequence, entries)?,
            slots: current.slots,
            active_slot: current.active_slot,
        };

        if fs.disk_sb.s_bdev_file.is_null() {
            if consume_fault(&fs.journal.fault_inject_checkpoint_write_error) {
                return Err(EngineError::Checkpoint(-5));
            }
        } else {
            write_persistent_checkpoint(fs, &mut next)?;
        }

        /* The checkpoint image is now durable (or is the durable in-memory
         * crash image).  Publish its identity before creating the next jset;
         * bch2_journal_flush() will propagate it just like btree roots. */
        fs.journal
            .checkpoint_seq
            .store(next.image.sequence, Ordering::Release);
        fs.journal
            .checkpoint_generation
            .store(next.image.generation, Ordering::Release);
        *self.lock_checkpoint()? = next.clone();
        self.publish_checkpoint_view(&next.image);

        /* This is the engine's synchronous node-write completion: the base
         * image contains every live key, so all node pins may be dropped only
         * after the image publication above. */
        complete_checkpoint_node_writes(fs);

        /* This is the completed-write accounting boundary in reclaim.c.  The
         * checkpoint header is already durable, so the old journal prefix can
         * be discarded before reserving the following anchor if space is
         * exhausted. */
        let reclaim_to = sequence.checked_add(1).ok_or(EngineError::Checkpoint(-1))?;
        let ret = bch2_journal_update_last_seq_ondisk(&fs.journal, reclaim_to);
        if ret != 0 {
            return Err(EngineError::Checkpoint(ret));
        }

        if consume_fault(&fs.journal.fault_inject_checkpoint_barrier_error) {
            return Err(EngineError::Checkpoint(-5));
        }

        /* Publish the advanced last_seq and repeated checkpoint anchor in a
         * following empty jset.  If this write fails, the already durable
         * base remains safe because recovery replays the still-retained old
         * journal window. */
        let ret = bch2_journal_flush(&fs.journal);
        if ret != 0 {
            return Err(EngineError::Journal(ret));
        }

        fs.journal
            .closed
            .lock()
            .unwrap()
            .retain(|record| record.get(3).is_some_and(|seq| *seq > sequence));
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
                for operation in operations {
                    let (btree, position, deleted, value) = match operation {
                        TransactionOperation::Put { btree, key } => {
                            (*btree, key.position(), false, key.value())
                        }
                        TransactionOperation::Delete { btree, position } => {
                            (*btree, *position, true, &[] as &[u64])
                        }
                    };
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
                    if ret != 0 {
                        break;
                    }
                }
                if ret == 0 {
                    ret = bch2_trans_commit(&mut trans);
                }

                if ret == -4 {
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
        let checkpoint;
        unsafe {
            configure_persistent_journal(&mut fs, file)?;
            let mut info = journal_start_info::default();
            let ret = bch2_journal_read(&mut **fs, &mut info);
            if ret != 0 {
                return Err(EngineError::Journal(ret));
            }
            checkpoint = read_persistent_checkpoint(&**fs)?;
            let records = fs.journal.closed.lock().unwrap().clone();
            validate_checkpoint_recovery(&checkpoint.image, &records)?;
            load_checkpoint_base(&mut **fs, &checkpoint.image)?;
            fs.journal
                .checkpoint_seq
                .store(checkpoint.image.sequence, Ordering::Release);
            fs.journal
                .checkpoint_generation
                .store(checkpoint.image.generation, Ordering::Release);
            let ret = bch2_journal_replay(&mut **fs);
            if ret != 0 {
                return Err(EngineError::Journal(ret));
            }
        }
        drop(fs);
        *self.lock_checkpoint()? = checkpoint.clone();
        self.publish_checkpoint_view(&checkpoint.image);
        Ok(())
    }

    fn publish_checkpoint_view(&self, image: &CheckpointImage) {
        let previous = self.inner.checkpoint_view.update(image.clone());
        drop(previous);
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
        EngineError::Transaction(error)
        | EngineError::Journal(error)
        | EngineError::Checkpoint(error) => *error,
        EngineError::Io(_) => -5,
        EngineError::ReclaimTimeout => -110,
        EngineError::InvalidBtreeId(_)
        | EngineError::ValueTooLarge(_)
        | EngineError::UnsupportedFormatVersion(_)
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

fn checkpoint_summary(image: &CheckpointImage) -> CheckpointSummary {
    CheckpointSummary {
        sequence: image.sequence,
        generation: image.generation,
        root_count: image.roots.len(),
        node_count: image.nodes.len(),
        key_count: image.key_count(),
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
        BTREE_ITER_not_extents,
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

unsafe fn collect_checkpoint_entries(
    fs: &mut bch_fs,
) -> Result<Vec<(BtreeId, BtreeKey)>, EngineError> {
    let mut entries = Vec::new();
    for id in 0..BTREE_ID_NR {
        let btree = BtreeId(id as u8);
        for key in scan_locked(fs, btree)? {
            entries.push((btree, key));
        }
    }
    Ok(entries)
}

fn validate_checkpoint_image(image: &CheckpointImage) -> Result<(), EngineError> {
    if image.sequence == 0 {
        if image.generation != 0 || !image.roots.is_empty() || !image.nodes.is_empty() {
            return Err(EngineError::Checkpoint(-1));
        }
        return Ok(());
    }
    if image.generation == 0 {
        return Err(EngineError::Checkpoint(-1));
    }

    let mut expected_first_node = 0usize;
    let mut previous_btree = None;
    for root in &image.roots {
        if root.btree.as_u8() as usize >= BTREE_ID_NR
            || root.level != 0
            || root.node_count == 0
            || root.first_node as usize != expected_first_node
        {
            return Err(EngineError::Checkpoint(-2));
        }
        if previous_btree.is_some_and(|previous| previous >= root.btree.as_u8()) {
            return Err(EngineError::Checkpoint(-2));
        }
        previous_btree = Some(root.btree.as_u8());

        let end = expected_first_node
            .checked_add(root.node_count as usize)
            .filter(|end| *end <= image.nodes.len())
            .ok_or(EngineError::Checkpoint(-2))?;
        let mut previous_position = None;
        for (index, node) in image.nodes[expected_first_node..end].iter().enumerate() {
            if node.btree != root.btree
                || node.level != root.level
                || node.page
                    != u32::try_from(expected_first_node + index)
                        .map_err(|_| EngineError::Checkpoint(-2))?
                || node.entries.is_empty()
            {
                return Err(EngineError::Checkpoint(-2));
            }
            let mut encoded_words = 0usize;
            for key in &node.entries {
                if key.value().len() > BKEY_VAL_U64S_MAX as usize {
                    return Err(EngineError::Checkpoint(-2));
                }
                if previous_position.is_some_and(|previous| previous >= key.position()) {
                    return Err(EngineError::Checkpoint(-2));
                }
                previous_position = Some(key.position());
                encoded_words = encoded_words
                    .checked_add(BKEY_U64S as usize + key.value().len())
                    .ok_or(EngineError::Checkpoint(-12))?;
            }
            if node.entries.len() > 1 && encoded_words > CHECKPOINT_NODE_KEY_WORDS {
                return Err(EngineError::Checkpoint(-2));
            }
        }
        expected_first_node = end;
    }
    if expected_first_node != image.nodes.len() {
        return Err(EngineError::Checkpoint(-2));
    }
    Ok(())
}

fn validate_checkpoint_recovery(
    image: &CheckpointImage,
    records: &[Vec<u64>],
) -> Result<(), EngineError> {
    validate_checkpoint_image(image)?;
    let markers = journal_checkpoint_markers(records).map_err(EngineError::Journal)?;

    if image.sequence == 0 {
        if !markers.is_empty() {
            return Err(EngineError::Checkpoint(-3));
        }
        return Ok(());
    }

    /* A newer marker is a completed checkpoint publication and must match
     * this base exactly.  Older markers are valid after a crash between a
     * newer header write and its following journal anchor: replay then still
     * contains the complete older journal window and is idempotent.  An empty
     * window is valid too: the durable base itself then represents a clean
     * journal state. */
    if let Some((_, sequence, generation)) = markers
        .into_iter()
        .filter(|(record_sequence, _, _)| *record_sequence > image.sequence)
        .max()
    {
        if sequence != image.sequence || generation != image.generation {
            return Err(EngineError::Checkpoint(-3));
        }
    }
    Ok(())
}

unsafe fn load_checkpoint_base(
    fs: &mut bch_fs,
    image: &CheckpointImage,
) -> Result<(), EngineError> {
    if image.sequence == 0 {
        return Ok(());
    }

    let seq = fs.journal.seq.load(Ordering::Acquire);
    if seq == 0 {
        return Err(EngineError::Checkpoint(-1));
    }
    for root in &image.roots {
        let start = root.first_node as usize;
        let end = start + root.node_count as usize;
        for node in &image.nodes[start..end] {
            for key in &node.entries {
                let mut raw = encode_key(key.position(), key.value(), false);
                let ret = bch2_journal_replay_key(
                    fs,
                    root.btree.as_u8(),
                    root.level,
                    raw.as_mut_ptr().cast::<bkey_i>(),
                    seq,
                );
                if ret != 0 {
                    return Err(EngineError::Transaction(ret));
                }
            }
        }
    }

    /* The checkpoint already contains these keys.  Clear the pins only after
     * the complete replay-style construction has succeeded, mirroring a
     * btree-node write completion before journal reclaim advances last_seq. */
    complete_checkpoint_node_writes(fs);
    Ok(())
}

unsafe fn complete_checkpoint_node_writes(fs: &mut bch_fs) {
    let nodes = fs.btree.cache.allocations.lock().unwrap().clone();
    for node in nodes {
        let node = node as *mut btree;
        if node.is_null() || (*node).data.is_null() {
            continue;
        }
        bch2_journal_pin_drop(&fs.journal, &mut (*node).writes[0].journal);
        bch2_journal_pin_drop(&fs.journal, &mut (*node).writes[1].journal);
        clear_btree_node_dirty(node);
        clear_btree_node_need_write(node);
        clear_btree_node_just_written(node);
        clear_btree_node_need_rewrite(node);
        bch2_btree_node_write_done_clean(fs, node);
    }
}

fn consume_fault(counter: &AtomicU32) -> bool {
    counter
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
            count.checked_sub(1)
        })
        .is_ok()
}

fn append_checkpoint_word(bytes: &mut Vec<u8>, word: u64) {
    bytes.extend_from_slice(&word.to_le_bytes());
}

fn checkpoint_node_words(node: &CheckpointNode) -> Result<Vec<u64>, EngineError> {
    let words = node.entries.iter().try_fold(0usize, |total, key| {
        total
            .checked_add(BKEY_U64S as usize + key.value().len())
            .ok_or(EngineError::Checkpoint(-12))
    })?;
    let mut output = Vec::new();
    output
        .try_reserve_exact(words)
        .map_err(|_| EngineError::Checkpoint(-12))?;
    for key in &node.entries {
        output.extend(encode_key(key.position(), key.value(), false));
    }
    Ok(output)
}

fn checkpoint_node_checksum(
    btree: BtreeId,
    level: u8,
    page: u32,
    entry_count: u64,
    words: &[u64],
) -> Result<crate::btree::bset::bch_csum, EngineError> {
    let bytes = CHECKPOINT_NODE_HEADER_WORDS
        .checked_sub(2)
        .and_then(|header| header.checked_add(words.len()))
        .and_then(|words| words.checked_mul(core::mem::size_of::<u64>()))
        .ok_or(EngineError::Checkpoint(-12))?;
    let mut encoded = Vec::new();
    encoded
        .try_reserve_exact(bytes)
        .map_err(|_| EngineError::Checkpoint(-12))?;
    append_checkpoint_word(&mut encoded, ENGINE_CHECKPOINT_NODE_MAGIC);
    append_checkpoint_word(&mut encoded, btree.as_u8() as u64);
    append_checkpoint_word(&mut encoded, level as u64);
    append_checkpoint_word(&mut encoded, page as u64);
    append_checkpoint_word(&mut encoded, entry_count);
    append_checkpoint_word(
        &mut encoded,
        u64::try_from(words.len()).map_err(|_| EngineError::Checkpoint(-12))?,
    );
    for word in words {
        append_checkpoint_word(&mut encoded, *word);
    }
    Ok(crate::checksum::bch2_checksum(
        crate::checksum::BCH_CSUM_xxhash,
        &encoded,
    ))
}

fn checkpoint_payload(image: &CheckpointImage) -> Result<Vec<u8>, EngineError> {
    validate_checkpoint_image(image)?;
    let mut node_words = Vec::new();
    node_words
        .try_reserve_exact(image.nodes.len())
        .map_err(|_| EngineError::Checkpoint(-12))?;
    let mut total_words = CHECKPOINT_PAYLOAD_HEADER_WORDS
        .checked_add(
            image
                .roots
                .len()
                .checked_mul(CHECKPOINT_ROOT_WORDS)
                .ok_or(EngineError::Checkpoint(-12))?,
        )
        .ok_or(EngineError::Checkpoint(-12))?;
    for node in &image.nodes {
        let words = checkpoint_node_words(node)?;
        total_words = total_words
            .checked_add(CHECKPOINT_NODE_HEADER_WORDS)
            .and_then(|total| total.checked_add(words.len()))
            .ok_or(EngineError::Checkpoint(-12))?;
        node_words.push(words);
    }
    let bytes = total_words
        .checked_mul(core::mem::size_of::<u64>())
        .ok_or(EngineError::Checkpoint(-12))?;
    let mut payload = Vec::new();
    payload
        .try_reserve_exact(bytes)
        .map_err(|_| EngineError::Checkpoint(-12))?;

    append_checkpoint_word(&mut payload, ENGINE_CHECKPOINT_PAYLOAD_MAGIC);
    append_checkpoint_word(&mut payload, STORAGE_FORMAT_VERSION as u64);
    append_checkpoint_word(&mut payload, image.sequence);
    append_checkpoint_word(&mut payload, image.generation);
    append_checkpoint_word(
        &mut payload,
        u64::try_from(image.roots.len()).map_err(|_| EngineError::Checkpoint(-12))?,
    );
    append_checkpoint_word(
        &mut payload,
        u64::try_from(image.nodes.len()).map_err(|_| EngineError::Checkpoint(-12))?,
    );
    for root in &image.roots {
        append_checkpoint_word(&mut payload, root.btree.as_u8() as u64);
        append_checkpoint_word(&mut payload, root.level as u64);
        append_checkpoint_word(&mut payload, root.first_node as u64);
        append_checkpoint_word(&mut payload, root.node_count as u64);
    }
    for (node, words) in image.nodes.iter().zip(node_words) {
        let entry_count =
            u64::try_from(node.entries.len()).map_err(|_| EngineError::Checkpoint(-12))?;
        let checksum =
            checkpoint_node_checksum(node.btree, node.level, node.page, entry_count, &words)?;
        append_checkpoint_word(&mut payload, ENGINE_CHECKPOINT_NODE_MAGIC);
        append_checkpoint_word(&mut payload, node.btree.as_u8() as u64);
        append_checkpoint_word(&mut payload, node.level as u64);
        append_checkpoint_word(&mut payload, node.page as u64);
        append_checkpoint_word(&mut payload, entry_count);
        append_checkpoint_word(
            &mut payload,
            u64::try_from(words.len()).map_err(|_| EngineError::Checkpoint(-12))?,
        );
        append_checkpoint_word(&mut payload, checksum.lo);
        append_checkpoint_word(&mut payload, checksum.hi);
        for word in words {
            append_checkpoint_word(&mut payload, word);
        }
    }
    Ok(payload)
}

fn checkpoint_payload_word(payload: &[u8], offset: &mut usize) -> Result<u64, EngineError> {
    let end = offset
        .checked_add(core::mem::size_of::<u64>())
        .ok_or(EngineError::Checkpoint(-2))?;
    let word = payload
        .get(*offset..end)
        .ok_or(EngineError::Checkpoint(-2))?;
    *offset = end;
    Ok(u64::from_le_bytes(word.try_into().unwrap()))
}

fn checkpoint_image_from_payload(
    payload: &[u8],
    expected_sequence: u64,
    expected_generation: u64,
) -> Result<CheckpointImage, EngineError> {
    if payload.len() % core::mem::size_of::<u64>() != 0 {
        return Err(EngineError::Checkpoint(-2));
    }
    let mut offset = 0usize;
    if checkpoint_payload_word(payload, &mut offset)? != ENGINE_CHECKPOINT_PAYLOAD_MAGIC
        || checkpoint_payload_word(payload, &mut offset)? != STORAGE_FORMAT_VERSION as u64
        || checkpoint_payload_word(payload, &mut offset)? != expected_sequence
        || checkpoint_payload_word(payload, &mut offset)? != expected_generation
    {
        return Err(EngineError::Checkpoint(-2));
    }
    let root_count = usize::try_from(checkpoint_payload_word(payload, &mut offset)?)
        .map_err(|_| EngineError::Checkpoint(-2))?;
    let node_count = usize::try_from(checkpoint_payload_word(payload, &mut offset)?)
        .map_err(|_| EngineError::Checkpoint(-2))?;
    let remaining_words = (payload.len() - offset) / core::mem::size_of::<u64>();
    if root_count > remaining_words / CHECKPOINT_ROOT_WORDS {
        return Err(EngineError::Checkpoint(-2));
    }

    let mut roots = Vec::new();
    roots
        .try_reserve_exact(root_count)
        .map_err(|_| EngineError::Checkpoint(-12))?;
    for _ in 0..root_count {
        let btree_id = checkpoint_payload_word(payload, &mut offset)?;
        if btree_id > u8::MAX as u64 || btree_id as usize >= BTREE_ID_NR {
            return Err(EngineError::Checkpoint(-2));
        }
        let level = checkpoint_payload_word(payload, &mut offset)?;
        let first_node = checkpoint_payload_word(payload, &mut offset)?;
        let root_node_count = checkpoint_payload_word(payload, &mut offset)?;
        if level > u8::MAX as u64
            || first_node > u32::MAX as u64
            || root_node_count > u32::MAX as u64
        {
            return Err(EngineError::Checkpoint(-2));
        }
        roots.push(CheckpointRoot {
            btree: BtreeId(btree_id as u8),
            level: level as u8,
            first_node: first_node as u32,
            node_count: root_node_count as u32,
        });
    }
    let remaining_words = (payload.len() - offset) / core::mem::size_of::<u64>();
    if node_count > remaining_words / CHECKPOINT_NODE_HEADER_WORDS {
        return Err(EngineError::Checkpoint(-2));
    }

    let mut nodes = Vec::new();
    nodes
        .try_reserve_exact(node_count)
        .map_err(|_| EngineError::Checkpoint(-12))?;
    for _ in 0..node_count {
        if checkpoint_payload_word(payload, &mut offset)? != ENGINE_CHECKPOINT_NODE_MAGIC {
            return Err(EngineError::Checkpoint(-2));
        }
        let btree_id = checkpoint_payload_word(payload, &mut offset)?;
        let level = checkpoint_payload_word(payload, &mut offset)?;
        let page = checkpoint_payload_word(payload, &mut offset)?;
        let entry_count = usize::try_from(checkpoint_payload_word(payload, &mut offset)?)
            .map_err(|_| EngineError::Checkpoint(-2))?;
        let page_words = usize::try_from(checkpoint_payload_word(payload, &mut offset)?)
            .map_err(|_| EngineError::Checkpoint(-2))?;
        let checksum_lo = checkpoint_payload_word(payload, &mut offset)?;
        let checksum_hi = checkpoint_payload_word(payload, &mut offset)?;
        if btree_id > u8::MAX as u64
            || btree_id as usize >= BTREE_ID_NR
            || level > u8::MAX as u64
            || page > u32::MAX as u64
            || entry_count == 0
            || entry_count > page_words / (BKEY_U64S as usize)
        {
            return Err(EngineError::Checkpoint(-2));
        }
        if page_words > (payload.len() - offset) / core::mem::size_of::<u64>() {
            return Err(EngineError::Checkpoint(-2));
        }
        let mut words = Vec::new();
        words
            .try_reserve_exact(page_words)
            .map_err(|_| EngineError::Checkpoint(-12))?;
        for _ in 0..page_words {
            words.push(checkpoint_payload_word(payload, &mut offset)?);
        }
        let checksum = checkpoint_node_checksum(
            BtreeId(btree_id as u8),
            level as u8,
            page as u32,
            entry_count as u64,
            &words,
        )?;
        if checksum.lo != checksum_lo || checksum.hi != checksum_hi {
            return Err(EngineError::Checkpoint(-2));
        }

        let mut entries = Vec::new();
        entries
            .try_reserve_exact(entry_count)
            .map_err(|_| EngineError::Checkpoint(-12))?;
        let mut word_offset = 0usize;
        for _ in 0..entry_count {
            if words.len().saturating_sub(word_offset) < BKEY_U64S as usize {
                return Err(EngineError::Checkpoint(-2));
            }
            let raw = words[word_offset..].as_ptr().cast::<bkey_i>();
            let key_u64s = unsafe { (*raw).k.u64s as usize };
            if key_u64s < BKEY_U64S as usize
                || key_u64s > BKEY_U64S as usize + BKEY_VAL_U64S_MAX as usize
                || word_offset
                    .checked_add(key_u64s)
                    .filter(|end| *end <= words.len())
                    .is_none()
            {
                return Err(EngineError::Checkpoint(-2));
            }
            let key = unsafe {
                if (*raw).k.format != KEY_FORMAT_CURRENT || (*raw).k.type_ != KEY_TYPE_cookie {
                    return Err(EngineError::Checkpoint(-2));
                }
                decode_key(bkey_s_c {
                    k: &(*raw).k,
                    v: &(*raw).v,
                })
                .map_err(|_| EngineError::Checkpoint(-2))?
            };
            entries.push(key);
            word_offset += key_u64s;
        }
        if word_offset != words.len() {
            return Err(EngineError::Checkpoint(-2));
        }
        nodes.push(CheckpointNode {
            btree: BtreeId(btree_id as u8),
            level: level as u8,
            page: page as u32,
            entries,
        });
    }
    if offset != payload.len() {
        return Err(EngineError::Checkpoint(-2));
    }

    let image = CheckpointImage {
        sequence: expected_sequence,
        generation: expected_generation,
        roots,
        nodes,
    };
    validate_checkpoint_image(&image)?;
    Ok(image)
}

fn checkpoint_slot_offset(slot: usize) -> u64 {
    debug_assert!(slot < CHECKPOINT_HEADER_SLOTS);
    slot as u64 * CHECKPOINT_HEADER_BYTES as u64
}

fn checkpoint_header_word(header: &[u8], index: usize) -> u64 {
    let start = index * core::mem::size_of::<u64>();
    u64::from_le_bytes(
        header[start..start + core::mem::size_of::<u64>()]
            .try_into()
            .unwrap(),
    )
}

fn set_checkpoint_header_word(header: &mut [u8], index: usize, value: u64) {
    let start = index * core::mem::size_of::<u64>();
    header[start..start + core::mem::size_of::<u64>()].copy_from_slice(&value.to_le_bytes());
}

fn checkpoint_header(
    slot: CheckpointSlot,
    payload_checksum: crate::btree::bset::bch_csum,
) -> [u8; CHECKPOINT_HEADER_BYTES] {
    let mut header = [0u8; CHECKPOINT_HEADER_BYTES];
    set_checkpoint_header_word(&mut header, 0, ENGINE_CHECKPOINT_MAGIC);
    set_checkpoint_header_word(
        &mut header,
        1,
        STORAGE_FORMAT_VERSION as u64 | (CHECKPOINT_HEADER_WORDS as u64) << 32,
    );
    set_checkpoint_header_word(&mut header, 2, slot.generation);
    set_checkpoint_header_word(&mut header, 3, slot.sequence);
    set_checkpoint_header_word(&mut header, 4, slot.offset);
    set_checkpoint_header_word(&mut header, 5, slot.bytes);
    set_checkpoint_header_word(&mut header, 6, slot.capacity);
    set_checkpoint_header_word(&mut header, 7, payload_checksum.lo);
    set_checkpoint_header_word(&mut header, 8, payload_checksum.hi);
    let checksum = crate::checksum::bch2_checksum(
        crate::checksum::BCH_CSUM_xxhash,
        &header[..9 * core::mem::size_of::<u64>()],
    );
    set_checkpoint_header_word(&mut header, 9, checksum.lo);
    set_checkpoint_header_word(&mut header, 10, checksum.hi);
    header
}

fn read_exact_at(file: &File, data: &mut [u8], offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;

    let mut read = 0usize;
    while read < data.len() {
        let nr = file.read_at(&mut data[read..], offset + read as u64)?;
        if nr == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "checkpoint read reached end of file",
            ));
        }
        read += nr;
    }
    Ok(())
}

fn write_all_at(file: &File, data: &[u8], offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;

    let mut written = 0usize;
    while written < data.len() {
        let nr = file.write_at(&data[written..], offset + written as u64)?;
        if nr == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "checkpoint write made no progress",
            ));
        }
        written += nr;
    }
    Ok(())
}

fn checkpoint_align_up(value: u64) -> Result<u64, EngineError> {
    value
        .checked_add(CHECKPOINT_ALIGN - 1)
        .map(|value| value / CHECKPOINT_ALIGN * CHECKPOINT_ALIGN)
        .ok_or(EngineError::Checkpoint(-12))
}

fn checkpoint_capacity(bytes: u64) -> Result<u64, EngineError> {
    bytes
        .max(CHECKPOINT_ALIGN)
        .checked_next_power_of_two()
        .ok_or(EngineError::Checkpoint(-12))
}

unsafe fn persistent_checkpoint_file(fs: &bch_fs) -> Result<&File, EngineError> {
    if fs.disk_sb.s_bdev_file.is_null() {
        return Err(EngineError::Checkpoint(-1));
    }
    Ok(&*fs.disk_sb.s_bdev_file.cast::<File>())
}

unsafe fn read_checkpoint_slot(
    file: &File,
    file_len: u64,
    slot_index: usize,
) -> Result<Option<(CheckpointSlot, CheckpointImage)>, EngineError> {
    let mut header = [0u8; CHECKPOINT_HEADER_BYTES];
    read_exact_at(file, &mut header, checkpoint_slot_offset(slot_index))?;
    if header.iter().all(|byte| *byte == 0) {
        return Ok(None);
    }
    if checkpoint_header_word(&header, 0) != ENGINE_CHECKPOINT_MAGIC {
        return Ok(None);
    }
    let version_and_words = checkpoint_header_word(&header, 1);
    let version = version_and_words as u32;
    let header_words = (version_and_words >> 32) as usize;
    if version != STORAGE_FORMAT_VERSION {
        return Err(EngineError::UnsupportedFormatVersion(version));
    }
    if header_words != CHECKPOINT_HEADER_WORDS {
        return Ok(None);
    }
    let expected = crate::checksum::bch2_checksum(
        crate::checksum::BCH_CSUM_xxhash,
        &header[..9 * core::mem::size_of::<u64>()],
    );
    if checkpoint_header_word(&header, 9) != expected.lo
        || checkpoint_header_word(&header, 10) != expected.hi
    {
        return Ok(None);
    }

    let slot = CheckpointSlot {
        generation: checkpoint_header_word(&header, 2),
        sequence: checkpoint_header_word(&header, 3),
        offset: checkpoint_header_word(&header, 4),
        bytes: checkpoint_header_word(&header, 5),
        capacity: checkpoint_header_word(&header, 6),
    };
    if slot.generation == 0
        || slot.sequence == 0
        || slot.offset < CHECKPOINT_DATA_START
        || slot.offset % CHECKPOINT_ALIGN != 0
        || slot.bytes == 0
        || slot.bytes > slot.capacity
        || slot.capacity < CHECKPOINT_ALIGN
        || slot.capacity % CHECKPOINT_ALIGN != 0
        || slot
            .offset
            .checked_add(slot.capacity)
            .filter(|end| *end <= file_len)
            .is_none()
    {
        return Ok(None);
    }
    let payload_len = match usize::try_from(slot.bytes) {
        Ok(length) => length,
        Err(_) => return Ok(None),
    };
    let mut payload = Vec::new();
    if payload.try_reserve_exact(payload_len).is_err() {
        return Ok(None);
    }
    payload.resize(payload_len, 0);
    read_exact_at(file, &mut payload, slot.offset)?;
    let expected_payload =
        crate::checksum::bch2_checksum(crate::checksum::BCH_CSUM_xxhash, &payload);
    if checkpoint_header_word(&header, 7) != expected_payload.lo
        || checkpoint_header_word(&header, 8) != expected_payload.hi
    {
        return Ok(None);
    }
    match checkpoint_image_from_payload(&payload, slot.sequence, slot.generation) {
        Ok(image) => Ok(Some((slot, image))),
        Err(EngineError::UnsupportedFormatVersion(version)) => {
            Err(EngineError::UnsupportedFormatVersion(version))
        }
        Err(_) => Ok(None),
    }
}

unsafe fn read_persistent_checkpoint(fs: &bch_fs) -> Result<CheckpointState, EngineError> {
    let file = persistent_checkpoint_file(fs)?;
    let file_len = file.metadata()?.len();
    if file_len < CHECKPOINT_DATA_START {
        return Err(EngineError::Checkpoint(-1));
    }

    let mut state = CheckpointState::default();
    let mut selected: Option<(usize, CheckpointSlot, CheckpointImage)> = None;
    for index in 0..CHECKPOINT_HEADER_SLOTS {
        let Some((slot, image)) = read_checkpoint_slot(file, file_len, index)? else {
            continue;
        };
        state.slots[index] = slot;
        match &selected {
            None => selected = Some((index, slot, image)),
            Some((_, selected_slot, selected_image)) => {
                if slot.generation == selected_slot.generation && &image != selected_image {
                    return Err(EngineError::Checkpoint(-3));
                }
                if (slot.generation, slot.sequence)
                    > (selected_slot.generation, selected_slot.sequence)
                {
                    selected = Some((index, slot, image));
                }
            }
        }
    }
    if let Some((index, _, image)) = selected {
        state.image = image;
        state.active_slot = Some(index);
    }
    Ok(state)
}

unsafe fn write_persistent_checkpoint(
    fs: &mut bch_fs,
    state: &mut CheckpointState,
) -> Result<(), EngineError> {
    let payload = checkpoint_payload(&state.image)?;
    let payload_len = u64::try_from(payload.len()).map_err(|_| EngineError::Checkpoint(-12))?;
    let file = persistent_checkpoint_file(fs)?;
    let target = state
        .active_slot
        .map(|active| (active + 1) % CHECKPOINT_HEADER_SLOTS)
        .unwrap_or(0);
    let active_offset = state.active_slot.map(|active| state.slots[active].offset);
    let mut slot = state.slots[target];
    let reusable = slot.offset >= CHECKPOINT_DATA_START
        && slot.offset % CHECKPOINT_ALIGN == 0
        && slot.capacity >= payload_len
        && Some(slot.offset) != active_offset;
    if !reusable {
        let offset = checkpoint_align_up(file.metadata()?.len().max(CHECKPOINT_DATA_START))?;
        let capacity = checkpoint_capacity(payload_len)?;
        let end = offset
            .checked_add(capacity)
            .ok_or(EngineError::Checkpoint(-12))?;
        file.set_len(end)?;
        slot.offset = offset;
        slot.capacity = capacity;
    }
    slot.generation = state.image.generation;
    slot.sequence = state.image.sequence;
    slot.bytes = payload_len;

    write_all_at(file, &payload, slot.offset)?;
    /* The payload must reach stable storage before its alternate header can
     * make it reachable after a crash. */
    file.sync_all()?;
    if consume_fault(&fs.journal.fault_inject_checkpoint_write_error) {
        return Err(EngineError::Checkpoint(-5));
    }

    let payload_checksum =
        crate::checksum::bch2_checksum(crate::checksum::BCH_CSUM_xxhash, &payload);
    let header = checkpoint_header(slot, payload_checksum);
    write_all_at(file, &header, checkpoint_slot_offset(target))?;
    file.sync_all()?;
    state.slots[target] = slot;
    state.active_slot = Some(target);
    Ok(())
}

unsafe fn configure_persistent_journal(
    fs: &mut bch_fs,
    file: std::fs::File,
) -> Result<(), EngineError> {
    if fs.disk_sb.sb.is_null() || !fs.disk_sb.s_bdev_file.is_null() {
        return Err(EngineError::Transaction(-1));
    }

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
        nbuckets: JOURNAL_FILE_SECTORS / JOURNAL_BUCKET_SIZE as u64,
        first_bucket: 0,
        bucket_size: JOURNAL_BUCKET_SIZE,
        ..Default::default()
    };

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

    fn newest_checkpoint_slot(path: &Path) -> (usize, CheckpointSlot) {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        let file_len = file.metadata().unwrap().len();
        (0..CHECKPOINT_HEADER_SLOTS)
            .filter_map(|index| unsafe {
                read_checkpoint_slot(&file, file_len, index)
                    .unwrap()
                    .map(|(slot, _)| (index, slot))
            })
            .max_by_key(|(_, slot)| (slot.generation, slot.sequence))
            .expect("checkpoint slot was published")
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
    fn checkpoint_reclaims_old_records_and_replays_the_tail() {
        let engine = StorageEngine::new().unwrap();
        let secondary = BtreeId::new(1).unwrap();
        engine.put(BtreeId::DEFAULT, key(21, &[1, 2])).unwrap();
        engine.put(secondary, key(22, &[3])).unwrap();
        engine.flush_journal().unwrap();
        assert_eq!(engine.durable_journal().unwrap().checkpoint_sequence(), 0);

        engine.reclaim_journal().unwrap();
        let checkpointed = engine.durable_journal().unwrap();
        assert_ne!(checkpointed.checkpoint_sequence(), 0);
        assert_eq!(checkpointed.checkpoint_key_count(), 2);
        /* The retained record is the empty post-checkpoint anchor, not the
         * original key-bearing transaction record. */
        assert_eq!(checkpointed.record_count(), 1);

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

        /* Once last_seq has advanced, the durable base alone is a valid
         * clean-recovery state even if no journal anchor remains to replay. */
        let mut clean = checkpointed.clone();
        clean.records.clear();
        let clean_recovered = StorageEngine::recover(&clean).unwrap();
        assert_eq!(
            clean_recovered.scan(BtreeId::DEFAULT).unwrap(),
            vec![key(21, &[1, 2])]
        );
        assert_eq!(
            clean_recovered.scan(secondary).unwrap(),
            vec![key(22, &[3])]
        );
    }

    #[test]
    fn checkpoint_header_failure_keeps_the_old_journal_window() {
        let engine = StorageEngine::new().unwrap();
        engine.put(BtreeId::DEFAULT, key(24, &[7])).unwrap();
        engine.flush_journal().unwrap();
        engine.inject_fault(FaultPoint::CheckpointWrite, 1).unwrap();
        assert!(matches!(
            engine.checkpoint(),
            Err(EngineError::Checkpoint(-5))
        ));

        let image = engine.durable_journal().unwrap();
        assert_eq!(image.checkpoint_sequence(), 0);
        assert_eq!(image.record_count(), 1);
        assert_eq!(
            StorageEngine::recover(&image)
                .unwrap()
                .get(BtreeId::DEFAULT, KeyPosition::new(1, 24, 0))
                .unwrap(),
            Some(key(24, &[7]))
        );
    }

    #[test]
    fn persistent_checkpoint_recovers_at_each_publication_cutpoint() {
        let path = std::env::temp_dir().join(format!(
            "subvol-engine-checkpoint-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));

        {
            let engine = StorageEngine::create_persistent(&path).unwrap();
            engine.put(BtreeId::DEFAULT, key(25, &[8, 9])).unwrap();
            engine.flush_journal().unwrap();
            engine.inject_fault(FaultPoint::CheckpointWrite, 1).unwrap();
            assert!(matches!(
                engine.checkpoint(),
                Err(EngineError::Checkpoint(-5))
            ));
        }
        let after_unpublished_header = StorageEngine::open_persistent(&path).unwrap();
        assert_eq!(
            after_unpublished_header
                .get(BtreeId::DEFAULT, KeyPosition::new(1, 25, 0))
                .unwrap(),
            Some(key(25, &[8, 9]))
        );
        drop(after_unpublished_header);

        {
            let engine = StorageEngine::open_persistent(&path).unwrap();
            engine
                .inject_fault(FaultPoint::CheckpointBarrier, 1)
                .unwrap();
            assert!(matches!(
                engine.checkpoint(),
                Err(EngineError::Checkpoint(-5))
            ));
            let image = engine.durable_journal().unwrap();
            assert_ne!(image.checkpoint_sequence(), 0);
            assert_eq!(
                StorageEngine::recover(&image)
                    .unwrap()
                    .get(BtreeId::DEFAULT, KeyPosition::new(1, 25, 0))
                    .unwrap(),
                Some(key(25, &[8, 9]))
            );
        }
        let after_published_header = StorageEngine::open_persistent(&path).unwrap();
        assert_eq!(
            after_published_header
                .get(BtreeId::DEFAULT, KeyPosition::new(1, 25, 0))
                .unwrap(),
            Some(key(25, &[8, 9]))
        );
        drop(after_published_header);

        {
            let engine = StorageEngine::open_persistent(&path).unwrap();
            engine.checkpoint().unwrap();
            engine.put(BtreeId::DEFAULT, key(26, &[10])).unwrap();
            engine.flush_journal().unwrap();
            engine.checkpoint().unwrap();
        }

        /* A complete checkpoint may reclaim every physical journal record.
         * The alternate checkpoint header must then bootstrap recovery on its
         * own, exactly as the written btree base precedes journal replay. */
        {
            use std::os::unix::fs::FileExt;

            let file = OpenOptions::new().write(true).open(&path).unwrap();
            let zeros = vec![0; JOURNAL_BUCKETS as usize * JOURNAL_BUCKET_SIZE as usize * 512];
            let offset = JOURNAL_BUCKET_START * JOURNAL_BUCKET_SIZE as u64 * 512;
            assert_eq!(file.write_at(&zeros, offset).unwrap(), zeros.len());
            file.sync_all().unwrap();
        }
        let recovered = StorageEngine::open_persistent(&path).unwrap();
        assert_eq!(
            recovered.scan(BtreeId::DEFAULT).unwrap(),
            vec![key(25, &[8, 9]), key(26, &[10])]
        );
        recovered.verify(BtreeId::DEFAULT).unwrap();
        drop(recovered);
        std::fs::remove_file(path).unwrap();
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
    fn generated_checkpoint_recovery_matches_the_model() {
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
                    engine.checkpoint().unwrap();
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
        let engine = StorageEngine::new().unwrap();
        let mut transaction = engine.transaction();
        transaction.put(BtreeId::DEFAULT, key(401, &[1, 2, 3]));
        let durable = transaction.commit_sync().unwrap();
        assert_ne!(durable.journal_sequence_ondisk, 0);
        assert_eq!(durable.checkpoint_generation, 0);

        let before = engine.metrics().unwrap();
        assert_eq!(before.checkpoint.generation, 0);
        assert_ne!(before.journal_records, 0);

        let checkpoint = engine.checkpoint_sync().unwrap();
        assert_ne!(checkpoint.checkpoint_generation, 0);
        let after = engine.metrics().unwrap();
        assert_eq!(
            after.checkpoint.generation,
            checkpoint.checkpoint_generation
        );
        assert_eq!(after.checkpoint.key_count, 1);
        assert_eq!(engine.durable_journal().unwrap().checkpoint_node_count(), 1);
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
        assert_ne!(engine.metrics().unwrap().checkpoint.generation, 0);
        assert_eq!(
            engine.scan(BtreeId::DEFAULT).unwrap(),
            vec![key(410, &[1]), key(411, &[2, 3])]
        );
    }

    #[test]
    fn checkpoint_pages_are_cow_and_corrupt_page_falls_back_to_prior_root() {
        let path = persistent_test_path("checkpoint-cow-pages");
        {
            let engine = StorageEngine::create_persistent(&path).unwrap();
            for batch in 0..2u64 {
                let mut transaction = engine.transaction();
                for offset in 0..16u64 {
                    let offset = batch * 16 + offset;
                    transaction.put(
                        BtreeId::DEFAULT,
                        key(
                            500 + offset,
                            &[offset, offset.wrapping_add(1), offset.wrapping_add(2)],
                        ),
                    );
                }
                transaction.commit().unwrap();
            }
            engine.sync().unwrap();
            let first = engine.checkpoint_sync().unwrap();
            assert!(first.checkpoint_generation >= 1);
            assert!(engine.read_transaction().checkpoint().node_count > 1);

            engine
                .put_sync(BtreeId::DEFAULT, key(900, &[0xfeed, 0xbeef]))
                .unwrap();
            engine.checkpoint_sync().unwrap();
        }

        let (slot_index, slot) = newest_checkpoint_slot(&path);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let mut payload = vec![0; usize::try_from(slot.bytes).unwrap()];
        read_exact_at(&file, &mut payload, slot.offset).unwrap();
        let first_page_word = (CHECKPOINT_PAYLOAD_HEADER_WORDS
            + CHECKPOINT_ROOT_WORDS
            + CHECKPOINT_NODE_HEADER_WORDS)
            * core::mem::size_of::<u64>();
        payload[first_page_word] ^= 0x80;

        /* Rewrite the whole-payload checksum and header: recovery must still
         * reject the image through the immutable page checksum, then select
         * the alternate COW root. */
        let checksum = crate::checksum::bch2_checksum(crate::checksum::BCH_CSUM_xxhash, &payload);
        write_all_at(&file, &payload, slot.offset).unwrap();
        write_all_at(
            &file,
            &checkpoint_header(slot, checksum),
            checkpoint_slot_offset(slot_index),
        )
        .unwrap();
        file.sync_all().unwrap();
        drop(file);
        clear_journal_region(&path);

        let recovered = StorageEngine::open_persistent(&path).unwrap();
        assert_eq!(recovered.scan(BtreeId::DEFAULT).unwrap().len(), 32);
        assert!(recovered
            .get(BtreeId::DEFAULT, KeyPosition::new(1, 900, 0))
            .unwrap()
            .is_none());
        drop(recovered);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn corrupt_journal_tail_never_overrides_a_valid_checkpoint_base() {
        let path = persistent_test_path("corrupt-journal-tail");
        {
            let engine = StorageEngine::create_persistent(&path).unwrap();
            engine.put_sync(BtreeId::DEFAULT, key(600, &[1])).unwrap();
            engine.checkpoint_sync().unwrap();
            engine
                .inject_fault(FaultPoint::CheckpointWrite, u32::MAX)
                .unwrap();
            engine.put_sync(BtreeId::DEFAULT, key(601, &[2])).unwrap();
        }

        clear_journal_region(&path);
        let file = OpenOptions::new().write(true).open(&path).unwrap();
        let journal_start = JOURNAL_BUCKET_START * JOURNAL_BUCKET_SIZE as u64 * 512;
        assert_eq!(file.write_at(&[0xa5; 64], journal_start).unwrap(), 64);
        file.sync_all().unwrap();
        drop(file);

        match StorageEngine::open_persistent(&path) {
            Ok(recovered) => {
                assert_eq!(
                    recovered
                        .get(BtreeId::DEFAULT, KeyPosition::new(1, 600, 0))
                        .unwrap(),
                    Some(key(600, &[1]))
                );
                assert!(recovered
                    .get(BtreeId::DEFAULT, KeyPosition::new(1, 601, 0))
                    .unwrap()
                    .is_none());
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
                    let _ = reader.checkpoint();
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
                engine.checkpoint_sync().unwrap();
            }
            "tail" => {
                engine.put_sync(BtreeId::DEFAULT, key(703, &[3])).unwrap();
                engine.checkpoint_sync().unwrap();
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
