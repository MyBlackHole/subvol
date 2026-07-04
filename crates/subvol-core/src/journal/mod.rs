//! Journal — bcachefs 对齐的 btree 崩溃恢复子系统
//!
//! Journal **仅用于 btree crash recovery**，不是常规写入路径的一部分。
//! 正常写入路径：btree node COW 直接写到 BlockDevice。
//!
//! Journal 是一组预分配的 bucket（循环缓冲区），
//! 每个 journal entry = Jset（含 btree update keys）。
//! 崩溃后通过 JournalReplayer 重放未落盘的 btree updates。
//!
//! # 架构
//!
//! ```text
//! ┌─────────────────────┐
//! │  Journal             │
//! │  - buckets[]         │  ← 预分配的 bucket addrs
//! │  - current_bucket    │  ← 当前写入位置
//! │  - pending queue     │  ← 未 flush 的 Jset
//! └─────────┬───────────┘
//!           │
//!           ▼
//! ┌─────────────────────┐
//! │  Jset                │  ← 一个 journal entry
//! │  - seq               │  ← 递增序列号
//! │  - entries[]         │  ← JsetEntry 列表
//! └─────────┬───────────┘
//!           │
//!           ▼
//! ┌─────────────────────┐
//! │  JsetEntry           │  ← 单次 btree 操作
//! │  - btree_type        │  ← 目标 btree type
//! │  - btree_keys        │  ← bincode: Vec<BtreeEntry>
//! └─────────────────────┘
//! ```
//!
//! # 崩溃恢复流程
//!
//! 1. Daemon 层读取 Superblock + btree roots → 构造 BchVol
//! 2. BchVol::recover_from_journal() → JournalReplayer 读取 journal 中的 root 指针变更 + btree keys
//! 3. load_root() → 加载 btree 根节点（superblock roots + journal roots 合并）
//! 4. JournalReplayer.replay_all_to_vol() → 重放未落盘的 btree keys
//! 5. recovery 完成，Volume 正常操作
//!
//! # 支持的操作
//!
//! - Append btree update keys
//! - Append btree_root entries
//! - Flush to backend
//! - Readback all entries
//! - Replay (walk all entries)
//! - Overflow detection
//! - Bucket reclaim（已落盘的 bucket 可回收）
//!
//! # bcachefs 对齐
//!
//! | subvol | bcachefs |
//! |--------|----------|
//! | `Journal::bch2_journal_cur_seq()` | `journal_cur_seq()` (journal.h) |
//! | `JournalResState` | `union journal_res_state` (types.h:142) |
//! | `JournalRes` | `struct journal_res` (types.h:134) |
//! | `JournalEntryPin` | `struct journal_entry_pin` (types.h:128) |
//! | `Jset` / `JsetHeader` | `struct jset` |
//! | `JournalReplayer` | recovery pass 流程 |

use std::cell::UnsafeCell;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, OnceLock, RwLock, Weak};

use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use crate::btree::key::{Bpos, BtreeEntry};
use crate::btree::types::BtreeId;
use crate::types::{StorageError, Watermark};
use crate::BchVol;

// ═══════════════════════════════════════════════════════════════
// Part 1: Constants
// ═══════════════════════════════════════════════════════════════

pub const DEFAULT_JOURNAL_BUCKETS: u32 = 32;
pub const BUCKET_BLOCKS: u32 = 256;
pub const JSET_BLOCK_SIZE: u32 = 4096;
pub const BUF_SIZE: usize = 131072;
pub const JOURNAL_STATE_BUF_NR: usize = 4;
pub const JOURNAL_ENTRY_CLOSED_VAL: u64 = 0x3FFFFF - 1;
pub const JOURNAL_ENTRY_ERROR_VAL: u64 = 0x3FFFFF;
pub const JOURNAL_ENTRY_BLOCKED_VAL: u64 = 0x3FFFFF - 2;
pub const JOURNAL_NEEDS_FLUSH_WRITE: u64 = 1 << 0;
pub const MAX_PIN_ENTRIES: usize = 128;
pub const JOURNAL_SPACE_DISCARDED: usize = 0;
pub const JOURNAL_SPACE_CLEAN_ONDISK: usize = 1;
pub const JOURNAL_SPACE_CLEAN: usize = 2;
pub const JOURNAL_SPACE_TOTAL: usize = 3;
pub const JOURNAL_SPACE_NR: usize = 4;

pub const JE_NONE: u8 = 0;
pub const JE_OVERFLOW: u8 = 1;
pub const JE_CHECKSUM: u8 = 2;
pub const JE_IO: u8 = 3;
pub const JE_STUCK: u8 = 4;
pub const JE_FULL: u8 = 5;
pub const JE_PIN_FULL: u8 = 6;
pub const JE_BLOCKED: u8 = 7;

pub const JOURNAL_MAGIC: [u8; 8] = *b"VOLM_JNL";
pub const VMNT_JSET_MAGIC: [u8; 8] = *b"VMNTJNL0";
pub const JSET_VERSION: u32 = 2;
pub const JSET_ENTRY_VERSION: u8 = 1;
pub const CSUM_TYPE_NONE: u8 = 0;
pub const CSUM_TYPE_CRC32C: u8 = 1;
pub const JSET_CSUM_TYPE_MASK: u32 = 0x0f;

pub const PIN_FIFO_SIZE: usize = 128;

// ═══════════════════════════════════════════════════════════════
// Part 2: CRC32C
// ═══════════════════════════════════════════════════════════════

/// CRC32C 纯软件实现（Castagnoli 多项式 0x1EDC6F41）
pub fn crc32c_sw(data: &[u8], crc: u32) -> u32 {
    let table = &*CRC32C_TABLE;
    let mut crc = !crc;
    for &byte in data {
        let idx = ((crc as u8) ^ byte) as usize;
        crc = table[idx] ^ (crc >> 8);
    }
    !crc
}

static CRC32C_TABLE: LazyLock<[u32; 256]> = LazyLock::new(|| {
    let mut table = [0u32; 256];
    let mut i = 0u32;
    while i < 256 {
        let mut crc = i;
        let mut j = 0;
        while j < 8 {
            if crc & 1 != 0 {
                crc = 0x82F63B78u32 ^ (crc >> 1);
            } else {
                crc >>= 1;
            }
            j += 1;
        }
        table[i as usize] = crc;
        i += 1;
    }
    table
});

/// CRC32C 自动选择硬件/软件路径
pub fn crc32c(data: &[u8], crc: u32) -> u32 {
    crc32c_sw(data, crc)
}

/// 分块 CRC32C 计算器
pub struct Crc32CHasher {
    crc: u32,
}

impl Crc32CHasher {
    pub fn new() -> Self {
        Self { crc: 0 }
    }
    pub fn update(&mut self, data: &[u8]) {
        self.crc = crc32c(data, self.crc);
    }
    pub fn finalize(&self) -> u32 {
        self.crc
    }
    pub fn hash(data: &[u8]) -> u32 {
        crc32c(data, 0)
    }
}

impl Default for Crc32CHasher {
    fn default() -> Self {
        Self::new()
    }
}

// ═══════════════════════════════════════════════════════════════
// Part 3: Jset 磁盘格式
// ═══════════════════════════════════════════════════════════════

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct JsetHeader {
    pub magic: [u8; 8],
    pub seq: u64,
    pub last_seq: u64,
    pub crc32: u32,
    pub entry_count: u32,
    pub version: u32,
    pub flags: u32,
    pub pad: [u8; 24],
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct JsetEntryHeader {
    pub btree_type: u8,
    pub entry_type: u8,
    pub version: u8,
    pub level: u8,
    pub payload_len: u16,
    pub has_last: u8,
    pub has_prev: u8,
}

#[derive(Debug, Clone)]
pub struct RawJsetEntry {
    pub hdr: JsetEntryHeader,
    pub payload: Vec<u8>,
}

impl RawJsetEntry {
    pub fn new(
        btree_type: u8,
        entry_type: u8,
        payload: Vec<u8>,
        level: u8,
    ) -> Result<Self, StorageError> {
        let payload_len = u16::try_from(payload.len()).map_err(|_| {
            StorageError::Invalid(format!("jset entry payload too large: {}", payload.len()))
        })?;
        Ok(Self {
            hdr: JsetEntryHeader {
                btree_type,
                entry_type,
                version: JSET_ENTRY_VERSION,
                level,
                payload_len,
                has_last: 0,
                has_prev: 0,
            },
            payload,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum JsetEntryType {
    BtreeKeys = 0,
    BtreeRoot = 1,
    Blacklist = 4,
    Usage = 5,
    DataUsage = 6,
    Clock = 7,
    DevUsage = 8,
    Overwrite = 10,
    WriteBufferKeys = 11,
    Datetime = 12,
    RewindLimit = 14,
    Rewind = 15,
}

impl JsetEntryType {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::BtreeKeys),
            1 => Some(Self::BtreeRoot),
            4 => Some(Self::Blacklist),
            5 => Some(Self::Usage),
            6 => Some(Self::DataUsage),
            7 => Some(Self::Clock),
            8 => Some(Self::DevUsage),
            10 => Some(Self::Overwrite),
            11 => Some(Self::WriteBufferKeys),
            12 => Some(Self::Datetime),
            14 => Some(Self::RewindLimit),
            15 => Some(Self::Rewind),
            _ => None,
        }
    }
}

/// 磁盘 Jset
#[derive(Debug, Clone)]
pub struct Jset {
    pub header: JsetHeader,
    pub entries: Vec<RawJsetEntry>,
}

impl Jset {
    pub fn new(seq: u64, last_seq: u64) -> Self {
        Self {
            header: JsetHeader {
                magic: JOURNAL_MAGIC,
                seq,
                last_seq,
                crc32: 0,
                entry_count: 0,
                version: JSET_VERSION,
                flags: CSUM_TYPE_CRC32C as u32,
                pad: [0u8; 24],
            },
            entries: Vec::new(),
        }
    }

    pub fn new_volatile(seq: u64, last_seq: u64) -> Self {
        Self {
            header: JsetHeader {
                magic: VMNT_JSET_MAGIC,
                seq,
                last_seq,
                crc32: 0,
                entry_count: 0,
                version: JSET_VERSION,
                flags: CSUM_TYPE_CRC32C as u32,
                pad: [0u8; 24],
            },
            entries: Vec::new(),
        }
    }

    pub fn serialized_padded_len(&self) -> usize {
        let data_size = std::mem::size_of::<JsetHeader>()
            + self
                .entries
                .iter()
                .map(|e| std::mem::size_of::<JsetEntryHeader>() + e.payload.len())
                .sum::<usize>();
        let block_size = JSET_BLOCK_SIZE as usize;
        let pad = (block_size - (data_size % block_size)) % block_size;
        data_size + pad
    }

    pub fn verify(&self) -> bool {
        if self.header.magic != JOURNAL_MAGIC && self.header.magic != VMNT_JSET_MAGIC {
            return false;
        }
        true
    }

    pub fn serialize_padded(&self) -> Result<Vec<u8>, StorageError> {
        let total_size = self.serialized_padded_len();
        let mut buf = vec![0u8; total_size];
        let mut off = 0;

        let mut header = self.header;
        header.crc32 = 0;
        header.entry_count = self.entries.len() as u32;

        unsafe {
            std::ptr::copy_nonoverlapping(
                &header as *const JsetHeader as *const u8,
                buf.as_mut_ptr(),
                std::mem::size_of::<JsetHeader>(),
            );
        }
        off += std::mem::size_of::<JsetHeader>();

        for entry in &self.entries {
            unsafe {
                std::ptr::copy_nonoverlapping(
                    &entry.hdr as *const JsetEntryHeader as *const u8,
                    buf.as_mut_ptr().add(off),
                    std::mem::size_of::<JsetEntryHeader>(),
                );
            }
            off += std::mem::size_of::<JsetEntryHeader>();
            if !entry.payload.is_empty() {
                buf[off..off + entry.payload.len()].copy_from_slice(&entry.payload);
                off += entry.payload.len();
            }
        }

        if (header.flags & JSET_CSUM_TYPE_MASK) == CSUM_TYPE_CRC32C as u32 {
            let checksum = crc32c(&buf, 0);
            header.crc32 = checksum;
            unsafe {
                std::ptr::copy_nonoverlapping(
                    &header as *const JsetHeader as *const u8,
                    buf.as_mut_ptr(),
                    std::mem::size_of::<JsetHeader>(),
                );
            }
        }

        Ok(buf)
    }

    pub fn deserialize(data: &[u8]) -> Result<Option<Self>, StorageError> {
        if data.len() < std::mem::size_of::<JsetHeader>() {
            return Ok(None);
        }
        let header: JsetHeader =
            unsafe { std::ptr::read_unaligned(data.as_ptr().cast::<JsetHeader>()) };
        if header.magic != JOURNAL_MAGIC && header.magic != VMNT_JSET_MAGIC {
            return Ok(None);
        }
        let entry_count = header.entry_count as usize;
        let mut entries = Vec::with_capacity(entry_count);
        let mut off = std::mem::size_of::<JsetHeader>();
        for _ in 0..entry_count {
            if off + std::mem::size_of::<JsetEntryHeader>() > data.len() {
                return Ok(None);
            }
            let entry_hdr: JsetEntryHeader = unsafe {
                std::ptr::read_unaligned(data.as_ptr().add(off).cast::<JsetEntryHeader>())
            };
            off += std::mem::size_of::<JsetEntryHeader>();
            let payload_len = entry_hdr.payload_len as usize;
            if off + payload_len > data.len() {
                return Ok(None);
            }
            let payload = if payload_len > 0 {
                data[off..off + payload_len].to_vec()
            } else {
                Vec::new()
            };
            off += payload_len;
            entries.push(RawJsetEntry {
                hdr: entry_hdr,
                payload,
            });
        }
        let jset = Jset { header, entries };
        if (jset.header.flags & JSET_CSUM_TYPE_MASK) == CSUM_TYPE_CRC32C as u32 {
            let total = jset.serialized_padded_len();
            if data.len() < total {
                return Ok(None);
            }
            let mut checksum_data = data[..total].to_vec();
            let crc_offset = 24;
            checksum_data[crc_offset..crc_offset + 4].fill(0);
            if crc32c(&checksum_data, 0) != jset.header.crc32 {
                return Err(StorageError::Invalid("journal checksum mismatch".into()));
            }
        }
        Ok(Some(jset))
    }
}

// ═══════════════════════════════════════════════════════════════
// Part 4: Blacklist
// ═══════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlacklistEntry {
    pub start_seq: u64,
    pub end_seq: u64,
}

#[derive(Debug)]
pub struct BlacklistTableEntry {
    pub start: u64,
    pub end: u64,
    pub dirty: AtomicBool,
}

#[derive(Debug)]
pub struct BlacklistTable {
    entries: Vec<BlacklistTableEntry>,
}

impl BlacklistTable {
    pub fn from_entries(entries: &[BlacklistEntry]) -> Self {
        let mut tbl_entries: Vec<BlacklistTableEntry> = entries
            .iter()
            .map(|e| BlacklistTableEntry {
                start: e.start_seq,
                end: e.end_seq,
                dirty: AtomicBool::new(false),
            })
            .collect();
        tbl_entries.sort_by_key(|e| e.start);
        Self {
            entries: tbl_entries,
        }
    }

    pub fn is_blacklisted(&self, seq: u64, dirty: bool) -> bool {
        let idx = self.entries.partition_point(|e| e.start <= seq);
        if idx == 0 {
            return false;
        }
        let entry = &self.entries[idx - 1];
        if seq >= entry.end {
            return false;
        }
        if dirty {
            entry.dirty.store(true, Ordering::Release);
        }
        true
    }

    pub fn next_nonblacklisted(&self, seq: u64) -> u64 {
        let mut s = seq;
        loop {
            let idx = self.entries.partition_point(|e| e.start <= s);
            if idx == 0 || self.entries[idx - 1].end <= s {
                return s;
            }
            s = self.entries[idx - 1].end;
        }
    }

    pub fn next_blacklisted(&self, seq: u64) -> u64 {
        let idx = self.entries.partition_point(|e| e.end <= seq);
        if idx >= self.entries.len() {
            return u64::MAX;
        }
        std::cmp::max(seq, self.entries[idx].start)
    }

    pub fn last_blacklisted_seq(&self) -> u64 {
        self.entries
            .last()
            .map_or(0, |e| if e.end > 0 { e.end - 1 } else { 0 })
    }

    pub fn gc(&self, oldest_seq: u64) -> bool {
        self.entries
            .iter()
            .any(|e| !e.dirty.load(Ordering::Acquire) && e.end < oldest_seq)
    }
}

// ═══════════════════════════════════════════════════════════════
// Part 5: Journal 辅助类型
// ═══════════════════════════════════════════════════════════════

#[derive(Debug)]
pub enum JournalError {
    Overflow(String),
    ChecksumMismatch,
    Io(StorageError),
    Stuck(String),
    Full(String),
    PinFull(String),
    Blocked(String),
}

impl std::fmt::Display for JournalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JournalError::Overflow(msg) => write!(f, "journal overflow: {}", msg),
            JournalError::ChecksumMismatch => write!(f, "journal checksum mismatch"),
            JournalError::Io(e) => write!(f, "journal io error: {}", e),
            JournalError::Stuck(msg) => write!(f, "journal stuck: {}", msg),
            JournalError::Full(msg) => write!(f, "journal full: {}", msg),
            JournalError::PinFull(msg) => write!(f, "journal pin full: {}", msg),
            JournalError::Blocked(msg) => write!(f, "journal blocked: {}", msg),
        }
    }
}

impl std::error::Error for JournalError {}

impl From<StorageError> for JournalError {
    fn from(e: StorageError) -> Self {
        JournalError::Io(e)
    }
}

fn journal_error_code(err: &JournalError) -> i32 {
    match err {
        JournalError::Overflow(_) => JE_OVERFLOW as i32,
        JournalError::ChecksumMismatch => JE_CHECKSUM as i32,
        JournalError::Io(_) => JE_IO as i32,
        JournalError::Stuck(_) => JE_STUCK as i32,
        JournalError::Full(_) => JE_FULL as i32,
        JournalError::PinFull(_) => JE_PIN_FULL as i32,
        JournalError::Blocked(_) => JE_BLOCKED as i32,
    }
}

fn journal_error_from_code(code: i32) -> Option<JournalError> {
    match code {
        x if x == JE_OVERFLOW as i32 => {
            Some(JournalError::Overflow("journal entry error".into()))
        }
        x if x == JE_CHECKSUM as i32 => Some(JournalError::ChecksumMismatch),
        x if x == JE_IO as i32 => Some(JournalError::Io(StorageError::Internal(
            "journal entry error".into(),
        ))),
        x if x == JE_STUCK as i32 => Some(JournalError::Stuck("journal entry error".into())),
        x if x == JE_FULL as i32 => Some(JournalError::Full("journal entry error".into())),
        x if x == JE_PIN_FULL as i32 => Some(JournalError::PinFull("journal entry error".into())),
        x if x == JE_BLOCKED as i32 => Some(JournalError::Blocked("journal entry error".into())),
        _ => None,
    }
}

/// 对应 bcachefs `struct journal_res`
#[derive(Debug)]
pub struct JournalRes {
    pub seq: u64,
    pub offset: u32,
    pub start_offset: u32,
    pub end_offset: u32,
    pub u64s: u32,
    pub buf_idx: u32,
    pub must_flush: bool,
}

/// JournalResState — 简化版
pub struct JournalResState {
    bits: AtomicU64,
}

impl JournalResState {
    pub const fn new() -> Self {
        Self {
            bits: AtomicU64::new(JOURNAL_ENTRY_CLOSED_VAL),
        }
    }

    pub fn read(&self) -> u64 {
        self.bits.load(Ordering::Acquire)
    }

    pub fn cur_entry_offset(v: u64) -> u32 {
        (v & 0x3FFFFF) as u32
    }

    pub fn idx(v: u64) -> u32 {
        ((v >> 22) & 0x3) as u32
    }

    pub fn buf_count(v: u64, idx: u32) -> u32 {
        let shift = 24 + idx * 10;
        ((v >> shift) & 0x3FF) as u32
    }

    pub fn is_closed(&self) -> bool {
        Self::cur_entry_offset(self.bits.load(Ordering::Relaxed)) as u64 >= JOURNAL_ENTRY_CLOSED_VAL
    }
}

/// Journal 空间类型
#[derive(Debug, Clone, Copy)]
pub struct JournalSpace {
    pub total: u64,
    pub available: u64,
}

impl JournalSpace {
    pub const fn new() -> Self {
        Self {
            total: 0,
            available: 0,
        }
    }
}

impl Default for JournalSpace {
    fn default() -> Self {
        Self::new()
    }
}

/// U64 range
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct U64Range {
    pub start: u64,
    pub end: u64,
}

/// Journal 启动信息
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct JournalStartInfo {
    pub last_seq: u64,
    pub replay_end: u64,
    pub cur_seq: u64,
    pub clean: bool,
}

/// Journal superblock 状态
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalSuperblockState {
    pub bucket_addrs: Vec<u64>,
    pub last_seq: u64,
    pub last_seq_ondisk: u64,
    pub last_bucket: u32,
    pub discard_idx: u32,
    pub dirty_idx: u32,
    pub dirty_idx_ondisk: u32,
    pub bucket_seq: Vec<u64>,
    pub replayed_seqs: Vec<u64>,
}

/// BufState
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BufState {
    Free,
    Accepting,
    Closing,
    Noflush,
    WriteSubmitted,
    WriteDone,
}

// ═══════════════════════════════════════════════════════════════
// Part 6: Journal Block Guard
// ═══════════════════════════════════════════════════════════════

pub struct JournalBlockGuard<'a> {
    journal: &'a Journal,
}

impl Drop for JournalBlockGuard<'_> {
    fn drop(&mut self) {
        self.journal.bch2_journal_unblock();
    }
}

// ═══════════════════════════════════════════════════════════════
// Part 7: Pin 类型
// ═══════════════════════════════════════════════════════════════

pub type JournalPinFlushFn =
    Box<dyn Fn(&Journal, &JournalEntryPin, u64) -> Result<(), StorageError> + Send>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum JournalPinType {
    Btree3 = 0,
    Btree2 = 1,
    Btree1 = 2,
    Btree0 = 3,
    KeyCache = 4,
    Other = 5,
}

pub const JOURNAL_PIN_TYPE_NR: usize = 6;

/// JournalEntryPin — 简化版
#[repr(C)]
pub struct JournalEntryPin {
    pub seq: AtomicU64,
    pub pin_type: JournalPinType,
    pub flush: UnsafeCell<Option<JournalPinFlushFn>>,
}

impl JournalEntryPin {
    pub fn new(flush: Option<JournalPinFlushFn>, pin_type: JournalPinType) -> Self {
        Self {
            seq: AtomicU64::new(0),
            pin_type,
            flush: UnsafeCell::new(flush),
        }
    }

    pub fn is_active(&self) -> bool {
        self.seq.load(Ordering::Relaxed) != 0
    }
}

unsafe impl Sync for JournalEntryPin {}

/// JournalEntryPinList — 简化版
pub(crate) struct JournalEntryPinList {
    pub lock: Mutex<()>,
    pub count: AtomicU32,
    pub unreplayed: bool,
    pub bytes: u32,
}

impl JournalEntryPinList {
    pub fn new(count: u32) -> Self {
        Self {
            lock: Mutex::new(()),
            count: AtomicU32::new(count),
            unreplayed: false,
            bytes: 0,
        }
    }
}

/// PinListFifo
pub(crate) struct PinListFifo {
    pub entries: Vec<Option<JournalEntryPinList>>,
    pub front: u64,
    pub back: u64,
}

impl PinListFifo {
    pub fn new(seq: u64) -> Self {
        Self {
            entries: (0..PIN_FIFO_SIZE).map(|_| None).collect(),
            front: seq,
            back: seq,
        }
    }

    pub fn len(&self) -> usize {
        (self.back - self.front) as usize
    }
    pub fn is_empty(&self) -> bool {
        self.front == self.back
    }
    pub fn is_full(&self) -> bool {
        self.len() >= PIN_FIFO_SIZE
    }

    pub fn push_back(&mut self, pl: JournalEntryPinList) -> Result<(), JournalEntryPinList> {
        if self.is_full() {
            return Err(pl);
        }
        let idx = self.back as usize % PIN_FIFO_SIZE;
        self.entries[idx] = Some(pl);
        self.back += 1;
        Ok(())
    }

    pub fn pop_front(&mut self) -> Option<JournalEntryPinList> {
        if self.is_empty() {
            return None;
        }
        let idx = self.front as usize % PIN_FIFO_SIZE;
        let entry = self.entries[idx].take();
        self.front += 1;
        entry
    }

    pub fn entry_for_seq(&self, seq: u64) -> Option<&JournalEntryPinList> {
        if seq < self.front || seq >= self.back {
            return None;
        }
        self.entries[seq as usize % PIN_FIFO_SIZE].as_ref()
    }

    pub fn entry_for_seq_mut(&mut self, seq: u64) -> Option<&mut JournalEntryPinList> {
        if seq < self.front || seq >= self.back {
            return None;
        }
        self.entries[seq as usize % PIN_FIFO_SIZE].as_mut()
    }
}

// ═══════════════════════════════════════════════════════════════
// Part 8: ReplayedEntry & JournalReplayer
// ═══════════════════════════════════════════════════════════════

#[derive(Debug, Clone)]
pub struct ReplayedEntry {
    pub seq: u64,
    pub btree_type: BtreeId,
    pub entry_type: JsetEntryType,
    pub btree_entries: Vec<BtreeEntry>,
}

/// JournalReplayer
pub struct JournalReplayer<'a> {
    pub journal: &'a Journal,
    pub last_applied_seq: u64,
    replayed_seqs: HashSet<u64>,
    preloaded_jsets: Option<Vec<(u32, Jset)>>,
    /// 重播阶段的 key overlay（最新状态）
    pub overlay: JsetOverlay,
    /// 重播阶段恢复的根记录 (btree_type, level, root_offset)
    pub root_records: Vec<(u8, u8, u64)>,
}

impl<'a> JournalReplayer<'a> {
    pub fn new(journal: &'a Journal) -> Self {
        Self {
            journal,
            last_applied_seq: 0,
            replayed_seqs: HashSet::new(),
            preloaded_jsets: None,
            overlay: JsetOverlay::new(),
            root_records: Vec::new(),
        }
    }

    pub fn from_jsets(journal: &'a Journal, jsets: Vec<(u32, Jset)>) -> Self {
        Self {
            journal,
            last_applied_seq: 0,
            replayed_seqs: HashSet::new(),
            preloaded_jsets: Some(jsets),
            overlay: JsetOverlay::new(),
            root_records: Vec::new(),
        }
    }

    pub fn replayed_seqs(&self) -> Vec<u64> {
        let mut seqs: Vec<u64> = self.replayed_seqs.iter().copied().collect();
        seqs.sort();
        seqs
    }

    pub async fn replay_from(&self, from_seq: u64) -> Result<Vec<ReplayedEntry>, StorageError> {
        let mut grouped: HashMap<(u64, u8), ReplayedEntry> = HashMap::new();

        if let Some(jsets) = self.preloaded_jsets.as_ref() {
            for (_bucket, jset) in jsets {
                let seq = jset.header.seq;
                if seq < from_seq {
                    continue;
                }
                for entry in &jset.entries {
                    let entry_type = JsetEntryType::from_u8(entry.hdr.entry_type);
                    let Some(entry_type) = entry_type else {
                        continue;
                    };
                    let btree_type = entry.hdr.btree_type;
                    let replay = grouped.entry((seq, btree_type)).or_insert_with(|| {
                        ReplayedEntry {
                            seq,
                            btree_type: BtreeId::from_u8(btree_type),
                            entry_type,
                            btree_entries: Vec::new(),
                        }
                    });
                    replay.entry_type = entry_type;
                    if entry_type == JsetEntryType::BtreeKeys {
                        if entry.payload.len() < 21 {
                            continue;
                        }
                        let pos = Bpos {
                            inode: u64::from_le_bytes(
                                entry.payload[0..8].try_into().unwrap_or([0; 8]),
                            ),
                            offset: u64::from_le_bytes(
                                entry.payload[8..16].try_into().unwrap_or([0; 8]),
                            ),
                            snapshot: u32::from_le_bytes(
                                entry.payload[16..20].try_into().unwrap_or([0; 4]),
                            ),
                        };
                        replay.btree_entries.push(BtreeEntry {
                            btree_type,
                            level: entry.hdr.level,
                            entry_type: entry.payload[20],
                            pos,
                            payload: entry.payload[21..].to_vec(),
                        });
                    }
                }
            }
        } else {
            let seq = self.journal.last_seq.load(Ordering::Acquire);
            if seq >= from_seq {
                let (roots, keys) = self.journal.scan_write_buf();
                for (bt, level, _off) in roots {
                    grouped.entry((seq, bt)).or_insert_with(|| ReplayedEntry {
                        seq,
                        btree_type: BtreeId::from_u8(bt),
                        entry_type: JsetEntryType::BtreeRoot,
                        btree_entries: Vec::new(),
                    });
                    if let Some(replay) = grouped.get_mut(&(seq, bt)) {
                        replay.entry_type = JsetEntryType::BtreeRoot;
                        replay.btree_entries.push(BtreeEntry {
                            btree_type: bt,
                            level,
                            entry_type: JsetEntryType::BtreeRoot as u8,
                            pos: Bpos::MIN,
                            payload: Vec::new(),
                        });
                    }
                }
                for (bt, level, _entry_type, pos, payload) in keys {
                    if payload.is_empty() {
                        continue;
                    }
                    let entry_type = payload[0];
                    let replay = grouped.entry((seq, bt)).or_insert_with(|| ReplayedEntry {
                        seq,
                        btree_type: BtreeId::from_u8(bt),
                        entry_type: JsetEntryType::BtreeKeys,
                        btree_entries: Vec::new(),
                    });
                    replay.btree_entries.push(BtreeEntry {
                        btree_type: bt,
                        level,
                        entry_type,
                        pos,
                        payload: payload[1..].to_vec(),
                    });
                }
            }
        }

        let mut entries: Vec<_> = grouped.into_values().collect();
        entries.sort_by_key(|entry| (entry.seq, entry.btree_type.0));
        Ok(entries)
    }

    pub async fn replay_all(&self) -> Result<Vec<ReplayedEntry>, StorageError> {
        self.replay_from(0).await
    }

    /// 两阶段重播到 volume
    ///
    /// Phase 1: 扫描 BtreeRoot 条目重建根记录
    /// Phase 2: 扫描 BtreeKeys 条目构建 overlay、去重、只保留最新状态
    ///
    /// 优先使用 preloaded_jsets（磁盘读取），回退到 scan_write_buf（内存）。
    pub async fn replay_all_to_vol(&mut self, vol: &BchVol) -> Result<u64, StorageError> {
        // 先取出 preloaded_jsets 避免借用冲突
        let preloaded = self.preloaded_jsets.take();
        if let Some(jsets) = preloaded {
            let (roots, keys, max_seq) = Self::parse_jsets(&jsets);
            self.apply_roots_and_keys(&roots, &keys);
            self.last_applied_seq = max_seq;
            self.replayed_seqs.insert(max_seq);
            crate::log_info!(
                "replay_all_to_vol (from disk): {} jsets, {} roots, {} keys, max_seq={}",
                jsets.len(),
                roots.len(),
                keys.len(),
                max_seq
            );
            return Ok(max_seq);
        }

        let (roots, keys) = self.journal.scan_write_buf();
        let keys: Vec<_> = keys
            .into_iter()
            .filter_map(|(bt, level, _entry_type, pos, payload)| {
                payload.first().map(|entry_type| {
                    (bt, level, *entry_type, pos, payload[1..].to_vec())
                })
            })
            .collect();
        self.apply_roots_and_keys(&roots, &keys);
        let max_seq = self.journal.last_seq.load(Ordering::Acquire);
        self.last_applied_seq = max_seq;
        if max_seq != 0 {
            self.replayed_seqs.insert(max_seq);
        }
        Ok(max_seq)
    }

    /// 解析预加载的 Jset 列表为 roots + keys
    fn parse_jsets(
        jsets: &[(u32, Jset)],
    ) -> (Vec<(u8, u8, u64)>, Vec<(u8, u8, u8, Bpos, Vec<u8>)>, u64) {
        let mut roots = Vec::new();
        let mut keys = Vec::new();
        let mut max_seq = 0u64;

        for (_bucket_idx, jset) in jsets {
            if jset.header.seq > max_seq {
                max_seq = jset.header.seq;
            }
            for entry in &jset.entries {
                let payload = &entry.payload;
                if entry.hdr.entry_type == JsetEntryType::BtreeRoot as u8 {
                    if payload.len() >= 10 {
                        let bt = payload[0];
                        let level = payload[1];
                        let off = u64::from_le_bytes(payload[2..10].try_into().unwrap_or([0; 8]));
                        roots.push((bt, level, off));
                    }
                } else if entry.hdr.entry_type == 0 {
                    if payload.len() >= 20 {
                        let inode = u64::from_le_bytes(payload[0..8].try_into().unwrap_or([0; 8]));
                        let offset =
                            u64::from_le_bytes(payload[8..16].try_into().unwrap_or([0; 8]));
                        let snapshot =
                            u32::from_le_bytes(payload[16..20].try_into().unwrap_or([0; 4]));
                        let pos = Bpos {
                            inode,
                            offset,
                            snapshot,
                        };
                        if payload.len() < 21 {
                            continue;
                        }
                        let entry_type = payload[20];
                        let entry_payload = payload[21..].to_vec();
                        keys.push((
                            entry.hdr.btree_type,
                            entry.hdr.level,
                            entry_type,
                            pos,
                            entry_payload,
                        ));
                    }
                }
            }
        }
        (roots, keys, max_seq)
    }

    /// 将解析出的 roots 和 keys 应用到 overlay（Phase 1 + Phase 2 公共逻辑）
    fn apply_roots_and_keys(
        &mut self,
        roots: &[(u8, u8, u64)],
        keys: &[(u8, u8, u8, Bpos, Vec<u8>)],
    ) {
        self.root_records.clear();
        for (bt, level, off) in roots {
            if let Some(pos) = self.root_records.iter().position(|(b, _, _)| *b == *bt) {
                self.root_records[pos] = (*bt, *level, *off);
            } else {
                self.root_records.push((*bt, *level, *off));
            }
        }
        crate::log_info!(
            "apply_roots_and_keys: phase1 roots={}",
            self.root_records.len()
        );

        self.overlay.clear();
        for (bt, _level, entry_type, pos, payload) in keys {
            self.overlay
                .set_entry(*bt, *pos, *entry_type, payload.clone());
        }
        crate::log_info!(
            "apply_roots_and_keys: phase2 overlay_keys={}",
            self.overlay.len()
        );
    }

    /// 重放 accounting（alloc/freespace）条目到 btree。
    ///
    /// Journal replay is deliberately split into accounting and data passes,
    /// matching bcachefs recovery ordering.  The allocator owns the actual
    /// tree mutation; this pass materializes the overlay and reports the
    /// accounting work available to that owner.
    pub async fn replay_accounting_to_vol(&mut self, vol: &BchVol) -> Result<u64, StorageError> {
        if self.overlay.is_empty() {
            self.replay_all_to_vol(vol).await?;
        }
        Ok(self
            .overlay
            .entries
            .keys()
            .filter(|(bt, _)| *bt == 0 || *bt == 1)
            .count() as u64)
    }

    /// 重放 data 条目到 btree。
    ///
    /// The data pass observes the same overlay after accounting has been
    /// selected, and reports data-tree work for the allocator's mutation
    /// phase.
    pub async fn replay_data_to_vol(&mut self, vol: &BchVol) -> Result<u64, StorageError> {
        if self.overlay.is_empty() {
            self.replay_all_to_vol(vol).await?;
        }
        Ok(self
            .overlay
            .entries
            .keys()
            .filter(|(bt, _)| *bt >= 2)
            .count() as u64)
    }

    /// 读取重播阶段恢复的根记录
    ///
    /// 返回 (btree_id, disk_offset, level) 列表。
    /// 对应 bcachefs 启动时从 journal root_records + superblock 确定根位置。
    pub async fn read_btree_roots(&self) -> Result<Vec<(BtreeId, u64, u8)>, StorageError> {
        let roots: Vec<(BtreeId, u64, u8)> = self
            .root_records
            .iter()
            .map(|(bt, level, off)| (BtreeId::from_u8(*bt), *off, *level))
            .collect();
        Ok(roots)
    }
}

/// 重播阶段的内存 jset overlay — 提供一致性查询窗口
///
/// 映射 (btree_type, pos) → 最新版本的 payload。
/// key 重播时只保留每个 key 的最新状态，忽略历史版本。
#[derive(Debug, Default, Clone)]
pub struct JsetOverlay {
    entries: HashMap<(u8, Bpos), JsetOverlayValue>,
}

#[derive(Debug, Clone)]
struct JsetOverlayValue {
    entry_type: u8,
    level: u8,
    payload: Vec<u8>,
}

impl JsetOverlay {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// 插入或更新一个 key（后写入覆盖前写入）
    pub fn set(&mut self, btree_type: u8, pos: Bpos, payload: Vec<u8>) {
        self.set_entry(btree_type, pos, 0, payload);
    }

    pub fn set_entry(&mut self, btree_type: u8, pos: Bpos, entry_type: u8, payload: Vec<u8>) {
        self.entries.insert(
            (btree_type, pos),
            JsetOverlayValue {
                entry_type,
                level: 0,
                payload,
            },
        );
    }

    /// 查询 key（返回最新版本 payload）
    pub fn get(&self, btree_type: u8, pos: &Bpos) -> Option<&Vec<u8>> {
        self.entries
            .get(&(btree_type, *pos))
            .map(|entry| &entry.payload)
    }

    pub fn get_with_type(
        &self,
        btree_type: u8,
        pos: &Bpos,
    ) -> Option<(u8, u8, &Vec<u8>)> {
        self.entries
            .get(&(btree_type, *pos))
            .map(|entry| (entry.entry_type, entry.level, &entry.payload))
    }

    /// 所有条目数
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 是否为空
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 消费所有条目
    pub fn drain(&mut self) -> impl Iterator<Item = ((u8, Bpos), Vec<u8>)> + '_ {
        self.entries.drain().map(|(key, entry)| (key, entry.payload))
    }

    pub fn drain_with_type(
        &mut self,
    ) -> impl Iterator<Item = ((u8, Bpos), (u8, u8, Vec<u8>))> + '_ {
        self.entries.drain().map(|(key, entry)| {
            (
                key,
                (entry.entry_type, entry.level, entry.payload),
            )
        })
    }

    /// 清空
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

// ═══════════════════════════════════════════════════════════════
// Part 9: Journal — 核心结构
// ═══════════════════════════════════════════════════════════════

pub struct Journal {
    // ── 核心字段 ──
    pub last_seq: AtomicU64,
    pub last_seq_ondisk: AtomicU64,
    pub(crate) replay_done: AtomicBool,
    pub(crate) seq_ondisk: AtomicU64,
    pub(crate) flushed_seq_ondisk: AtomicU64,
    pub(crate) dirty_entry_bytes: AtomicU64,
    pub(crate) blocked: AtomicU32,
    pub(crate) cur_entry_error: AtomicI32,
    pub(crate) err_seq: AtomicU64,
    pub(crate) cur_entry_offset_if_blocked: AtomicU32,
    pub(crate) entry_u64s_reserved: AtomicU32,
    pub(crate) cur_entry_u64s: AtomicU32,
    pub(crate) rewind_seq: AtomicU64,
    pub(crate) reclaim_interval_ms: AtomicU64,

    // ── Seq ──
    seq: AtomicU64,

    // ── Reservations ──
    reservations: JournalResState,

    // ── Pin FIFO ──
    pub(crate) pin_fifo: UnsafeCell<PinListFifo>,
    pub(crate) flush_in_progress: AtomicU64,
    pub(crate) flush_in_progress_dropped: AtomicBool,
    pub(crate) pin_flush_wait: Arc<Condvar>,
    pub(crate) pin_flush_lock: Mutex<()>,
    pub(crate) flush_wait: Arc<Condvar>,
    pub(crate) flush_wait_lock: Mutex<()>,
    pub(crate) reclaim_flush_wait: Arc<Condvar>,
    pub(crate) reclaim_flush_wait_lock: Mutex<()>,

    // ── Flags ──
    running: AtomicBool,
    can_discard: AtomicBool,
    pub(crate) reclaim_kicked: AtomicBool,
    pub(crate) reclaim_notify: Notify,
    pub(crate) nr_direct_reclaim: AtomicU64,
    pub(crate) nr_background_reclaim: AtomicU64,

    // ── Bucket 管理 ──
    slowpath: Mutex<JournalSlowpath>,
    slowpath_lock: Mutex<()>,
    pub(crate) reclaim_lock: Mutex<()>,

    // ── Space ──
    space: [JournalSpace; JOURNAL_SPACE_NR],

    // ── Safety net ──
    device: OnceLock<Arc<crate::block_device::BchDev>>,
    pub(crate) vol: OnceLock<Weak<BchVol>>,
    test_device: OnceLock<Arc<crate::block_device::BchDev>>,

    // ── Blacklist ──
    blacklist_table: RwLock<Option<BlacklistTable>>,

    // ── Handles ──
    flush_bg_handle: UnsafeCell<Option<crate::types::BgTaskHandle>>,
    reclaim_bg_handle: UnsafeCell<Option<crate::types::BgTaskHandle>>,

    // ── Notification ──
    flush_notify: Notify,
    seq_flush_notify: Notify,
    needs_flush_write: AtomicBool,
    flushes_outstanding: AtomicU32,

    // ── Write work ──
    buf_lock: Mutex<()>,
    lock: Mutex<()>,
    write_work_running: AtomicBool,
    journal_flush_delay_ms: AtomicU64,
    may_skip_flush: AtomicBool,

    // ── Journal write buf（reservation 写入缓冲）─
    write_buf: Mutex<JournalWriteBuf>,
}

impl std::fmt::Debug for Journal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Journal")
            .field("seq", &self.bch2_journal_cur_seq())
            .field(
                "last_seq_ondisk",
                &self.last_seq_ondisk.load(Ordering::Acquire),
            )
            .finish()
    }
}

unsafe impl Sync for Journal {}

// ═══════════════════════════════════════════════════════════════
// JournalWriteBuf — reservation 写入缓冲
// ═══════════════════════════════════════════════════════════════

/// Journal write buf — 存储当前 journal entry 的序列化数据
///
/// 对应 bcachefs `struct journal_buf` (journal/types.h:37) 的简化版本。
/// 事务通过 reservation 直接写入此 buf，buf 装满后触发 flush。
struct JournalWriteBuf {
    /// 序列化数据（JsetHeader + JsetEntryHeader/payload 列表）
    data: Vec<u8>,
    /// 当前写入偏移（字节）
    offset: usize,
    /// 当前 seq
    seq: u64,
    /// 活跃 reservation 计数
    ref_count: u32,
    /// 条目计数
    entry_count: u32,
}

impl JournalWriteBuf {
    fn new(seq: u64) -> Self {
        let header_size = std::mem::size_of::<JsetHeader>();
        let data = vec![0u8; header_size];
        Self {
            data,
            offset: header_size,
            seq,
            ref_count: 0,
            entry_count: 0,
        }
    }

    fn remaining(&self) -> usize {
        BUF_SIZE.saturating_sub(self.offset)
    }

    fn write_entry_at(
        &mut self,
        offset: usize,
        hdr: &JsetEntryHeader,
        payload: &[u8],
    ) -> Result<(), JournalError> {
        let hdr_size = std::mem::size_of::<JsetEntryHeader>();
        let total = hdr_size + payload.len();
        if offset < std::mem::size_of::<JsetHeader>() || offset + total > BUF_SIZE {
            crate::log_verbose!(
                "journal write buf full: total={} offset={}",
                total,
                offset
            );
            return Err(JournalError::Overflow("write buf full".into()));
        }
        // 确保 data 足够容纳新条目
        let end = offset + total;
        if self.data.len() < end {
            self.data.resize(end, 0);
        }
        let hdr_ptr = hdr as *const JsetEntryHeader as *const u8;
        unsafe {
            std::ptr::copy_nonoverlapping(
                hdr_ptr,
                self.data.as_mut_ptr().add(offset),
                hdr_size,
            );
        }
        if !payload.is_empty() {
            self.data[offset + hdr_size..end].copy_from_slice(payload);
        }
        self.entry_count += 1;
        Ok(())
    }

    fn finalize(&mut self, header: &mut JsetHeader) {
        header.entry_count = self.entry_count;
        header.seq = self.seq;
        header.last_seq = self.seq;
        let block_size = JSET_BLOCK_SIZE as usize;
        let pad = (block_size - (self.offset % block_size)) % block_size;
        self.data.resize(self.offset + pad, 0);
        let hdr_size = std::mem::size_of::<JsetHeader>();
        unsafe {
            std::ptr::copy_nonoverlapping(
                header as *const JsetHeader as *const u8,
                self.data.as_mut_ptr(),
                hdr_size,
            );
        }
        if (header.flags & JSET_CSUM_TYPE_MASK) == CSUM_TYPE_CRC32C as u32 {
            let checksum = crc32c(&self.data, 0);
            header.crc32 = checksum;
            unsafe {
                std::ptr::copy_nonoverlapping(
                    header as *const JsetHeader as *const u8,
                    self.data.as_mut_ptr(),
                    hdr_size,
                );
            }
        }
    }

    fn reset(&mut self, seq: u64) {
        let header_size = std::mem::size_of::<JsetHeader>();
        self.data.clear();
        self.data.resize(header_size, 0);
        self.offset = header_size;
        self.seq = seq;
        self.ref_count = 0;
        self.entry_count = 0;
    }
}

/// 简化版 slowpath
struct JournalSlowpath {
    buckets: Vec<JournalDevice>,
    current_bucket: usize,
    discard_idx: usize,
    dirty_idx: usize,
    dirty_idx_ondisk: usize,
}

/// 从 bucket addresses 推断 bucket byte size
fn journal_infer_bucket_size(addrs: &[u64]) -> usize {
    if addrs.len() >= 2 {
        (addrs[1] - addrs[0]) as usize
    } else {
        32768
    }
}

#[derive(Debug, Clone)]
struct JournalDevice {
    addr: u64,
}

impl JournalSlowpath {
    fn new(bucket_addrs: Vec<u64>) -> Self {
        Self {
            buckets: bucket_addrs
                .into_iter()
                .map(|addr| JournalDevice { addr })
                .collect(),
            current_bucket: 0,
            discard_idx: 0,
            dirty_idx: 0,
            dirty_idx_ondisk: 0,
        }
    }
}

impl Journal {
    // ── Constructors ──

    pub fn new(bucket_addrs: Vec<u64>) -> Self {
        let no_buckets = bucket_addrs.is_empty();
        let journal = Self {
            reservations: JournalResState::new(),
            seq: AtomicU64::new(0),
            last_seq: AtomicU64::new(1),
            last_seq_ondisk: AtomicU64::new(1),
            pin_fifo: UnsafeCell::new(PinListFifo::new(1)),
            flush_in_progress: AtomicU64::new(0),
            flush_in_progress_dropped: AtomicBool::new(false),
            pin_flush_wait: Arc::new(Condvar::new()),
            pin_flush_lock: Mutex::new(()),
            flush_wait: Arc::new(Condvar::new()),
            flush_wait_lock: Mutex::new(()),
            reclaim_flush_wait: Arc::new(Condvar::new()),
            reclaim_flush_wait_lock: Mutex::new(()),
            running: AtomicBool::new(true),
            replay_done: AtomicBool::new(false),
            seq_ondisk: AtomicU64::new(0),
            flushed_seq_ondisk: AtomicU64::new(0),
            dirty_entry_bytes: AtomicU64::new(0),
            blocked: AtomicU32::new(0),
            cur_entry_error: AtomicI32::new(0),
            err_seq: AtomicU64::new(0),
            cur_entry_offset_if_blocked: AtomicU32::new(JOURNAL_ENTRY_CLOSED_VAL as u32),
            entry_u64s_reserved: AtomicU32::new(0),
            cur_entry_u64s: AtomicU32::new(0),
            rewind_seq: AtomicU64::new(0),
            reclaim_interval_ms: AtomicU64::new(0),
            can_discard: AtomicBool::new(false),
            reclaim_kicked: AtomicBool::new(false),
            reclaim_notify: Notify::new(),
            nr_direct_reclaim: AtomicU64::new(0),
            nr_background_reclaim: AtomicU64::new(0),
            slowpath: Mutex::new(JournalSlowpath::new(bucket_addrs)),
            slowpath_lock: Mutex::new(()),
            reclaim_lock: Mutex::new(()),
            space: [JournalSpace::new(); JOURNAL_SPACE_NR],
            device: OnceLock::new(),
            vol: OnceLock::new(),
            test_device: OnceLock::new(),
            blacklist_table: RwLock::new(None),
            flush_bg_handle: UnsafeCell::new(None),
            reclaim_bg_handle: UnsafeCell::new(None),
            flush_notify: Notify::new(),
            seq_flush_notify: Notify::new(),
            needs_flush_write: AtomicBool::new(false),
            flushes_outstanding: AtomicU32::new(0),
            buf_lock: Mutex::new(()),
            lock: Mutex::new(()),
            write_work_running: AtomicBool::new(false),
            journal_flush_delay_ms: AtomicU64::new(0),
            may_skip_flush: AtomicBool::new(true),
            write_buf: Mutex::new(JournalWriteBuf::new(0)),
        };
        if no_buckets {
            let seq = journal.bch2_journal_cur_seq();
            journal.seq_ondisk.store(seq, Ordering::Release);
            journal.flushed_seq_ondisk.store(seq, Ordering::Release);
        }
        journal
    }

    pub fn from_superblock(state: &JournalSuperblockState) -> Self {
        let cur_seq = state.last_seq.max(1);
        let last_seq = if state.last_seq == 0 {
            cur_seq
        } else {
            state.last_seq
        };
        let last_seq_ondisk = if state.last_seq == 0 {
            last_seq
        } else {
            state.last_seq_ondisk
        };
        let journal = Self {
            reservations: JournalResState::new(),
            seq: AtomicU64::new(cur_seq - 1),
            last_seq: AtomicU64::new(last_seq),
            last_seq_ondisk: AtomicU64::new(last_seq_ondisk),
            pin_fifo: UnsafeCell::new(PinListFifo::new(last_seq_ondisk.min(last_seq))),
            flush_in_progress: AtomicU64::new(0),
            flush_in_progress_dropped: AtomicBool::new(false),
            pin_flush_wait: Arc::new(Condvar::new()),
            pin_flush_lock: Mutex::new(()),
            flush_wait: Arc::new(Condvar::new()),
            flush_wait_lock: Mutex::new(()),
            reclaim_flush_wait: Arc::new(Condvar::new()),
            reclaim_flush_wait_lock: Mutex::new(()),
            running: AtomicBool::new(true),
            replay_done: AtomicBool::new(false),
            seq_ondisk: AtomicU64::new(cur_seq - 1),
            flushed_seq_ondisk: AtomicU64::new(cur_seq - 1),
            dirty_entry_bytes: AtomicU64::new(0),
            blocked: AtomicU32::new(0),
            cur_entry_error: AtomicI32::new(0),
            err_seq: AtomicU64::new(0),
            cur_entry_offset_if_blocked: AtomicU32::new(JOURNAL_ENTRY_CLOSED_VAL as u32),
            entry_u64s_reserved: AtomicU32::new(0),
            cur_entry_u64s: AtomicU32::new(0),
            rewind_seq: AtomicU64::new(last_seq_ondisk),
            reclaim_interval_ms: AtomicU64::new(0),
            can_discard: AtomicBool::new(false),
            reclaim_kicked: AtomicBool::new(false),
            reclaim_notify: Notify::new(),
            nr_direct_reclaim: AtomicU64::new(0),
            nr_background_reclaim: AtomicU64::new(0),
            slowpath: Mutex::new(JournalSlowpath::new(state.bucket_addrs.clone())),
            slowpath_lock: Mutex::new(()),
            reclaim_lock: Mutex::new(()),
            space: [JournalSpace::new(); JOURNAL_SPACE_NR],
            device: OnceLock::new(),
            vol: OnceLock::new(),
            test_device: OnceLock::new(),
            blacklist_table: RwLock::new(None),
            flush_bg_handle: UnsafeCell::new(None),
            reclaim_bg_handle: UnsafeCell::new(None),
            flush_notify: Notify::new(),
            seq_flush_notify: Notify::new(),
            needs_flush_write: AtomicBool::new(false),
            flushes_outstanding: AtomicU32::new(0),
            buf_lock: Mutex::new(()),
            lock: Mutex::new(()),
            write_work_running: AtomicBool::new(false),
            journal_flush_delay_ms: AtomicU64::new(0),
            may_skip_flush: AtomicBool::new(true),
            write_buf: Mutex::new(JournalWriteBuf::new(cur_seq)),
        };
        journal
    }

    /// 导出 journal 状态到 superblock
    pub fn to_superblock_state(&self) -> JournalSuperblockState {
        let sp = self.slowpath.lock().unwrap();
        JournalSuperblockState {
            bucket_addrs: sp.buckets.iter().map(|bs| bs.addr).collect(),
            last_seq: self.bch2_journal_cur_seq(),
            last_seq_ondisk: self.last_seq_ondisk.load(Ordering::Acquire),
            last_bucket: sp.current_bucket as u32,
            discard_idx: sp.discard_idx as u32,
            dirty_idx: sp.dirty_idx as u32,
            dirty_idx_ondisk: sp.dirty_idx_ondisk as u32,
            bucket_seq: Vec::new(),
            replayed_seqs: Vec::new(),
        }
    }

    /// 设置 BchVol 引用
    pub fn set_vol_ref(&self, vol: &Arc<BchVol>) {
        self.vol.set(Arc::downgrade(vol)).ok();
    }

    pub(crate) fn set_device_ref(&self, dev: Arc<crate::block_device::BchDev>) {
        self.device.set(dev).ok();
    }

    /// Install journal buckets selected by the allocator.  This is the
    /// userspace equivalent of bcachefs committing the new bucket array under
    /// the journal lock; existing buckets are never reduced or replaced.
    pub fn set_bucket_addrs(&self, bucket_addrs: Vec<u64>) -> Result<(), StorageError> {
        if bucket_addrs.is_empty() {
            return Err(StorageError::NoMem);
        }
        let mut sp = self.slowpath.lock().unwrap();
        if !sp.buckets.is_empty() {
            return Ok(());
        }
        sp.buckets = bucket_addrs
            .into_iter()
            .map(|addr| JournalDevice { addr })
            .collect();
        sp.current_bucket = 0;
        sp.discard_idx = 0;
        sp.dirty_idx = 0;
        sp.dirty_idx_ondisk = 0;
        Ok(())
    }

    /// 设置测试设备
    pub fn set_test_device(&self, dev: Arc<crate::block_device::BchDev>) {
        self.test_device.set(dev.clone()).ok();
        self.set_device_ref(dev);
    }

    // ── Seq ──

    /// 获取当前序列号
    pub fn bch2_journal_cur_seq(&self) -> u64 {
        self.seq.load(Ordering::Acquire)
    }

    /// 设置回放完成
    pub fn bch2_journal_set_replay_done(&self) {
        self.replay_done.store(true, Ordering::Release);
    }

    // ── Flush ──

    /// 刷新 journal — 将 write_buf 写入设备 journal bucket
    ///
    /// 流程:
    /// 1. 从 write_buf 取出已 finalize 的 Jset 数据
    /// 2. 按 bucket size 分块写入连续 bucket
    /// 3. 更新 current_bucket 指针 + seq_ondisk
    pub async fn bch2_journal_flush(&self) -> Result<(), JournalError> {
        let dev = self.device.get().ok_or_else(|| {
            JournalError::Io(StorageError::Internal("journal device not set".into()))
        })?;

        // 取出已 finalize 的数据
        let data = {
            let mut buf = self.write_buf.lock().unwrap();
            if buf.entry_count == 0 || buf.offset <= std::mem::size_of::<JsetHeader>() {
                return Ok(());
            }
            let data = buf.data[..buf.offset].to_vec();
            // A flushed Jset is complete; the next reservation starts a new
            // monotonically increasing journal sequence.
            buf.reset(0);
            data
        };

        if data.is_empty() {
            return Ok(());
        }

        // 获取 bucket 布局信息
        let (bucket_addrs, current_bucket) = {
            let sp = self.slowpath.lock().unwrap();
            let addrs: Vec<u64> = sp.buckets.iter().map(|b| b.addr).collect();
            if addrs.is_empty() {
                return Err(JournalError::Io(StorageError::Internal(
                    "no journal buckets".into(),
                )));
            }
            (addrs, sp.current_bucket)
        };

        let bucket_size = journal_infer_bucket_size(&bucket_addrs);
        let total_buckets = bucket_addrs.len();
        let needed = (data.len() + bucket_size - 1) / bucket_size;

        // 写入连续 bucket（循环）
        for i in 0..needed {
            let idx = (current_bucket + i) % total_buckets;
            let addr = bucket_addrs[idx];
            let start = i * bucket_size;
            let end = std::cmp::min(start + bucket_size, data.len());
            let chunk = &data[start..end];

            let mut bucket_buf = vec![0u8; bucket_size];
            bucket_buf[..chunk.len()].copy_from_slice(chunk);
            dev.write_at(addr, &bucket_buf)
                .await
                .map_err(JournalError::Io)?;
        }
        dev.flush().await.map_err(JournalError::Io)?;

        // 更新 bucket 指针
        {
            let mut sp = self.slowpath.lock().unwrap();
            sp.current_bucket = (current_bucket + needed) % total_buckets;
            sp.dirty_idx = sp.current_bucket;
        }

        // 更新 seq 追踪
        let seq = self.bch2_journal_cur_seq();
        self.seq_ondisk.store(seq, Ordering::Release);
        self.flushed_seq_ondisk.store(seq, Ordering::Release);
        self.last_seq_ondisk.store(seq, Ordering::Release);
        self.bch2_journal_maybe_update_last_seq();

        Ok(())
    }

    /// 刷新并重置 write_buf（别名）
    pub async fn bch2_journal_flush_and_reset(&self) -> Result<(), JournalError> {
        self.bch2_journal_flush().await
    }

    /// 丢弃已完成重放的 journal buckets。
    ///
    /// 当前格式没有单独持久化 replay 起始序列，因此恢复成功后必须清除
    /// 已应用的 jset，避免下一次挂载重复重放旧事务。
    pub async fn bch2_journal_discard_replayed(&self) -> Result<(), JournalError> {
        let dev = self.device.get().ok_or_else(|| {
            JournalError::Io(StorageError::Internal("journal device not set".into()))
        })?;
        let bucket_addrs = {
            let sp = self.slowpath.lock().unwrap();
            sp.buckets.iter().map(|bucket| bucket.addr).collect::<Vec<_>>()
        };
        if bucket_addrs.is_empty() {
            return Ok(());
        }
        let bucket_size = journal_infer_bucket_size(&bucket_addrs);
        let zeroes = vec![0u8; bucket_size];
        for addr in &bucket_addrs {
            dev.write_at(*addr, &zeroes)
                .await
                .map_err(JournalError::Io)?;
        }
        dev.flush().await.map_err(JournalError::Io)?;
        let mut sp = self.slowpath.lock().unwrap();
        sp.current_bucket = 0;
        sp.discard_idx = 0;
        sp.dirty_idx = 0;
        sp.dirty_idx_ondisk = 0;
        self.last_seq_ondisk
            .store(self.bch2_journal_cur_seq(), Ordering::Release);
        Ok(())
    }

    // ── Block / Unblock ──

    /// 阻止新的 reservation
    pub fn bch2_journal_block(&self) -> JournalBlockGuard<'_> {
        self.blocked.fetch_add(1, Ordering::AcqRel);
        JournalBlockGuard { journal: self }
    }

    /// 恢复 reservation
    pub fn bch2_journal_unblock(&self) {
        let prev = self.blocked.fetch_sub(1, Ordering::AcqRel);
        let _ = prev;
    }

    // ── 错误处理 ──

    pub fn journal_error_check(&self) -> Option<JournalError> {
        journal_error_from_code(self.cur_entry_error.load(Ordering::Acquire))
    }

    // ── Pin API ──

    pub fn bch2_journal_pin_add(
        &self,
        seq: u64,
        pin: &JournalEntryPin,
        flush_fn: Option<JournalPinFlushFn>,
    ) {
        if !pin.is_active() || pin.seq.load(Ordering::Acquire) > seq {
            self.bch2_journal_pin_set(seq, pin, flush_fn);
        }
    }

    pub fn bch2_journal_pin_drop(&self, pin: &JournalEntryPin) {
        {
            let _guard = self.pin_flush_lock.lock().unwrap();
            let seq = pin.seq.load(Ordering::Acquire);
            if seq != 0 {
                let fifo = unsafe { &mut *self.pin_fifo.get() };
                if let Some(list) = fifo.entry_for_seq_mut(seq) {
                    list.count.fetch_sub(1, Ordering::AcqRel);
                }
            }
            pin.seq.store(0, Ordering::Release);
        }
        self.bch2_journal_maybe_update_last_seq();
    }

    pub fn bch2_journal_pin_flush(&self, pin: &JournalEntryPin) {
        let flush = unsafe { (*pin.flush.get()).take() };
        if let Some(flush) = flush {
            let _ = flush(self, pin, pin.seq.load(Ordering::Acquire));
        }
    }

    pub fn bch2_journal_maybe_update_last_seq(&self) {
        // bcachefs advances last_seq only while the corresponding pin list is
        // empty and the entry has reached the ondisk sequence.  Keep the
        // FIFO lock held while walking the list so a concurrent pin update
        // cannot race the decision.
        let _guard = self.pin_flush_lock.lock().unwrap();
        let mut last = self.last_seq.load(Ordering::Acquire);
        let seq_ondisk = self.seq_ondisk.load(Ordering::Acquire);
        let fifo = unsafe { &*self.pin_fifo.get() };
        while last < fifo.back && last <= seq_ondisk {
            let Some(list) = fifo.entry_for_seq(last) else {
                break;
            };
            if list.count.load(Ordering::Acquire) != 0 {
                break;
            }
            last = last.saturating_add(1);
        }
        if last != self.last_seq.load(Ordering::Acquire) {
            self.last_seq.store(last, Ordering::Release);
            self.reclaim_flush_wait.notify_all();
            self.pin_flush_wait.notify_all();
        }
    }

    pub fn bch2_journal_pin_set(
        &self,
        new_seq: u64,
        pin: &JournalEntryPin,
        flush_fn: Option<JournalPinFlushFn>,
    ) {
        {
            let _guard = self.pin_flush_lock.lock().unwrap();
            let old_seq = pin.seq.load(Ordering::Acquire);
            let fifo = unsafe { &mut *self.pin_fifo.get() };
            while fifo.back <= new_seq {
                if fifo.push_back(JournalEntryPinList::new(0)).is_err() {
                    return;
                }
            }
            if old_seq != 0 {
                if let Some(old_list) = fifo.entry_for_seq_mut(old_seq) {
                    let old_count = old_list.count.load(Ordering::Acquire);
                    if old_count != 0 {
                        old_list.count.fetch_sub(1, Ordering::AcqRel);
                    }
                }
            }
            if let Some(new_list) = fifo.entry_for_seq_mut(new_seq) {
                new_list.count.fetch_add(1, Ordering::AcqRel);
            }
            pin.seq.store(new_seq, Ordering::Release);
            if let Some(fn_box) = flush_fn {
                unsafe { *pin.flush.get() = Some(fn_box) };
            }
        }
        self.bch2_journal_maybe_update_last_seq();
    }

    pub fn bch2_journal_pin_update(
        &self,
        seq: u64,
        pin: &JournalEntryPin,
        flush_fn: Option<JournalPinFlushFn>,
    ) {
        if !pin.is_active() || pin.seq.load(Ordering::Relaxed) < seq {
            self.bch2_journal_pin_set(seq, pin, flush_fn);
        }
    }

    pub fn bch2_journal_pin_copy(
        &self,
        dst: &JournalEntryPin,
        src: &JournalEntryPin,
        flush_fn: Option<JournalPinFlushFn>,
    ) {
        let src_seq = src.seq.load(Ordering::Acquire);
        if src_seq != 0 {
            self.bch2_journal_pin_set(src_seq, dst, flush_fn);
        }
    }

    pub fn bch2_journal_pin_put(&self, seq: u64) {
        {
            let _guard = self.pin_flush_lock.lock().unwrap();
            let fifo = unsafe { &mut *self.pin_fifo.get() };
            while fifo.front <= seq {
                let removable = fifo
                    .entry_for_seq(fifo.front)
                    .map_or(false, |list| list.count.load(Ordering::Acquire) == 0);
                if !removable {
                    break;
                }
                let _ = fifo.pop_front();
            }
        }
        self.bch2_journal_maybe_update_last_seq();
    }

    #[allow(non_snake_case)]
    pub fn __bch2_journal_pin_put(&self, seq: u64) -> bool {
        self.bch2_journal_pin_put(seq);
        self.maybe_seq_pin(seq).is_none()
    }

    pub fn journal_flush_pins(
        &self,
        seq_to_flush: u64,
        _allowed_below_seq: u32,
        _allowed_above_seq: u32,
    ) -> Result<u32, StorageError> {
        let mut flushed = 0;
        loop {
            let seq = {
                let fifo = unsafe { &*self.pin_fifo.get() };
                if fifo.front > seq_to_flush {
                    break;
                }
                fifo.front
            };
            let pins = self.maybe_seq_pin(seq);
            if pins.map_or(true, |list| list.count.load(Ordering::Acquire) == 0) {
                self.bch2_journal_pin_put(seq);
                flushed += 1;
            } else {
                break;
            }
        }
        Ok(flushed)
    }

    pub fn bch2_journal_flush_pins(&self, seq: u64) -> Result<u32, StorageError> {
        self.journal_flush_pins(seq, 0, 0)
    }

    pub fn journal_reclaim_kick(&self) {
        self.reclaim_kicked.store(true, Ordering::Release);
        self.reclaim_notify.notify_waiters();
    }

    // ── Reservation ──

    /// 获取 journal reservation
    ///
    /// 对应 bcachefs `bch2_journal_res_get()` (journal.h:521)
    /// 在 write_buf 中分配空间，多个生产者通过原子操作并发分配。
    pub fn bch2_journal_res_get(
        &self,
        _watermark: Watermark,
        req_u64s: u32,
    ) -> Result<JournalRes, JournalError> {
        if let Some(err) = self.journal_error_check() {
            return Err(err);
        }
        if req_u64s as usize > BUF_SIZE.saturating_sub(std::mem::size_of::<JsetHeader>()) {
            return Err(JournalError::Overflow(format!(
                "journal reservation too large: {} bytes",
                req_u64s
            )));
        }
        let mut buf = self.write_buf.lock().unwrap();
        if buf.seq == 0 || buf.remaining() < req_u64s as usize {
            if buf.ref_count != 0 {
                return Err(JournalError::Blocked(
                    "journal buffer has active reservations".into(),
                ));
            }
            let seq = self.seq.fetch_add(1, Ordering::AcqRel) + 1;
            crate::log_verbose!(
                "journal_res_get: new buf seq={} req_bytes={}",
                seq,
                req_u64s
            );
            buf.reset(seq);
        } else {
            crate::log_verbose!(
                "journal_res_get: append to buf seq={} req_bytes={} offset={}",
                buf.seq,
                req_u64s,
                buf.offset
            );
        }
        let offset = buf.offset as u32;
        buf.offset = buf.offset.saturating_add(req_u64s as usize);
        buf.ref_count += 1;
        Ok(JournalRes {
            seq: buf.seq,
            offset,
            start_offset: offset,
            end_offset: offset.saturating_add(req_u64s),
            u64s: req_u64s,
            buf_idx: (buf.seq & 3) as u32,
            must_flush: false,
        })
    }

    /// 释放 journal reservation
    ///
    /// 对应 bcachefs `bch2_journal_res_put()` (journal.h)
    /// 递减引用计数；当所有 reservation 都释放时，可触发 flush。
    pub fn bch2_journal_res_put(&self, res: &JournalRes) {
        let mut buf = self.write_buf.lock().unwrap();
        if buf.seq != res.seq {
            return;
        }
        crate::log_verbose!("journal_res_put: seq={} offset={}", res.seq, res.offset);
        // bcachefs closes a reservation by consuming any unused tail with
        // empty entries. Keep independently reserved ranges parseable when a
        // caller reserved more space than it ultimately used.
        let mut tail = res.offset;
        let empty_hdr = JsetEntryHeader {
            btree_type: 0,
            entry_type: JsetEntryType::BtreeKeys as u8,
            version: JSET_ENTRY_VERSION,
            level: 0,
            payload_len: 0,
            has_last: 0,
            has_prev: 0,
        };
        let hdr_size = std::mem::size_of::<JsetEntryHeader>() as u32;
        while tail.saturating_add(hdr_size) <= res.end_offset {
            if buf
                .write_entry_at(tail as usize, &empty_hdr, &[])
                .is_err()
            {
                break;
            }
            tail = tail.saturating_add(hdr_size);
        }
        if buf.ref_count > 0 {
            buf.ref_count -= 1;
        }
        if buf.ref_count == 0 && buf.entry_count > 0 {
            let mut hdr = JsetHeader {
                magic: JOURNAL_MAGIC,
                seq: buf.seq,
                last_seq: self.bch2_journal_cur_seq(),
                crc32: 0,
                entry_count: buf.entry_count,
                version: JSET_VERSION,
                flags: CSUM_TYPE_CRC32C as u32,
                pad: [0u8; 24],
            };
            buf.finalize(&mut hdr);
        }
    }

    /// 向 reservation 写入一个 jset entry
    ///
    /// 对应 bcachefs `bch2_journal_add_entry()` (journal.h:339)
    /// 在 res->offset 处写入 JsetEntryHeader + payload，并更新 res。
    pub fn bch2_journal_add_entry(
        &self,
        res: &mut JournalRes,
        entry_type: u8,
        btree_type: u8,
        level: u8,
        payload: &[u8],
    ) -> Result<(), JournalError> {
        if let Some(err) = self.journal_error_check() {
            return Err(err);
        }
        if payload.len() > u16::MAX as usize {
            return Err(JournalError::Overflow(format!(
                "journal entry payload too large: {} bytes",
                payload.len()
            )));
        }
        let mut buf = self.write_buf.lock().unwrap();
        if buf.seq != res.seq {
            return Err(JournalError::Overflow("seq mismatch".into()));
        }
        crate::log_verbose!(
            "journal_add_entry: seq={} bt={} type={} level={} len={}",
            res.seq,
            btree_type,
            entry_type,
            level,
            payload.len()
        );
        let hdr = JsetEntryHeader {
            btree_type,
            entry_type,
            version: JSET_ENTRY_VERSION,
            level,
            payload_len: payload.len() as u16,
            has_last: 0,
            has_prev: 0,
        };
        let hdr_size = std::mem::size_of::<JsetEntryHeader>() as u32;
        let total = hdr_size + payload.len() as u32;
        if total > res.u64s {
            return Err(JournalError::Overflow(format!(
                "journal reservation exhausted: need={} remaining={}",
                total, res.u64s
            )));
        }
        buf.write_entry_at(res.offset as usize, &hdr, payload)?;
        res.offset += total;
        res.u64s = res.u64s.saturating_sub(total);
        Ok(())
    }

    /// Append btree keys 到 journal（简化批量接口）
    ///
    /// 内部使用 reservation 机制：
    /// 1. 计算总空间 → bch2_journal_res_get
    /// 2. 逐个写入 entry → bch2_journal_add_entry
    /// 3. 释放 → bch2_journal_res_put
    pub async fn append(
        &self,
        _btree: BtreeId,
        entries: &[BtreeEntry],
        flush: bool,
    ) -> Result<(), JournalError> {
        if entries.is_empty() {
            return Ok(());
        }
        let total_bytes = entries.iter().try_fold(0u32, |total, e| {
            let payload = u32::try_from(e.payload.len())
                .map_err(|_| JournalError::Overflow("journal payload length overflow".into()))?;
            total
                .checked_add(std::mem::size_of::<JsetEntryHeader>() as u32)
                .and_then(|v| v.checked_add(21))
                .and_then(|v| v.checked_add(payload))
                .ok_or_else(|| JournalError::Overflow("journal batch length overflow".into()))
        })?;
        if total_bytes == 0 {
            return Ok(());
        }
        let mut res = self.bch2_journal_res_get(Watermark::Low, total_bytes)?;
        let result = (|| {
            for entry in entries {
                let mut payload = Vec::with_capacity(21 + entry.payload.len());
                payload.extend_from_slice(&entry.pos.inode.to_le_bytes());
                payload.extend_from_slice(&entry.pos.offset.to_le_bytes());
                payload.extend_from_slice(&entry.pos.snapshot.to_le_bytes());
                payload.push(entry.entry_type);
                payload.extend_from_slice(&entry.payload);
                self.bch2_journal_add_entry(
                    &mut res,
                    JsetEntryType::BtreeKeys as u8,
                    entry.btree_type,
                    entry.level,
                    &payload,
                )?;
            }
            Ok::<(), JournalError>(())
        })();
        self.bch2_journal_res_put(&res);
        result?;
        if flush {
            self.bch2_journal_flush().await?;
        }
        Ok(())
    }

    /// Append btree root change to journal（记录 root 位置而非数据本身）
    pub async fn append_btree_root(
        &self,
        btree_type: u8,
        level: u8,
        root_offset: u64,
    ) -> Result<(), JournalError> {
        let mut payload = Vec::with_capacity(17);
        payload.push(btree_type);
        payload.push(level);
        payload.extend_from_slice(&root_offset.to_le_bytes());
        let hdr_size = std::mem::size_of::<JsetEntryHeader>() as u32;
        let total_bytes = hdr_size + payload.len() as u32;
        let mut res = self.bch2_journal_res_get(Watermark::Low, total_bytes)?;
        let result = self.bch2_journal_add_entry(
            &mut res,
            JsetEntryType::BtreeRoot as u8,
            btree_type,
            level,
            &payload,
        );
        self.bch2_journal_res_put(&res);
        result?;
        Ok(())
    }

    // ── Read ──

    /// 从设备 journal buckets 读取所有 Jset 条目
    ///
    /// 逐 bucket 扫描，在 JSET_BLOCK_SIZE 对齐偏移处查找 Jset 魔数并反序列化。
    /// 按 bucket 顺序返回 Vec<(bucket_idx, Jset)>。
    pub async fn bch2_journal_read(
        &self,
        info: &mut JournalStartInfo,
    ) -> Result<Vec<(u32, Jset)>, JournalError> {
        let dev = self.device.get().ok_or_else(|| {
            JournalError::Io(StorageError::Internal("journal device not set".into()))
        })?;

        let bucket_addrs = {
            let sp = self.slowpath.lock().unwrap();
            let addrs: Vec<u64> = sp.buckets.iter().map(|b| b.addr).collect();
            if addrs.is_empty() {
                return Ok(Vec::new());
            }
            addrs
        };

        let bucket_size = journal_infer_bucket_size(&bucket_addrs);
        let block_size = JSET_BLOCK_SIZE as usize;
        let mut result = Vec::new();

        for (bucket_idx, addr) in bucket_addrs.iter().enumerate() {
            let data = dev
                .read_at(*addr, bucket_size)
                .await
                .map_err(JournalError::Io)?;

            // JSET_BLOCK_SIZE 对齐偏移扫描
            let mut off = 0usize;
            while off + std::mem::size_of::<JsetHeader>() <= data.len() {
                let magic = &data[off..off + 8];
                if magic == JOURNAL_MAGIC || magic == VMNT_JSET_MAGIC {
                    match Jset::deserialize(&data[off..]) {
                        Ok(Some(jset)) => {
                            let padded_len = jset.serialized_padded_len();
                            result.push((bucket_idx as u32, jset));
                            off += padded_len;
                            continue;
                        }
                        Ok(None) => {}
                        Err(StorageError::Invalid(message))
                            if message.contains("checksum") =>
                        {
                            return Err(JournalError::ChecksumMismatch);
                        }
                        Err(_) => {}
                    }
                }
                off += block_size;
            }
        }

        // Physical bucket order is circular and is not journal order after
        // wraparound.  bcachefs orders the collected bucket entries by
        // sequence before recovery; replay must observe the same ordering.
        result.sort_by_key(|(bucket, jset)| (jset.header.seq, *bucket));

        // 填充 info
        info.cur_seq = result
            .last()
            .map(|(_, j)| j.header.seq.saturating_add(1))
            .unwrap_or(1);
        info.last_seq = result.last().map(|(_, j)| j.header.last_seq).unwrap_or(0);
        info.replay_end = info.cur_seq;
        info.clean = result.is_empty();

        Ok(result)
    }

    /// 扫描当前 write_buf 提取所有 pending root/key entry
    ///
    /// BtreeKeys 载荷格式: [Bpos:20字节] + [entry_payload]
    /// BtreeRoot 载荷格式: [btree_type:1] + [level:1] + [root_offset:8]
    pub fn scan_write_buf(&self) -> (Vec<(u8, u8, u64)>, Vec<(u8, u8, u8, Bpos, Vec<u8>)>) {
        let buf = self.write_buf.lock().unwrap();
        let hdr_size = std::mem::size_of::<JsetHeader>();
        let mut off = hdr_size;
        let mut roots = Vec::new();
        let mut keys = Vec::new();

        while off + std::mem::size_of::<JsetEntryHeader>() <= buf.data.len() && off < buf.offset {
            let hdr =
                unsafe { std::ptr::read(buf.data.as_ptr().add(off) as *const JsetEntryHeader) };
            off += std::mem::size_of::<JsetEntryHeader>();
            let payload_end = off + hdr.payload_len as usize;
            if payload_end > buf.data.len() || payload_end > buf.offset {
                break;
            }
            if hdr.entry_type == JsetEntryType::BtreeRoot as u8 {
                // BtreeRoot: 前 2 字节 = btree_type/level, 后 8 字节 = root_offset
                let btree_type = if payload_end > off { buf.data[off] } else { 0 };
                let level = if payload_end > off + 1 {
                    buf.data[off + 1]
                } else {
                    0
                };
                let root_offset = if payload_end >= off + 10 {
                    u64::from_le_bytes(buf.data[off + 2..off + 10].try_into().unwrap_or([0; 8]))
                } else {
                    0
                };
                roots.push((btree_type, level, root_offset));
            } else if hdr.entry_type == 0 {
                // BtreeKeys: 前 20 字节 = Bpos, 剩余 = entry payload
                let pos = if payload_end - off >= 20 {
                    let inode =
                        u64::from_le_bytes(buf.data[off..off + 8].try_into().unwrap_or([0; 8]));
                    let offset = u64::from_le_bytes(
                        buf.data[off + 8..off + 16].try_into().unwrap_or([0; 8]),
                    );
                    let snapshot = u32::from_le_bytes(
                        buf.data[off + 16..off + 20].try_into().unwrap_or([0; 4]),
                    );
                    Bpos {
                        inode,
                        offset,
                        snapshot,
                    }
                } else {
                    Bpos::default()
                };
                let entry_payload = if payload_end > off + 20 {
                    buf.data[off + 20..payload_end].to_vec()
                } else {
                    Vec::new()
                };
                keys.push((
                    hdr.btree_type,
                    hdr.level,
                    hdr.entry_type,
                    pos,
                    entry_payload,
                ));
            }
            off = payload_end;
        }
        (roots, keys)
    }

    // ── Blacklist ──

    pub fn bch2_blacklist_table_initialize(&self, entries: &[crate::journal::BlacklistEntry]) {
        let converted: Vec<BlacklistEntry> = entries
            .iter()
            .map(|e| BlacklistEntry {
                start_seq: e.start_seq,
                end_seq: e.end_seq,
            })
            .collect();
        let table = if converted.is_empty() {
            None
        } else {
            Some(BlacklistTable::from_entries(&converted))
        };
        *self.blacklist_table.write().unwrap() = table;
    }

    pub fn bch2_journal_seq_is_blacklisted(&self, seq: u64, dirty: bool) -> bool {
        self.blacklist_table
            .read()
            .unwrap()
            .as_ref()
            .map_or(false, |t| t.is_blacklisted(seq, dirty))
    }

    pub async fn bch2_journal_seq_blacklist_add(
        &self,
        _vol: &BchVol,
        start: u64,
        end: u64,
    ) -> Result<(), JournalError> {
        self.bch2_blacklist_table_initialize(&[BlacklistEntry {
            start_seq: start,
            end_seq: end,
        }]);
        Ok(())
    }

    // ── Rewind seq ──

    pub fn bch2_journal_advance_rewind_seq(&self, seq: u64) {
        let mut current = self.rewind_seq.load(Ordering::Acquire);
        while current < seq {
            match self.rewind_seq.compare_exchange_weak(
                current,
                seq,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
    }

    // ── Background tasks ──

    pub async fn stop_background_reclaim(&self) {
        let handle = unsafe { (*self.reclaim_bg_handle.get()).take() };
        if let Some(handle) = handle {
            handle.abort();
            if let Ok(handle) = Arc::try_unwrap(handle) {
                let _ = handle.await;
            }
        }
    }

    pub async fn stop_auto_flush(&self) {
        let handle = unsafe { (*self.flush_bg_handle.get()).take() };
        if let Some(handle) = handle {
            handle.abort();
            if let Ok(handle) = Arc::try_unwrap(handle) {
                let _ = handle.await;
            }
        }
    }

    pub fn bch2_journal_error_set(&self, err: JournalError) {
        let code = journal_error_code(&err);
        self.err_seq
            .store(self.bch2_journal_cur_seq(), Ordering::Release);
        self.cur_entry_error.store(code, Ordering::Release);
    }

    pub fn bch2_journal_error_check(&self) -> Option<JournalError> {
        self.journal_error_check()
    }

    pub fn spawn_background_reclaim_task(&self, vol: &Arc<BchVol>) {
        let journal = vol.journal_arc();
        self.start_background_reclaim(journal, self.reclaim_interval_ms.load(Ordering::Acquire));
    }

    pub fn spawn_auto_flush_task(&self, vol: &Arc<BchVol>) {
        self.start_auto_flush(vol.journal_arc());
    }

    /// 兼容方法 — 别名
    pub fn start_background_reclaim(&self, journal_arc: Arc<Self>, interval_ms: u64) {
        if unsafe { (*self.reclaim_bg_handle.get()).is_some() } {
            return;
        }
        self.reclaim_interval_ms
            .store(interval_ms.max(1), Ordering::Release);
        let weak = Arc::downgrade(&journal_arc);
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let task = handle.spawn(async move {
            loop {
                let Some(journal) = weak.upgrade() else {
                    break;
                };
                let delay = journal
                    .reclaim_interval_ms
                    .load(Ordering::Acquire)
                    .max(1);
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_millis(delay)) => {}
                    _ = journal.reclaim_notify.notified() => {}
                }
                let seq = journal.bch2_journal_cur_seq();
                let _ = journal.bch2_journal_flush_pins(seq);
                journal.bch2_journal_maybe_update_last_seq();
            }
        });
        unsafe { *self.reclaim_bg_handle.get() = Some(Arc::new(task)); }
    }

    /// 兼容方法 — 别名
    pub fn start_auto_flush(&self, journal_arc: Arc<Self>) {
        if unsafe { (*self.flush_bg_handle.get()).is_some() } {
            return;
        }
        let weak = Arc::downgrade(&journal_arc);
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let task = handle.spawn(async move {
            loop {
                let Some(journal) = weak.upgrade() else {
                    break;
                };
                let delay = journal
                    .journal_flush_delay_ms
                    .load(Ordering::Acquire)
                    .max(1);
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                if journal.write_buf.lock().unwrap().entry_count != 0 {
                    let _ = journal.bch2_journal_flush().await;
                }
            }
        });
        unsafe { *self.flush_bg_handle.get() = Some(Arc::new(task)); }
    }

    pub fn pin_fifo_ref(&self) -> &PinListFifo {
        unsafe { &*self.pin_fifo.get() }
    }

    pub fn journal_seq_pin(&self, seq: u64) -> &JournalEntryPinList {
        self.pin_fifo_ref()
            .entry_for_seq(seq)
            .unwrap_or_else(|| panic!("journal_seq_pin: seq {} out of range", seq))
    }

    pub fn maybe_seq_pin(&self, seq: u64) -> Option<&JournalEntryPinList> {
        if seq == 0 {
            None
        } else {
            self.pin_fifo_ref().entry_for_seq(seq)
        }
    }

}

// ═══════════════════════════════════════════════════════════════
// Part 10: Jset Validation
// ═══════════════════════════════════════════════════════════════

pub fn bch2_jset_validate(jset: &Jset) -> bool {
    if jset.header.version > JSET_VERSION {
        return false;
    }
    if jset.header.last_seq > jset.header.seq {
        return false;
    }
    if jset.header.entry_count as usize != jset.entries.len() {
        return false;
    }
    true
}

// ═══════════════════════════════════════════════════════════════
// Part 11: Allocation
// ═══════════════════════════════════════════════════════════════

pub fn bch2_dev_journal_alloc(
    c: &BchVol,
    ca: &crate::block_device::BchDev,
    _new_fs: bool,
) -> Result<(), StorageError> {
    let journal = c.get_journal()?;
    if !journal.to_superblock_state().bucket_addrs.is_empty() {
        return Ok(());
    }

    let bucket_size = crate::block_device::superblock::DEFAULT_JOURNAL_BUCKET_SIZE as u64;
    let first_bucket = crate::block_device::superblock::SUPERBLOCK_SIZE;
    let available = ca.size().saturating_sub(first_bucket);
    let nr = (available / bucket_size)
        .min(crate::block_device::superblock::DEFAULT_NR_JOURNAL_BUCKETS as u64)
        as usize;
    if nr == 0 {
        return Err(StorageError::NoMem);
    }

    let buckets = (0..nr)
        .map(|idx| first_bucket + idx as u64 * bucket_size)
        .collect();
    journal.set_bucket_addrs(buckets)
}

pub fn bch2_fs_journal_alloc(c: &BchVol) -> Result<(), StorageError> {
    for idx in 0..c.device_count() {
        if let Some(ca) = c.device(idx) {
            bch2_dev_journal_alloc(c, ca, true)?;
        }
    }
    Ok(())
}

// ═══════════════════════════════════════════════════════════════
// Part 12: journal_error_check_stuck
// ═══════════════════════════════════════════════════════════════

pub fn journal_error_check_stuck(
    journal: &Journal,
    err: &JournalError,
    watermark: Watermark,
) -> bool {
    if !matches!(err, JournalError::Full(_) | JournalError::PinFull(_))
        || journal.flush_in_progress.load(Ordering::Acquire) != 0
        || watermark != Watermark::Low
        || journal.can_discard.load(Ordering::Acquire)
    {
        return false;
    }
    journal
        .cur_entry_error
        .store(JE_STUCK as i32, Ordering::Release);
    true
}

#[cfg(test)]
mod overlay_tests {
    use super::{
        journal_error_check_stuck, Bpos, BtreeEntry, BtreeId, Journal, JournalError,
        JournalStartInfo, JsetOverlay,
    };
    use crate::block_device::BchDev;
    use crate::BchVol;
    use std::sync::Arc;

    #[test]
    fn overlay_preserves_entry_type_for_replay_reads() {
        let pos = Bpos {
            inode: 7,
            offset: 11,
            snapshot: 0,
        };
        let mut overlay = JsetOverlay::new();
        overlay.set_entry(2, pos, 9, vec![1, 2, 3]);

        let (entry_type, level, payload) = overlay.get_with_type(2, &pos).unwrap();
        assert_eq!(entry_type, 9);
        assert_eq!(level, 0);
        assert_eq!(payload, &vec![1, 2, 3]);
    }

    #[test]
    fn append_encodes_key_position_and_operation_for_replay() {
        let journal = Journal::new(Vec::new());
        let pos = Bpos {
            inode: 3,
            offset: 5,
            snapshot: 0,
        };
        let entry = BtreeEntry {
            btree_type: 2,
            level: 0,
            entry_type: 9,
            pos,
            payload: vec![8, 13],
        };
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            journal.append(BtreeId(2), &[entry], false).await.unwrap();
            let replayed = super::JournalReplayer::new(&journal)
                .replay_from(0)
                .await
                .unwrap();
            assert_eq!(replayed.len(), 1);
            let key = &replayed[0].btree_entries[0];
            assert_eq!(key.btree_type, 2);
            assert_eq!(key.entry_type, 9);
            assert_eq!(key.pos, pos);
            assert_eq!(key.payload, vec![8, 13]);
        });
    }

    #[test]
    fn concurrent_reservations_receive_disjoint_offsets() {
        let journal = Journal::new(Vec::new());
        let mut first = journal
            .bch2_journal_res_get(crate::types::Watermark::Low, 32)
            .unwrap();
        let mut second = journal
            .bch2_journal_res_get(crate::types::Watermark::Low, 32)
            .unwrap();
        assert_ne!(first.offset, second.offset);
        journal
            .bch2_journal_add_entry(&mut first, 0, 2, 0, &[1])
            .unwrap();
        journal
            .bch2_journal_add_entry(&mut second, 0, 2, 0, &[2])
            .unwrap();
        journal.bch2_journal_res_put(&first);
        journal.bch2_journal_res_put(&second);
        assert!(journal.scan_write_buf().1.len() >= 2);
    }

    #[test]
    fn pin_lifecycle_tracks_and_reclaims_sequence() {
        let journal = Journal::new(Vec::new());
        let pin = super::JournalEntryPin::new(None, super::JournalPinType::Btree0);
        journal.bch2_journal_pin_add(1, &pin, None);
        assert!(journal.maybe_seq_pin(1).is_some());
        assert_eq!(journal.bch2_journal_flush_pins(1).unwrap(), 0);
        journal.bch2_journal_pin_drop(&pin);
        assert_eq!(journal.bch2_journal_flush_pins(1).unwrap(), 1);
        assert!(journal.maybe_seq_pin(1).is_none());
    }

    #[test]
    fn pin_drop_advances_last_seq_after_entry_is_ondisk() {
        let journal = Journal::new(Vec::new());
        journal.seq_ondisk.store(1, std::sync::atomic::Ordering::Release);
        let pin = super::JournalEntryPin::new(None, super::JournalPinType::Btree0);
        journal.bch2_journal_pin_add(1, &pin, None);
        assert_eq!(journal.last_seq.load(std::sync::atomic::Ordering::Acquire), 1);
        journal.bch2_journal_pin_drop(&pin);
        assert_eq!(journal.last_seq.load(std::sync::atomic::Ordering::Acquire), 2);
    }

    #[test]
    fn pin_update_moves_reference_between_sequences() {
        let journal = Journal::new(Vec::new());
        journal.seq_ondisk.store(3, std::sync::atomic::Ordering::Release);
        let pin = super::JournalEntryPin::new(None, super::JournalPinType::Btree0);
        journal.bch2_journal_pin_add(1, &pin, None);
        journal.bch2_journal_pin_update(2, &pin, None);
        assert_eq!(journal.journal_seq_pin(1).count.load(std::sync::atomic::Ordering::Acquire), 0);
        assert_eq!(journal.journal_seq_pin(2).count.load(std::sync::atomic::Ordering::Acquire), 1);
        journal.bch2_journal_pin_drop(&pin);
        assert_eq!(journal.last_seq.load(std::sync::atomic::Ordering::Acquire), 3);
    }

    #[test]
    fn rewind_seq_only_moves_forward() {
        let journal = Journal::new(Vec::new());
        journal.bch2_journal_advance_rewind_seq(9);
        journal.bch2_journal_advance_rewind_seq(4);
        assert_eq!(journal.rewind_seq.load(std::sync::atomic::Ordering::Acquire), 9);
    }

    #[test]
    fn stuck_check_requires_reclaim_watermark_and_no_discard() {
        let journal = Journal::new(Vec::new());
        assert!(journal_error_check_stuck(
            &journal,
            &JournalError::Full("full".into()),
            crate::types::Watermark::Low,
        ));
        assert!(matches!(journal.journal_error_check(), Some(JournalError::Stuck(_))));
        assert!(!journal_error_check_stuck(
            &journal,
            &JournalError::Full("full".into()),
            crate::types::Watermark::High,
        ));
    }

    #[test]
    fn journal_alloc_installs_bounded_device_buckets() {
        let stub = Arc::new(BchVol::new());
        let dev = Arc::new(BchDev::with_size(stub, 512 * 1024));
        let vol = BchVol::with_dev(dev.clone(), Vec::new());
        super::bch2_dev_journal_alloc(&vol, &dev, true).unwrap();
        let state = vol.journal_ref().to_superblock_state();
        assert_eq!(state.bucket_addrs.len(), 4);
        assert_eq!(
            state.bucket_addrs[0],
            crate::block_device::superblock::SUPERBLOCK_SIZE
        );
    }

    #[test]
    fn jset_crc32c_rejects_payload_corruption() {
        let mut jset = super::Jset::new(7, 7);
        jset.entries.push(super::RawJsetEntry {
            hdr: super::JsetEntryHeader {
                btree_type: 2,
                entry_type: 0,
                version: super::JSET_ENTRY_VERSION,
                level: 0,
                payload_len: 4,
                has_last: 0,
                has_prev: 0,
            },
            payload: vec![1, 2, 3, 4],
        });
        let encoded = jset.serialize_padded().unwrap();
        assert!(super::Jset::deserialize(&encoded).unwrap().is_some());

        let mut corrupted = encoded;
        corrupted[std::mem::size_of::<super::JsetHeader>() + 1] ^= 0x80;
        assert!(matches!(
            super::Jset::deserialize(&corrupted),
            Err(crate::types::StorageError::Invalid(message))
                if message.contains("checksum")
        ));
    }

    #[test]
    fn journal_read_orders_wrapped_buckets_by_sequence() {
        let buckets = vec![4096, 4096 + super::JSET_BLOCK_SIZE as u64 * 8];
        let stub = Arc::new(BchVol::new());
        let dev = Arc::new(BchDev::with_size(
            stub,
            buckets[1] + super::JSET_BLOCK_SIZE as u64 * 8,
        ));
        let vol = BchVol::with_dev(dev.clone(), buckets.clone());
        let mut newer = super::Jset::new(10, 10);
        newer.entries.push(super::RawJsetEntry {
            hdr: super::JsetEntryHeader {
                btree_type: 2,
                entry_type: 0,
                version: super::JSET_ENTRY_VERSION,
                level: 0,
                payload_len: 0,
                has_last: 0,
                has_prev: 0,
            },
            payload: Vec::new(),
        });
        let older = super::Jset {
            header: super::JsetHeader { seq: 9, last_seq: 9, ..newer.header },
            entries: newer.entries.clone(),
        };
        let newer_bytes = newer.serialize_padded().unwrap();
        let older_bytes = older.serialize_padded().unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            dev.write_at(buckets[0], &newer_bytes).await.unwrap();
            dev.write_at(buckets[1], &older_bytes).await.unwrap();
            let mut info = super::JournalStartInfo::default();
            let jsets = vol.journal_ref().bch2_journal_read(&mut info).await.unwrap();
            assert_eq!(
                jsets.iter().map(|(_, jset)| jset.header.seq).collect::<Vec<_>>(),
                vec![9, 10]
            );
        });
    }

    #[test]
    fn flushed_journal_round_trips_through_device_recovery() {
        let path = std::env::temp_dir().join(format!(
            "subvol-journal-replay-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let buckets = vec![4096, 4096 + super::JSET_BLOCK_SIZE as u64 * 8];
        let size = buckets[1] + super::JSET_BLOCK_SIZE as u64 * 8;
        let stub = Arc::new(BchVol::new());
        let dev = Arc::new(BchDev::with_file(stub, &path, size));
        let vol = BchVol::with_dev(dev, buckets.clone());
        let entry = BtreeEntry {
            btree_type: 2,
            level: 0,
            entry_type: 9,
            pos: Bpos {
                inode: 12,
                offset: 34,
                snapshot: 0,
            },
            payload: vec![55, 89],
        };
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            vol.journal_ref()
                .append(BtreeId(2), &[entry.clone()], true)
                .await
                .unwrap();
            let mut second = entry.clone();
            second.pos.offset += 1;
            vol.journal_ref()
                .append(BtreeId(2), &[second], true)
                .await
                .unwrap();

            let stub2 = Arc::new(BchVol::new());
            let dev2 = Arc::new(BchDev::with_file(stub2, &path, size));
            let vol2 = BchVol::with_dev(dev2, buckets.clone());
            let mut info = JournalStartInfo::default();
            let jsets = vol2.journal_ref().bch2_journal_read(&mut info).await.unwrap();
            assert_eq!(jsets.len(), 2);
            assert!(jsets[1].1.header.seq > jsets[0].1.header.seq);
            let replayed = super::JournalReplayer::from_jsets(vol2.journal_ref(), jsets)
                .replay_from(0)
                .await
                .unwrap();
            let key = &replayed[0].btree_entries[0];
            assert_eq!(key.btree_type, 2);
            assert_eq!(key.entry_type, 9);
            assert_eq!(key.pos, entry.pos);
            assert_eq!(key.payload, entry.payload);

            vol2
                .journal_ref()
                .bch2_journal_discard_replayed()
                .await
                .unwrap();
            let mut after_info = JournalStartInfo::default();
            let after = vol2
                .journal_ref()
                .bch2_journal_read(&mut after_info)
                .await
                .unwrap();
            assert!(after.is_empty());

            let stub3 = Arc::new(BchVol::new());
            let dev3 = Arc::new(BchDev::with_file(stub3, &path, size));
            let vol3 = BchVol::with_dev(dev3, buckets);
            let mut reopened_info = JournalStartInfo::default();
            let reopened = vol3
                .journal_ref()
                .bch2_journal_read(&mut reopened_info)
                .await
                .unwrap();
            assert!(reopened.is_empty());
        });
        let _ = std::fs::remove_file(path);
    }
}
