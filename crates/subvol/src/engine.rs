//! Safe, single-format storage-engine API over the bcachefs-style btree,
//! transaction and journal core.
//!
//! The raw port remains internal: every mutation below is staged through an
//! intent iterator, committed in a transaction, and made recoverable only by
//! a successfully flushed journal record.  This is deliberately an engine
//! core, not a filesystem-compatibility layer.

use std::{
    fmt,
    fs::OpenOptions,
    io,
    path::Path,
    sync::{atomic::Ordering, Mutex, MutexGuard},
};

use crate::{
    btree::{
        bkey::{
            bkey, bkey_err, bkey_i, bkey_s_c, bkey_val_u64s, bpos, bpos_eq, BKEY_U64S,
            BKEY_VAL_U64S_MAX, KEY_FORMAT_CURRENT, POS_MIN,
        },
        bset::{KEY_TYPE_cookie, KEY_TYPE_deleted},
        cache::bch2_fs_btree_cache_init,
        interior::{bch2_btree_node_check_topology, bch2_btree_root_alloc_fake},
        iter::{
            bch2_btree_iter_next, bch2_btree_iter_peek, bch2_btree_iter_traverse, bch2_trans_begin,
            bch2_trans_init, bch2_trans_iter_exit, bch2_trans_iter_init, bch2_trans_put,
            btree_iter, btree_trans, BTREE_ITER_intent, BTREE_ITER_not_extents,
        },
        types::{
            bch2_btree_id_root_b, bch_fs, clear_btree_node_fake, clear_btree_node_need_rewrite,
            BTREE_ID_NR,
        },
        update::{bch2_trans_commit, bch2_trans_update},
    },
    journal::{
        bch2_journal_flush, bch2_journal_read, bch2_journal_replay,
        bch2_journal_restore_for_replay, journal_start_info,
    },
    sb::{
        bcachefs_metadata_version_current, bch_member, bch_sb_field_journal_v2,
        bch_sb_field_journal_v2_entry, bch_sb_field_members_v2, BCH_SB_FIELD_journal_v2,
        BCH_SB_FIELD_members_v2, BCHFS_MAGIC,
    },
};

/// The only durable engine data format accepted by this crate.
pub const STORAGE_FORMAT_VERSION: u32 = 1;

const JOURNAL_FILE_SECTORS: u64 = 128;
const JOURNAL_BUCKET_START: u64 = 32;
const JOURNAL_BUCKETS: u64 = 4;
const JOURNAL_BUCKET_SIZE: u16 = 2;
const ENGINE_JOURNAL_UUID: [u8; 16] = [0x53; 16];

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

#[derive(Debug)]
pub enum EngineError {
    InvalidBtreeId(u8),
    ValueTooLarge(usize),
    UnsupportedFormatVersion(u32),
    Transaction(i32),
    Journal(i32),
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
}

/// A self-contained btree/transaction/journal storage engine.
pub struct StorageEngine {
    /*
     * btree nodes and journal state retain raw references to their owning
     * bch_fs, just as the C implementation obtains it with container_of().
     * Keep that owner at one stable heap address before any such references
     * are initialized; moving StorageEngine must never invalidate them.
     */
    fs: Mutex<Box<bch_fs>>,
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
        Ok(Self { fs: Mutex::new(fs) })
    }

    /// Creates and initializes a persistent bcachefs-style journal device.
    /// The btree remains an engine-core in-memory base; durable updates are
    /// recovered by replaying this journal into a fresh base after a crash.
    pub fn create_persistent(path: impl AsRef<Path>) -> Result<Self, EngineError> {
        let engine = Self::new()?;
        engine.attach_persistent_journal(path.as_ref(), true)?;
        Ok(engine)
    }

    /// Opens a journal created by `create_persistent()` and replays all
    /// durable records before returning the engine.
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

    pub fn put(&self, btree: BtreeId, key: BtreeKey) -> Result<(), EngineError> {
        let mut transaction = self.transaction();
        transaction.put(btree, key);
        transaction.commit()
    }

    pub fn delete(&self, btree: BtreeId, position: KeyPosition) -> Result<(), EngineError> {
        let mut transaction = self.transaction();
        transaction.delete(btree, position);
        transaction.commit()
    }

    pub fn get(
        &self,
        btree: BtreeId,
        position: KeyPosition,
    ) -> Result<Option<BtreeKey>, EngineError> {
        let mut fs = self.lock_fs()?;
        unsafe {
            let mut trans = btree_trans::default();
            bch2_trans_init(&mut trans, &mut **fs);
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
    }

    /// Returns all live keys ordered by their iterator search position.
    pub fn scan(&self, btree: BtreeId) -> Result<Vec<BtreeKey>, EngineError> {
        let mut fs = self.lock_fs()?;
        unsafe {
            let mut trans = btree_trans::default();
            bch2_trans_init(&mut trans, &mut **fs);
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
        let fs = self.lock_fs()?;
        let ret = bch2_journal_flush(&fs.journal);
        if ret == 0 {
            Ok(())
        } else {
            Err(EngineError::Journal(ret))
        }
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

    fn lock_fs(&self) -> Result<MutexGuard<'_, Box<bch_fs>>, EngineError> {
        self.fs.lock().map_err(|_| EngineError::Poisoned)
    }

    fn commit_operations(&self, operations: &[TransactionOperation]) -> Result<(), EngineError> {
        if operations.is_empty() {
            return Ok(());
        }

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
        }
        Ok(())
    }
}

impl Drop for StorageEngine {
    fn drop(&mut self) {
        let fs = match self.fs.get_mut() {
            Ok(fs) => fs,
            Err(poisoned) => poisoned.into_inner(),
        };
        unsafe { crate::sb::io::bch2_free_super(&mut (**fs).disk_sb) };
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
        first_bucket: 8,
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
    use std::collections::BTreeMap;

    use super::*;

    fn key(offset: u64, value: &[u64]) -> BtreeKey {
        BtreeKey::new(KeyPosition::new(1, offset, 0), value.to_vec()).unwrap()
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
}
