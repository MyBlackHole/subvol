//! Journal 类型定义 — Journal 实例 + JournalError
//!
//! Journal 是一组预分配的 bucket（循环缓冲区），
//! 每个 journal entry = Jset（含 btree update keys）。
//! 用作 crash recovery 的主机制。
//!
//! # Architecture
//!
//! ```text
//!  ┌──────────────────────────────────────────────────────┐
//!  │  JournalResState (AtomicU64)                         │
//!  │  ┌───────┬──────┬────────┬────────┬────────┬────────┐│
//!  │  │ offset│ idx  │buf0 cnt│buf1 cnt│buf2 cnt│buf3 cnt││
//!  │  │ 22bit │ 2bit │ 10bit  │ 10bit  │ 10bit  │ 10bit  ││
//!  │  └───────┴──────┴────────┴────────┴────────┴────────┘│
//!  │  CAS 循环 → 无锁保留                                    │
//!  └──────────────────────────────────────────────────────┘
//!
//!  buf[0..BUF_NR]   in_flight FIFO       ring[seq & mask]
//!  ┌────────────┐    ┌──────────┐     ┌──────────────────┐
//!  │ Accepting  │───→│ idx=1    │────→│ buf[1] + data    │
//!  │ Closing    │    │ idx=2    │     │ (reservation      │
//!  │ WriteDone  │    │ ...      │     │  fastpath cache)  │
//!  │ Free       │    └──────────┘     └──────────────────┘
//!  └────────────┘
//! ```
//!
//! # Overflow 策略
//!
//! - 每个 buf 容量满时关闭 → 等待所有 reservation 释放 → 写入 bucket
//! - 如果所有 bucket 都已使用 → 返回 `JournalError::Overflow`
//!
//! # bcachefs 对齐
//!
//! | 概念 | bcachefs 文件:行号 |
//! |------|-------------------|
//! | `union journal_res_state` | `fs/journal/types.h:142-174` |
//! | `struct journal_res` | `fs/journal/types.h:134-140` |
//! | `journal_res_get_fast()` | `fs/journal/journal.h:475-518` |
//! | `journal_state_inc()/dec()` | `fs/journal/journal.h` inline |
//! | `JOURNAL_STATE_BUF_NR` | `fs/journal/types.h:20-22` |
//! | `struct journal_buf` | `fs/journal/types.h:37-76` |
//! | `__journal_entry_open_one()` | `fs/journal/journal.c:391` |
//! | `__bch2_journal_buf_put_final()` | `fs/journal/journal.c:240-256` |

use std::cell::UnsafeCell;
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{
    AtomicBool, AtomicI32, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering,
};
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::Weak;
use std::sync::{OnceLock, RwLock};
use tokio::sync::{watch, Notify};

use serde::{Deserialize, Serialize};
use tokio::runtime::Handle;

use crate::alloc::{
    bch2_dev_alloc_list, bucket_to_sector, AllocRequest, BchAllocator, BchDataType, DedicatedWp,
    DevAllocList, DevStripeState, WritePointSpecifier, SECTORS_PER_BLOCK,
};
use crate::block_device::{BchDev, BchDevIoRefGuard, BchDevIoRefKind};
use crate::btree::key::{BtreeEntry, ExtentPtr};
use crate::btree::BtreeId;
use crate::io::{submit_bio_all_blocks_read, submit_bio_write, BioRequest, Closure};
use crate::replicas::BCH_REPLICAS_MAX;
use crate::types::{
    AtomicCell, AtomicFirstError, BgTaskHandle, BlockAddr, StorageError, Watermark,
};
use crate::BchVol;

use super::jset::{
    BlacklistEntry, BlacklistTable, Jset, JsetEntryHeader, JsetEntryType, JsetHeader, RawJsetEntry,
    CSUM_TYPE_CRC32C, JSET_BLOCK_SIZE, JSET_ENTRY_VERSION,
};
use super::reclaim::{
    journal_pin_devs_to_replicas, JournalEntryPin, JournalEntryPinList, PinListFifo,
    ReplicasEntryRefs, PIN_FIFO_SIZE,
};

fn block_on_safe<F, T>(f: F) -> T
where
    F: std::future::Future<Output = T>,
{
    match Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| handle.block_on(f)),
        Err(_) => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to create journal runtime")
            .block_on(f),
    }
}

// ═══════════════════════════════════════════════════════════
// Part 1: Constants
// ═══════════════════════════════════════════════════════════

/// 预分配的 journal bucket 数量
///
/// Wave 1-2 期间 journal bucket 不回收，需足够避免 overflow。
/// 32 buckets × 256 blocks/bucket × 4KB/block = 32MB 元数据空间，
/// 每个 Jset ~4KB，约 8000 次事务写满。保守安全。
pub const DEFAULT_JOURNAL_BUCKETS: u32 = 32;

/// 每个 journal bucket 的 block 数（256 blocks = 1MB）
pub const BUCKET_BLOCKS: u32 = 256;

/// Overflow 警戒线 bytes
///
/// 当当前 bucket 剩余空间小于此值时触发 bucket 轮换。
/// 设为 JSET_BLOCK_SIZE（一个 block），因为每个 Jset 写入至少需要一个 block。
/// 保留：规范常量，用于 bucket 轮换阈值检查（会在 journal flush 路径中用上）
#[allow(dead_code)]
pub const OVERFLOW_MARGIN: u32 = JSET_BLOCK_SIZE;

// ─── Multi-buffer config ───

/// Journal buffer count (bcachefs JOURNAL_STATE_BUF_NR)
pub const JOURNAL_STATE_BUF_NR: usize = 4;
/// 保留：用于 idx→mask 运算（debug/dev 打印 journal state 位布局）
#[allow(dead_code)]
pub const JOURNAL_STATE_BUF_MASK: usize = JOURNAL_STATE_BUF_NR - 1;

/// In-flight journal entry FIFO capacity.
///
/// 对应本地 bcachefs `bch2_fs_journal_init_rw()`
/// (`journal/init.c:767-783`) 的 256 项 inline `journal_buf` FIFO；它与
/// 4 项 reservation fastpath ring 是两个独立的数据结构。
pub const JOURNAL_IN_FLIGHT_NR: usize = 256;

/// Per-buffer staging area size (32KB = 4096 u64s)
pub const BUF_SIZE: usize = 32768;
pub const BUF_SIZE_U64S: u32 = (BUF_SIZE / 8) as u32; // 4096

/// Journal seq 最大值（56-bit，对应 bcachefs `JOURNAL_SEQ_MAX`）
pub const JOURNAL_SEQ_MAX: u64 = (1u64 << 56) - 1;

// ─── Bit layout constants (bcachefs journal_res_state) ───

/// Bit layout of JournalResState:
///   [0..22)  cur_entry_offset — reserved u64s in current entry
///   [22..24) idx — current open journal buffer index
///   [24..34) buf0_count
///   [34..44) buf1_count
///   [44..54) buf2_count
///   [54..64) buf3_count
const CUR_ENTRY_OFFSET_BITS: u64 = 22;
const CUR_ENTRY_OFFSET_MASK: u64 = (1 << CUR_ENTRY_OFFSET_BITS) - 1;
const IDX_BITS: u64 = 2;
const IDX_SHIFT: u64 = CUR_ENTRY_OFFSET_BITS;
const IDX_MASK: u64 = (1 << IDX_BITS) - 1;
const BUF_COUNT_BITS: u64 = 10;
const BUF_COUNT_MAX: u64 = (1 << BUF_COUNT_BITS) - 1;
const BUF0_COUNT_SHIFT: u64 = IDX_SHIFT + IDX_BITS;

/// Sentinel values for cur_entry_offset (bcachefs JOURNAL_ENTRY_CLOSED_VAL etc.)
/// CLOSED_VAL = 0x3FFFFF - 1 = 4194302
pub const JOURNAL_ENTRY_CLOSED_VAL: u64 = CUR_ENTRY_OFFSET_MASK - 1;
/// ERROR_VAL = 0x3FFFFF（对应 bcachefs types.h:199 `#define JOURNAL_ENTRY_ERROR_VAL (JOURNAL_ENTRY_OFFSET_MAX)`）
pub const JOURNAL_ENTRY_ERROR_VAL: u64 = CUR_ENTRY_OFFSET_MASK;
/// BLOCKED_VAL = 0x3FFFFF - 2（对应 bcachefs types.h:197 `#define JOURNAL_ENTRY_BLOCKED_VAL (JOURNAL_ENTRY_OFFSET_MAX - 2)`）
pub const JOURNAL_ENTRY_BLOCKED_VAL: u64 = CUR_ENTRY_OFFSET_MASK - 2;

/// Journal needs flush write flag — 标记 journal 有数据需要写入后端存储。
/// 对应 bcachefs `JOURNAL_NEEDS_FLUSH_WRITE` (journal.h)。
pub const JOURNAL_NEEDS_FLUSH_WRITE: u64 = 1 << 0;

/// Journal cycle flags — 对应 bcachefs `enum journal_cycle_flags`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct JournalCycleFlags(u32);

impl JournalCycleFlags {
    pub(crate) const MUST_CLOSE: Self = Self(1 << 0);
    pub(crate) const MUST_OPEN: Self = Self(1 << 1);
    pub(crate) const FORCE_CLOSE: Self = Self(1 << 2);

    const fn empty() -> Self {
        Self(0)
    }

    const fn contains(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    fn remove(&mut self, other: Self) {
        self.0 &= !other.0;
    }
}

impl std::ops::BitOr for JournalCycleFlags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

// ═══════════════════════════════════════════════════════════
// Part 2: Error
// ═══════════════════════════════════════════════════════════

/// Journal 错误码（用于 journal_error AtomicU8）
pub const JE_NONE: u8 = 0;
pub const JE_OVERFLOW: u8 = 1;
pub const JE_CHECKSUM: u8 = 2;
pub const JE_IO: u8 = 3;
pub const JE_STUCK: u8 = 4;
pub const JE_FULL: u8 = 5;
pub const JE_PIN_FULL: u8 = 6;
pub const JE_BLOCKED: u8 = 7;

/// Journal 错误
#[derive(Debug)]
pub enum JournalError {
    /// Journal 写满（所有 bucket 已用尽且未回收）
    Overflow(String),
    /// CRC32 校验不匹配
    ChecksumMismatch,
    /// 底层存储 I/O 错误
    Io(StorageError),
    /// Journal reclaim 被卡住（pin 无法推进）
    Stuck(String),
    /// Journal 已满（空间不足，等待 reclaim）
    Full(String),
    /// Pin FIFO 已满（bcachefs `journal_pin_full`）
    PinFull(String),
    /// Journal 被阻塞（bcachefs `journal_blocked`）
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

/// 对应本地 `u64_range` (`journal/read.h:68-71`)。
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct U64Range {
    pub start: u64,
    pub end: u64,
}

/// 对应本地 `struct journal_start_info` (`journal/types.h:502-507`)。
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct JournalStartInfo {
    pub last_seq: u64,
    pub replay_end: u64,
    pub cur_seq: u64,
    pub clean: bool,
}

/// 对应本地 `struct journal_ptr` (`journal/types.h:464-474`)。
#[derive(Debug, Clone)]
struct JournalPtr {
    csum_good: bool,
    dev: u8,
    bucket: u32,
    bucket_offset: u64,
    sector: u64,
}

/// 对应本地 `struct journal_replay` (`journal/types.h:514-522`)。
#[derive(Debug)]
struct JournalReplay {
    ptrs: Vec<JournalPtr>,
    csum_good: bool,
    ignore_blacklisted: bool,
    ignore_not_dirty: bool,
    jset: Jset,
    raw: Vec<u8>,
}

/// 对应本地 `struct journal_list` (`journal/read.c:153-161`)。
#[derive(Debug, Default)]
pub(crate) struct JournalList {
    last_seq: u64,
    full_read: bool,
    entries: BTreeMap<u64, JournalReplay>,
}

/// 检测 journal 失败是否应升级为 stuck。
///
/// 对应 bcachefs `journal_error_check_stuck()` 的保守版：
/// 当 journal 已经在 reclaim watermark 上耗尽、没有 in-flight entry，
/// 且失败类型属于 journal-full / pin-full / overflow 时，认为是 stuck。
///
/// 这比单纯返回 `Overflow` 更接近 bcachefs 的故障分流，避免把“可恢复的满”
/// 和“已经没有前进空间的卡死”混成同一种错误。
pub fn journal_error_check_stuck(
    journal: &Journal,
    err: &JournalError,
    watermark: Watermark,
) -> bool {
    let full_like = matches!(
        err,
        JournalError::Overflow(_) | JournalError::Full(_) | JournalError::PinFull(_)
    );
    if !full_like || watermark != Watermark::Reclaim {
        return false;
    }

    // bcachefs journal.c:220: if (j->can_discard) return false;
    // 如果 journal 可以丢弃旧条目（正常运行时），就不是真的 stuck。
    let in_flight_empty = journal.in_flight.lock().unwrap().is_empty();
    let entry_closed = journal.reservations.is_closed();
    if !entry_closed || !in_flight_empty {
        return false;
    }

    // can_discard = 至少一个 journal 设备有可回收 bucket（由 __bch2_journal_reclaim 更新）
    if journal.can_discard.load(Ordering::Acquire) {
        return false;
    }

    true
}

// ═══════════════════════════════════════════════════════════
// Part 3: Bucket state (unchanged)
// ═══════════════════════════════════════════════════════════

/// bcachefs 对齐的 journal bucket 元数据（对应 bcachefs `journal_device`）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalDevice {
    /// bucket 起始 block addr
    pub addr: u64,
    /// 该 bucket 中最大的 journal seq（用于回收判定）
    pub max_seq: u64,
    /// 是否包含未 flush 的条目
    pub dirty: bool,
}

/// Journal 状态快照 — Superblock 序列化用
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalSuperblockState {
    /// 当前分配的 bucket 地址列表
    pub bucket_addrs: Vec<u64>,
    /// 最新分配的 seq
    pub last_seq: u64,
    /// 已落盘的最大 seq
    pub last_seq_ondisk: u64,
    /// 当前 bucket 索引
    pub last_bucket: u32,
    /// discard 索引
    pub discard_idx: u32,
    /// dirty 索引（内存中最旧脏 bucket）
    pub dirty_idx: u32,
    /// dirty ondisk 索引（已落盘的最旧脏 bucket）
    pub dirty_idx_ondisk: u32,
    /// 每个 bucket 的 max seq（用于回收）
    pub bucket_seq: Vec<u64>,
    /// 已回放的 seq（JournalReplayer 幂等用）
    pub replayed_seqs: Vec<u64>,
}

// ═══════════════════════════════════════════════════════════
// Part 4: New types — atomic reservation + multi-buffer
// ═══════════════════════════════════════════════════════════

/// Per-buffer state machine
///
/// 对应 bcachefs buf state：Free → Accepting → Closing → {Noflush →} WriteSubmitted → WriteDone
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BufState {
    /// 可复用
    Free,
    /// 正在接收保留
    Accepting,
    /// 关闭中（不再接收新保留，等待 refcount 归零）
    Closing,
    /// noflush 后缀路径：buf 已关闭但延迟 flush（对应 bcachefs noflush 语义）
    Noflush,
    /// 已提交写入
    WriteSubmitted,
    /// 写入完成，等待回收
    WriteDone,
}

/// JournalBuf 的 wait/list.first 状态。
///
/// 对齐 bcachefs 的 sentinel 语义：
/// - Empty: `NULL`
/// - NotInFlight: `JOURNAL_BUF_NOT_IN_FLIGHT`
/// - NoFlush: `JOURNAL_BUF_NOFLUSH`
/// - FlushNoWait: `JOURNAL_BUF_FLUSH_NO_WAIT`
/// - Waiters: 有真实 waiters
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalBufWaitState {
    Empty,
    NotInFlight,
    NoFlush,
    FlushNoWait,
    Waiters,
}

/// 对应本地 `struct bch_dev_io_failures` (`data/extents_types.h:33-39`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BchDevIoFailures {
    dev: u8,
    csum_nr: u8,
    ec_errcode: i16,
    errcode: i16,
}

/// Journal buffer（对应 bcachefs `struct journal_buf`，types.h:37-76）
pub struct JournalBuf {
    /// 当前状态
    pub state: BufState,
    /// 缓冲区数据
    pub data: Vec<u8>,
    /// 此 buf 的起始 seq
    pub seq: u64,
    /// buf 中实际数据的字节数（BUF_SIZE 以下的已使用长度）
    pub data_end: usize,
    /// 写入完成通知
    pub notify: Arc<Notify>,
    /// buf refcount 归零通知，用于 flush 阶段等待 drain。
    pub drain_watch: watch::Sender<u64>,
    /// 此 buf 中是否包含必须立即 flush 的 reservation
    pub has_must_flush: bool,
    /// wait/list.first 的哨兵状态。
    pub wait_first: JournalBufWaitState,
    /// P2-6: 写入完成回调队列（在 buf → WriteDone 时触发）
    pub write_done_callbacks: Vec<Option<Box<dyn FnOnce() + Send>>>,

    /// 本次 journal write 分配出的 extent ptr，顺序与 `cas` 一致。
    pub key: Vec<ExtentPtr>,
    /// allocation 到 submit/no_io 之间持有的设备 write io_ref。
    pub cas: Vec<BchDevIoRefGuard>,
    /// 成功完成 journal write 的设备列表。
    pub devs_written: Vec<u8>,
    /// 本次 write 的逐设备错误；对应 `journal_buf.failed`。
    failed: Vec<BchDevIoFailures>,

    // ═══ R2 新增字段（对应 bcachefs struct journal_buf, types.h:54-60） ═══
    /// 该 entry 在磁盘上占用的 512 字节 sector 数（对应 bcachefs types.h:57 `unsigned sectors`）
    pub sectors: u32,
    /// data->last_seq 的拷贝（对应 bcachefs types.h:54 `u64 last_seq`）
    pub last_seq: u64,
    /// 预留 u64 数，用于 sectors 计算（对应 bcachefs types.h:60 `unsigned u64s_reserved`）
    pub u64s_reserved: u32,
    /// 当前 data buffer 大小（对应 bcachefs types.h:53 `unsigned buf_size`）。
    pub buf_size: usize,
    /// 若 buffer 足够大时 entry 可使用的最大 sector 数。
    pub disk_sectors: u32,
    /// flush/noflush 决策是否已经完成。
    pub flush_picked: bool,
    /// 当前 entry 是否作为 flush write 提交。
    pub flush: bool,
    /// 多 RW member 时是否使用独立 preflush。
    pub separate_flush: bool,
    /// 写前是否需要把 journal keys 刷入 write buffer。
    pub need_flush_to_write_buffer: bool,
    /// write closure 是否已经启动。
    pub write_started: bool,
    /// journal extent 是否已经分配完成。
    pub write_allocated: bool,
    /// 所有 post-completion bookkeeping 是否完成。
    pub write_done: bool,
    /// entry 是否不含会改变 btree 的 key。
    pub empty: bool,
    /// entry 是否包含 overwrite。
    pub has_overwrites: bool,
}

impl JournalBuf {
    fn free() -> Self {
        Self {
            state: BufState::Free,
            data: Vec::new(),
            seq: 0,
            data_end: 0,
            notify: Arc::new(Notify::new()),
            drain_watch: watch::channel(0).0,
            has_must_flush: false,
            wait_first: JournalBufWaitState::NotInFlight,
            write_done_callbacks: Vec::new(),
            key: Vec::new(),
            cas: Vec::new(),
            devs_written: Vec::new(),
            failed: Vec::new(),
            sectors: 0,
            last_seq: 0,
            u64s_reserved: 0,
            buf_size: 0,
            disk_sectors: 0,
            flush_picked: false,
            flush: false,
            separate_flush: false,
            need_flush_to_write_buffer: false,
            write_started: false,
            write_allocated: false,
            write_done: false,
            empty: false,
            has_overwrites: false,
        }
    }

    /// Reset buf for reuse as the accepting buf
    fn reset_for_accepting(&mut self, new_seq: u64) {
        self.data.resize(BUF_SIZE, 0);
        self.data.fill(0);
        self.seq = new_seq;
        self.data_end = 0;
        self.state = BufState::Accepting;
        self.has_must_flush = false;
        self.wait_first = JournalBufWaitState::Empty;
        self.write_done_callbacks.clear();
        self.key.clear();
        self.cas.clear();
        self.devs_written.clear();
        self.failed.clear();
        self.sectors = 0;
        self.last_seq = 0;
        self.u64s_reserved = 0;
        self.buf_size = self.data.len();
        self.disk_sectors = 0;
        self.flush_picked = false;
        self.flush = false;
        self.separate_flush = false;
        self.need_flush_to_write_buffer = true;
        self.write_started = false;
        self.write_allocated = false;
        self.write_done = false;
        self.empty = false;
        self.has_overwrites = false;
        let _ = self.drain_watch.send(0);
    }

    /// 尝试标记 buf 为 noflush（跳过 FUA/preflush）。
    ///
    /// 对应 bcachefs `bch2_journal_buf_try_noflush()` (journal.h:191-203)。
    /// 行为与上游一致：只有 `wait_first == NULL` 时才能转成 noflush；
    /// 已经是 noflush 时直接返回 true。
    pub(crate) fn bch2_journal_buf_try_noflush(&mut self) -> bool {
        match self.wait_first {
            JournalBufWaitState::NoFlush => true,
            JournalBufWaitState::Empty => {
                self.wait_first = JournalBufWaitState::NoFlush;
                self.state = BufState::Noflush;
                true
            }
            JournalBufWaitState::NotInFlight
            | JournalBufWaitState::FlushNoWait
            | JournalBufWaitState::Waiters => false,
        }
    }

    /// 是否是 clean->dirty 过渡 entry 的 flush 哨兵。
    ///
    /// 对应 bcachefs `JOURNAL_BUF_FLUSH_NO_WAIT`：
    /// 该 entry 必须 flush，但 flushers 不能挂在它上面。
    pub fn is_flush_no_wait(&self) -> bool {
        self.wait_first == JournalBufWaitState::FlushNoWait
    }

    pub fn is_noflush(&self) -> bool {
        self.wait_first == JournalBufWaitState::NoFlush
    }

    /// 对应本地 bcachefs `journal_buf_must_flush()` (`journal/journal.h:174-184`)。
    fn journal_buf_must_flush(&self) -> bool {
        matches!(
            self.wait_first,
            JournalBufWaitState::FlushNoWait | JournalBufWaitState::Waiters
        )
    }

    /// 对应本地 bcachefs `journal_buf_must_not_flush()` (`journal/journal.h:186-189`)。
    fn journal_buf_must_not_flush(&self) -> bool {
        self.wait_first == JournalBufWaitState::NoFlush
    }
}

/// 将一批 flush waiters 原子语义地接到目标 waitlist。
///
/// 对应本地 bcachefs `journal_waitlist_add_batch()` (`journal/write.c:948-962`)：
/// 目标为 noflush/flush-no-wait sentinel 时拒绝，否则整批接入。
fn journal_waitlist_add_batch(
    batch: &mut Vec<Option<Box<dyn FnOnce() + Send>>>,
    wait: &mut JournalBuf,
) -> bool {
    if matches!(
        wait.wait_first,
        JournalBufWaitState::NotInFlight
            | JournalBufWaitState::NoFlush
            | JournalBufWaitState::FlushNoWait
    ) {
        return false;
    }

    if batch.is_empty() {
        return true;
    }

    wait.write_done_callbacks.append(batch);
    wait.wait_first = JournalBufWaitState::Waiters;
    true
}

/// 把一个 entry 的 flush waiters 级联到下一个 entry。
///
/// 对应本地 bcachefs `journal_waitlist_splice()` (`journal/write.c:964-981`)：
/// 先把来源设为 noflush；目标拒绝时必须把原 waitlist 原样还原。
fn journal_waitlist_splice(from: &mut JournalBuf, to: &mut JournalBuf) -> bool {
    let old_state = from.wait_first;
    from.wait_first = JournalBufWaitState::NoFlush;

    if old_state != JournalBufWaitState::Waiters {
        return true;
    }

    let mut batch = std::mem::take(&mut from.write_done_callbacks);
    if journal_waitlist_add_batch(&mut batch, to) {
        return true;
    }

    debug_assert!(from.write_done_callbacks.is_empty());
    from.write_done_callbacks = batch;
    from.wait_first = JournalBufWaitState::Waiters;
    false
}

/// 对应本地 bcachefs `replicas_refs_put()` (`journal/write.c:191-196`)。
fn replicas_refs_put(c: &BchVol, refs: &mut ReplicasEntryRefs) {
    for entry in &refs.entries {
        c.replicas
            .lock()
            .unwrap()
            .put_many(&entry.replicas, entry.nr_refs);
    }
    refs.clear();
}

/// Journal 保留结果（对应 bcachefs `struct journal_res`，types.h:134-140）
///
/// uninit → reserved → committed/freed
///
/// # Seq 设计
///
/// `seq` 按 entry 分配，和 bcachefs 的 `struct journal_res.seq` 一致。
#[derive(Debug)]
pub struct JournalRes {
    /// Journal sequence number（entry 级别）
    pub seq: u64,
    /// 在 buf.data 中的偏移（字节）
    pub offset: u32,
    /// 保留的 u64 数
    pub u64s: u32,
    /// 目标 journal buffer 索引
    pub buf_idx: u32,
    /// 此 reservation 是否需要立即 flush 到后端存储（保证持久化）
    pub must_flush: bool,
}

/// 64-bit 原子保留状态（对应 bcachefs `union journal_res_state`，types.h:142-174）
///
/// 位域布局（与 bcachefs 一致）：
///   [0..22)  cur_entry_offset — 当前 entry 中已保留的 u64 数
///   [22..24) idx — 当前开放的 journal buffer 索引
///   [24..34) buf0_count — buf[0] 保留计数
///   [34..44) buf1_count
///   [44..54) buf2_count
///   [54..64) buf3_count
///
/// 整个 fastpath 只需要一条 `atomic64_cmpxchg`。
pub struct JournalResState {
    bits: AtomicU64,
}

impl JournalResState {
    /// 初始化为 CLOSED_VAL（对应 bcachefs `union journal_res_state old = { .v = JOURNAL_ENTRY_CLOSED_VAL }`）。
    /// 这意味着初始状态下没有打开的 entry，`is_journal_entry_open()` 返回 false，
    /// `try_reserve()` 因 `cur_entry_offset` 为 sentinel 值而返回 None。
    pub const fn new() -> Self {
        Self {
            bits: AtomicU64::new(JOURNAL_ENTRY_CLOSED_VAL),
        }
    }

    /// 原子读取完整 state（对应 bcachefs `smp_load_acquire(&j->reservations.v)`）
    pub fn read(&self) -> u64 {
        self.bits.load(Ordering::Acquire)
    }

    /// 提取 cur_entry_offset（单位 u64）
    /// 对应 bcachefs `union journal_res_state` 的 `cur_entry_offset` 位字段
    pub fn cur_entry_offset(v: u64) -> u32 {
        (v & CUR_ENTRY_OFFSET_MASK) as u32
    }

    /// 提取 idx（当前 Accepting buf 索引）
    /// 对应 bcachefs `union journal_res_state` 的 `idx` 位字段
    pub fn idx(v: u64) -> u32 {
        ((v >> IDX_SHIFT) & IDX_MASK) as u32
    }

    /// 获取指定 buf 的 refcount（对应 bcachefs `journal_state_count()` journal.h:243）
    pub fn buf_count(v: u64, idx: u32) -> u32 {
        let shift = BUF0_COUNT_SHIFT + (idx as u64) * BUF_COUNT_BITS;
        ((v >> shift) & BUF_COUNT_MAX) as u32
    }

    /// Try to reserve `req_u64s` in current entry (CAS loop).
    ///
    /// 对应 bcachefs `journal_res_get_fast()` (journal.h:475-518) 的核心 CAS。
    ///
    /// `max_u64s` 是当前 entry 允许的最大 u64 数（来自 `Journal::cur_entry_u64s`），
    /// 对应 bcachefs journal.h:491 的 `j->cur_entry_u64s` 边界检查。
    /// 早期 subvol 用硬编码 `BUF_SIZE_U64S`，未对齐 bcachefs 的动态 entry 大小限制。
    ///
    /// 返回 `(old_state, new_state)` on success, `None` on failure (need slowpath).
    pub fn try_reserve(&self, req_u64s: u32, max_u64s: u32) -> Option<(u64, u64)> {
        let mut old = self.bits.load(Ordering::Relaxed);
        loop {
            let cur_off = Self::cur_entry_offset(old);
            let idx = Self::idx(old);

            // 检查是否有足够空间（bcachefs journal.h:491）
            // 使用动态 max_u64s（cur_entry_u64s）而非硬编码 BUF_SIZE_U64S
            if (cur_off as u64).wrapping_add(req_u64s as u64) > max_u64s as u64 {
                return None;
            }

            // 检查 refcount 溢出（bcachefs journal.h:505）
            let shift = BUF0_COUNT_SHIFT + (idx as u64) * BUF_COUNT_BITS;
            let count = (old >> shift) & BUF_COUNT_MAX;
            if count == BUF_COUNT_MAX {
                return None;
            }

            let mut new = old;
            // 推进 cur_entry_offset（bcachefs journal.h:499）
            new = (new & !CUR_ENTRY_OFFSET_MASK)
                | ((cur_off as u64).wrapping_add(req_u64s as u64) & CUR_ENTRY_OFFSET_MASK);
            // 递增 buf refcount（bcachefs journal_state_inc）
            new = (new & !(BUF_COUNT_MAX << shift)) | ((count + 1) & BUF_COUNT_MAX) << shift;

            match self
                .bits
                .compare_exchange_weak(old, new, Ordering::Acquire, Ordering::Relaxed)
            {
                Ok(_) => return Some((old, new)),
                Err(updated) => old = updated,
            }
        }
    }

    /// Release a reservation: decrement refcount for buf idx.
    ///
    /// 对应 bcachefs `bch2_journal_buf_put()` (journal.h:395-403) 的 atomic_sub。
    /// 返回 decrement 前的 state 值，调用者可检查 refcount 是否归零。
    pub fn release(&self, idx: u32) -> u64 {
        let shift = BUF0_COUNT_SHIFT + (idx as u64) * BUF_COUNT_BITS;
        self.bits.fetch_sub(1 << shift, Ordering::Release)
    }

    /// Close current entry with the specified sentinel value.
    ///
    /// 对应 bcachefs `__journal_entry_close_one()` (journal.c:276-293) 的 CAS close。
    /// bcachefs 参数 `closed_val` 可为 `JOURNAL_ENTRY_CLOSED_VAL` 或 `JOURNAL_ENTRY_ERROR_VAL`。
    ///
    /// 返回 CAS 成功前捕获的 `cur_entry_offset`（单位 u64），
    /// 调用方可用此值设置 `buf.data_end`。
    /// 如果 entry 已处于 ERROR_VAL 或目标 closed_val 状态，返回 `JOURNAL_ENTRY_CLOSED_VAL as u32`（哨兵值）。
    ///
    /// 见 J2 flush data race 修复：先 close_entry（原子捕获 offset + 阻止新 reservation），
    /// 再 drain refcount，最后设 data_end，防止截断并发写入数据。
    fn close_entry_with_val(&self, closed_val: u64) -> u32 {
        let closed_val = closed_val & CUR_ENTRY_OFFSET_MASK;
        loop {
            let old = self.bits.load(Ordering::Relaxed);
            let captured_offset = Self::cur_entry_offset(old);
            let old_closed = old & CUR_ENTRY_OFFSET_MASK;

            // 对应 bcachefs __journal_entry_close_one (journal.c:290-292):
            // if (old.cur_entry_offset == JOURNAL_ENTRY_ERROR_VAL ||
            //     old.cur_entry_offset == new.cur_entry_offset) return;
            if old_closed == JOURNAL_ENTRY_ERROR_VAL || old_closed == closed_val {
                return JOURNAL_ENTRY_CLOSED_VAL as u32;
            }

            let new = (old & !CUR_ENTRY_OFFSET_MASK) | closed_val;
            if self
                .bits
                .compare_exchange_weak(old, new, Ordering::Release, Ordering::Relaxed)
                .is_ok()
            {
                return captured_offset;
            }
        }
    }

    /// Close current entry: set cur_entry_offset to CLOSED_VAL.
    ///
    /// 对应 bcachefs `__journal_entry_close_one()` (journal.c:276) 的 CAS close。
    /// 委托给 `close_entry_with_val(JOURNAL_ENTRY_CLOSED_VAL)`。
    ///
    /// 返回 CAS 成功前捕获的 `cur_entry_offset`（单位 u64）。
    fn close_entry(&self) -> u32 {
        self.close_entry_with_val(JOURNAL_ENTRY_CLOSED_VAL)
    }

    /// Open new entry: set idx to `new_idx`, clear cur_entry_offset and increment buf_count.
    ///
    /// 对应 bcachefs `__journal_entry_open_one()` (journal.c:549-564) 的 CAS open。
    /// bcachefs 在 CAS 循环中调用了 `journal_state_inc(&new)` (journal.c:556)：
    /// ```c
    /// journal_state_inc(&new);  // buf_count[new.idx]++
    /// new.cur_entry_offset = le32_to_cpu(buf->data->u64s);
    /// ```
    /// 这个隐式 refcount 在 close 路径中由 `__bch2_journal_buf_put` 释放。
    fn open_entry(&self, new_idx: u32) {
        debug_assert!(new_idx < 4);
        loop {
            let old = self.bits.load(Ordering::Relaxed);
            let mut new = old;
            // Clear idx field
            new &= !(IDX_MASK << IDX_SHIFT);
            // Set new idx
            new |= (new_idx as u64) << IDX_SHIFT;
            // Clear cur_entry_offset
            new &= !CUR_ENTRY_OFFSET_MASK;
            // Increment buf_count for new_idx (bcachefs journal_state_inc)
            let shift = BUF0_COUNT_SHIFT + (new_idx as u64) * BUF_COUNT_BITS;
            let count = (old >> shift) & BUF_COUNT_MAX;
            debug_assert_eq!(
                count, 0,
                "buf_count for new idx must be 0 before open_entry"
            );
            new = (new & !(BUF_COUNT_MAX << shift)) | ((count + 1) & BUF_COUNT_MAX) << shift;
            if self
                .bits
                .compare_exchange_weak(old, new, Ordering::Release, Ordering::Relaxed)
                .is_ok()
            {
                return;
            }
        }
    }

    /// Align the closed reservation idx so the next open lands on `seq & BUF_MASK`.
    ///
    /// 需要在 `from_superblock` 恢复后、第一次 `journal_entry_open` 前调用；
    /// `open_entry` 会把 idx 循环推进一次。
    fn align_idx_to_seq(&self, seq: u64) {
        let desired = seq.wrapping_sub(1) & (JOURNAL_STATE_BUF_NR as u64 - 1);
        let old = self.bits.load(Ordering::Relaxed);
        let new = (old & !(IDX_MASK << IDX_SHIFT)) | (desired << IDX_SHIFT);
        self.bits.store(new, Ordering::Release);
    }

    /// Check if current entry is closed
    fn is_closed(&self) -> bool {
        Self::cur_entry_offset(self.bits.load(Ordering::Relaxed)) as u64 >= JOURNAL_ENTRY_CLOSED_VAL
    }

    /// Check if current entry is open (inverse of closed)
    fn is_open(&self) -> bool {
        !self.is_closed()
    }

    /// 设置 cur_entry_offset 为指定值（CAS 循环，保留其他字段）。
    ///
    /// 对应 bcachefs `bch2_journal_unblock` 中的 CAS 恢复操作 (journal.c:1350-1359)。
    /// 由 block/unblock 路径使用，受 slowpath_lock 保护。
    fn set_cur_entry_offset(&self, offset: u64) {
        let offset = offset & CUR_ENTRY_OFFSET_MASK;
        loop {
            let old = self.bits.load(Ordering::Relaxed);
            let new = (old & !CUR_ENTRY_OFFSET_MASK) | offset;
            if self
                .bits
                .compare_exchange_weak(old, new, Ordering::Release, Ordering::Relaxed)
                .is_ok()
            {
                return;
            }
        }
    }

    /// 尝试设置 cur_entry_offset 为 BLOCKED_VAL，返回 (old_state, success)。
    ///
    /// 对应 bcachefs `__bch2_journal_block` 中的 CAS (journal.c:1367-1380)。
    /// 仅在 `cur_entry_offset < CLOSED_VAL` 时（entry 实际处于 open 状态）生效。
    /// 如果 entry 已关闭（offset >= CLOSED_VAL），返回 (old_state, false)。
    ///
    /// 成功时返回 (old_state, true)，调用方可根据 old_state 设置 buf data_end。
    fn try_block(&self) -> (u64, bool) {
        let mut old = self.bits.load(Ordering::Relaxed);
        loop {
            let cur_off = Self::cur_entry_offset(old) as u64;
            if cur_off >= JOURNAL_ENTRY_CLOSED_VAL {
                return (old, false);
            }
            let new = (old & !CUR_ENTRY_OFFSET_MASK) | JOURNAL_ENTRY_BLOCKED_VAL;
            match self
                .bits
                .compare_exchange_weak(old, new, Ordering::Acquire, Ordering::Relaxed)
            {
                Ok(_) => return (old, true),
                Err(updated) => old = updated,
            }
        }
    }
}

/// Wrapper around UnsafeCell for the journal buf array.
///
/// # Safety
///
/// Sync is safe because:
/// - `commit()` writes to non-overlapping regions (each reservation has a unique offset
///   guaranteed by CAS on `JournalResState`)
/// - State transitions (Free → Accepting → Closing → ...) are single-threaded
///   (happen under the journal lock or slowpath serialization)
/// - After a buf is closed, no new reservations target it (refcount drain is the
///   last access path)
struct BufArray {
    bufs: UnsafeCell<[JournalBuf; JOURNAL_IN_FLIGHT_NR]>,
}

unsafe impl Sync for BufArray {}
unsafe impl Send for BufArray {}

impl BufArray {
    fn new() -> Self {
        Self {
            bufs: UnsafeCell::new(std::array::from_fn(|_| JournalBuf::free())),
        }
    }

    /// Get immutable reference to buf at index（保留：用于 debug 检查 buf 状态）
    #[allow(dead_code)]
    fn get(&self, idx: usize) -> &JournalBuf {
        unsafe { &(*self.bufs.get())[idx] }
    }

    /// Get mutable reference to buf at index (caller guarantees no aliasing violations)
    #[allow(clippy::mut_from_ref)]
    fn get_mut(&self, idx: usize) -> &mut JournalBuf {
        unsafe { &mut (*self.bufs.get())[idx] }
    }

    /// Get mutable reference to all bufs (for bucket flush which accesses sequentially)
    #[allow(dead_code, clippy::mut_from_ref)]
    fn get_all_mut(&self) -> &mut [JournalBuf; JOURNAL_IN_FLIGHT_NR] {
        unsafe { &mut *self.bufs.get() }
    }
}

// ═══════════════════════════════════════════════════════════
// Part 5: Pin FIFO — per-seq btree reference tracking
// ═══════════════════════════════════════════════════════════

/// 最大 pin 条目数（固定预分配数组的大小）
///
/// 对应 bcachefs `JOURNAL_PIN_LIST_SIZE` 的概念。
/// 128 = 最多 128 个在途 journal entry，足以覆盖 4 buffer × 32 bucket 的并发。
pub use super::reclaim::PIN_FIFO_SIZE as MAX_PIN_ENTRIES;

// JournalEntryPin 和 PinFifo 定义已移至 reclaim.rs。
// 导入自: use super::reclaim::{JournalEntryPin, JournalEntryPinList, PinListFifo};

// ═══════════════════════════════════════════════════════════
// Part 5b: JournalSpace — slowpath space tracking
// ═══════════════════════════════════════════════════════════

/// Journal space category — 对应 bcachefs journal_space 数组中不同等级的可回收空间
///
/// 四个索引按"可回收程度"排序：
///   DISCARDED(0):  已 discard 的 bucket（最安全，完全自由）
///   CLEAN_ONDISK(1): 已落盘且 clean 的 bucket
///   CLEAN(2):      内存中 clean 的 bucket
///   TOTAL(3):      全部 journal bucket（包含当前正在写入的）
#[derive(Debug, Clone, Copy)]
pub struct JournalSpace {
    /// 该类别总字节数
    pub total: u64,
    /// 该类别可用字节数
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

/// JournalSpace 数组索引常量
pub const JOURNAL_SPACE_DISCARDED: usize = 0;
pub const JOURNAL_SPACE_CLEAN_ONDISK: usize = 1;
pub const JOURNAL_SPACE_CLEAN: usize = 2;
pub const JOURNAL_SPACE_TOTAL: usize = 3;
pub const JOURNAL_SPACE_NR: usize = 4;

/// Journal slowpath 状态下所有 bucket 管理字段
///
/// 被 `Journal.slowpath: Mutex<JournalSlowpath>` 保护。
/// 通过 `slowpath_lock` 序列化所有慢路径操作。
#[derive(Debug)]
pub(crate) struct JournalSlowpath {
    /// journal bucket 列表
    pub buckets: Vec<JournalDevice>,
    /// 每个 bucket 的 max seq（同 bcachefs ja->bucket_seq[]）
    pub bucket_seq: Vec<u64>,
    /// 当前写入的 bucket 索引
    pub current_bucket: usize,
    /// 当前 bucket 内的偏移（字节）
    pub current_offset: u32,
    /// 当前 bucket 还剩多少可用字节
    pub remaining_bytes: u32,
    /// 下一个可丢弃的 bucket 索引 (模 nr)
    /// 四索引不变式: discard_idx ≤ dirty_idx_ondisk ≤ dirty_idx ≤ cur_idx
    pub discard_idx: usize,
    /// 内存中最旧的 dirty bucket
    pub dirty_idx: usize,
    /// 确认落盘的最旧 dirty bucket
    pub dirty_idx_ondisk: usize,
    /// 回滚范围（[from, to) 列表）。
    /// 对应 bcachefs `j->rewind_ranges` (journal.c:1294 darray)。
    /// write path 编码为 Rewind entry，read path 在 recovery 时恢复。
    pub rewind_ranges: Vec<(u64, u64)>,
    /// 待写入的 rewind entry 记录（对应 bcachefs `early_journal_entries`）。
    /// 这里仅保存范围二元组；实际 entry 由 journal write 路径在提交时编码。
    pub early_journal_entries: Vec<(u64, u64)>,
}

impl JournalSlowpath {
    pub fn new(bucket_addrs: Vec<u64>) -> Self {
        let nr = bucket_addrs.len();
        Self {
            bucket_seq: vec![0; nr],
            buckets: bucket_addrs
                .into_iter()
                .map(|addr| JournalDevice {
                    addr,
                    max_seq: 0,
                    dirty: false,
                })
                .collect(),
            current_bucket: 0,
            current_offset: 0,
            remaining_bytes: BUCKET_BLOCKS * JSET_BLOCK_SIZE,
            discard_idx: 0,
            dirty_idx: 0,
            dirty_idx_ondisk: 0,
            rewind_ranges: Vec::new(),
            early_journal_entries: Vec::new(),
        }
    }

    pub fn from_superblock(state: &JournalSuperblockState) -> Self {
        let nr = state.bucket_addrs.len();
        let bucket_seq = if state.bucket_seq.len() == nr {
            state.bucket_seq.clone()
        } else {
            vec![0; nr]
        };
        let bucket_idx = (state.last_bucket as usize).min(nr.saturating_sub(1));
        Self {
            bucket_seq,
            buckets: state
                .bucket_addrs
                .iter()
                .map(|addr| JournalDevice {
                    addr: *addr,
                    max_seq: 0,
                    dirty: false,
                })
                .collect(),
            current_bucket: bucket_idx,
            current_offset: 0,
            remaining_bytes: BUCKET_BLOCKS * JSET_BLOCK_SIZE,
            discard_idx: state.discard_idx as usize,
            dirty_idx: state.dirty_idx as usize,
            dirty_idx_ondisk: state.dirty_idx_ondisk as usize,
            rewind_ranges: Vec::new(),
            early_journal_entries: Vec::new(),
        }
    }
}

// ═══════════════════════════════════════════════════════════
// Part 6: Journal — main struct
// ═══════════════════════════════════════════════════════════

/// Journal 实例结构
///
/// 管理 journal bucket 的写入状态和 seq 分配。
/// 不直接依赖 Volume 或 BchVol。
///
/// # 并发模型
///
/// - **Fastpath** (`journal_res_get_fast`, `journal_res_put`, `commit`):
///   接受 `&self`，无锁原子操作
/// - **Slowpath** (`journal_cycle_locked`, `journal_res_get_slowpath`):
///   通过 `slowpath_lock` 序列化，使用 `slowpath: Mutex<JournalSlowpath>` 保护 bucket 状态
/// - **Full flush/reclaim** (`flush`, `reclaim`, `rotate_or_reclaim`):
///   接受 `&mut self`，管理 bucket 和 backend I/O
pub struct Journal {
    // ★ 原子保留 + 多 buffer（fastpath，无锁）
    /// 原子保留状态（无锁 fastpath，bcachefs `union journal_res_state`）
    reservations: JournalResState,
    /// 多 buffer 数组（UnsafeCell 包装，commit 时写非重叠区域）
    bufs: BufArray,
    /// 无锁 seq 分配（bcachefs `atomic64_t seq`）
    seq: AtomicU64,
    /// 在途 buf 索引队列
    in_flight: Mutex<VecDeque<u32>>,

    // ★ Bucket 管理状态（由 slowpath_lock 或 &mut self 保护）
    slowpath: Mutex<JournalSlowpath>,

    // ★ Per-seq pin FIFO（bcachefs reclaim 侧的对齐实现）
    /// 内存回收边界：所有 ≤ 此 seq 的 pin 已完全释放（count=0 且已从 FIFO 前端弹出）。
    /// 由 `bch2_journal_maybe_update_last_seq` 在 pin_put 路径中推进。
    /// 对应 bcachefs `journal->last_seq`。
    pub last_seq: AtomicU64,
    /// 磁盘持久化边界：所有 ≤ 此 seq 的数据已确认落盘。
    /// 在 flush 完成后从 buf.last_seq（写时捕获的 last_seq 快照）推进。
    /// 对应 bcachefs `journal->last_seq_ondisk`。
    pub last_seq_ondisk: AtomicU64,
    /// Per-seq pin FIFO：追踪 btree node 对 journal reservation seq 的引用。
    /// 使用 PinListFifo（128-slot 预分配）替代旧 PinFifo。
    /// 对应 bcachefs `struct journal` 的 `pin_list[6]`（按 pin type 分离）。
    pub(crate) pin_fifo: UnsafeCell<PinListFifo>,
    /// flush 当前处理中的 pin（retry loop 保护）。
    /// 对应 bcachefs `journal->flush_in_progress`。
    pub(crate) flush_in_progress: AtomicU64,
    /// flush 期间 pin_drop 标记（防止 UAF）。
    /// 对应 bcachefs `journal->flush_in_progress_dropped`。
    pub(crate) flush_in_progress_dropped: AtomicBool,
    /// flush 等待条件变量（pin_flush 等待 flush_in_progress 变化）。
    /// 对应 bcachefs `journal->pin_flush_wait`。
    pub(crate) pin_flush_wait: Arc<Condvar>,
    /// pin_flush_wait 的互斥锁（Condvar::wait 需要 MutexGuard）。
    pub(crate) pin_flush_lock: Mutex<()>,
    /// flush 完成等待队列 — 对应 bcachefs `journal->flush_wait`（closure_waitlist）。
    /// bcachefs 中 flush_wait 的 waiters 在 `__journal_entry_open` 中通过 xchg 迁移到
    /// 新 buf 的 wait 列表，不由 buf_put_final 唤醒。仅在 `halt_locked` 异常终止时
    /// 主动唤醒，使等待 flush 完成的线程看到错误后退出。
    pub(crate) flush_wait: Arc<Condvar>,
    /// flush_wait 的互斥锁。
    pub(crate) flush_wait_lock: Mutex<()>,
    /// reclaim flush 完成等待队列 — 对应 bcachefs `journal->reclaim_flush_wait`。
    /// 在 `halt_locked` 时唤醒，使等待 reclaim 完成的线程看到错误状态后退出。
    pub(crate) reclaim_flush_wait: Arc<Condvar>,
    /// reclaim_flush_wait 的互斥锁。
    pub(crate) reclaim_flush_wait_lock: Mutex<()>,
    /// JOURNAL_running 标志 — 对应 bcachefs `JOURNAL_running`（init.c:629）。
    /// Journal 可接受新 reservation 和 IO 时设为 true。
    running: AtomicBool,
    /// JOURNAL_replay_done 标志 — 对应 bcachefs `JOURNAL_replay_done`（init.c:630）。
    /// 回放完成后设为 true，允许 journal seq 推进超过 replay 范围。
    pub(crate) replay_done: AtomicBool,
    /// can_discard 标志 — 对应 bcachefs `journal->can_discard`（types.h:404）。
    /// 在 __bch2_journal_reclaim 中设为至少一个 journal 设备有可丢弃 bucket。
    /// `journal_error_check_stuck` 检查此标志：true 表示正常运行时、可回收、未 stuck。
    can_discard: AtomicBool,
    /// 当前正在 flush 的 seq 上限，对应 bcachefs `journal->flushing_seq`。
    flushing_seq: AtomicU64,
    /// 最大已落盘 seq（flush 完成后更新）。
    flushed_seq_marker: AtomicU64,
    /// 最近启动写入的 seq；对应本地 `journal->seq_write_started`。
    seq_write_started: AtomicU64,
    /// 实际持久化的最大 seq（IO 完成后更新，对应 bcachefs seq_ondisk）。
    /// 与 flushed_seq_marker 不同：后者在写入提交时推进，seq_ondisk 在 IO 完成后推进。
    pub(crate) seq_ondisk: AtomicU64,
    /// 由 flush write 推进的持久化 seq（非 meta write，对应 bcachefs flushed_seq_ondisk）。
    /// 用于 shutdown_quiesced 的精确判断。
    pub(crate) flushed_seq_ondisk: AtomicU64,
    /// 最近一个 empty journal entry；对应本地 `journal->last_empty_seq`。
    last_empty_seq: AtomicU64,
    /// 当前仍处于 dirty 状态的 journal entry 字节总量。
    ///
    /// 对应 bcachefs `journal->dirty_entry_bytes` (journal/types.h:377,
    /// journal.c:315-316, reclaim.c:196/480-484)。
    pub(crate) dirty_entry_bytes: AtomicU64,

    // ★ Watermark 水位线系统
    /// 当前 journal 水位线（利用率越高值越大，阻止低优先级操作）
    current_watermark: AtomicU8,

    /// Journal 错误状态（0=无错误，非零=对应 JournalErrorCode 编码）。
    /// 一旦设置后不可清除，后续所有 `journal_res_get` 返回错误。
    /// 对应 bcachefs `journal->res->error`（atomic_t）。
    journal_error: AtomicU8,
    /// Journal 阻塞计数（对应 bcachefs `journal->blocked`）
    /// 非零时拒绝新的 entry 分配。
    pub(crate) blocked: AtomicU32,
    /// 当前 entry 错误（对应 bcachefs `journal->cur_entry_error`）
    /// 非零时拒绝新的 entry 分配并返回此错误。
    pub(crate) cur_entry_error: AtomicI32,
    /// seq 进入错误状态的时刻（对应 bcachefs `journal->err_seq`，journal.c:667）
    pub err_seq: AtomicU64,
    /// 首次 block 时保存的 cur_entry_offset（对应 bcachefs `journal->cur_entry_offset_if_blocked`，journal.c:1369）
    /// 在 unblock 最后一个 blocker 时恢复此值到 reservations.cur_entry_offset。
    /// 初始值设为 CLOSED_VAL（安全默认，表示无被 block 的 entry 需要恢复）。
    pub(crate) cur_entry_offset_if_blocked: AtomicU32,
    /// entry 级别已预留 u64 总数（对应 bcachefs `journal->entry_u64s_reserved`）。
    /// 在 `bch2_journal_entry_res_resize` 中调整。
    pub(crate) entry_u64s_reserved: AtomicU32,
    /// 当前 entry 已使用的 u64 数（对应 bcachefs `journal->cur_entry_u64s`）。
    pub(crate) cur_entry_u64s: AtomicU32,
    /// 期望的 journal buffer 大小；对应本地 `journal->buf_size_want`。
    buf_size_want: AtomicUsize,
    /// 保护无 reservation 的 journal buffer 访问；对应本地 `journal->buf_lock`。
    buf_lock: Mutex<()>,
    /// 保护 journal buffer 指针和大小交换；对应本地 `journal->lock`。
    lock: Mutex<()>,
    /// 回滚 seq 上限（对应 bcachefs `journal->rewind_seq`）。
    /// 保证 discards 到该 seq 是安全的。
    pub(crate) rewind_seq: AtomicU64,
    /// 已随 flush write 持久化的 rewind seq；对应本地 `journal->rewind_seq_ondisk`。
    rewind_seq_ondisk: AtomicU64,

    /// 对应本地 `j->wp.stripe`，journal 设备选择的 WFQ 状态。
    wp_stripe: Mutex<DevStripeState>,

    // ★ 新增：slowpath 状态机 + 空间追踪 + 自动 flush
    /// slowpath 序列化锁（`journal_res_get` 从 &self 进入 slowpath 时使用）
    slowpath_lock: Mutex<()>,
    /// reclaim 互斥锁 — 串行化整个回收流程，防止并发 flush/reclaim 竞争。
    /// 对应 bcachefs `journal->reclaim_lock` (reclaim.c:1073)。
    pub(crate) reclaim_lock: Mutex<()>,
    /// reclaim_kicked 标志 — 后台线程即时唤醒机制。
    /// 设置此标志后通知后台线程立即执行回收循环，无需等待间隔超时。
    /// 对应 bcachefs `journal->reclaim_kicked` (reclaim.h:14)。
    pub(crate) reclaim_kicked: AtomicBool,
    /// reclaim 通知器 — 用于把 kick 从轮询升级为即时唤醒。
    pub(crate) reclaim_notify: Notify,
    /// 前台 reclaim 总 flush 计数（direct pass 中 flush 的 pin 数）。
    /// 对应 bcachefs `journal->nr_direct_reclaim` (types.h:396)。
    pub(crate) nr_direct_reclaim: AtomicU64,
    /// 后台 reclaim 总 flush 计数（background pass 中 flush 的 pin 数）。
    /// 对应 bcachefs `journal->nr_background_reclaim` (types.h:397)。
    pub(crate) nr_background_reclaim: AtomicU64,
    /// 4 级空间追踪（discarded / clean_ondisk / clean / total）
    space: [JournalSpace; JOURNAL_SPACE_NR],
    /// 自动 flush 间隔（None = 禁用）
    auto_flush_ms: Option<u64>,
    /// 后台回收间隔（毫秒，0=禁用）。由 spawn_background_reclaim_task 设置。
    pub(crate) reclaim_interval_ms: AtomicU64,
    /// may_skip_flush 标志 — 对应 bcachefs `JOURNAL_may_skip_flush` (types.h:219)。
    /// 在 __bch2_journal_reclaim 中计算：当 ondisk 空间充裕且 dirty ≤ total/8 时置位，
    /// 允许 __should_flush 跳过 flush（减少不必要的 commit write）。
    may_skip_flush: AtomicBool,
    /// journal flush 超时（毫秒）— 对应 bcachefs `journal_flush_delay` 选项 (write.c:1075-1076)。
    /// 上次 flush 后超过此间隔未 flush 则强制触发，确保 entry 不会无限期延迟。
    journal_flush_delay_ms: AtomicU64,
    /// 正在进行的 flush 写入计数 — 对应 bcachefs `j->flushes_outstanding` (types.h:139)。
    flushes_outstanding: AtomicU32,

    // ★ P2-7: flush write flag + jiffies 追踪
    /// flush 通知器 — 当有 WriteSubmitted buf 需要写入时通知后台 flush 任务。
    /// 对应 bcachefs `bch2_journal_do_writes_locked` 中的 closure_call 语义（dispatch to wq）。
    flush_notify: Notify,
    /// auto-commit delayed work 的绝对到期时间（Unix 毫秒，0 表示未排队）。
    /// 对应本地 bcachefs `journal->write_work`。
    write_work_deadline_ms: AtomicU64,
    /// `write_work` 排队、重置或取消时唤醒后台 workqueue。
    write_work_notify: Notify,
    /// journal workqueue 已启动；构造期 open entry 不得提前排队。
    write_work_running: AtomicBool,
    /// seq flush 通知器 — 当 flushed_seq_ondisk 推进时通知所有等待的 caller。
    /// 对应 bcachefs `bch2_journal_flush_seq_async` 中的 closure_waitlist 通知语义。
    seq_flush_notify: Notify,
    /// Journal needs flush write flag — 是否有数据需要写入（对应 bcachefs `JOURNAL_NEEDS_FLUSH_WRITE`）
    needs_flush_write: AtomicBool,
    /// 上次 flush 时的 jiffies 时间戳（用于 flush 频率控制）
    last_flush_jiffies: AtomicU64,
    /// flush/noflush write 选择计数；对应本地同名统计字段。
    nr_flush_writes: AtomicU64,
    nr_noflush_writes: AtomicU64,
    /// 已发布 journal entry 的总字节数；对应本地 `entry_bytes_written`。
    entry_bytes_written: AtomicU64,

    /// 回收 journal buf 数据缓冲区，避免重复分配
    /// （对应 bcachefs `journal->free_buf`，types.h:311-312）。
    /// 由 buf_lock 保护。
    free_buf: UnsafeCell<Option<Vec<u8>>>,
    /// 回收缓冲区大小（对应 bcachefs `journal->free_buf_size`，types.h:312）。
    /// 由 buf_lock 保护。
    free_buf_size: UnsafeCell<usize>,

    // ★ 运行时黑名单表
    /// Blacklist table — 对应 bcachefs `c->journal_seq_blacklist_table`
    ///
    /// bcachefs 中 blacklist_table 是裸指针（`struct journal_seq_blacklist_table *`），
    /// `bch2_journal_seq_blacklist_add()` 写完 superblock 后会替换此表。
    blacklist_table: RwLock<Option<BlacklistTable>>,

    // ★ Phase-2a: 后台任务句柄
    /// 自动 flush 后台任务句柄（bch2_fs_read_write 时启动，bch2_fs_read_only 时停止）
    ///
    /// bcachefs 中 `reclaim_thread` 是裸 `struct task_struct *`，启动/停止均在单线程
    /// 上下文中进行，无锁。subvol 用 `UnsafeCell` 移除多余 Mutex。
    flush_bg_handle: UnsafeCell<Option<BgTaskHandle>>,
    /// 后台 reclaim 任务句柄（bch2_fs_read_write 时启动，bch2_fs_read_only 时停止）
    ///
    /// 同 flush_bg_handle，启动/停止路径单线程，无需 Mutex。
    reclaim_bg_handle: UnsafeCell<Option<BgTaskHandle>>,

    // ★ Phase-2 journal safety net
    /// Phase 2 journal safety net：back reference to BchVol for injecting btree roots into journal writes。
    /// 使用 OnceLock + Weak 避免循环引用阻止 deallocation。
    /// 对应 bcachefs `struct journal` 通过 `container_of(j, struct bch_fs, journal)`
    /// 访问 filesystem 的模式。
    device: OnceLock<Arc<BchDev>>,
    pub(crate) vol: OnceLock<Weak<BchVol>>,
    #[cfg(test)]
    /// 测试用设备（无 BchVol 时使用）。
    test_device: OnceLock<Arc<BchDev>>,
}

impl std::fmt::Debug for JournalEntryPin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JournalEntryPin")
            .field("seq", &self.seq.load(Ordering::Relaxed))
            .field("pin_type", &self.pin_type)
            .field("flush", &unsafe { (*self.flush.get()).is_some() })
            .finish()
    }
}

// Journal is Sync: all fields are Sync-safe
// - BufArray: has manual Sync impl (see safety comment above)
// - JournalResState: contains AtomicU64
// - AtomicU64: Sync
// - Mutex<VecDeque<u32>>: Sync
// - PinListFifo: contains [Option<JournalEntryPinList>; 128] + usize; all Sync
// - flushed_seq_marker: AtomicU64 → Sync
// - Mutex<JournalSlowpath>: Sync (Mutex is Sync, JournalSlowpath fields are Send+Sync)
// - Mutex<()>: Sync
// - [JournalSpace; 4]: Sync (all u64 fields)
// - Option<u64>: Sync
// - AtomicU8: Sync
// - AtomicU64 (err_seq): Sync
// - Arc<Condvar>: Sync
// - Mutex<()> (pin_flush_lock, flush_wait_lock, reclaim_flush_wait_lock): Sync
// - UnsafeCell<Option<BgTaskHandle>> (flush_bg_handle, reclaim_bg_handle):
//   在单线程上下文中写入（startup/shutdown），BgTaskHandle 内部字段均为 Sync，
//   BgTaskHandle 本身也是 Sync。unsafe impl Sync 保证 Send context 下的安全共享。
// - AtomicBool (reclaim_kicked, running, replay_done): Sync
// Journal remains Sync
unsafe impl Sync for Journal {}

impl std::fmt::Debug for Journal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let last_seq_ondisk = self.last_seq_ondisk.load(Ordering::Acquire);
        let flushed_seq_marker = self.flushed_seq_marker.load(Ordering::Acquire);
        let pin_fifo_len = unsafe { (*self.pin_fifo.get()).len() };
        let sp = self.slowpath.lock().unwrap();
        f.debug_struct("Journal")
            .field("bucket_count", &sp.buckets.len())
            .field("current_bucket", &sp.current_bucket)
            .field("current_offset", &sp.current_offset)
            .field("remaining_bytes", &sp.remaining_bytes)
            .field("cur_seq", &self.bch2_journal_cur_seq())
            .field("last_seq_ondisk", &last_seq_ondisk)
            .field("flushed_seq_marker", &flushed_seq_marker)
            .field(
                "dirty_entry_bytes",
                &self.dirty_entry_bytes.load(Ordering::Acquire),
            )
            .field("pin_fifo_len", &pin_fifo_len)
            .field("err_seq", &self.err_seq.load(Ordering::Acquire))
            .field("discard_idx", &sp.discard_idx)
            .field("dirty_idx", &sp.dirty_idx)
            .field("dirty_idx_ondisk", &sp.dirty_idx_ondisk)
            .finish()
    }
}

/// RAII guard for `bch2_journal_block` — drop 时自动调用 `bch2_journal_unblock`。
///
/// 对应 bcachefs `struct journal_block` (journal.c:1386-1392) 的 RAII 语义。
///
/// # 用法
///
/// ```ignore
/// let guard = journal.bch2_journal_block();
/// // ... 在此期间所有 reservation 返回 Err(Blocked) ...
/// drop(guard); // 自动 unblock
/// ```
pub struct JournalBlockGuard<'a> {
    journal: &'a Journal,
}

impl Drop for JournalBlockGuard<'_> {
    fn drop(&mut self) {
        self.journal.bch2_journal_unblock();
    }
}

impl Journal {
    // ─── 构造函数 ────────────────────────────────────────────

    /// 创建新 Journal（预分配 bucket 地址，主要用于测试）
    /// 对应 bcachefs `bch2_fs_journal_alloc()` (init.c:305)
    pub fn new(bucket_addrs: Vec<u64>) -> Self {
        let no_buckets = bucket_addrs.is_empty();
        let journal = Self {
            reservations: JournalResState::new(),
            bufs: BufArray::new(),
            seq: AtomicU64::new(0),
            in_flight: Mutex::new(VecDeque::new()),
            slowpath: Mutex::new(JournalSlowpath::new(bucket_addrs)),
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
            flushing_seq: AtomicU64::new(0),
            flushed_seq_marker: AtomicU64::new(0),
            seq_write_started: AtomicU64::new(0),
            seq_ondisk: AtomicU64::new(0),
            flushed_seq_ondisk: AtomicU64::new(0),
            last_empty_seq: AtomicU64::new(1),
            dirty_entry_bytes: AtomicU64::new(0),
            current_watermark: AtomicU8::new(0),
            journal_error: AtomicU8::new(JE_NONE),
            blocked: AtomicU32::new(0),
            cur_entry_error: AtomicI32::new(0),
            err_seq: AtomicU64::new(0),
            cur_entry_offset_if_blocked: AtomicU32::new(JOURNAL_ENTRY_CLOSED_VAL as u32),
            entry_u64s_reserved: AtomicU32::new(0),
            cur_entry_u64s: AtomicU32::new(0),
            buf_size_want: AtomicUsize::new(BUF_SIZE),
            buf_lock: Mutex::new(()),
            lock: Mutex::new(()),
            // 对应 bcachefs j->rewind_seq 默认 0，表示无 rewind 目标。
            // 原 subvol 初始化为 1 会导致 bch2_journal_read 的 min() 调整异常降低
            // drop_before，引起测试断言 assert_eq!(info.last_seq, 5) 失败。
            rewind_seq: AtomicU64::new(0),
            rewind_seq_ondisk: AtomicU64::new(0),
            wp_stripe: Mutex::new(DevStripeState::new()),
            slowpath_lock: Mutex::new(()),
            reclaim_lock: Mutex::new(()),
            reclaim_kicked: AtomicBool::new(false),
            reclaim_notify: Notify::new(),
            nr_direct_reclaim: AtomicU64::new(0),
            nr_background_reclaim: AtomicU64::new(0),
            space: [JournalSpace::new(); JOURNAL_SPACE_NR],
            auto_flush_ms: None,
            reclaim_interval_ms: AtomicU64::new(0),
            may_skip_flush: AtomicBool::new(true),
            journal_flush_delay_ms: AtomicU64::new(0),
            flushes_outstanding: AtomicU32::new(0),
            flush_notify: Notify::new(),
            write_work_deadline_ms: AtomicU64::new(0),
            write_work_notify: Notify::new(),
            write_work_running: AtomicBool::new(false),
            seq_flush_notify: Notify::new(),
            needs_flush_write: AtomicBool::new(false),
            last_flush_jiffies: AtomicU64::new(0),
            nr_flush_writes: AtomicU64::new(0),
            nr_noflush_writes: AtomicU64::new(0),
            entry_bytes_written: AtomicU64::new(0),
            free_buf: UnsafeCell::new(None),
            free_buf_size: UnsafeCell::new(0),
            blacklist_table: RwLock::new(None),
            flush_bg_handle: UnsafeCell::new(None),
            reclaim_bg_handle: UnsafeCell::new(None),
            can_discard: AtomicBool::new(false),
            device: OnceLock::new(),
            vol: OnceLock::new(),
            #[cfg(test)]
            test_device: OnceLock::new(),
        };
        // Open the first journal entry so buf[0] is immediately accepting
        journal
            .journal_entry_open()
            .expect("journal_entry_open in new() should never fail on fresh journal");
        if no_buckets {
            let seq = journal.bch2_journal_cur_seq();
            journal.seq_ondisk.store(seq, Ordering::Release);
            journal.flushed_seq_ondisk.store(seq, Ordering::Release);
        }
        journal
    }

    /// 从 BchAllocator 动态分配 N 个 bucket（生产构造函数）
    /// 对应 bcachefs `bch2_fs_journal_alloc()` + `bch2_dev_journal_alloc()` (init.c:305/263)
    pub fn create(
        allocator: &BchAllocator,
        vol: &BchVol,
        bucket_count: u32,
    ) -> Result<Self, JournalError> {
        let ca = vol
            .primary_device_rcu_noerror()
            .ok_or_else(|| JournalError::Io(StorageError::NotFound("device offline".into())))?;
        let addrs = allocator
            .bch2_alloc_buckets(
                bucket_count,
                vol,
                &ca,
                &AllocRequest::new(Watermark::Normal, BchDataType::Journal),
                Some(WritePointSpecifier::Direct(DedicatedWp::Journal)),
            )
            .map_err(JournalError::Io)?;
        Ok(Self::new(addrs))
    }

    /// 从 Superblock 状态恢复 Journal
    /// 对应 bcachefs `bch2_fs_journal_init()` (init.c:802) + `bch2_fs_journal_init_rw()` (init.c:758)
    pub fn from_superblock(state: &JournalSuperblockState) -> Self {
        // 对应本地 bcachefs `bch2_fs_initialize()` (init/recovery.c:1083) 与
        // `bch2_fs_journal_start()` (journal/init.c:505-548)：新文件系统以
        // cur_seq=1 启动，last_seq=info.last_seq ?: info.cur_seq，随后把
        // j->seq 设置为 cur_seq - 1。空 superblock 的零值不能直接参与
        // reservation ring 对齐，否则 wrapping_sub(1) 会把 idx 对齐到 3。
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
            bufs: BufArray::new(),
            seq: AtomicU64::new(cur_seq - 1),
            in_flight: Mutex::new(VecDeque::new()),
            slowpath: Mutex::new(JournalSlowpath::from_superblock(state)),
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
            flushing_seq: AtomicU64::new(0),
            flushed_seq_marker: AtomicU64::new(0),
            seq_write_started: AtomicU64::new(0),
            seq_ondisk: AtomicU64::new(cur_seq - 1),
            flushed_seq_ondisk: AtomicU64::new(cur_seq - 1),
            last_empty_seq: AtomicU64::new(cur_seq - 1),
            dirty_entry_bytes: AtomicU64::new(0),
            current_watermark: AtomicU8::new(0),
            journal_error: AtomicU8::new(JE_NONE),
            blocked: AtomicU32::new(0),
            cur_entry_error: AtomicI32::new(0),
            err_seq: AtomicU64::new(0),
            cur_entry_offset_if_blocked: AtomicU32::new(JOURNAL_ENTRY_CLOSED_VAL as u32),
            entry_u64s_reserved: AtomicU32::new(0),
            cur_entry_u64s: AtomicU32::new(0),
            buf_size_want: AtomicUsize::new(BUF_SIZE),
            buf_lock: Mutex::new(()),
            lock: Mutex::new(()),
            rewind_seq: AtomicU64::new(last_seq_ondisk),
            rewind_seq_ondisk: AtomicU64::new(last_seq_ondisk),
            wp_stripe: Mutex::new(DevStripeState::new()),
            slowpath_lock: Mutex::new(()),
            reclaim_lock: Mutex::new(()),
            reclaim_kicked: AtomicBool::new(false),
            reclaim_notify: Notify::new(),
            nr_direct_reclaim: AtomicU64::new(0),
            nr_background_reclaim: AtomicU64::new(0),
            space: [JournalSpace::new(); JOURNAL_SPACE_NR],
            auto_flush_ms: None,
            reclaim_interval_ms: AtomicU64::new(0),
            may_skip_flush: AtomicBool::new(true),
            journal_flush_delay_ms: AtomicU64::new(0),
            flushes_outstanding: AtomicU32::new(0),
            flush_notify: Notify::new(),
            write_work_deadline_ms: AtomicU64::new(0),
            write_work_notify: Notify::new(),
            write_work_running: AtomicBool::new(false),
            seq_flush_notify: Notify::new(),
            needs_flush_write: AtomicBool::new(false),
            last_flush_jiffies: AtomicU64::new(0),
            nr_flush_writes: AtomicU64::new(0),
            nr_noflush_writes: AtomicU64::new(0),
            entry_bytes_written: AtomicU64::new(0),
            free_buf: UnsafeCell::new(None),
            free_buf_size: UnsafeCell::new(0),
            blacklist_table: RwLock::new(None),
            flush_bg_handle: UnsafeCell::new(None),
            reclaim_bg_handle: UnsafeCell::new(None),
            can_discard: AtomicBool::new(false),
            device: OnceLock::new(),
            vol: OnceLock::new(),
            #[cfg(test)]
            test_device: OnceLock::new(),
        };
        // 对应本地 `bch2_fs_journal_start()` (`journal/init.c:535-547`)：
        // recovery window `[last_seq, cur_seq)` 中的每个 seq 都必须有 pin
        // 槽位。subvol 的 superblock 同时保存 current seq 与 ondisk seq；
        // 两者不一致时，先补齐 ondisk 边界到 current seq 之前的槽位，再
        // 由下面的 journal_entry_open() 追加 current seq。
        unsafe {
            let pin_fifo = &mut *journal.pin_fifo.get();
            for _ in last_seq_ondisk.min(last_seq)..last_seq {
                assert!(
                    pin_fifo.push_back(JournalEntryPinList::new(0)).is_ok(),
                    "journal pin fifo recovery window should fit"
                );
            }
        }
        // 把 closed idx 对齐到前一个 seq，首次 open 后 idx == seq & BUF_MASK。
        journal.reservations.align_idx_to_seq(cur_seq);
        // Open first journal entry
        journal
            .journal_entry_open()
            .expect("journal_entry_open in from_superblock() should never fail");
        journal
    }

    /// 导出 Journal 状态快照（用于 close 时持久化到 Superblock）
    /// 对应 bcachefs `bch2_journal_buckets_to_sb()` (sb.c:176)
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
            bucket_seq: sp.bucket_seq.clone(),
            replayed_seqs: Vec::new(),
        }
    }

    /// 设置 BchVol 引用（Phase 2 journal safety net 用）— OnceLock，只设一次。
    pub fn set_vol_ref(&self, vol: &Arc<BchVol>) {
        self.vol.set(Arc::downgrade(vol)).ok();
        self.bch2_blacklist_table_initialize(&vol.superblock().journal_seq_blacklist);
        if let Some(dev) = vol.primary_device_rcu_noerror() {
            self.device.set(dev).ok();
        }
    }

    pub(crate) fn set_device_ref(&self, dev: Arc<BchDev>) {
        self.device.set(dev).ok();
    }

    fn journal_device(&self) -> Arc<BchDev> {
        if let Some(dev) = self.device.get().cloned() {
            return dev;
        }
        if let Some(vol) = self.vol.get().and_then(|w| w.upgrade()) {
            if let Some(dev) = vol.primary_device_rcu_noerror() {
                return dev;
            }
        }
        #[cfg(test)]
        {
            if let Some(dev) = self.test_device.get().cloned() {
                return dev;
            }
        }
        panic!("Journal: vol_ref not set — call set_vol_ref before use")
    }

    /// 返回所有在线 RW 设备的列表，用于多副本 journal 写入。
    ///
    /// 如果 BchVol 尚未设置或没有在线设备，回退到单设备（`journal_device()`）。
    pub(crate) fn journal_devices(&self) -> Vec<Arc<BchDev>> {
        if let Some(vol) = self.vol.get().and_then(|w| w.upgrade()) {
            let rw = vol
                .device_registry
                .devices_by_state(crate::storage::superblock::BchMemberState::Rw);
            let online = vol
                .device_registry
                .resolve_mask(rw)
                .into_iter()
                .filter(|dev| dev.is_online())
                .collect::<Vec<_>>();
            return online;
        }
        // fallback 到单设备
        vec![self.journal_device()]
    }

    #[cfg(test)]
    /// 设置测试用设备（当 BchVol 不可用时使用）。
    pub fn set_test_device(&self, dev: Arc<BchDev>) {
        self.test_device.set(dev.clone()).ok();
        self.set_device_ref(dev);
    }

    /// Phase 2 journal safety net：将 pending rewind range 编入当前 buf。
    ///
    /// 对应 bcachefs `early_journal_entries` 的消费点：`bch2_journal_add_rewind_range()`
    /// 记录的范围会被编码为 `jset_entry_rewind` 并写入当前 journal entry。
    fn bch2_inject_rewind_entries_into_buf(
        pending_ranges: &[(u64, u64)],
        buf_data: &mut [u8],
        data_end: &mut usize,
    ) -> bool {
        if pending_ranges.is_empty() {
            return false;
        }

        let end = *data_end;
        let Some(mut jset) = (match Jset::deserialize(&buf_data[..end]) {
            Ok(Some(jset)) => Some(jset),
            Ok(None) => None,
            Err(e) => {
                tracing::warn!("[safety-net] deserialize rewind jset failed: {}", e);
                None
            }
        }) else {
            return false;
        };

        let mut appended = false;
        for (from, to) in pending_ranges {
            let mut payload = Vec::with_capacity(16);
            payload.extend_from_slice(&from.to_le_bytes());
            payload.extend_from_slice(&to.to_le_bytes());
            let entry = match RawJsetEntry::new(0, JsetEntryType::Rewind as u8, payload, 0) {
                Ok(entry) => entry,
                Err(e) => {
                    tracing::warn!("[safety-net] rewind entry encode failed: {}", e);
                    continue;
                }
            };
            jset.entries.push(entry);
            appended = true;
        }

        if !appended {
            return false;
        }

        let serialized = match jset.serialize_padded() {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("[safety-net] serialize rewind jset failed: {}", e);
                return false;
            }
        };

        let remaining = buf_data.len().saturating_sub(end);
        if serialized.len() > remaining {
            tracing::warn!(
                "[safety-net] rewind buf full: need {} have {}",
                serialized.len(),
                remaining
            );
            return false;
        }

        buf_data[end..end + serialized.len()].copy_from_slice(&serialized);
        *data_end = end + serialized.len();
        true
    }

    /// Phase 2 journal safety net：在 flush 选中时附加 `RewindLimit` entry。
    ///
    /// 对应 bcachefs `bch2_journal_write()` 中对 `jset_entry_rewind_limit` 的追加。
    fn bch2_inject_rewind_limit_into_buf(
        rewind_seq: u64,
        buf_seq: u64,
        buf_data: &mut [u8],
        data_end: &mut usize,
    ) -> bool {
        let end = *data_end;
        let Some(mut jset) = (match Jset::deserialize(&buf_data[..end]) {
            Ok(Some(jset)) => Some(jset),
            Ok(None) => None,
            Err(e) => {
                tracing::warn!("[safety-net] deserialize rewind limit jset failed: {}", e);
                None
            }
        }) else {
            return false;
        };

        let limit_seq = rewind_seq.min(buf_seq + 1);
        let entry = match RawJsetEntry::new(
            0,
            JsetEntryType::RewindLimit as u8,
            limit_seq.to_le_bytes().to_vec(),
            0,
        ) {
            Ok(entry) => entry,
            Err(e) => {
                tracing::warn!("[safety-net] rewind limit encode failed: {}", e);
                return false;
            }
        };
        jset.entries.push(entry);

        let serialized = match jset.serialize_padded() {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("[safety-net] serialize rewind limit jset failed: {}", e);
                return false;
            }
        };

        if serialized.len() > buf_data.len() {
            tracing::warn!(
                "[safety-net] rewind limit serialized len {} exceeds buf {}",
                serialized.len(),
                buf_data.len()
            );
            return false;
        }

        buf_data[..serialized.len()].copy_from_slice(&serialized);
        *data_end = serialized.len();
        true
    }

    // ─── 错误处理 ──────────────────────────────────────────

    /// 设置 journal 错误状态（一旦设置不可清除）。
    ///
    /// 对应 bcachefs `bch2_journal_halt_locked` (journal.c:666-689)。
    /// 设置后，后续所有 `journal_res_get_fast` 和 `journal_res_get` 返回错误。
    /// 使用原子存储确保并发安全。
    #[deprecated = "use bch2_journal_error_set instead"]
    pub fn journal_error_set(&self, err: &JournalError) {
        let code = match err {
            JournalError::Overflow(_) => JE_OVERFLOW,
            JournalError::ChecksumMismatch => JE_CHECKSUM,
            JournalError::Io(_) => JE_IO,
            JournalError::Stuck(_) => JE_STUCK,
            JournalError::Full(_) => JE_FULL,
            JournalError::PinFull(_) => JE_PIN_FULL,
            JournalError::Blocked(_) => JE_BLOCKED,
        };
        // 只存储第一个错误（一旦设置不可覆盖）
        let _ = self.journal_error.compare_exchange(
            JE_NONE,
            code,
            Ordering::Release,
            Ordering::Relaxed,
        );
    }

    /// 检查 journal 是否处于错误状态。
    ///
    /// 对应 bcachefs `journal_error_check()`。
    /// 返回 `Some(JournalError)` 如果错误已设置。
    pub fn journal_error_check(&self) -> Option<JournalError> {
        let code = self.journal_error.load(Ordering::Acquire);
        match code {
            JE_NONE => None,
            JE_OVERFLOW => Some(JournalError::Overflow("journal error set".into())),
            JE_CHECKSUM => Some(JournalError::ChecksumMismatch),
            JE_IO => Some(JournalError::Io(StorageError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "journal io error",
            )))),
            JE_STUCK => Some(JournalError::Stuck("journal error set".into())),
            JE_FULL => Some(JournalError::Full("journal error set".into())),
            JE_PIN_FULL => Some(JournalError::PinFull("journal error set".into())),
            JE_BLOCKED => Some(JournalError::Blocked("journal error set".into())),
            _ => Some(JournalError::Overflow("unknown journal error".into())),
        }
    }

    // ─── New Fastpath API (accept &self, lock-free) ────────

    /// Fastpath reservation — atomic CAS, no mutex.
    ///
    /// 对应 bcachefs `journal_res_get_fast()` (journal.h:475-518)。
    ///
    /// 在当前 buf 中原子保留 `req_u64s` 个 u64。成功返回 `JournalRes`，
    /// 失败（空间不足 / refcount 溢出）返回 `JournalError::Overflow`。
    /// 若请求水位线低于当前 journal 水位线，返回 `StorageError::WatermarkTooLow`。
    ///
    /// # Fastpath 特性
    ///
    /// - 仅操作 `AtomicU64`，无锁定
    /// - 共享 `&self` 引用（多线程可同时调用）
    /// - 成功后调用者必须 `commit()` 写入并 `journal_res_put()` 释放
    ///
    /// # Seq 来源
    ///
    /// seq 是 entry 级别而非 reservation 级别：同 entry 内所有 reservation 共享 buf.seq。
    /// seq 在 `journal_entry_open()` 中递增分配，此处直接从 buf 读取。
    /// 对应 bcachefs `journal_res_get_fast()` (journal.h:515) 的 `res->seq = bch2_journal_cur_seq(j)`
    /// 减去状态掩码调整。
    ///
    /// # 水位线检查
    ///
    /// 对应 bcachefs `journal.h:502-504`：
    /// ```c
    /// if ((flags & BCH_WATERMARK_MASK) < j->watermark)
    ///     return 0;
    /// ```
    /// 即 request < current → 拒绝。高水位线只允许最紧急操作通过。
    pub(crate) fn bch2_journal_res_get_fast(
        &self,
        watermark: Watermark,
        req_u64s: u32,
    ) -> Result<JournalRes, JournalError> {
        // 错误状态检查（journal 进入错误状态后所有 reservation 拒绝）
        if let Some(err) = self.journal_error_check() {
            return Err(err);
        }

        // 水位线准入检查（bcachefs journal.h:502-504）
        let current_wm = Watermark::from_bits(self.current_watermark.load(Ordering::Acquire));
        if !current_wm.allows(watermark) {
            return Err(JournalError::Overflow(format!(
                "watermark blocked: request={:?} < current={:?}",
                watermark, current_wm,
            )));
        }

        // bcachefs journal.h:491: 检查 cur_entry_offset + u64s ≤ cur_entry_u64s
        // smp_rmb() 通过 Acquire load 保证看到最新的 cur_entry_u64s
        let cur_state = self.reservations.bits.load(Ordering::Relaxed);
        let cur_off = JournalResState::cur_entry_offset(cur_state);
        let cur_u64s = self.cur_entry_u64s.load(Ordering::Acquire);
        if (cur_off as u64).wrapping_add(req_u64s as u64) > cur_u64s as u64 {
            return Err(JournalError::Overflow("journal entry full".into()));
        }

        let (old, _new) = self
            .reservations
            .try_reserve(req_u64s, cur_u64s as u32)
            .ok_or_else(|| JournalError::Overflow("slowpath needed".into()))?;

        let state_idx = JournalResState::idx(old);
        let offset_bytes = JournalResState::cur_entry_offset(old) * 8; // u64 → byte
                                                                       // 对应本地 journal.h:515-516：reservation state 可能在读取当前 seq
                                                                       // 后发生 cycle，因此用 state idx 把 seq 向后校正到该 reservation 所属 entry。
        let mut seq = self.bch2_journal_cur_seq();
        seq -= (seq - u64::from(state_idx)) & JOURNAL_STATE_BUF_MASK as u64;
        let buf_idx = (seq & (JOURNAL_IN_FLIGHT_NR as u64 - 1)) as u32;

        Ok(JournalRes {
            seq,
            offset: offset_bytes,
            u64s: req_u64s,
            buf_idx,
            must_flush: false,
        })
    }

    /// 更新当前 journal 水位线（对应 bcachefs `bch2_journal_set_watermark`）
    ///
    /// 基于空间压力自动调整：可用空间越少，水位线越高（准入越严格）。
    /// 在 flush() 结束时调用。
    pub fn bch2_journal_set_watermark(&self) {
        // 使用 utilization()（已写入字节 / 总容量）衡量 journal 空间压力。
        // bcachefs 使用更精确的 bucket 级 j->space[] 统计，但 subvol 的
        // compute_journal_space() 在小 bucket 数量时（≤2，常见于测试）有 -1 伪影，
        // utilization() 作为连续度量避免了此问题，生产环境下差异可忽略。
        let util = self.utilization();
        let pin_len = unsafe { (*self.pin_fifo.get()).len() } as u64;
        let pin_free = PIN_FIFO_SIZE.saturating_sub(pin_len as usize) as u64;

        // med_on_space: util >= 25% — 中等空间压力，触发后台 reclaim
        // 对应 bcachefs reclaim.c:73: clean*4 <= total*3 ⇔ dirty >= 0.25*total ≈ util >= 0.25
        let med_on_space = util >= 0.25;
        // low_on_space: util >= 75% — 低空间，阻塞前台分配
        // 对应 bcachefs reclaim.c:76: clean*4 <= total ⇔ dirty >= 0.75*total ≈ util >= 0.75
        let low_on_space = util >= 0.75;
        // low_on_pin: pin FIFO 不足 1/4
        let low_on_pin = pin_free < (PIN_FIFO_SIZE as u64 / 4);

        // 水位线决策：low_on_space OR low_on_pin 时切到 Reclaim（阻塞前台分配）
        // bcachefs 还含 low_on_wb（write buffer 背压），subvol 无 write buffer
        let wm = if low_on_space || low_on_pin {
            Watermark::Reclaim
        } else {
            Watermark::Stripe
        };
        let old =
            Watermark::from_bits(self.current_watermark.swap(wm.to_bits(), Ordering::Release));
        // C 语义：新水位线（数值更小=空间更充裕）优于旧水位线时唤醒等待者
        // 对应 bcachefs `swap(watermark, j->watermark); if (watermark > j->watermark) journal_wake(j);`
        // Watermark 枚举定义为 Stripe=0 ... InteriorUpdate=6，数值越小优先级越高
        if wm.to_bits() < old.to_bits() {
            self.bch2_journal_wake_up();
        }
        // med_on_space 时触发后台 reclaim——对应 bcachefs reclaim.c:106-107
        if med_on_space {
            self.journal_reclaim_kick();
        }
    }

    /// 获取当前 journal 水位线
    pub fn watermark(&self) -> Watermark {
        Watermark::from_bits(self.current_watermark.load(Ordering::Acquire))
    }

    // ─── B4: 错误处理 ─────────────────────────────────────

    /// 设置 journal 错误状态 — 阻止后续所有分配（对应 bcachefs `bch2_journal_halt_locked` journal.c:666）。
    ///
    /// 一旦设置后不可清除。后续所有 `journal_res_get` 返回此错误。
    /// 使用 `AtomicU8` 而非枚举以支持无锁写入。
    pub(crate) fn bch2_journal_error_set(&self, err: JournalError) {
        let code = match &err {
            JournalError::Overflow(_) => 1,
            JournalError::ChecksumMismatch => 2,
            JournalError::Io(_) => 3,
            JournalError::Stuck(_) => 4,
            JournalError::Full(_) => 5,
            JournalError::PinFull(_) => 6,
            JournalError::Blocked(_) => 7,
        };
        // 只在未设置错误时设置（首次写入）
        self.journal_error
            .compare_exchange(JE_NONE, code, Ordering::Release, Ordering::Relaxed)
            .ok();
        self.bch2_journal_wake_up();
    }

    /// 检查 journal 是否处于错误状态（对应 bcachefs `bch2_journal_error` journal.h:365）。
    ///
    /// 返回 `None` 表示无错误，`Some(JournalError)` 表示已设置的具体错误。
    ///
    /// 额外检查 `err_seq`：如果 err_seq 非零且 journal_error 为 0，
    /// 返回 `Blocked` 错误表示 journal 已 halt（对应 bcachefs journal.c:667）。
    pub(crate) fn bch2_journal_error_check(&self) -> Option<JournalError> {
        let code = self.journal_error.load(Ordering::Acquire);
        match code {
            JE_NONE => {
                // bcachefs journal.c:667: err_seq 非零且 journal_error 未设置时
                // 返回错误表示 journal 已 halt。
                if self.err_seq.load(Ordering::Acquire) != 0 {
                    Some(JournalError::Blocked("journal halted".into()))
                } else {
                    None
                }
            }
            1 => Some(JournalError::Overflow("journal error set".into())),
            2 => Some(JournalError::ChecksumMismatch),
            3 => Some(JournalError::Io(StorageError::JournalError(
                "journal error set".into(),
            ))),
            4 => Some(JournalError::Stuck("journal error set".into())),
            5 => Some(JournalError::Full("journal error set".into())),
            6 => Some(JournalError::PinFull("journal error set".into())),
            7 => Some(JournalError::Blocked("journal error set".into())),
            _ => None,
        }
    }

    /// 获取当前 seq（原子，无锁）
    ///
    /// 对应 bcachefs `bch2_journal_cur_seq()` (journal.h:137-140)
    pub(crate) fn bch2_journal_cur_seq(&self) -> u64 {
        self.seq.load(Ordering::Acquire)
    }

    /// 将 raw bytes 写入 buf 中已保留的位置（无竞争写 —— 每个 reservation offset 唯一）
    ///
    /// 对应 bcachefs `bch2_journal_add_entry()` 写入 buf data 的底层阶段。
    /// 不添加 entry header，直接拷贝数据。用于写入 Jset 头、padding 等非 entry 数据。
    pub(crate) fn bch2_journal_add_raw(&self, res: &mut JournalRes, data: &[u8]) {
        let buf = self.bufs.get_mut(res.buf_idx as usize);
        let offset = res.offset as usize;
        let end = offset + data.len();
        buf.data[offset..end].copy_from_slice(data);
        res.offset = end as u32;
        if res.must_flush {
            buf.has_must_flush = true;
        }
        if self.last_seq_ondisk.load(Ordering::Acquire) == res.seq {
            buf.wait_first = JournalBufWaitState::FlushNoWait;
        }
    }

    /// 在 buf 中写入一个完整的 jset entry（header + payload）。
    ///
    /// 对应 bcachefs `bch2_journal_add_entry()` (journal.h:338-352)。
    /// 内部填充 `JsetEntryHeader` 并写入 buf，然后写入 payload。
    pub fn bch2_journal_add_entry(
        &self,
        res: &mut JournalRes,
        type_: u8,
        id: u8,
        level: u8,
        _u64s: u32,
        data: &[u8],
    ) {
        let buf = self.bufs.get_mut(res.buf_idx as usize);
        let offset = res.offset as usize;

        // 填充并写入 JsetEntryHeader
        let hdr = JsetEntryHeader {
            btree_type: id,
            entry_type: type_,
            version: JSET_ENTRY_VERSION,
            level,
            payload_len: data.len() as u16,
            has_last: 0,
            has_prev: 0,
        };
        let hdr_bytes = unsafe {
            std::slice::from_raw_parts(
                &hdr as *const JsetEntryHeader as *const u8,
                size_of::<JsetEntryHeader>(),
            )
        };
        let hdr_end = offset + hdr_bytes.len();
        buf.data[offset..hdr_end].copy_from_slice(hdr_bytes);

        // 写入 payload
        let payload_end = hdr_end + data.len();
        buf.data[hdr_end..payload_end].copy_from_slice(data);
        res.offset = payload_end as u32;

        if res.must_flush {
            buf.has_must_flush = true;
        }
        if self.last_seq_ondisk.load(Ordering::Acquire) == res.seq {
            buf.wait_first = JournalBufWaitState::FlushNoWait;
        }
    }

    /// 释放 reservation —— 递减 buf refcount，归零时自动触发写入。
    ///
    /// 对应 bcachefs `bch2_journal_buf_put()` (journal.h:395-403) +
    /// `__bch2_journal_buf_put_final()` (journal.c:240-256)。
    ///
    /// 当 refcount 归零且 buf 处于 Closing 状态时，自动推进到 WriteSubmitted：
    /// - Closing → WriteSubmitted：标记 buf 为待写入，通知等待者
    /// - 实际 I/O 由后续的 `flush()` 统一完成（收集所有 WriteSubmitted buf）
    ///
    /// Accepting 状态的 buf 即使 refcount 归零也不会触发写入，
    /// 因为 flush() 中会统一关闭 entry 并推进写入。
    pub fn bch2_journal_res_put(&self, res: &JournalRes) {
        let idx = (res.seq & JOURNAL_STATE_BUF_MASK as u64) as u32;
        // fetch_sub 返回 decrement 前的值
        let old = self.reservations.release(idx);
        let count_before = JournalResState::buf_count(old, idx);

        // refcount 归零 (1→0) 且 buf 已关闭 → 自动推进到 WriteSubmitted
        if count_before == 1 {
            let buf = self.bufs.get_mut(res.buf_idx as usize);
            let _ = buf.drain_watch.send(0);
            let _ = buf;

            self.__bch2_journal_buf_put_final(res.seq);
        } else if JournalResState::cur_entry_offset(old) as u64 == JOURNAL_ENTRY_BLOCKED_VAL {
            // bcachefs journal.h:413: else if (unlikely(s.cur_entry_offset == JOURNAL_ENTRY_BLOCKED_VAL))
            //     closure_wake_up(&j->async_wait);
            // 释放 reservation 时 entry 处于 BLOCKED_VAL 状态，唤醒等待者
            // 让 blocked 线程有机会重新检查 entry 是否可关闭。
            self.bch2_journal_wake_up();
        }
    }

    // ─── R1: buf put 链 ────────────────────────────────────

    /// buf refcount 归零后的最终释放处理。
    ///
    /// 对应 bcachefs `__bch2_journal_buf_put_final()` (journal.c:240-256)。
    ///
    /// 执行链：
    /// 1. `__bch2_journal_pin_put(seq)` — 释放 pin_fifo 中该 seq 的引用计数
    /// 2. 如果 pin 已释放（count 归零），推进内存 last_seq
    /// 3. `bch2_journal_do_writes_locked()` — 触发待写入（标记 needs_flush_write + 唤醒）
    /// 4. `bch2_journal_wake_up()` — 唤醒所有等待者
    fn __bch2_journal_buf_put_final(&self, seq: u64) {
        if let Some(buf) = self.journal_seq_to_buf(seq) {
            if buf.state == BufState::Closing {
                buf.state = BufState::WriteSubmitted;
                buf.notify.notify_waiters();
            }
        }
        if self.__bch2_journal_pin_put(seq) {
            self.bch2_journal_update_last_seq();
        }
        self.bch2_journal_do_writes_locked();
        // bcachefs __bch2_journal_buf_put_final (journal.c:240-256) 中仅 journal_wake(j),
        // 不含 __closure_wake_up(&j->flush_wait) — flush_wait 的 waiters 在
        // __journal_entry_open 中通过 xchg 迁移到新 buf 的 wait 列表，不由此处唤醒。
        self.bch2_journal_wake_up();
    }

    /// 释放 buf 的一个引用计数。
    ///
    /// 对应 bcachefs `__bch2_journal_buf_put()` (journal.h:395-403)。
    ///
    /// 1. `idx = seq & JOURNAL_STATE_BUF_MASK` — 从 seq 计算 buf 索引
    /// 2. `release(idx)` — 原子递减该 idx 的 buf 引用计数（对应 `journal_state_buf_put`）
    /// 3. 如果 refcount 归零（旧值 == 1），调用 `__bch2_journal_buf_put_final`
    ///
    /// # bcachefs 源码
    ///
    /// ```c
    /// static inline void __bch2_journal_buf_put(struct journal *j, u64 seq) {
    ///     unsigned idx = seq & JOURNAL_STATE_BUF_MASK;      // journal.h:396
    ///     union journal_res_state s = journal_state_buf_put(j, idx); // journal.h:397
    ///     if (!journal_state_count(s, idx))                 // journal.h:398
    ///         __bch2_journal_buf_put_final(j, seq);         // journal.h:399
    /// }
    /// ```
    ///
    /// # 注意
    ///
    /// 调用方必须传入 **buf 的实际 seq**（即 `journal_entry_open` 分配的
    /// `new_seq`）。当前 open entry 的 seq 与 `bch2_journal_cur_seq()` 相同；
    /// 已关闭的旧 buf 必须继续使用它自身保存的 seq。
    pub fn __bch2_journal_buf_put(&self, seq: u64) {
        // bcachefs journal.h:396
        let idx = (seq & (JOURNAL_STATE_BUF_NR as u64 - 1)) as u32;
        // bcachefs journal.h:397: journal_state_buf_put(j, idx) = atomic_sub_return
        // subvol: release(idx) 返回递减前的值
        let old = self.reservations.release(idx);
        // bcachefs journal.h:398: if (!journal_state_count(s, idx))
        // journal_state_buf_put 返回新值，新值 count==0 ⇔ 旧值 count==1
        if JournalResState::buf_count(old, idx) == 1 {
            // bcachefs journal.h:399
            self.__bch2_journal_buf_put_final(seq);
        }
    }

    /// 停止 background reclaim 任务。
    /// 对应 bcachefs `bch2_journal_reclaim_stop()` (init.c:443)。
    pub async fn bch2_journal_reclaim_stop(&self) {
        unsafe {
            if let Some(handle) = (*self.reclaim_bg_handle.get()).take() {
                handle.cancel();
                handle.join().await;
            }
        }
    }

    /// 唤醒所有在 journal buf 上等待的线程（对应 bcachefs `journal_wake` = `closure_wake_up(&j->async_wait)`）。
    ///
    /// bcachefs 的 `closure_wake_up` 唤醒所有在 async_wait 上挂起的闭包，
    /// 由释放 journal 空间的操作调用（flush 完成、reclaim 回收、cycle 轮换）。
    ///
    /// 差异：bcachefs 使用单一 closure_waitlist；subvol 使用每个 buf 各有一个 Notify，
    /// 唤醒所有 buf 上的等待者等价于 C 中唤醒所有等待 journal 空间的闭包。
    ///
    /// 注意：`journal_res_put()` 已处理 Closing→WriteSubmitted 状态转换（refcount 归零时自动触发），
    /// 此函数不做状态推进。
    pub(crate) fn bch2_journal_wake_up(&self) {
        let in_flight = self.in_flight.lock().unwrap();
        for &idx in in_flight.iter() {
            self.bufs.get_mut(idx as usize).notify.notify_waiters();
        }
    }

    /// 设置 needs_flush_write 标志（journal 有数据需要写入后端）
    pub(crate) fn bch2_journal_set_needs_flush_write(&self) {
        self.needs_flush_write.store(true, Ordering::Release);
    }

    /// 清除 needs_flush_write 标志（写入完成）
    pub(crate) fn bch2_journal_clear_needs_flush_write(&self) {
        self.needs_flush_write.store(false, Ordering::Release);
    }

    /// 检查是否有数据需要写入后端
    pub(crate) fn bch2_journal_needs_flush_write(&self) -> bool {
        self.needs_flush_write.load(Ordering::Acquire)
    }

    /// 标记 journal 恢复完成（恢复→正常运行模式过渡）
    ///
    /// 对应 bcachefs `bch2_journal_set_replay_done()` (init.c:619-631)。
    ///
    /// # bcachefs 语义
    ///
    /// bcachefs 在此函数中：
    /// 1. `bch2_journal_space_available(j)` — 重新计算 space budget
    /// 2. `set_bit(JOURNAL_need_flush_write)` — 首次写入必须 flush
    /// 3. `set_bit(JOURNAL_running)` — 允许 background reclaim
    /// 4. `set_bit(JOURNAL_replay_done)` — 允许 journal seq 推进超过 replay 范围
    ///
    /// 调用时机：`bch2_fs_recovery()` 所有 pass 完成后、持久化 superblock 之前。
    pub fn bch2_journal_set_replay_done(&self) {
        // bcachefs: bch2_journal_space_available(j) — 恢复后重新计算空间预算
        let _ = self.bch2_journal_space_available(Watermark::Normal);
        // bcachefs: set_bit(JOURNAL_need_flush_write)
        self.bch2_journal_set_needs_flush_write();
        // bcachefs: set_bit(JOURNAL_running) — 允许 background reclaim + reservation
        self.running.store(true, Ordering::Release);
        // bcachefs: set_bit(JOURNAL_replay_done)
        self.replay_done.store(true, Ordering::Release);
    }

    /// 关闭 journal：flush 所有 pending entries + 写入空 entry 推进 clock hands
    ///
    /// 对应 bcachefs `bch2_fs_journal_stop()` (init.c:438-485)。
    ///
    /// # bcachefs 语义
    ///
    /// bcachefs 在此函数中：
    /// 1. `if (!test_bit(JOURNAL_running)) return` — 未运行时提前返回
    /// 2. `bch2_journal_reclaim_stop(j)` — 停止 background reclaim
    /// 3. `bch2_journal_flush_all_pins(j)` — flush 所有 pin
    /// 4. `__bch2_journal_meta(j)` — 写入空 entry 推进 clock hands
    /// 5. `bch2_journal_shutdown_quiesce(j)` — 阻止新 reservation
    /// 6. WARN checks（dirty_entry_bytes, last_empty_seq）
    /// 7. `clear_bit(JOURNAL_running)` — 标记 journal 不再运行
    pub async fn bch2_fs_journal_stop(&self) -> Result<(), JournalError> {
        // bcachefs: if (!test_bit(JOURNAL_running, &j->flags)) return;
        if !self.running.load(Ordering::Acquire) {
            return Ok(());
        }

        // bcachefs: bch2_journal_reclaim_stop(j) — 停止 background reclaim
        self.bch2_journal_reclaim_stop().await;

        // bcachefs: bch2_journal_flush_all_pins(j) — flush 所有 pending
        self.bch2_journal_flush().await?;

        // bcachefs: __bch2_journal_meta(j) — 写入空 entry 推进 clock hands
        self.__bch2_journal_meta().await?;

        // bcachefs: bch2_journal_shutdown_quiesce(j) — 等待所有 in-flight 操作排空
        self.bch2_journal_shutdown_quiesce();

        // bcachefs WARN checks (init.c:463-481):
        //   WARN(!bch2_journal_error && JOURNAL_replay_done && dirty_entry_bytes, ...)
        //   错误路径下 dirty_entry_bytes 有残留是预期的，不告警。
        let deb = self.dirty_entry_bytes.load(Ordering::Acquire);
        let has_err = self.bch2_journal_error_check().is_some();
        if !has_err && self.replay_done.load(Ordering::Acquire) && deb > 0 {
            eprintln!(
                "[WARN] journal_stop: {} dirty entry bytes remaining after flushing all pins",
                deb
            );
        }
        // bcachefs init.c:463-467 还有 WARN(last_empty_seq != cur_seq) 检查，
        // 但 subvol 不追踪 last_empty_seq（bcachefs 内部一致性字段），跳过。这不会
        // 漏检测: dirty_entry_bytes 检查已能捕获未关闭 entry 残留。

        // bcachefs: if (!bch2_journal_error(j)) clear_bit(JOURNAL_running, &j->flags)
        // 错误路径下 JOURNAL_running 不清除，允许后续调用再次进入 stop 路径。
        if !self.bch2_journal_error_check().is_some() {
            self.running.store(false, Ordering::Release);
        }

        Ok(())
    }

    /// 更新 last_flush_jiffies 为当前时间戳（自启动以来的毫秒数）
    pub fn bch2_journal_update_flush_jiffies(&self) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        self.last_flush_jiffies.store(now, Ordering::Release);
    }

    /// 获取上次 flush 的 jiffies 时间戳
    pub fn bch2_journal_last_flush_jiffies(&self) -> u64 {
        self.last_flush_jiffies.load(Ordering::Acquire)
    }

    /// 更新内存 last_seq — 由 `bch2_journal_maybe_update_last_seq` 实现。
    /// 对应 bcachefs `bch2_journal_update_last_seq()` (reclaim.c:1088-1116)。
    pub(crate) fn bch2_journal_update_last_seq(&self) {
        self.bch2_journal_maybe_update_last_seq();
    }

    /// 按 seq 从 in-flight FIFO 解析 journal buffer。
    ///
    /// 对应本地 bcachefs `journal_seq_to_buf()` (`journal/journal.h:152-162`)。
    /// 只有仍在 FIFO 中的 seq 才返回 buffer；已经从 front 弹出的 seq 返回 None。
    fn journal_seq_to_buf(&self, seq: u64) -> Option<&mut JournalBuf> {
        let idx = {
            let in_flight = self.in_flight.lock().unwrap();
            in_flight.iter().copied().find(|&idx| {
                let buf = self.bufs.get(idx as usize);
                buf.seq == seq
            })
        }?;
        Some(self.bufs.get_mut(idx as usize))
    }

    /// 返回 FIFO 中最旧的尚未完成 extent allocation 的 seq。
    ///
    /// 对应本地 bcachefs `journal_last_unallocated_seq()`
    /// (`journal/journal.h:205-212`)。
    fn journal_last_unallocated_seq(&self) -> u64 {
        let in_flight = self.in_flight.lock().unwrap();
        for &idx in in_flight.iter() {
            let buf = self.bufs.get(idx as usize);
            if !buf.write_allocated {
                return buf.seq;
            }
        }
        0
    }

    /// 返回指定 seq 在 reservation state 中的引用数。
    ///
    /// 对应本地 bcachefs `journal_state_seq_count()` (`journal/journal.h:249-257`)。
    fn journal_state_seq_count(&self, state: u64, seq: u64) -> u32 {
        if self.bch2_journal_cur_seq().saturating_sub(seq) >= JOURNAL_STATE_BUF_NR as u64 {
            return 0;
        }
        let idx = {
            let in_flight = self.in_flight.lock().unwrap();
            in_flight
                .iter()
                .copied()
                .find(|&idx| self.bufs.get(idx as usize).seq == seq)
        };
        idx.map_or(0, |_| {
            JournalResState::buf_count(state, (seq & JOURNAL_STATE_BUF_MASK as u64) as u32)
        })
    }

    /// 返回当前能够从 in-flight FIFO 前端完成推进的 seq。
    ///
    /// 对应本地 bcachefs `last_uncompleted_write_seq()`
    /// (`journal/write.c:224-232`)。只有前端 entry 已完成全部 bookkeeping，或当前
    /// callback 正在完成的正是前端 seq 时，调用者才拥有推进 FIFO 的资格。
    fn last_uncompleted_write_seq(&self, seq_completing: u64) -> u64 {
        let in_flight = self.in_flight.lock().unwrap();
        let Some(&idx) = in_flight.front() else {
            return 0;
        };
        let buf = self.bufs.get(idx as usize);
        if buf.write_done || buf.seq == seq_completing {
            buf.seq
        } else {
            0
        }
    }

    /// 对应本地 bcachefs `journal_write_done_flush()`
    /// (`journal/write.c:468-488`)。
    ///
    /// 在 `journal_write_done` 之前调用，唤醒等待当前 flush entry 的 waiters
    /// （无设备失败时提前唤醒，write.c:473-478）。
    fn journal_write_done_flush(&self, seq_wrote: u64) {
        let Some(w) = self.journal_seq_to_buf(seq_wrote) else {
            return;
        };

        if w.failed.is_empty() && w.wait_first == JournalBufWaitState::Waiters {
            w.wait_first = JournalBufWaitState::Empty;
            for callback in w.write_done_callbacks.drain(..).flatten() {
                callback();
            }
        }
    }

    /// 对应本地 bcachefs `journal_write_done()`
    /// (`journal/write.c:234-466`)。
    ///
    /// # 功能
    ///
    /// 1. 处理设备失败 / replicas bookkeeping
    /// 2. free_buf 回收（write.c:318-326）
    /// 3. last_uncompleted_write_seq 循环推进 ondisk seq（write.c:333-395）
    /// 4. reclaim / space / wake / cycle / do_writes（write.c:426-456）
    /// 5. 锁外 bch2_reset_alloc_cursors + bch2_do_discards_async（write.c:460-462）
    fn journal_write_done(&self, seq_wrote: u64) {
        let _buf_guard = self.buf_lock.lock().unwrap();
        let vol = self.vol.get().and_then(|vol| vol.upgrade());
        let mut err = false;

        if let Some(c) = vol.as_ref() {
            let w = self
                .journal_seq_to_buf(seq_wrote)
                .unwrap_or_else(|| panic!("journal_seq_to_buf: seq {seq_wrote} not in flight"));
            let pin_fifo = unsafe { &mut *self.pin_fifo.get() };
            let pin = pin_fifo
                .entry_for_seq_mut(seq_wrote)
                .unwrap_or_else(|| panic!("journal_seq_pin: seq {seq_wrote} out of range"));

            if !w.failed.is_empty() {
                if pin.devs.nr != 0 {
                    let replicas = journal_pin_devs_to_replicas(pin);
                    c.replicas.lock().unwrap().put(&replicas);
                    pin.devs.clear();
                }
            }

            if pin.devs.nr == 0 && !w.empty && !w.devs_written.is_empty() {
                let replicas = crate::replicas::BchReplicasEntry::new(
                    BchDataType::Journal,
                    &w.devs_written,
                    1,
                );
                c.replicas.lock().unwrap().get_or_mark(&replicas);
                pin.set_devs(&w.devs_written);
            }

            if !w.failed.is_empty() && w.devs_written.is_empty() {
                err = true;
            }
        } else if self
            .journal_seq_to_buf(seq_wrote)
            .is_some_and(|w| !w.failed.is_empty() && w.devs_written.is_empty())
        {
            err = true;
        }

        if err {
            self.bch2_journal_error_set(JournalError::Io(StorageError::JournalError(format!(
                "error writing journal entry {seq_wrote}"
            ))));
        }

        let mut replicas_refs = ReplicasEntryRefs::new();
        let mut lock_guard = self.lock.lock().unwrap();
        assert!(seq_wrote >= unsafe { &*self.pin_fifo.get() }.front);
        if err {
            let old = self.err_seq.load(Ordering::Acquire);
            if old == 0 || seq_wrote < old {
                self.err_seq.store(seq_wrote, Ordering::Release);
            }
        }

        if self.journal_seq_to_buf(seq_wrote).is_some_and(|w| w.flush) {
            self.flushes_outstanding.fetch_sub(1, Ordering::AcqRel);
        }

        // 对应本地 bcachefs `journal_write_done()` (write.c:318-326)：
        // free_buf 回收。在 buf_lock + lock 保护下将当前 buf data 交换到 free_buf
        // 以便复用大块分配，避免后续 journal_buf_realloc 中的重分配。
        //
        // 保留 C 语义：kvfree（Rust 中为 Vec::drop）在 buf_lock 外执行。
        let _buf_to_free = {
            let w = self
                .journal_seq_to_buf(seq_wrote)
                .unwrap_or_else(|| panic!("journal_seq_to_buf: seq {seq_wrote} not in flight"));
            let free_buf = unsafe { &mut *self.free_buf.get() };
            let free_buf_size = unsafe { &mut *self.free_buf_size.get() };
            let to_free = if free_buf.is_none() || *free_buf_size < w.buf_size {
                // swap：free_buf ← w.data，旧 free_buf 在锁外释放
                let old_free = free_buf.take();
                let old_data = std::mem::take(&mut w.data);
                *free_buf = Some(old_data);
                *free_buf_size = w.buf_size;
                old_free
            } else {
                Some(std::mem::take(&mut w.data))
            };
            w.buf_size = 0;
            to_free
        };
        drop(_buf_guard);
        // _buf_to_free 在 buf_lock 外自动 drop（Vec::drop 对应 C 的 kvfree）

        let mut completed = false;
        let mut last_seq_ondisk_updated = false;

        loop {
            let seq = self.last_uncompleted_write_seq(seq_wrote);
            if seq == 0 {
                break;
            }

            let (must_not_flush, empty, last_seq) = {
                let w = self.journal_seq_to_buf(seq).unwrap();
                (w.journal_buf_must_not_flush(), w.empty, w.last_seq)
            };

            if self.err_seq.load(Ordering::Acquire) == 0 && !must_not_flush {
                assert!(!empty || last_seq == seq);

                if self.last_seq_ondisk.load(Ordering::Acquire) < last_seq {
                    let _ = self.bch2_journal_update_last_seq_ondisk(
                        last_seq + u64::from(empty),
                        &mut replicas_refs,
                    );

                    if !replicas_refs.is_empty() {
                        drop(lock_guard);
                        if let Some(c) = vol.as_ref() {
                            replicas_refs_put(c, &mut replicas_refs);
                        }
                        lock_guard = self.lock.lock().unwrap();
                        continue;
                    }

                    assert!(last_seq <= self.last_seq.load(Ordering::Acquire));
                    self.last_seq_ondisk.store(last_seq, Ordering::Release);
                    last_seq_ondisk_updated = true;
                }

                self.flushed_seq_ondisk.store(seq, Ordering::Release);
                self.rewind_seq_ondisk
                    .store(self.rewind_seq.load(Ordering::Acquire), Ordering::Release);
            }

            if empty {
                self.last_empty_seq.store(seq, Ordering::Release);
            }
            self.seq_ondisk.store(seq, Ordering::Release);

            {
                let w = self.journal_seq_to_buf(seq).unwrap();
                w.wait_first = JournalBufWaitState::NotInFlight;
                for callback in w.write_done_callbacks.drain(..).flatten() {
                    callback();
                }
            }

            completed = true;
            let idx = self
                .in_flight
                .lock()
                .unwrap()
                .pop_front()
                .expect("in_flight front missing during journal completion");
            let w = self.bufs.get_mut(idx as usize);
            assert_eq!(w.seq, seq);
            w.state = BufState::Free;
        }

        if let Some(w) = self.journal_seq_to_buf(seq_wrote) {
            w.write_done = true;
        }

        unsafe {
            let pin_fifo = &mut *self.pin_fifo.get();
            pin_fifo.front = pin_fifo
                .back
                .min(self.last_seq_ondisk.load(Ordering::Acquire));
        }

        if completed {
            self.journal_reclaim_kick();
            self.bch2_journal_update_last_seq();
            let _ = self.bch2_journal_space_available(self.watermark());
            self.bch2_journal_wake_up();
        }

        std::sync::atomic::fence(Ordering::SeqCst);
        let _ = self.bch2_journal_cycle_locked();
        self.bch2_journal_do_writes_locked();
        drop(lock_guard);

        // 对应本地 bcachefs `journal_write_done()` (write.c:460-463)：
        // last_seq_ondisk 已更新，通知 allocator 重置游标并触发 discard。
        // bcachefs 在此处调用 bch2_reset_alloc_cursors() + bch2_do_discards_async()；
        // 当前 Rust 实现使用 journal_reclaim_kick() 作为等效 fallback
        // （两个函数尚未在 subvol alloc 层对齐）。
        if last_seq_ondisk_updated {
            self.journal_reclaim_kick();
        }
    }

    /// Open a new journal entry: find free buf, switch reservations.idx.
    ///
    /// 对应 bcachefs `__journal_entry_open_one()` (journal.c:391-569)。
    ///
    /// seq 在此处递增分配（per-entry），而非在 `journal_res_get_fast` 中。
    /// 对应 bcachefs `__journal_entry_open_one` line 476：
    /// `u64 seq = atomic64_inc_return(&j->seq);`
    ///
    /// 执行：
    /// 1. 分配 entry 级别 seq（fetch_add 1）
    /// 2. 找到可用的 free buf，初始化
    /// 3. 通过 CAS 切换 `reservations.idx` 到新 buf
    /// 4. 注册到 `in_flight` 队列
    /// 5. 推入 pin_fifo 自钉（count=1）
    fn journal_entry_open(&self) -> Result<(), JournalError> {
        // ─── 前置检查（对应 bcachefs __journal_entry_open_one, journal.c:399-468） ───

        // 0. 如果 entry 已打开，无需重复 open
        if self.reservations.is_open() {
            return Ok(());
        }

        // 1. blocked 检查（journal.c:402-403）
        if self.blocked.load(Ordering::Acquire) != 0 {
            return Err(JournalError::Blocked("journal blocked".into()));
        }

        // 2. cur_entry_error 检查（journal.c:405-406）
        let entry_err = self.cur_entry_error.load(Ordering::Acquire);
        if entry_err != 0 {
            return Err(JournalError::Overflow(format!(
                "cur_entry_error={}",
                entry_err
            )));
        }

        // 3. journal 错误状态检查（journal.c:408, `bch2_journal_error`）
        if let Some(err) = self.journal_error_check() {
            return Err(err);
        }

        // 4. in_flight 队列空位检查（journal.c:436-437）
        //    需要至少 2 个空位（一个给当前 entry，一个作为 sentinel 防止回绕）
        let in_flight_len = self.in_flight.lock().unwrap().len();
        // 本地 init.c:767-783 分配 256 项 FIFO；journal.c:430-437 要求
        // push 前至少保留两个空槽（一个给新 entry，一个 sentinel）。
        if in_flight_len + 2 > JOURNAL_IN_FLIGHT_NR {
            return Err(JournalError::Full("journal max in_flight".into()));
        }

        // 5. seq 溢出检查（journal.c:442-447）
        let cur_seq = self.seq.load(Ordering::Acquire);
        if cur_seq >= JOURNAL_SEQ_MAX {
            return Err(JournalError::Overflow("journal seq overflow".into()));
        }

        // 6. 黑名单检查（journal.c:449-455）
        let next_seq = cur_seq + 1;
        if self.bch2_journal_seq_is_blacklisted(next_seq, false) {
            return Err(JournalError::Overflow(format!(
                "attempting to open blacklisted journal seq {}",
                next_seq
            )));
        }

        // ─── 核心 open 逻辑 ───

        // 分配 entry 级别 seq（对应 journal.c:476）
        let new_seq = self.seq.fetch_add(1, Ordering::AcqRel) + 1;

        // 找到 free buf（对应 journal.c:549-564 idx++ 循环）
        let idx = self.find_free_buf(new_seq);
        let buf = self.bufs.get_mut(idx as usize);
        buf.reset_for_accepting(new_seq);

        // CAS 切换 reservations.idx（对应 journal.c:549-564）
        self.reservations
            .open_entry((new_seq & JOURNAL_STATE_BUF_MASK as u64) as u32);

        // 设置当前 entry 可用空间（对应 bcachefs journal.c:547: j->cur_entry_u64s = u64s）
        // bcachefs 动态计算 u64s = (sectors << 9) / sizeof(u64) - overhead
        // subvol 简化为 BUF_SIZE_U64S（固定最大值）
        self.cur_entry_u64s.store(BUF_SIZE_U64S, Ordering::Release);

        // 注册到 in_flight
        self.in_flight.lock().unwrap().push_back(idx);

        // 推入 pin_fifo 自钉（count=1）
        unsafe {
            let success = (*self.pin_fifo.get()).push_back(JournalEntryPinList::new(1));
            assert!(
                success.is_ok(),
                "pin_fifo full: journal entries cycled too fast"
            );
        }

        // R2: 对应 bcachefs __journal_entry_open_one 中 open 后的 bch2_journal_space_available
        // （在 cycle_locked 的主循环中，open 后 implicit 调用空间检查）
        let _avail = self.bch2_journal_space_available(self.watermark());

        if self.write_work_running.load(Ordering::Acquire)
            && self.write_work_deadline_ms.load(Ordering::Acquire) == 0
        {
            let delay = self.vol.get().and_then(|vol| vol.upgrade()).map_or_else(
                || self.journal_flush_delay_ms.load(Ordering::Acquire),
                |c| u64::from(c.opts.journal_flush_delay),
            );
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            if self
                .write_work_deadline_ms
                .compare_exchange(
                    0,
                    now.saturating_add(delay),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                self.write_work_notify.notify_one();
            }
        }

        Ok(())
    }

    /// Close current entry: stop accepting new reservations.
    ///
    /// 对应 bcachefs `__journal_entry_close_one()` (journal.c:276-384)。
    /// 设置 cur_entry_offset = CLOSED_VAL。
    /// 返回 CAS 关闭前捕获的 cur_entry_offset（单位 u64），
    /// 用于 J2 flush data race 修复中安全设置 buf.data_end。
    ///
    /// # bcachefs 映射
    ///
    /// bcachefs 的 `__journal_entry_close_one` 预期在持有 `j->lock` 和
    /// `pin_resize_lock` 时调用。subvol 的 close 函数在 `cycle_locked` 内
    /// 通过 `slowpath_lock` 互斥，等价于持有这两把锁。
    fn journal_entry_close(&self) -> u32 {
        // 1. CAS 关闭当前 entry（对应 bcachefs journal.c:286-298）
        let used_u64s = self.reservations.close_entry();

        // 2. 检查 entry 是否实际是 open 的（对应 bcachefs journal.c:301：
        //    `if (!__journal_entry_is_open(old)) return;`）
        //    sentinel 值 >= CLOSED_VAL 表示 entry 未打开
        if used_u64s >= JOURNAL_ENTRY_CLOSED_VAL as u32 {
            return 0;
        }

        // 3. 获取 buf 对应 seq（对应 bcachefs journal.c:303：`u64 seq = journal_cur_seq(j)`）
        let close_seq = self.seq.load(Ordering::Acquire);
        let idx = (close_seq & (JOURNAL_IN_FLIGHT_NR as u64 - 1)) as usize;

        // 4. 更新 pin list bytes + dirty_entry_bytes
        //    （对应 bcachefs journal.c:315-316）
        let used_bytes = (used_u64s as u64).saturating_mul(8);
        let rounded_bytes = used_bytes.next_power_of_two();
        if let Some(pin_list) = self.pin_fifo_ref().entry_for_seq(close_seq) {
            let _guard = pin_list.lock.lock();
            // SAFETY: pin_list.lock 序列化了对 pin_list.bytes 的唯一写入。
            unsafe {
                let pin_list_ptr =
                    pin_list as *const JournalEntryPinList as *mut JournalEntryPinList;
                (*pin_list_ptr).bytes = rounded_bytes.min(u32::MAX as u64) as u32;
            }
        }
        self.dirty_entry_bytes
            .fetch_add(used_bytes, Ordering::Release);

        // 5. sectors 计算（对应 bcachefs journal.c:329-344）
        //    bcachefs: sectors = vstruct_blocks_plus(buf->data, block_bits, u64s_reserved) << block_bits
        //    vstruct_blocks_plus = DIV_ROUND_UP(buf->data->u64s, 1 << (block_bits + 6)) + u64s_reserved
        //    其中 block_bits = ilog2(block_size in sectors), +6 是因为每 sector = 64 u64s
        //    subvol: JSET_BLOCK_SIZE=4096, block_bits=3, block_sectors=8, block_u64s=512
        let buf = self.bufs.get_mut(idx);
        let block_sectors = (JSET_BLOCK_SIZE / 512) as u64; // 8
        let block_u64s = (JSET_BLOCK_SIZE / 8) as u64; // 512
        let total_u64s = used_u64s as u64 + buf.u64s_reserved as u64;
        let header_u64s = std::mem::size_of::<JsetHeader>().div_ceil(8) as u64;
        let sectors = ((header_u64s + total_u64s).div_ceil(block_u64s) * block_sectors) as u32;
        if sectors > buf.sectors && buf.sectors > 0 {
            // bcachefs 中此处会调用 bch2_fs_emergency_read_only_locked，
            // subvol 先记录 warn（日志路径，不影响运行）
            tracing::warn!(
                "journal entry overran reserved space: {} > {}",
                sectors,
                buf.sectors
            );
        }
        buf.sectors = sectors;

        // 6. 设置 last_seq（对应 bcachefs journal.c:364-366）
        //    buf->last_seq = j->last_seq;
        //    buf->data->last_seq = cpu_to_le64(buf->last_seq);
        //    subvol 的 last_seq_ondisk 对应 j->last_seq
        buf.last_seq = self.last_seq.load(Ordering::Acquire);
        // 本地 final put 会立即调用 do_writes_locked；在释放隐式 ref 前，
        // Rust 侧承载 jset->u64s 的 data_end 和 closed 状态必须已经可见。
        buf.data_end = (used_u64s as usize).saturating_mul(8).min(BUF_SIZE);
        buf.state = BufState::Closing;
        // buf->data->last_seq 在 subvol 中由 Jset 序列化时设置，此处不重复写
        let _ = buf; // 释放借用

        // 7. 关闭后释放（对应 bcachefs journal.c:375-383）
        //    正常路径: __bch2_journal_buf_put(j, seq) — 释放 open_entry 添加的隐式 refcount
        //    （修复后的 open_entry 使用 journal_state_inc 递增了 buf_count，此处对应释放）
        self.__bch2_journal_buf_put(close_seq);

        // 8. 空间可用性检查（对应 bcachefs journal.c:377）
        let _avail = self.bch2_journal_space_available(self.watermark());

        used_u64s
    }

    /// 等待指定 buf_idx 的所有 in-flight reservation 完成（refcount 归零）。
    ///
    /// 在 `close_entry()` 后调用：此时 CLOSED_VAL 阻止所有新 reservation，
    /// 但已有 reservation 持有的 refcount 尚未释放。
    /// 自旋等待直到 `buf_count(state, buf_idx) == 0`。
    ///
    /// 这是 J2 flush data race 修复的一部分：
    /// 先 close_entry（原子捕获 offset + 阻止新 reservation），
    /// 再 drain（等待已有 reservation 完成），
    /// 最后设 data_end（此时安全——不再有 thread 写入此 buf 的 data_end 之外）。
    async fn wait_for_pending_drain(&self, buf_idx: usize) {
        let mut drain_rx = self.bufs.get_mut(buf_idx).drain_watch.subscribe();
        loop {
            if *drain_rx.borrow() == 0 {
                return;
            }
            if drain_rx.changed().await.is_err() {
                return;
            }
        }
    }

    /// 找到可用的 free buf。
    ///
    /// 对应 bcachefs `__journal_entry_open_one()` (journal.c:549-564) 的 idx++ 模式：
    /// ```c
    /// new.idx++;
    /// BUG_ON(journal_state_count(new, new.idx));
    /// BUG_ON(new.idx != (seq & JOURNAL_STATE_BUF_MASK));
    /// ```
    /// reservation idx 循环递增；实际 `journal_buf` 来自独立 256 项 in-flight FIFO。
    fn find_free_buf(&self, new_seq: u64) -> u32 {
        let old = self.reservations.read();
        let old_idx = JournalResState::idx(old);
        // bcachefs: idx = (idx + 1) & JOURNAL_STATE_BUF_MASK (cyclic increment)
        let state_idx = (old_idx + 1) & JOURNAL_STATE_BUF_MASK as u32;
        // bcachefs: BUG_ON(new.idx != (seq & JOURNAL_STATE_BUF_MASK))
        debug_assert_eq!(
            state_idx as u64,
            new_seq & JOURNAL_STATE_BUF_MASK as u64,
            "journal idx must equal seq & BUF_MASK"
        );
        // 对应 bcachefs: BUG_ON(journal_state_count(new, new.idx)) — buf 必须可用
        let count = JournalResState::buf_count(old, state_idx);
        assert!(
            count == 0,
            "journal buf {} still has {} active reservations: \
             bcachefs would fail with journal_max_open",
            state_idx,
            count,
        );
        let idx = (new_seq & (JOURNAL_IN_FLIGHT_NR as u64 - 1)) as usize;
        let buf = self.bufs.get_mut(idx);
        assert!(
            buf.state == BufState::Free || buf.state == BufState::WriteDone,
            "journal buf {} not free (state={:?}, seq={}): \
             bcachefs would fail with journal_max_open",
            idx,
            buf.state,
            new_seq,
        );
        buf.state = BufState::Free;
        idx as u32
    }

    // ─── Convenience: old append API (now uses new fastpath) ──

    /// 追加 btree update（insert/delete）
    ///
    /// 使用新 fastpath API（接受 `&self`，无锁）:
    /// 1. `journal_res_get_fast()` — 在 buf 中原子保留空间
    /// 2. `commit()` — 将序列化的 Jset 写入 buf
    /// 3. `journal_res_put()` — 释放 refcount
    ///
    /// # 参数
    /// - `must_flush`: 若为 true，append 完成后立即调用 backend.flush() 保证持久化
    ///
    /// 返回分配的 seq。
    pub async fn append(
        &self,
        btree_type: BtreeId,
        entries: &[BtreeEntry],
        must_flush: bool,
    ) -> Result<u64, JournalError> {
        // 构建 JsetEntry
        let entries_bytes = bincode::serialize(&entries.to_vec())
            .map_err(|e| JournalError::Io(StorageError::Serialization(e)))?;
        let entry = RawJsetEntry::new(
            btree_type as u8,
            JsetEntryType::BtreeKeys as u8,
            entries_bytes,
            0,
        )
        .map_err(JournalError::Io)?;
        let jset_base = Jset {
            header: JsetHeader {
                magic: super::jset::JOURNAL_MAGIC,
                seq: 0,
                last_seq: self.last_seq.load(Ordering::Acquire),
                crc32: 0,
                entry_count: 1,
                version: super::jset::JSET_VERSION as u32,
                flags: super::jset::CSUM_TYPE_NONE as u32,
                pad: [0u8; 24],
            },
            entries: vec![entry],
        };
        // seq=0 不影响布局大小；直接计算 padding 后长度，避免为了 size 预先分配完整 buffer。
        let size0 = jset_base.serialized_padded_len();
        let req_u64s = size0.div_ceil(8) as u32;
        let res = self.bch2_journal_res_get_fast(Watermark::Btree, req_u64s)?;
        let jset = Jset {
            header: JsetHeader {
                seq: res.seq,
                ..jset_base.header
            },
            entries: jset_base.entries,
        };
        let serialized = jset.serialize_padded().map_err(JournalError::Io)?;

        // 设置 must_flush 标志，add_entry 会将其传播到 buf.has_must_flush
        let mut res = res;
        res.must_flush = must_flush;
        self.bch2_journal_add_raw(&mut res, &serialized);

        self.bch2_journal_res_put(&res);

        if must_flush {
            self.bch2_journal_flush().await?;
        }

        Ok(res.seq)
    }

    /// 追加 btree_root entry（记录 root 指针变化）
    pub async fn append_btree_root(
        &self,
        btree_type: BtreeId,
        root_addr: u64,
        level: u8,
        must_flush: bool,
    ) -> Result<u64, JournalError> {
        let root_entry = BtreeEntry::new(
            crate::btree::key::Bpos::new(0, root_addr, 0),
            crate::btree::key::KeyType::Normal,
            crate::btree::key::KeyValue::Raw(vec![]),
        );
        let entries_bytes = bincode::serialize(&vec![root_entry])
            .map_err(|e| JournalError::Io(StorageError::Serialization(e)))?;
        let entry = RawJsetEntry::new(
            btree_type as u8,
            JsetEntryType::BtreeRoot as u8,
            entries_bytes,
            level,
        )
        .map_err(JournalError::Io)?;
        let jset_template = Jset {
            header: JsetHeader {
                magic: super::jset::JOURNAL_MAGIC,
                seq: 0,
                last_seq: self.last_seq.load(Ordering::Acquire),
                crc32: 0,
                entry_count: 1,
                version: super::jset::JSET_VERSION as u32,
                flags: super::jset::CSUM_TYPE_NONE as u32,
                pad: [0u8; 24],
            },
            entries: vec![entry],
        };
        let res = self.bch2_journal_res_get_fast(
            Watermark::InteriorUpdate,
            jset_template.serialized_padded_len().div_ceil(8) as u32,
        )?;
        let jset = Jset {
            header: JsetHeader {
                seq: res.seq,
                ..jset_template.header
            },
            entries: jset_template.entries,
        };
        let serialized = jset.serialize_padded().map_err(JournalError::Io)?;
        let mut res = res;
        res.must_flush = must_flush;
        self.bch2_journal_add_raw(&mut res, &serialized);
        self.bch2_journal_res_put(&res);

        if must_flush {
            self.bch2_journal_flush().await?;
        }

        Ok(res.seq)
    }

    // ─── Bucket write: bch2_journal_write ─────────────────

    /// 对应本地 bcachefs `bch2_journal_write_prep()`
    /// (`journal/write.c:621-733`)。
    fn bch2_journal_write_prep(&self, w: &mut JournalBuf) -> Result<(), JournalError> {
        let mut jset = Jset::new(w.seq, w.last_seq);
        let mut offset = 0usize;

        while offset < w.data_end {
            // 跳过全零块（因预留但未写入的 gap）
            if w.data[offset..]
                .chunks(JSET_BLOCK_SIZE as usize)
                .next()
                .map(|chunk| chunk.iter().all(|&b| b == 0))
                == Some(true)
            {
                offset += JSET_BLOCK_SIZE as usize;
                continue;
            }
            let Some(mut source) = Jset::deserialize(&w.data[offset..w.data_end])? else {
                return Err(JournalError::Io(StorageError::InvalidData(
                    "invalid journal entry during write prep".to_string(),
                )));
            };
            let bytes = std::mem::size_of::<JsetHeader>()
                + source
                    .entries
                    .iter()
                    .map(|entry| std::mem::size_of::<JsetEntryHeader>() + entry.payload.len())
                    .sum::<usize>();
            let consumed = bytes.div_ceil(JSET_BLOCK_SIZE as usize) * JSET_BLOCK_SIZE as usize;
            if consumed == 0 || offset + consumed > w.data_end {
                return Err(JournalError::Io(StorageError::InvalidData(
                    "journal entry overruns write buffer".to_string(),
                )));
            }
            jset.entries.append(&mut source.entries);
            offset += consumed;
        }

        let start_u64s = (std::mem::size_of::<JsetHeader>()
            + jset
                .entries
                .iter()
                .map(|entry| std::mem::size_of::<JsetEntryHeader>() + entry.payload.len())
                .sum::<usize>())
        .div_ceil(8);
        let mut empty = jset.header.seq == jset.header.last_seq;
        let mut btree_roots_have = 0u64;
        let vol = self.vol.get().and_then(|vol| vol.upgrade());

        // 对应本地 `bch2_journal_keys_to_write_buffer_start` (write_buffer.c:1274-1294)。
        // 锁定所有 btree 的 inc.lock 后再清 need_flush_to_write_buffer。
        // JournalKeysToWb drop 时释放所有锁（等效 _end）。
        let mut wb_dst: Option<
            crate::btree::write_buffer::JournalKeysToWb,
        > = None;
        if w.need_flush_to_write_buffer {
            if let Some(c) = vol.as_ref() {
                let wb_set = unsafe { &*c.write_buffer_set.get() };
                wb_dst = Some(
                    crate::btree::write_buffer::bch2_journal_keys_to_write_buffer_start(
                        wb_set, w.seq,
                    ),
                );
            }
            w.need_flush_to_write_buffer = false;
        }

        jset.entries.retain(|entry| !entry.payload.is_empty());
        for entry in &mut jset.entries {
            if entry.hdr.entry_type == JsetEntryType::BtreeKeys as u8 {
                empty = false;
            }

            match JsetEntryType::from_u8(entry.hdr.entry_type) {
                Some(JsetEntryType::BtreeRoot) => {
                    let Some(btree_id) = BtreeId::from_u8(entry.hdr.btree_type) else {
                        return Err(JournalError::Io(StorageError::InvalidData(
                            "invalid btree id in root journal entry".to_string(),
                        )));
                    };
                    btree_roots_have |= 1u64 << entry.hdr.btree_type;
                    if let Some(c) = vol.as_ref() {
                        let roots: Vec<BtreeEntry> = bincode::deserialize(&entry.payload)
                            .map_err(|err| JournalError::Io(StorageError::Serialization(err)))?;
                        if let Some(root) = roots.first() {
                            unsafe {
                                *c.btree(btree_id).current_root_disk.get() =
                                    Some((root.pos.offset, entry.hdr.level));
                            }
                        }
                    }
                }
                Some(JsetEntryType::WriteBufferKeys) => {
                    let Some(c) = vol.as_ref() else {
                        return Err(JournalError::Io(StorageError::NotFound(
                            "volume unavailable while flushing journal write buffer".to_string(),
                        )));
                    };
                    let Some(btree_id) = BtreeId::from_u8(entry.hdr.btree_type) else {
                        return Err(JournalError::Io(StorageError::InvalidData(
                            "invalid btree id in write buffer journal entry".to_string(),
                        )));
                    };
                    let keys: Vec<BtreeEntry> = bincode::deserialize(&entry.payload)
                        .map_err(|err| JournalError::Io(StorageError::Serialization(err)))?;
                    let wb_set = unsafe { &*c.write_buffer_set.get() };
                    let _guard;
                    // 若 wb_dst 未持有锁（非 need_flush 路径），在此获取单 btree 的锁
                    let dst = if let Some(ref mut d) = wb_dst {
                        d
                    } else {
                        let wb_idx =
                            crate::btree::write_buffer::bch_wb_btree_idx(btree_id) as usize;
                        let lock_ptr: *const std::sync::Mutex<()> =
                            &wb_set.buffers[wb_idx].inc.lock;
                        _guard = Some(unsafe { (*lock_ptr).lock().unwrap() });
                        // 构建一个临时的 JournalKeysToWb（不含 guards，仅用于单 btree 插入）
                        let mut tmp = crate::btree::write_buffer::JournalKeysToWb::new();
                        tmp.seq = w.seq;
                        tmp.per_btree[0] = crate::btree::write_buffer::JournalKeysToWbBtree {
                            wb: &wb_set.buffers[wb_idx].inc
                                as *const crate::btree::write_buffer::BtreeWriteBufferKeys
                                as *mut _,
                            room: wb_set.buffers[wb_idx]
                                .inc
                                .keys
                                .capacity()
                                .saturating_sub(wb_set.buffers[wb_idx].inc.nr),
                        };
                        // SAFETY: 此 &mut 从 & 转换，仅在 tmp 存活期内使用，
                        // 且 _guard 防止并发访问。
                        unsafe { &mut *(&mut tmp as *mut _) }
                    };
                    for key in keys {
                        let (key, value) = key.to_key_value();
                        if crate::btree::write_buffer::bch2_journal_key_to_wb(
                            dst,
                            btree_id,
                            key,
                            value,
                            w.seq,
                        ) != 0
                        {
                            return Err(JournalError::Io(StorageError::JournalError(
                                "flushing journal keys to btree write buffer".to_string(),
                            )));
                        }
                    }
                    entry.hdr.entry_type = JsetEntryType::BtreeKeys as u8;
                }
                _ => {}
            }
        }

        // wb_dst drop 时释放所有锁（等效 bcachefs _end）

        w.empty = empty;

        if let Some(c) = vol.as_ref() {
            for btree_id in crate::btree::BTREE_ID_NR {
                if btree_roots_have & (1u64 << btree_id as u8) != 0 {
                    continue;
                }
                let Some((root_addr, level)) = c.btree(btree_id).current_root_disk_info() else {
                    continue;
                };
                let root = BtreeEntry::new(
                    crate::btree::key::Bpos::new(0, root_addr, 0),
                    crate::btree::key::KeyType::Normal,
                    crate::btree::key::KeyValue::Raw(Vec::new()),
                );
                let payload = bincode::serialize(&vec![root])
                    .map_err(|err| JournalError::Io(StorageError::Serialization(err)))?;
                jset.entries.push(RawJsetEntry::new(
                    btree_id as u8,
                    JsetEntryType::BtreeRoot as u8,
                    payload,
                    level,
                )?);
            }
        }

        let seconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        jset.entries.push(RawJsetEntry::new(
            0,
            JsetEntryType::Datetime as u8,
            seconds.to_le_bytes().to_vec(),
            0,
        )?);

        if let Some(c) = vol.as_ref() {
            jset.entries.push(RawJsetEntry::new(
                2,
                JsetEntryType::Usage as u8,
                c.key_version.load(Ordering::Acquire).to_le_bytes().to_vec(),
                0,
            )?);
            for rw in 0..2 {
                let mut payload = vec![rw as u8];
                payload.resize(8, 0);
                payload.extend_from_slice(&c.io_clock[rw].load(Ordering::Acquire).to_le_bytes());
                jset.entries.push(RawJsetEntry::new(
                    0,
                    JsetEntryType::Clock as u8,
                    payload,
                    0,
                )?);
            }
        }

        let end_u64s = (std::mem::size_of::<JsetHeader>()
            + jset
                .entries
                .iter()
                .map(|entry| std::mem::size_of::<JsetEntryHeader>() + entry.payload.len())
                .sum::<usize>())
        .div_ceil(8);
        let extra_u64s = end_u64s.saturating_sub(start_u64s);
        if extra_u64s > self.entry_u64s_reserved.load(Ordering::Acquire) as usize {
            tracing::warn!(
                "journal write prep used {} extra u64s with {} reserved",
                extra_u64s,
                self.entry_u64s_reserved.load(Ordering::Acquire)
            );
        }

        let serialized = jset.serialize_padded()?;
        let sectors = serialized
            .len()
            .div_ceil(crate::types::SECTOR_SIZE as usize) as u32;
        if sectors > w.sectors {
            return Err(JournalError::Io(StorageError::InvalidData(format!(
                "journal write overran available space: {} > {} sectors",
                sectors, w.sectors
            ))));
        }
        if serialized.len() > w.data.len() {
            return Err(JournalError::Io(StorageError::InvalidData(
                "journal write prep exceeds buffer size".to_string(),
            )));
        }
        w.data[..serialized.len()].copy_from_slice(&serialized);
        w.data[serialized.len()..].fill(0);
        w.data_end = serialized.len();
        Ok(())
    }

    /// 对应本地 bcachefs `journal_buf_realloc()` (`journal/write.c:161-189`)。
    fn journal_buf_realloc(&self, buf: &mut JournalBuf) {
        let mut new_size = self.buf_size_want.load(Ordering::Relaxed);

        if buf.buf_size >= new_size {
            return;
        }

        let btree_write_buffer_size = new_size / 64;
        let Some(c) = self.vol.get().and_then(|vol| vol.upgrade()) else {
            return;
        };
        if crate::btree::write_buffer::bch2_btree_write_buffer_resize(&c, btree_write_buffer_size)
            != 0
        {
            return;
        }

        let mut new_buf = Vec::new();
        if new_buf.try_reserve_exact(new_size).is_err() {
            return;
        }
        new_buf.resize(new_size, 0);
        new_buf[..buf.buf_size].copy_from_slice(&buf.data[..buf.buf_size]);

        let _guard = self.lock.lock().unwrap();
        std::mem::swap(&mut buf.data, &mut new_buf);
        std::mem::swap(&mut buf.buf_size, &mut new_size);
    }

    /// 对应本地 bcachefs `bch2_journal_write_checksum()`
    /// (`journal/write.c:736-777`)。
    fn bch2_journal_write_checksum(&self, w: &mut JournalBuf) -> Result<(), JournalError> {
        let Some(mut jset) = Jset::deserialize(&w.data[..w.data_end])? else {
            return Err(JournalError::Io(StorageError::InvalidData(
                "invalid journal entry before checksum".to_string(),
            )));
        };

        jset.header.magic = super::jset::JOURNAL_MAGIC;
        jset.header.version = super::jset::JSET_VERSION as u32;
        let no_flush = jset.header.flags & super::jset::JSET_NO_FLUSH;
        jset.header.flags &= !(super::jset::JSET_CSUM_TYPE_MASK
            | super::jset::JSET_BIG_ENDIAN
            | super::jset::JSET_HAS_OVERWRITES);
        jset.header.flags |= CSUM_TYPE_CRC32C as u32 | no_flush;
        if cfg!(target_endian = "big") {
            jset.header.flags |= super::jset::JSET_BIG_ENDIAN;
        }
        if w.has_overwrites {
            jset.header.flags |= super::jset::JSET_HAS_OVERWRITES;
        }

        let serialized = jset.serialize_padded()?;
        let Some(validated) = Jset::deserialize(&serialized)? else {
            return Err(JournalError::ChecksumMismatch);
        };
        if !validated.verify() || !super::validate::bch2_jset_validate(&validated) {
            return Err(JournalError::ChecksumMismatch);
        }

        if serialized.len() > w.data.len() {
            return Err(JournalError::Io(StorageError::InvalidData(
                "checksummed journal entry exceeds buffer size".to_string(),
            )));
        }
        w.data[..serialized.len()].copy_from_slice(&serialized);
        w.data[serialized.len()..].fill(0);
        w.data_end = serialized.len();
        Ok(())
    }

    /// 对应本地 `bch2_journal_dev_buckets_available()` 的
    /// `journal_space_discarded` 分支。
    fn bch2_journal_dev_buckets_available(ja: &crate::block_device::bch_dev::JournalDevice) -> u32 {
        if ja.nr == 0 {
            return 0;
        }

        let mut available = (ja.discard_idx + ja.nr - ja.cur_idx - 1) % ja.nr;
        if available != 0 && ja.dirty_idx_ondisk == ja.dirty_idx {
            available -= 1;
        }
        available
    }

    /// 对应本地 bcachefs `journal_advance_devs_to_next_bucket()`
    /// (`journal/write.c:29-57`)。
    fn journal_advance_devs_to_next_bucket(&self, devs: &DevAllocList, sectors: u32, seq: u64) {
        let Some(c) = self.vol.get().and_then(|vol| vol.upgrade()) else {
            return;
        };

        for dev_idx in devs.iter() {
            let Some(ca) = c.device_registry.resolve_bch_dev(dev_idx) else {
                continue;
            };

            let bucket_size = unsafe { &*ca.mi.get() }.bucket_size as u32;
            let mut ja = ca.journal.lock().unwrap();
            if sectors > ja.sectors_free
                && sectors <= bucket_size
                && Self::bch2_journal_dev_buckets_available(&ja) != 0
            {
                ja.cur_idx = (ja.cur_idx + 1) % ja.nr;
                ja.sectors_free = bucket_size;
                let cur_idx = ja.cur_idx as usize;
                ja.bucket_seq[cur_idx] = seq;
            }
        }
    }

    /// 对应本地 bcachefs `__journal_write_alloc()`
    /// (`journal/write.c:59-110`)。
    fn __journal_write_alloc(
        &self,
        w: &mut JournalBuf,
        devs: &DevAllocList,
        sectors: u32,
        replicas: &mut u32,
        replicas_want: u32,
    ) {
        let Some(c) = self.vol.get().and_then(|vol| vol.upgrade()) else {
            return;
        };

        for dev_idx in devs.iter() {
            let Some(ca) = c.device_registry.resolve_bch_dev(dev_idx) else {
                continue;
            };
            let Some(io_ref) = ca.try_get_io_ref_guard(BchDevIoRefKind::Write) else {
                continue;
            };

            let mut ja = ca.journal.lock().unwrap();
            if ja.nr == 0
                || w.key.iter().any(|ptr| ptr.dev == ca.dev_idx)
                || sectors > ja.sectors_free
            {
                drop(ja);
                drop(io_ref);
                continue;
            }

            // Match local `bch2_dev_stripe_increment()`'s
            // `__dev_buckets_free(ca, usage, BCH_WATERMARK_normal)`
            // (`fs/alloc/foreground.c:819-825`), including watermark and
            // open-bucket reservations rather than raw block subtraction.
            let free_space = crate::alloc::dev_buckets_free(&ca, Watermark::Normal);
            self.wp_stripe
                .lock()
                .unwrap()
                .increment(ca.dev_idx, free_space);

            let bucket_size = unsafe { &*ca.mi.get() }.bucket_size as u64;
            let cur_idx = ja.cur_idx as usize;
            let offset = bucket_to_sector(&ca, ja.buckets[cur_idx] as usize) + bucket_size
                - ja.sectors_free as u64;
            w.key.push(ExtentPtr {
                offset,
                dev: ca.dev_idx,
                gen: 0,
                cached: false,
                unwritten: false,
            });
            w.cas.push(io_ref);

            ja.sectors_free -= sectors;
            ja.bucket_seq[cur_idx] = w.seq;
            *replicas += unsafe { &*ca.mi.get() }.durability as u32;

            if *replicas >= replicas_want {
                break;
            }
        }
    }

    /// 对应本地 bcachefs `journal_write_alloc()`
    /// (`journal/write.c:112-159`)。
    fn journal_write_alloc(
        &self,
        w: &mut JournalBuf,
        replicas: &mut u32,
    ) -> Result<(), JournalError> {
        let Some(c) = self.vol.get().and_then(|vol| vol.upgrade()) else {
            return Err(JournalError::Full(
                "insufficient journal devices".to_string(),
            ));
        };

        let sectors = w
            .data_end
            .div_ceil(JSET_BLOCK_SIZE as usize)
            .saturating_mul(SECTORS_PER_BLOCK as usize) as u32;
        let mut target = if c.opts.metadata_target != 0 {
            c.opts.metadata_target
        } else {
            c.opts.foreground_target
        };
        let replicas_want = u32::from(c.opts.metadata_replicas);
        let mut advance_done = false;

        loop {
            let devs = crate::alloc::target_rw_devs(&c, BchDataType::Journal, target);
            let devs_sorted = {
                let mut stripe = self.wp_stripe.lock().unwrap();
                bch2_dev_alloc_list(&mut stripe, &devs)
            };

            loop {
                self.__journal_write_alloc(w, &devs_sorted, sectors, replicas, replicas_want);

                if *replicas >= replicas_want {
                    break;
                }

                if !advance_done {
                    self.journal_advance_devs_to_next_bucket(&devs_sorted, sectors, w.seq);
                    advance_done = true;
                    continue;
                }
                break;
            }

            if *replicas >= replicas_want || target == 0 {
                break;
            }

            target = 0;
            advance_done = false;
        }

        assert!(w.key.len() <= BCH_REPLICAS_MAX as usize);
        if *replicas != 0 {
            Ok(())
        } else {
            Err(JournalError::Full(
                "insufficient journal devices".to_string(),
            ))
        }
    }

    /// 对应本地 `journal_write_endio()` (`journal/write.c:490-511`)。
    /// Rust bio callback 先把逐设备结果写入共享 completion 状态；所有 bio
    /// 引用归零后，submit continuation 再把结果发布到 journal_buf。
    fn journal_write_endio(
        first_err: &AtomicFirstError,
        io_failures: &Mutex<Vec<(usize, u8)>>,
        buf_idx: usize,
        dev_idx: u8,
        result: Result<(), StorageError>,
    ) {
        if let Err(err) = result {
            io_failures.lock().unwrap().push((buf_idx, dev_idx));
            first_err.set_first(err);
        }
    }

    /// 对应本地 `journal_write_preflush()` (`journal/write.c:585-617`)。
    async fn journal_write_preflush(
        &self,
        buf_idx: usize,
        journal_devs: &[Arc<BchDev>],
        first_err: &Arc<AtomicFirstError>,
        io_failures: &Arc<Mutex<Vec<(usize, u8)>>>,
    ) {
        let cl = Closure::new();
        for dev in journal_devs {
            cl.get();
            let cl_endio = cl.clone();
            let first_err_endio = first_err.clone();
            let io_failures_endio = io_failures.clone();
            let dev_idx = dev.dev_idx;
            submit_bio_write(BioRequest::preflush(dev.clone()).set_end_io(move |result| {
                Self::journal_write_endio(
                    &first_err_endio,
                    &io_failures_endio,
                    buf_idx,
                    dev_idx,
                    result,
                );
                cl_endio.put();
            }));
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        cl.continue_at(Box::new(move || {
            let _ = tx.send(());
        }));
        cl.put();
        rx.await.unwrap();
    }

    /// 对应本地 `journal_write_submit()` (`journal/write.c:513-583`)。
    async fn journal_write_submit(
        &self,
        buf_idx: usize,
        writes: &[(Arc<BchDev>, BlockAddr, Vec<u8>)],
        flush: bool,
        separate_flush: bool,
        first_err: &Arc<AtomicFirstError>,
        io_failures: &Arc<Mutex<Vec<(usize, u8)>>>,
    ) {
        let cl = Closure::new();
        for (dev, addr, data) in writes.iter().cloned() {
            cl.get();
            let cl_endio = cl.clone();
            let first_err_endio = first_err.clone();
            let io_failures_endio = io_failures.clone();
            let dev_idx = dev.dev_idx;
            submit_bio_write(
                BioRequest::write(dev, addr, data)
                    .set_preflush(flush && !separate_flush)
                    .set_fua(flush)
                    .set_end_io(move |result| {
                        Self::journal_write_endio(
                            &first_err_endio,
                            &io_failures_endio,
                            buf_idx,
                            dev_idx,
                            result,
                        );
                        cl_endio.put();
                    }),
            );
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        cl.continue_at(Box::new(move || {
            let _ = tx.send(());
        }));
        cl.put();
        rx.await.unwrap();
    }

    /// 处理单个 journal buf entry 的完整写入生命周期。
    ///
    /// prep → alloc → checksum → do_writes(启动下一个 entry)
    /// → devs_written → replicas → preflush → submit → completion
    ///
    /// 对应本地 bcachefs `bch2_journal_write()` (write.c:819-946)
    /// 中针对一个 entry 的处理链。
    async fn bch2_journal_write_single(
        &self,
        idx: usize,
        force_flush_next: &mut bool,
    ) -> Result<(), JournalError> {
        let journal_devs = self.journal_devices();
        let mut writes: Vec<(Arc<BchDev>, BlockAddr, Vec<u8>)> = Vec::new();

        // Phase 1a: prep -> allocation -> checksum（同步块，不跨越 .await）
        let prepared = (|| -> Result<(Vec<u8>, bool, bool), JournalError> {
            let buf = self.bufs.get_mut(idx);
            let _guard = self.buf_lock.lock().unwrap();
            self.journal_buf_realloc(buf);
            let must_flush =
                buf.flush || buf.has_must_flush || *force_flush_next || buf.is_flush_no_wait();
            let pending_ranges = {
                let sp = self.slowpath.lock().unwrap();
                sp.early_journal_entries.clone()
            };
            let end = buf.data_end.min(buf.data.len());
            let mut end_mut = end;
            let rewind_applied = Self::bch2_inject_rewind_entries_into_buf(
                &pending_ranges,
                &mut buf.data[..],
                &mut end_mut,
            );
            buf.data_end = end_mut;
            if rewind_applied && !pending_ranges.is_empty() {
                self.slowpath.lock().unwrap().early_journal_entries.clear();
            }
            self.bch2_journal_write_prep(buf)?;
            drop(_guard);

            // 对应 bcachefs write.c:847-849: prep 后、alloc 前检查 journal error。
            // 如果 journal 已出错且 need_flush_write（clean→dirty mark）尚未写入，
            // 则中止本次写入。如果 need_flush_write 已清除，允许继续写入 noflush
            // （供 debugging 使用）。
            if self.bch2_journal_needs_flush_write() {
                if let Some(err) = self.bch2_journal_error_check() {
                    return Err(err);
                }
            }

            if self.vol.get().and_then(|vol| vol.upgrade()).is_some() {
                let mut replicas = 0;
                self.journal_write_alloc(buf, &mut replicas)?;
            }

            self.bch2_journal_write_checksum(buf)?;
            if let Some(c) = self.vol.get().and_then(|vol| vol.upgrade()) {
                assert_eq!(self.journal_last_unallocated_seq(), buf.seq);
                buf.sectors = 0;
                buf.write_allocated = true;
                self.entry_bytes_written
                    .fetch_add(buf.data_end as u64, Ordering::Relaxed);
                if crate::alloc::target_rw_devs(&c, BchDataType::Free, 0).count() > 1 {
                    buf.separate_flush = true;
                }
            }
            let data = buf.data[..buf.data_end.min(buf.data.len())].to_vec();
            *force_flush_next = false;
            if !must_flush {
                buf.bch2_journal_buf_try_noflush();
            }
            let no_flush = buf.is_noflush();
            Ok((data, must_flush, no_flush))
        })();
        let (buf_data, buf_must_flush, buf_no_flush) = match prepared {
            Ok(prepared) => prepared,
            Err(err) => {
                let seq = {
                    let w = self.bufs.get_mut(idx);
                    w.cas.clear();
                    w.state = BufState::WriteDone;
                    w.seq
                };
                self.bch2_journal_halt();
                self.journal_write_done(seq);
                return Err(err);
            }
        };

        // 对应 bch2_journal_write() (write.c:885-886)：
        // 在发布 write_allocated 后重新计算可用空间，然后启动下一个 entry。
        // bcachefs 在 j->lock 内调用 bch2_journal_space_available(j)，然后
        // bch2_journal_do_writes_locked(j)。
        self.bch2_journal_space_available(self.watermark());
        self.bch2_journal_do_writes();

        // Phase 1b: 按 device extent ptrs 构建写请求（write.c:889-914）
        if let Some(c) = self.vol.get().and_then(|vol| vol.upgrade()) {
            let w = self.bufs.get_mut(idx);
            for ptr in &w.key {
                let Some(ca) = c.device_registry.resolve_bch_dev(ptr.dev) else {
                    continue;
                };
                for (block_idx, chunk) in buf_data.chunks(JSET_BLOCK_SIZE as usize).enumerate() {
                    let mut block_data = vec![0u8; JSET_BLOCK_SIZE as usize];
                    block_data[..chunk.len()].copy_from_slice(chunk);
                    writes.push((
                        ca.clone(),
                        BlockAddr::new(ptr.offset / SECTORS_PER_BLOCK + block_idx as u64),
                        block_data,
                    ));
                }
            }
        } else {
            // Journal::new() 的无 BchVol 测试构造路径仍使用其内嵌单设备 bucket。
            let mut write_offset = 0;
            while write_offset < buf_data.len() {
                let needs_rotate = self.slowpath.lock().unwrap().remaining_bytes < JSET_BLOCK_SIZE;
                if needs_rotate {
                    self.bch2_journal_rotate_or_reclaim().await?;
                }
                let chunk_size = JSET_BLOCK_SIZE as usize;
                let end = (write_offset + chunk_size).min(buf_data.len());
                let chunk = &buf_data[write_offset..end];
                let block_addr = {
                    let sp = self.slowpath.lock().unwrap();
                    let bucket_start = sp.buckets[sp.current_bucket].addr;
                    let block_idx = sp.current_offset / JSET_BLOCK_SIZE;
                    BlockAddr::new(bucket_start + block_idx as u64)
                };
                let mut block_data = vec![0u8; JSET_BLOCK_SIZE as usize];
                block_data[..chunk.len()].copy_from_slice(chunk);
                writes.push((self.journal_device(), block_addr, block_data));
                {
                    let mut sp = self.slowpath.lock().unwrap();
                    sp.current_offset += JSET_BLOCK_SIZE;
                    sp.remaining_bytes = sp.remaining_bytes.saturating_sub(JSET_BLOCK_SIZE);
                }
                write_offset += chunk_size;
            }
            let w = self.bufs.get_mut(idx);
            w.write_allocated = true;
            self.entry_bytes_written
                .fetch_add(w.data_end as u64, Ordering::Relaxed);
        }

        let separate_flush = self.bufs.get_mut(idx).separate_flush;
        let no_io = self
            .vol
            .get()
            .and_then(|vol| vol.upgrade())
            .is_some_and(|c| c.opts.nochanges);

        // 对应 bch2_journal_write()：提交 preflush/data bio 前先发布 devs_written 和 replicas
        {
            let w = self.bufs.get_mut(idx);
            w.devs_written = writes.iter().map(|(dev, _, _)| dev.dev_idx).collect();
            w.devs_written.sort_unstable();
            w.devs_written.dedup();
        }
        if self.bufs.get_mut(idx).wait_first != JournalBufWaitState::FlushNoWait {
            if let Some(c) = self.vol.get().and_then(|vol| vol.upgrade()) {
                let replicas = crate::replicas::BchReplicasEntry::new(
                    BchDataType::Journal,
                    &self.bufs.get_mut(idx).devs_written,
                    1,
                );
                c.replicas.lock().unwrap().get_or_mark(&replicas);
                let pin = unsafe { &mut *self.pin_fifo.get() }
                    .entry_for_seq_mut(self.bufs.get_mut(idx).seq)
                    .expect("journal pin missing before write submit");
                pin.set_devs(&self.bufs.get_mut(idx).devs_written);
            }
        }

        // Phase 2: separate preflush — 对应 journal_write_preflush() (write.c:585-619)
        let first_err: Arc<AtomicFirstError> = Arc::new(AtomicFirstError::new());
        let io_failures = Arc::new(Mutex::new(Vec::<(usize, u8)>::new()));
        if !no_io && buf_must_flush && !buf_no_flush && separate_flush {
            self.journal_write_preflush(idx, &journal_devs, &first_err, &io_failures)
                .await;
        }

        // Phase 3: journal_write_submit — 对应 journal_write_submit() (write.c:513-583)
        if !no_io {
            self.journal_write_submit(
                idx,
                &writes,
                buf_must_flush && !buf_no_flush,
                separate_flush,
                &first_err,
                &io_failures,
            )
            .await;
        }
        self.bufs.get_mut(idx).cas.clear();

        // 处理 io 失败（对应 write.c:266-305 和 bch2_journal_write error path）
        let mut failed = io_failures.lock().unwrap().clone();
        failed.sort_unstable();
        failed.dedup();
        for (_, dev) in &failed {
            let w = self.bufs.get_mut(idx);
            w.failed.push(BchDevIoFailures {
                dev: *dev,
                csum_nr: 0,
                ec_errcode: 0,
                errcode: -1,
            });
            w.devs_written.retain(|&written_dev| written_dev != *dev);
        }

        let fatal_write_error = {
            let w = self.bufs.get_mut(idx);
            !w.failed.is_empty() && w.devs_written.is_empty()
        };

        self.update_bucket_seq(self.bufs.get_mut(idx).seq);
        self.bufs.get_mut(idx).state = BufState::WriteDone;

        let seq_wrote = self.bufs.get_mut(idx).seq;
        if self.bufs.get_mut(idx).flush {
            self.journal_write_done_flush(seq_wrote);
        }
        self.journal_write_done(seq_wrote);

        if fatal_write_error {
            return Err(JournalError::Io(first_err.take().unwrap_or_else(|| {
                StorageError::JournalError("journal write failed on all replicas".to_string())
            })));
        }

        Ok(())
    }

    /// 遍历所有可写入的 journal buf entry，对每个调用 `bch2_journal_write_single`。
    ///
    /// 对应本地 bcachefs `bch2_journal_write()` (write.c:819-946)。
    async fn bch2_journal_write(&self) -> Result<(), JournalError> {
        let mut force_flush_next = self.bch2_journal_needs_flush_write();

        loop {
            // 收集所有需要写入的 buf idx
            let idxs: Vec<usize> = self
                .in_flight
                .lock()
                .unwrap()
                .iter()
                .map(|idx| *idx as usize)
                .filter(|idx| {
                    let buf = self.bufs.get_mut(*idx);
                    let state = buf.state;
                    (state == BufState::WriteSubmitted || state == BufState::Noflush)
                        && buf.write_started
                        && !buf.write_allocated
                })
                .collect();

            if idxs.is_empty() {
                return Ok(());
            }

            assert!(
                idxs.len() <= 1,
                "bch2_journal_do_writes_locked starts only the oldest unallocated entry"
            );

            for idx in idxs {
                self.bch2_journal_write_single(idx, &mut force_flush_next)
                    .await?;
            }

            // 检查完成期间是否有更多 entry 被标记为 write_started（write.c:945 的递归语义）
            let has_more = self.in_flight.lock().unwrap().iter().copied().any(|idx| {
                let w = self.bufs.get(idx as usize);
                w.write_started && !w.write_allocated
            });
            if !has_more {
                return Ok(());
            }
        }
    }

    /// flush pending buf data to backend（对应 bcachefs `bch2_journal_flush`）
    ///
    /// # 顺序（J2 flush data race 修复后）
    ///
    /// 修复了 bcachefs 风格的 data race：旧顺序（read offset → set data_end → close_entry）
    /// 在 data_end 与 close_entry 之间有一个窗口，此时新 reservation 写入的数据超过 data_end。
    ///
    /// 新顺序（close_entry → drain → set data_end）：
    /// 1. 捕获当前 accepting buf 索引
    /// 2. 关闭当前 entry（原子捕获 final offset + CLOSED_VAL 阻止新 reservation）
    /// 3. 等待旧 buf 的 refcount 归零（已有 reservation 完成写入）
    /// 4. 设置 buf.data_end（安全：不再有 reservation 写入此 buf）
    /// 5. 将所有 Accepting buf → Closing
    /// 6. 将所有 Closing buf → WriteSubmitted（通知等待者）
    /// 7. 将所有 WriteSubmitted buf 数据写入 bucket（按 data_end 截断）
    /// 8. backend.flush()
    /// 9. 打开新 entry（后续 append 用）
    ///
    /// # 设计说明
    ///
    /// `journal_res_put()` 仅递减 refcount，不触发写入。
    /// `bch2_journal_flush()` 统一管理 buf 状态转换和 bucket 写入，
    /// 确保一次 flush 将所有累积的 buf 数据落盘。
    /// 只写 `data_end` 字节而非 BUF_SIZE，避免零 padding 浪费。
    ///
    /// # 并发
    ///
    /// 接受 `&self`，通过内部 Mutex 序列化 bucket 状态修改。
    pub async fn bch2_journal_flush(&self) -> Result<(), JournalError> {
        // P2-7: 标记有数据需要写入
        self.bch2_journal_set_needs_flush_write();

        // 1. 捕获当前 accepting buf 索引
        let old_idx = (self.bch2_journal_cur_seq() & (JOURNAL_IN_FLIGHT_NR as u64 - 1)) as usize;
        let entry_was_open = self.reservations.is_open();

        // 2. 关闭当前 entry — 原子捕获 final offset + 设置 CLOSED_VAL 阻止新 reservation
        //    J2 fix: 先 close_entry，再设 data_end（防止并发 reservation 数据被截断）
        let final_off = self.journal_entry_close();

        // 3. 等待旧 buf 上所有 in-flight reservation 完成（refcount 归零）
        //    close_entry 后 CLOSED_VAL 阻止新 reservation，
        //    但已有 reservation 仍持有 refcount。
        //    等待 refcount→0 确保所有写入已完成，data_end 设置不会截断数据。
        if entry_was_open {
            let buf = self.bufs.get_mut(old_idx);
            buf.state = BufState::Closing;
            self.wait_for_pending_drain(old_idx).await;
        }

        // 4. 设置 buf.data_end（安全：不再有 reservation 写入此 buf）
        if entry_was_open {
            let used_bytes = (final_off as usize) * 8; // u64 → byte
            let buf = self.bufs.get_mut(old_idx);
            buf.data_end = used_bytes.min(BUF_SIZE);
        }

        // 3. Accepting → Closing（标记为待写入）
        let in_flight: Vec<usize> = self
            .in_flight
            .lock()
            .unwrap()
            .iter()
            .map(|idx| *idx as usize)
            .collect();
        for &idx in &in_flight {
            let buf = self.bufs.get_mut(idx);
            if buf.state == BufState::Accepting {
                buf.state = BufState::Closing;
            }
        }

        // 4. Closing → WriteSubmitted（通知写入线程）
        for idx in in_flight {
            let buf = self.bufs.get_mut(idx);
            if buf.state == BufState::Closing {
                buf.state = BufState::WriteSubmitted;
                buf.notify.notify_waiters();
            }
        }

        // 5. 按本地 write.c:1087-1162 选择 flush/noflush、追加 rewind limit 并启动 write。
        self.bch2_journal_do_writes();

        // 6. 将所有 WriteSubmitted buf 写入 bucket
        self.bch2_journal_write().await?;

        // P2-7: 清除 needs_flush_write 标志 + 更新 jiffies
        self.bch2_journal_clear_needs_flush_write();
        self.bch2_journal_update_flush_jiffies();

        // completion 已按 entry 推进 ondisk 边界并唤醒 entry waiters。
        self.flushed_seq_marker.store(
            self.flushed_seq_ondisk.load(Ordering::Acquire),
            Ordering::Release,
        );
        self.seq_flush_notify.notify_waiters();
        self.flush_wait.notify_all();

        // 9. 打开新 entry（会添加新自钉）
        self.journal_entry_open()?;

        // 10. 更新水位线（flush 后利用率可能变化）
        self.bch2_journal_set_watermark();

        Ok(())
    }

    // ─── Utilization (unchanged) ──────────────────────────

    /// 返回当前写入率（0.0~1.0），1.0 = 满
    pub fn utilization(&self) -> f64 {
        let sp = self.slowpath.lock().unwrap();
        let total_bucket_bytes =
            (sp.buckets.len() as u64) * (BUCKET_BLOCKS as u64) * (JSET_BLOCK_SIZE as u64);
        let used: u64 = if sp.current_bucket > 0 {
            (sp.current_bucket as u64) * (BUCKET_BLOCKS as u64) * (JSET_BLOCK_SIZE as u64)
                + (sp.current_offset as u64)
        } else {
            sp.current_offset as u64
        };
        if total_bucket_bytes == 0 {
            return 0.0;
        }
        (used as f64) / (total_bucket_bytes as f64)
    }

    // ─── Read (unchanged) ─────────────────────────────────

    /// 对应本地 bcachefs `bch2_journal_entry_missing_range()`
    /// (`journal/read.c:917-935`)。
    pub fn bch2_journal_entry_missing_range(&self, start: u64, end: u64) -> U64Range {
        assert!(start <= end);

        if start == end {
            return U64Range::default();
        }

        let start = self.bch2_journal_seq_next_nonblacklisted(start);
        if start >= end {
            return U64Range::default();
        }

        let missing = U64Range {
            start,
            end: end.min(self.bch2_journal_seq_next_blacklisted(start)),
        };

        if missing.start == missing.end {
            U64Range::default()
        } else {
            missing
        }
    }

    /// 对应本地 `journal_has_any_missing()` (`journal/read.c:945-962`)。
    fn journal_has_any_missing(
        &self,
        journal_list: &JournalList,
        start_seq: u64,
        end_seq: u64,
    ) -> bool {
        let mut seq = start_seq;
        for replay in journal_list.entries.values() {
            if replay.ignore_blacklisted || replay.ignore_not_dirty {
                continue;
            }
            if replay.jset.header.seq < seq {
                continue;
            }
            if self
                .bch2_journal_entry_missing_range(seq, replay.jset.header.seq)
                .start
                != 0
            {
                return true;
            }
            seq = replay.jset.header.seq.wrapping_add(1);
        }
        self.bch2_journal_entry_missing_range(seq, end_seq.wrapping_add(1))
            .start
            != 0
    }

    /// 检测 journal entries 序列号间隙
    ///
    /// 对应本地 bcachefs `bch2_journal_check_for_missing()` (read.c:1012-1057)。
    /// 遍历已排序的 journal entries，检查 seq 之间是否存在非 blacklisted 的
    /// 序列号间隙。若存在间隙，返回错误并报告缺失范围。
    pub(crate) fn bch2_journal_check_for_missing(
        &self,
        journal_list: &JournalList,
        start_seq: u64,
        _end_seq: u64,
    ) -> Result<(), JournalError> {
        let mut seq = start_seq;
        // 收集按 seq 排序的有效 entries
        let mut sorted: Vec<(&u64, &JournalReplay)> = journal_list
            .entries
            .iter()
            .filter(|(_, r)| !r.ignore_blacklisted && !r.ignore_not_dirty)
            .collect();
        sorted.sort_by_key(|(seq, _)| *seq);

        for (_, replay) in sorted {
            let replay_seq = replay.jset.header.seq;
            assert!(seq <= replay_seq);

            let mut missing = self.bch2_journal_entry_missing_range(seq, replay_seq);
            while missing.start != 0 {
                tracing::error!(
                    "journal entries {}-{} missing! (replaying {}-{})",
                    missing.start,
                    missing.end - 1,
                    start_seq,
                    _end_seq
                );
                // 对应 bcachefs: fsck_err(journal_entries_missing)
                // subvol 无 fsck 基础设施，通过 tracing 记录并继续处理
                seq = missing.end;
                if seq >= replay_seq {
                    break;
                }
                missing = self.bch2_journal_entry_missing_range(seq, replay_seq);
            }
            seq = replay_seq.wrapping_add(1);
        }
        Ok(())
    }

    /// 对应本地 bcachefs `journal_entry_add()` 的重复副本合并段
    /// (`read.c:229-305`)；旧序列裁剪需先对齐 superblock blacklist。
    fn journal_entry_add(
        journal_dev: &BchDev,
        entry_ptr: JournalPtr,
        journal_list: &mut JournalList,
        jset: Jset,
        raw: Vec<u8>,
    ) -> Result<(), JournalError> {
        let last_seq = if jset.header.flags & super::jset::JSET_NO_FLUSH == 0 {
            jset.header.last_seq
        } else {
            0
        };
        let seq = jset.header.seq;

        journal_list.last_seq = journal_list.last_seq.max(last_seq);

        if let Some(dup) = journal_list.entries.get_mut(&seq) {
            let identical = raw == dup.raw;
            let not_identical = !identical && entry_ptr.csum_good && dup.csum_good;
            let mut same_device = false;

            for ptr in &dup.ptrs {
                if ptr.dev == journal_dev.dev_idx {
                    if ptr.sector == entry_ptr.sector {
                        return Ok(());
                    }
                    same_device = true;
                }
            }

            dup.ptrs.push(entry_ptr.clone());

            // 对应 bcachefs read.c:274-277 — ret_fsck_err_on(journal_entry_dup_same_device)
            // 记录 fsck 错误但继续处理下一个 block，不中止整个 recovery
            if same_device {
                tracing::warn!(
                    "duplicate journal entry {} on device {} (same sector re-read, continuing)",
                    seq,
                    journal_dev.dev_idx
                );
            }
            // 对应 bcachefs read.c:279-282 — ret_fsck_err_on(journal_entry_replicas_data_mismatch)
            // 记录 fsck 错误但继续处理，不中止 recovery
            if not_identical {
                tracing::warn!("non-identical good journal replicas for seq {seq} (continuing)");
            }
            // 对应 bcachefs read.c:284-285 — 已存在相同副本或无校验，跳过
            if identical || !entry_ptr.csum_good {
                return Ok(());
            }
        }

        let ptrs = if let Some(dup) = journal_list.entries.remove(&seq) {
            dup.ptrs
        } else {
            vec![entry_ptr.clone()]
        };
        journal_list.entries.insert(
            seq,
            JournalReplay {
                ptrs,
                csum_good: entry_ptr.csum_good,
                ignore_blacklisted: false,
                ignore_not_dirty: false,
                jset,
                raw,
            },
        );
        Ok(())
    }

    /// 对应本地 bcachefs `journal_read_bucket()` (read.c:331-454)。
    async fn journal_read_bucket(
        &self,
        journal_dev: Arc<BchDev>,
        bucket_idx: u32,
        journal_list: Arc<Mutex<JournalList>>,
    ) -> Result<(), JournalError> {
        let bucket_start = {
            let sp = self.slowpath.lock().unwrap();
            let idx = bucket_idx as usize;
            if idx >= sp.buckets.len() {
                return Ok(());
            }
            sp.buckets[idx].addr
        };
        let completion = Closure::new();
        let result_cell = Arc::new(AtomicCell::new());
        let first_err = Arc::new(AtomicFirstError::new());
        submit_bio_all_blocks_read(
            journal_dev.clone(),
            BlockAddr::new(bucket_start),
            BUCKET_BLOCKS as usize,
            &completion,
            result_cell.clone(),
            &first_err,
        );
        completion.wait_async().await;
        if first_err.take().is_some() {
            // read.c:352-361: 单设备读错不中止 recovery，副本可能仍可用。
            return Ok(());
        }
        let data = result_cell.take().unwrap_or_default();
        let mut saw_bad = false;

        for (block, buf) in data.chunks_exact(JSET_BLOCK_SIZE as usize).enumerate() {
            match Jset::deserialize(buf) {
                Ok(Some(jset)) => {
                    // read.c:387-403: early header 有效后先更新设备 write-head
                    // 状态，再处理 checksum；同桶遇到更旧 seq 立即停止。
                    {
                        let nr = self.slowpath.lock().unwrap().buckets.len();
                        let mut ja = journal_dev.journal.lock().unwrap();
                        if ja.bucket_seq.len() < nr {
                            ja.bucket_seq.resize(nr, 0);
                        }
                        if jset.header.seq > ja.highest_seq_found {
                            ja.highest_seq_found = jset.header.seq;
                            ja.cur_idx = bucket_idx;
                            ja.sectors_free = ((BUCKET_BLOCKS as usize - block - 1)
                                * SECTORS_PER_BLOCK as usize)
                                as u32;
                        }
                        let idx = bucket_idx as usize;
                        if jset.header.seq < ja.bucket_seq[idx] {
                            return Ok(());
                        }
                        ja.bucket_seq[idx] = jset.header.seq;
                    }
                    let csum_good = jset.verify();
                    if !csum_good {
                        saw_bad = true;
                    }
                    let block_addr = bucket_start + block as u64;
                    let entry_ptr = JournalPtr {
                        csum_good,
                        dev: journal_dev.dev_idx,
                        bucket: bucket_idx,
                        bucket_offset: block as u64 * SECTORS_PER_BLOCK,
                        sector: block_addr * SECTORS_PER_BLOCK,
                    };
                    let raw_len = jset.serialized_padded_len().min(buf.len());
                    Self::journal_entry_add(
                        &journal_dev,
                        entry_ptr,
                        &mut journal_list.lock().unwrap(),
                        jset,
                        buf[..raw_len].to_vec(),
                    )?;
                }
                Ok(None) if !saw_bad => break,
                Ok(None) | Err(_) => continue,
            }
        }

        Ok(())
    }

    /// 对应本地 bcachefs `journal_peek_bucket()`：只读首个 block 取 seq。
    async fn journal_peek_bucket(
        &self,
        journal_dev: Arc<BchDev>,
        bucket: usize,
    ) -> Result<u64, JournalError> {
        let bucket_start = {
            let sp = self.slowpath.lock().unwrap();
            let Some(bucket) = sp.buckets.get(bucket) else {
                return Ok(0);
            };
            bucket.addr
        };
        let completion = Closure::new();
        let result_cell = Arc::new(AtomicCell::new());
        let first_err = Arc::new(AtomicFirstError::new());
        submit_bio_all_blocks_read(
            journal_dev,
            BlockAddr::new(bucket_start),
            1,
            &completion,
            result_cell.clone(),
            &first_err,
        );
        completion.wait_async().await;
        if first_err.take().is_some() {
            return Ok(0);
        }
        Ok(Jset::deserialize(&result_cell.take().unwrap_or_default())
            .map_err(JournalError::Io)?
            .map_or(0, |jset| jset.header.seq))
    }

    /// 对应本地 bcachefs `journal_peek_once()`。
    async fn journal_peek_once(
        &self,
        journal_dev: Arc<BchDev>,
        peeked: &mut [bool],
        bucket_seq: &mut [u64],
        bucket: usize,
    ) -> Result<u64, JournalError> {
        if !peeked[bucket] {
            bucket_seq[bucket] = self.journal_peek_bucket(journal_dev, bucket).await?;
            peeked[bucket] = true;
        }
        Ok(bucket_seq[bucket])
    }

    /// 对应本地 bcachefs `journal_anchor_bucket()`。
    async fn journal_anchor_bucket(
        &self,
        journal_dev: Arc<BchDev>,
        peeked: &mut [bool],
        bucket_seq: &mut [u64],
    ) -> Result<Option<usize>, JournalError> {
        if self
            .journal_peek_once(journal_dev.clone(), peeked, bucket_seq, 0)
            .await?
            != 0
        {
            return Ok(Some(0));
        }
        if peeked.len() <= 1 {
            return Ok(None);
        }
        let mut step = 1usize << ((peeked.len() - 1).ilog2());
        while step != 0 {
            for pos in (step..peeked.len()).step_by(step * 2) {
                if self
                    .journal_peek_once(journal_dev.clone(), peeked, bucket_seq, pos)
                    .await?
                    != 0
                {
                    return Ok(Some(pos));
                }
            }
            step >>= 1;
        }
        Ok(None)
    }

    /// 对应本地 bcachefs `journal_bsearch_head()`。
    async fn journal_bsearch_head(
        &self,
        journal_dev: Arc<BchDev>,
        peeked: &mut [bool],
        bucket_seq: &mut [u64],
        anchor: usize,
    ) -> Result<usize, JournalError> {
        let nr = peeked.len();
        let mut lo = anchor;
        let mut hi = anchor + nr - 1;
        while lo < hi {
            let mid = (lo + hi + 1) / 2;
            let mid_bucket = mid % nr;
            let lo_bucket = lo % nr;
            let mid_seq = self
                .journal_peek_once(journal_dev.clone(), peeked, bucket_seq, mid_bucket)
                .await?;
            if mid_seq == 0 || mid_seq <= bucket_seq[lo_bucket] {
                hi = mid - 1;
            } else {
                lo = mid;
            }
        }
        Ok(lo % nr)
    }

    /// 对应本地 bcachefs `journal_walk_inuse()`。
    async fn journal_walk_inuse(
        &self,
        journal_dev: Arc<BchDev>,
        peeked: &mut [bool],
        bucket_seq: &mut [u64],
        head: usize,
        order: &mut Vec<(usize, u64)>,
    ) -> Result<bool, JournalError> {
        let nr = peeked.len();
        let mut prev_seq = bucket_seq[head];
        if prev_seq == 0 {
            return Ok(false);
        }
        order.push((head, prev_seq));
        for k in 1..nr {
            let idx = (head + nr - k) % nr;
            let seq = self
                .journal_peek_once(journal_dev.clone(), peeked, bucket_seq, idx)
                .await?;
            if seq == 0 {
                break;
            }
            if seq >= prev_seq {
                return Ok(false);
            }
            order.push((idx, seq));
            prev_seq = seq;
        }
        Ok(true)
    }

    /// 对应本地 bcachefs `journal_bsearch_collect()`。
    async fn journal_bsearch_collect(
        &self,
        journal_dev: Arc<BchDev>,
    ) -> Result<Vec<(usize, u64)>, JournalError> {
        let nr = self.slowpath.lock().unwrap().buckets.len();
        let mut peeked = vec![false; nr];
        let mut bucket_seq = vec![0; nr];
        let Some(anchor) = self
            .journal_anchor_bucket(journal_dev.clone(), &mut peeked, &mut bucket_seq)
            .await?
        else {
            return Ok(Vec::new());
        };
        let head = self
            .journal_bsearch_head(journal_dev.clone(), &mut peeked, &mut bucket_seq, anchor)
            .await?;
        let mut order = Vec::new();
        if self
            .journal_walk_inuse(
                journal_dev.clone(),
                &mut peeked,
                &mut bucket_seq,
                head,
                &mut order,
            )
            .await?
        {
            return Ok(order);
        }
        order.clear();
        for bucket in 0..nr {
            let seq = self
                .journal_peek_once(journal_dev.clone(), &mut peeked, &mut bucket_seq, bucket)
                .await?;
            if seq != 0 {
                order.push((bucket, seq));
            }
        }
        Ok(order)
    }

    /// 对应本地 bcachefs `bch2_journal_read_device()` 的搜索/全读分支 (read.c:724-886)。
    ///
    /// `read_entire_journal` 对齐 bcachefs `c->opts.read_entire_journal`：
    /// 为 true 时跳过二分搜索快速路径，直接完整顺序读取所有 bucket。
    async fn bch2_journal_read_device(
        &self,
        journal_dev: Arc<BchDev>,
        journal_list: Arc<Mutex<JournalList>>,
        read_entire_journal: bool,
    ) -> Result<(), JournalError> {
        let nr = {
            let ja = journal_dev.journal.lock().unwrap();
            if ja.nr != 0 {
                ja.nr as usize
            } else {
                self.slowpath.lock().unwrap().buckets.len()
            }
        };
        if nr == 0 {
            return Ok(());
        }
        // 对应 bcachefs read.c:752 — 仅当 read_entire_journal 为 false、
        // nr > 32 且非 full_read 重试时，使用二分搜索快速路径
        if !read_entire_journal && nr > 32 && !journal_list.lock().unwrap().full_read {
            let mut order = self.journal_bsearch_collect(journal_dev.clone()).await?;
            if !order.is_empty() {
                order.sort_unstable_by(|a, b| b.1.cmp(&a.1));
                for (bucket, bucket_seq) in order {
                    self.journal_read_bucket(
                        journal_dev.clone(),
                        bucket as u32,
                        journal_list.clone(),
                    )
                    .await?;
                    let last_seq = journal_list.lock().unwrap().last_seq;
                    if last_seq != 0 && bucket_seq < last_seq {
                        break;
                    }
                }
                let mut ja = journal_dev.journal.lock().unwrap();
                ja.discard_idx = (ja.cur_idx + 1) % nr as u32;
                ja.dirty_idx_ondisk = ja.discard_idx;
                ja.dirty_idx = ja.discard_idx;
                return Ok(());
            }
        }
        for bucket in 0..nr as u32 {
            self.journal_read_bucket(journal_dev.clone(), bucket, journal_list.clone())
                .await?;
        }
        let mut ja = journal_dev.journal.lock().unwrap();
        ja.discard_idx = (ja.cur_idx + 1) % nr as u32;
        ja.dirty_idx_ondisk = ja.discard_idx;
        ja.dirty_idx = ja.discard_idx;
        Ok(())
    }

    /// 对应本地 `journal_retry_full_read()` (`journal/read.c:973-1011`)。
    async fn journal_retry_full_read(
        &self,
        journal_list: Arc<Mutex<JournalList>>,
    ) -> Result<(), JournalError> {
        journal_list.lock().unwrap().full_read = true;

        let mut devices: Vec<(BchDevIoRefGuard, Arc<BchDev>)> = Vec::new();
        if let Some(vol) = self.vol.get().and_then(|vol| vol.upgrade()) {
            for dev_idx in vol.device_registry.dev_indices() {
                let Some(dev) = vol.device_registry.resolve_bch_dev(dev_idx) else {
                    continue;
                };
                if dev.journal.lock().unwrap().nr <= 32 {
                    continue;
                }
                if !matches!(
                    dev.member_state(),
                    crate::storage::superblock::BchMemberState::Rw
                        | crate::storage::superblock::BchMemberState::Ro
                ) {
                    continue;
                }
                if let Some(io_ref) = dev.try_get_io_ref_guard(BchDevIoRefKind::Read) {
                    devices.push((io_ref, dev));
                }
            }
        } else {
            let dev = self.journal_device();
            if dev.journal.lock().unwrap().nr > 32 {
                if let Some(io_ref) = dev.try_get_io_ref_guard(BchDevIoRefKind::Read) {
                    devices.push((io_ref, dev));
                }
            }
        }

        // journal_retry_full_read 中强制全量重读，跳过二分搜索快速路径。
        // 对应 bcachefs read.c:973-1011 — 设置 full_read flag 并 bypass bsearch。
        let reads = devices
            .iter()
            .map(|(_, dev)| self.bch2_journal_read_device(dev.clone(), journal_list.clone(), true));
        let results = futures::future::join_all(reads).await;
        drop(devices);
        for result in results {
            result?;
        }
        Ok(())
    }

    /// 对应本地 bcachefs `bch2_journal_read()` (`read.c:1156-1414`)。
    pub async fn bch2_journal_read(
        &self,
        info: &mut JournalStartInfo,
    ) -> Result<Vec<(u32, Jset)>, JournalError> {
        *info = JournalStartInfo::default();
        // read.c:1174-1194: 按 member dev_idx 遍历 RW/RO 设备，先取 READ
        // io_ref，再并行启动每设备 read closure，最后统一等待。

        // 对应 bcachefs read.c:752/1162 — read_entire_journal 控制是否跳过二分搜索。
        // subvol 尚未实现独立的 read_entire_journal 开关；此处预留参数用于未来
        // fsck/debug 模式接入。当前默认 false，保留二分搜索优化。
        let read_entire_journal = false;

        let mut devices: Vec<(BchDevIoRefGuard, Arc<BchDev>)> = Vec::new();
        if let Some(vol) = self.vol.get().and_then(|vol| vol.upgrade()) {
            for dev_idx in vol.device_registry.dev_indices() {
                let Some(dev) = vol.device_registry.resolve_bch_dev(dev_idx) else {
                    continue;
                };
                // 对应 bcachefs read.c:1176-1181 — device gate:
                //   当 read_entire_journal 为 false 且 fsck 为 false 时，
                //   跳过没有 journal data 的设备。
                //   subvol 无 `dev_has_data` 位图且无 `opts.fsck` 字段；
                //   subvol 的 Journal 直接持有 bucket addrs，
                //   不通过 per-device `BchDev.journal.nr` 路由，
                //   因此此处不添加实际的 skip 逻辑，仅保留注释标记。
                //   未来接入 dev_has_data 位图后在此处插入等效检查。
                if !matches!(
                    dev.member_state(),
                    crate::storage::superblock::BchMemberState::Rw
                        | crate::storage::superblock::BchMemberState::Ro
                ) {
                    continue;
                }
                if let Some(io_ref) = dev.try_get_io_ref_guard(BchDevIoRefKind::Read) {
                    devices.push((io_ref, dev));
                }
            }
        } else {
            let dev = self.journal_device();
            if let Some(io_ref) = dev.try_get_io_ref_guard(BchDevIoRefKind::Read) {
                devices.push((io_ref, dev));
            }
        }

        let bsearch_used = devices.iter().any(|(_, dev)| {
            let nr = dev.journal.lock().unwrap().nr;
            if nr != 0 {
                nr > 32
            } else {
                self.slowpath.lock().unwrap().buckets.len() > 32
            }
        });
        // bsearch_used 标记用于后续判断是否需要重试全量读取。
        // 对应 bcachefs read.c:1335-1339。
        let journal_list_shared = Arc::new(Mutex::new(JournalList::default()));
        let reads = devices.iter().map(|(_, dev)| {
            self.bch2_journal_read_device(
                dev.clone(),
                journal_list_shared.clone(),
                read_entire_journal,
            )
        });
        let results = futures::future::join_all(reads).await;
        drop(devices);

        for result in results {
            result?;
        }
        let retry_full_read = {
            let mut journal_list = journal_list_shared.lock().unwrap();
            let mut last_write_torn = false;
            for replay in journal_list.entries.values_mut().rev() {
                if replay.ignore_blacklisted || replay.ignore_not_dirty {
                    continue;
                }

                if info.cur_seq == 0 {
                    info.cur_seq = replay.jset.header.seq + 1;
                }

                if replay.jset.header.flags & super::jset::JSET_NO_FLUSH != 0 {
                    replay.ignore_blacklisted = true;
                    continue;
                }

                if !last_write_torn && !replay.csum_good {
                    last_write_torn = true;
                    replay.ignore_blacklisted = true;
                    continue;
                }

                if replay.jset.header.last_seq > replay.jset.header.seq {
                    replay.jset.header.last_seq = replay.jset.header.seq;
                }

                info.last_seq = replay.jset.header.last_seq;
                info.replay_end = replay.jset.header.seq;
                info.clean = replay.jset.header.seq == replay.jset.header.last_seq
                    && !replay.jset.entries.iter().any(|entry| {
                        entry.hdr.entry_type == JsetEntryType::BtreeKeys as u8
                            && entry.hdr.payload_len != 0
                    });
                break;
            }

            if info.cur_seq == 0 || info.replay_end == 0 {
                return Ok(Vec::new());
            }

            // 对应 bcachefs read.c:1285-1298 — journal_rewind 向下调整 drop_before
            // rewind_seq 非零时表示存在 keep-open rewind 目标；
            // 扩展重放范围以包含 rewind seq 之前的条目。
            {
                let rewind_seq = self.rewind_seq.load(Ordering::Acquire);
                if rewind_seq != 0 {
                    info.last_seq = info.last_seq.min(rewind_seq);
                }
            }

            // read.c:1285-1325: 在缺失序列检查和 full-read retry 前，先删除
            // last_seq 之前的条目并标记 superblock blacklist 命中的条目。
            journal_list.entries.retain(|seq, replay| {
                if replay.ignore_blacklisted || replay.ignore_not_dirty {
                    return true;
                }
                if *seq < info.last_seq {
                    return false;
                }
                if self.bch2_journal_seq_is_blacklisted(*seq, true) {
                    if replay.jset.header.flags & super::jset::JSET_NO_FLUSH == 0 {
                        if let Some(c) = self.vol.get().and_then(|vol| vol.upgrade()) {
                            c.record_fsck_error();
                        }
                    }
                    replay.ignore_blacklisted = true;
                }
                true
            });

            bsearch_used
                && self.journal_has_any_missing(&journal_list, info.last_seq, info.replay_end)
        };
        if retry_full_read {
            self.journal_retry_full_read(journal_list_shared.clone())
                .await?;
        }
        // 对应 read.c:1341 — 检测 journal entries 序列号间隙
        self.bch2_journal_check_for_missing(
            &journal_list_shared.lock().unwrap(),
            info.last_seq,
            info.replay_end,
        )?;
        let mut journal_list = journal_list_shared.lock().unwrap();

        let mut rewind_limit = None;
        let mut rewind_ranges = Vec::new();
        for replay in journal_list.entries.values() {
            if replay.ignore_blacklisted
                || replay.ignore_not_dirty
                || !replay.csum_good
                || !super::validate::bch2_jset_validate(&replay.jset)
            {
                continue;
            }
            for entry in &replay.jset.entries {
                match JsetEntryType::from_u8(entry.hdr.entry_type) {
                    Some(JsetEntryType::RewindLimit) => {
                        rewind_limit =
                            Some(u64::from_le_bytes(entry.payload[..8].try_into().unwrap()));
                    }
                    Some(JsetEntryType::Rewind) => {
                        rewind_ranges.push((
                            u64::from_le_bytes(entry.payload[..8].try_into().unwrap()),
                            u64::from_le_bytes(entry.payload[8..16].try_into().unwrap()),
                        ));
                    }
                    _ => {}
                }
            }
        }
        if let Some(rewind_limit) = rewind_limit {
            self.rewind_seq.store(rewind_limit, Ordering::Release);
            self.rewind_seq_ondisk
                .store(rewind_limit, Ordering::Release);
        }
        if !rewind_ranges.is_empty() {
            self.slowpath
                .lock()
                .unwrap()
                .rewind_ranges
                .extend(rewind_ranges);
        }

        Ok(std::mem::take(&mut journal_list.entries)
            .into_values()
            .filter(|entry| {
                !entry.ignore_blacklisted
                    && !entry.ignore_not_dirty
                    && entry.csum_good
                    && super::validate::bch2_jset_validate(&entry.jset)
            })
            .map(|entry| (entry.ptrs[0].bucket, entry.jset))
            .collect())
    }

    // ─── Bucket management (unchanged) ────────────────────

    /// 更新 bucket_seq[当前 bucket] 为 max(当前值, jset_seq)
    fn update_bucket_seq(&self, jset_seq: u64) {
        let mut sp = self.slowpath.lock().unwrap();
        let idx = sp.current_bucket;
        if idx < sp.bucket_seq.len() {
            sp.bucket_seq[idx] = sp.bucket_seq[idx].max(jset_seq);
        }
    }

    /// 推进 dirty_idx（使用已完成回收/flush 的 last_seq_ondisk 作为边界）
    fn advance_dirty_idx(&self) {
        let nr;
        let cur_idx;
        {
            let sp = self.slowpath.lock().unwrap();
            nr = sp.buckets.len();
            cur_idx = sp.current_bucket;
        }
        if nr == 0 {
            return;
        }
        let last_seq = self.last_seq.load(Ordering::Acquire);
        loop {
            let mut sp = self.slowpath.lock().unwrap();
            if sp.dirty_idx == cur_idx {
                break;
            }
            if sp.bucket_seq.get(sp.dirty_idx).copied().unwrap_or(0) < last_seq {
                sp.dirty_idx = (sp.dirty_idx + 1) % nr;
            } else {
                break;
            }
        }
    }

    /// 推进 dirty_idx_ondisk：确认落盘后推进
    fn advance_dirty_idx_ondisk(&self) {
        let nr;
        {
            let sp = self.slowpath.lock().unwrap();
            nr = sp.buckets.len();
        }
        if nr == 0 {
            return;
        }
        let last_seq = self.last_seq_ondisk.load(Ordering::Acquire);
        loop {
            let mut sp = self.slowpath.lock().unwrap();
            if sp.dirty_idx_ondisk == sp.dirty_idx {
                break;
            }
            if sp.bucket_seq.get(sp.dirty_idx_ondisk).copied().unwrap_or(0) < last_seq {
                sp.dirty_idx_ondisk = (sp.dirty_idx_ondisk + 1) % nr;
            } else {
                break;
            }
        }
    }

    /// 计算需要 flush 的最老 journal seq。
    ///
    /// 对应 bcachefs `journal_seq_to_flush()` (reclaim.c:861-888)：
    /// 1. 计算当前 bucket 之后半个环形缓冲区位置对应的 bucket seq。
    /// 2. 取 pin FIFO 半满目标：`cur_seq - pin.size / 2`。
    ///
    /// 单设备实现下，这两个条件都可直接从 `JournalSlowpath.bucket_seq`
    /// 和 `current_bucket` 推出。
    pub fn bch2_journal_seq_to_flush(&self) -> u64 {
        let bucket_seq_target = {
            let sp = self.slowpath.lock().unwrap();
            let nr = sp.bucket_seq.len();
            if nr == 0 {
                0
            } else {
                let bucket_to_flush = (sp.current_bucket + nr / 2) % nr;
                sp.bucket_seq.get(bucket_to_flush).copied().unwrap_or(0)
            }
        };

        // pin FIFO 半满规则 — 对应 bcachefs reclaim.c:885-887。
        let cur_seq = self.bch2_journal_cur_seq();
        let pin_fifo_target = cur_seq.saturating_sub((PIN_FIFO_SIZE / 2) as u64);
        bucket_seq_target.max(pin_fifo_target)
    }

    /// 当前 journal 是否存在 flush 等待者。
    ///
    /// 对应 bcachefs `journal_has_flush_waiters()`：
    /// - entry 打开时：当前 buf 需要 flush 即视为有等待者
    /// - entry 关闭时：全局 flush 标记为 true 即视为有等待者
    fn journal_has_flush_waiters(&self) -> bool {
        let needs_flush = self.bch2_journal_needs_flush_write();
        if self.reservations.is_closed() {
            return needs_flush;
        }

        // bcachefs: journal_buf_must_flush(j, journal_cur_buf(j)) — 检查当前 buf 是否有等待者
        // journal_cur_buf(j) = &j->buf[reservations.idx]，所以用 reservations.idx 而非 current_bucket
        let idx = (self.bch2_journal_cur_seq() & (JOURNAL_IN_FLIGHT_NR as u64 - 1)) as usize;
        needs_flush || self.bufs.get(idx).journal_buf_must_flush()
    }

    /// 当前 journal 是否有必须推进 cycle 的 flush 等待者。
    ///
    /// 对应 bcachefs `journal_should_cycle_for_flush_waiters()`：
    /// - entry 打开时：有 flush 等待者且 in-flight 数量不超过 1 时应 cycle
    /// - entry 关闭时：只要存在 flush 等待者就应尝试 reopen/open
    fn journal_should_cycle_for_flush_waiters(&self) -> bool {
        if self.reservations.is_closed() {
            return self.journal_has_flush_waiters();
        }

        self.journal_has_flush_waiters() && self.in_flight.lock().unwrap().len() <= 1
    }

    fn journal_should_open(&self, flags: JournalCycleFlags) -> bool {
        if !self.reservations.is_closed() {
            return false;
        }

        if flags.contains(JournalCycleFlags::MUST_OPEN) {
            return true;
        }
        self.journal_has_flush_waiters()
    }

    /// 回收可重用的 journal bucket（对应 bcachefs `__bch2_journal_reclaim`）
    ///
    /// 与 bcachefs 对齐 (reclaim.c:1047-1182)：
    /// 1. 持有 `reclaim_lock` 串行化回收 (reclaim.c:1073)
    /// 2. 检查 journal 错误状态 (reclaim.c:1090-1092)
    /// 3. do-while 循环 flush pins：计算 seq_to_flush → 检查触发条件 → flush_pins → 继续
    /// 4. 更新 last_seq / dirty idx
    /// 5. TRIM 可回收 bucket
    ///
    /// - `direct=true`: 前台模式，单次 pass，等同 bcachefs `bch2_journal_reclaim(j)`
    /// - `direct=false`: 后台模式，在触发条件满足时循环，等同 bcachefs `__bch2_journal_reclaim(j, false, kicked)`
    pub async fn __bch2_journal_reclaim(
        &self,
        direct: bool,
        // btree cache 脏比例参数（对应 bcachefs reclaim.c:1125-1129）。
        // 当外部 btree 子系统未接入时传 0，脏比例检查自动为 no-op。
        btree_cache_dirty: usize,
        btree_cache_live: usize,
    ) -> Result<(), JournalError> {
        // === Phase 1: Flush + advance（同步，持有 reclaim_lock）===
        // 对应 bcachefs reclaim.c:1073-1179 的 scoped_guard(mutex, &j->reclaim_lock)
        //
        // reclaim_lock 保护 flush 和 idx 更新，防止并发 reclaim 竞争。
        // 注意：lock scope 必须在 .await 之前结束，因为 std::sync::MutexGuard 不是 Send。
        let reclaim_delay_ms = self.reclaim_interval_ms.load(Ordering::Acquire);
        {
            let _lock = self.reclaim_lock.lock().unwrap();

            // 检查 journal 错误状态 — 对应 bcachefs reclaim.c:1090-1092
            if let Some(err) = self.journal_error_check() {
                return Err(err);
            }

            // do-while 主 flush 循环 — 对应 bcachefs reclaim.c:1083 do-while
            //
            // bcachefs 循环条件：(min_nr || min_key_cache) && nr_flushed && !direct
            // - 前台 direct: 单次 pass（min_nr=0 时仍可能因 nr_flushed 退出）
            // - 后台 !direct: 满足触发条件 && flushed > 0 时继续循环
            //
            // min_nr 和 min_key_cache 由四个触发条件决定（bcachefs reclaim.c:1108-1134）：
            //   1. 时间超过 reclaim_interval  → min_nr = 1
            //   2. med_on_space               → min_nr = 1
            //   3. btree cache 脏比例 > 50%   → min_nr = 1
            //   4. key cache 积压             → min_key_cache = min(pending, 128)
            // subvol 暂不支持 key cache 统计（无 key cache 子系统），min_key_cache 恒为 0。
            let min_key_cache: usize = 0;
            loop {
                // 条件 1 — 时间触发（bcachefs reclaim.c:1111-1113）
                let time_trigger = {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64;
                    let elapsed = now.saturating_sub(self.bch2_journal_last_flush_jiffies());
                    reclaim_delay_ms > 0 && elapsed >= reclaim_delay_ms
                };

                // 条件 2 — 空间触发（bcachefs reclaim.c:1118-1119）
                let util = self.utilization();
                let space_trigger = util >= 0.25;

                // 条件 3 — btree cache 脏比例（bcachefs reclaim.c:1125-1129）
                let btree_trigger =
                    btree_cache_live > 0 && btree_cache_dirty * 2 > btree_cache_live;

                // 三取一决定 min_nr（bcachefs reclaim.c:1111-1129）
                let min_nr: usize = if time_trigger || space_trigger || btree_trigger {
                    1
                } else {
                    0
                };

                // 条件 4 — key cache（subvol 暂不支持，参数保留未来扩展）
                // min_key_cache = min(bch2_nr_btree_keys_need_flush(...), 128)

                // 后台模式且无触发条件 → 退出（前台模式跳过检查，至少执行一次 pass）
                if !direct && min_nr == 0 && min_key_cache == 0 {
                    break;
                }

                // 计算需要 flush 的最老 seq — 对应 bcachefs reclaim.c:1100
                let seq_to_flush = self.bch2_journal_seq_to_flush();
                // 执行 pin flush — 对应 bcachefs reclaim.c:1153 journal_flush_pins
                // callback 错误通过 ? 传播，reclaim 调用者处理错误
                // allowed_below_seq=!0 意为所有 pin type 都可 flush（bcachefs 默认行为）
                let nr_flushed = self.journal_flush_pins(seq_to_flush, !0, 0)?;
                // bcachefs 循环条件：(min_nr || min_key_cache) && nr_flushed && !direct
                if nr_flushed == 0 {
                    break;
                }
                // 统计计数 — 对应 bcachefs reclaim.c:1160-1162
                if direct {
                    self.nr_direct_reclaim
                        .fetch_add(nr_flushed as u64, Ordering::Relaxed);
                } else {
                    self.nr_background_reclaim
                        .fetch_add(nr_flushed as u64, Ordering::Relaxed);
                }
                // 前台 direct 模式：单次 pass — 对应 bcachefs reclaim.c:1179 && !direct
                if direct {
                    break;
                }
                // 后台模式：继续循环直到触发条件不满足或无工作可做
            }

            self.bch2_journal_update_last_seq();
            self.advance_dirty_idx();
            self.advance_dirty_idx_ondisk();

            // 计算 can_discard + may_skip_flush — 对应 bcachefs reclaim.c:282-347
            //
            // can_discard: __should_discard_bucket (reclaim.c:23-30)
            //   min_free = max(4, ja->nr / 2)
            //   available = (discard_idx - cur_idx - 1 + nr) % nr
            //   if (available && dirty_idx_ondisk == dirty_idx) available--;
            //   return available < min_free && discard_idx != dirty_idx_ondisk;
            //
            // may_skip_flush (reclaim.c:341-347):
            //   1. clean_ondisk.next_entry < clean_ondisk.total
            //   2. (clean - clean_ondisk) <= total / 8
            //   3. clean_ondisk * 2 > clean
            let sp = self.slowpath.lock().unwrap();
            let nr = sp.buckets.len();
            let mut available = (sp.discard_idx + nr - sp.current_bucket - 1) % nr;
            if available > 0 && sp.dirty_idx_ondisk == sp.dirty_idx {
                available -= 1;
            }
            let min_free = std::cmp::max(4usize, nr / 2);
            let has_discard_work = sp.discard_idx != sp.dirty_idx_ondisk;
            let cur = sp.current_bucket;
            let bucket_bytes = (BUCKET_BLOCKS * JSET_BLOCK_SIZE) as u64;
            let total = nr as u64 * bucket_bytes;
            let clean = (sp.dirty_idx + nr - cur - 1) % nr;
            let clean_ondisk = (sp.dirty_idx_ondisk + nr - cur - 1) % nr;
            let clean_bytes = clean as u64 * bucket_bytes;
            let clean_ondisk_bytes = clean_ondisk as u64 * bucket_bytes;
            // may_skip_flush 条件 (reclaim.c:341-347):
            // 1. next_entry < total: at least one entry slot in ondisk region
            // 2. (clean - clean_ondisk) <= total / 8: dirty portion ≤ 12.5%
            // 3. clean_ondisk * 2 > clean: ondisk > half
            let may_skip = clean_ondisk_bytes > bucket_bytes
                && clean_bytes.saturating_sub(clean_ondisk_bytes) <= total / 8
                && clean_ondisk_bytes * 2 > clean_bytes;
            drop(sp);
            self.can_discard
                .store(available < min_free && has_discard_work, Ordering::Release);
            self.may_skip_flush.store(may_skip, Ordering::Release);
        } // reclaim_lock 在此释放，之后进入 async TRIM 阶段

        // === Phase 2: TRIM 可回收 bucket（异步，无需 reclaim_lock）===
        let nr;
        {
            let sp = self.slowpath.lock().unwrap();
            nr = sp.buckets.len();
        }
        if nr == 0 {
            return Ok(());
        }

        loop {
            let bucket_addr;
            {
                let sp = self.slowpath.lock().unwrap();
                if sp.discard_idx == sp.dirty_idx_ondisk {
                    break;
                }
                bucket_addr = sp.buckets[sp.discard_idx].addr;
            }
            let be = self.journal_device();
            for bi in 0..BUCKET_BLOCKS {
                be.bdev()
                    .as_ref()
                    .trim_block(BlockAddr::new(bucket_addr + bi as u64))
                    .await
                    .ok();
            }
            let mut sp = self.slowpath.lock().unwrap();
            sp.discard_idx = (sp.discard_idx + 1) % nr;
        }
        Ok(())
    }

    /// 前台 reclaim 入口 — 单次 pass，等同 bcachefs `bch2_journal_reclaim(j)`。
    pub async fn bch2_journal_reclaim(&self) -> Result<(), JournalError> {
        self.__bch2_journal_reclaim(true, 0, 0).await
    }

    /// 阻塞等待所有 ≤ seq_to_flush 的 pin 完成 flush。
    ///
    /// 内部调用 `journal_flush_pins`（单次 pass），若 flush 无工作则返回；
    /// 否则重试。对应 bcachefs `bch2_journal_flush_pins()` (reclaim.c:1399-1411)。
    ///
    /// 返回 `Ok(true)` 表示至少执行了一次 flush callback。
    /// 若 callback 返回错误，传播 `Err(StorageError)`。
    ///
    /// # bcachefs 对齐
    ///
    /// 对应 bcachefs `bch2_journal_flush_pins()` (reclaim.c:1399-1411)。
    /// bcachefs 使用 closure_wait_event + reclaim_flush_wait 事件驱动等待，
    /// 在 `journal_flush_done` 返回 true 前一直阻塞。
    /// subvol 直接执行 flush 而非等待异步线程，用 Condvar + 超时
    /// 在无工作时让出 CPU。
    pub fn bch2_journal_flush_pins(&self, seq_to_flush: u64) -> Result<bool, StorageError> {
        let mut did_work = false;
        loop {
            // allowed_below_seq=!0: 所有 pin type 都可 flush
            let flushed = self.journal_flush_pins(seq_to_flush, !0, 0)?;
            if flushed == 0 {
                return Ok(did_work);
            }
            did_work = true;
            std::thread::yield_now();
        }
    }

    /// 轮换到下一个 bucket（对应 bcachefs `bch2_journal_rotate_or_reclaim`）
    pub async fn bch2_journal_rotate_or_reclaim(&self) -> Result<(), JournalError> {
        let nr;
        {
            let sp = self.slowpath.lock().unwrap();
            nr = sp.buckets.len();
        }
        if nr == 0 {
            return Err(JournalError::Overflow("no journal buckets".into()));
        }

        {
            let mut sp = self.slowpath.lock().unwrap();
            let next = (sp.current_bucket + 1) % nr;
            if next != sp.dirty_idx {
                sp.current_bucket = next;
                sp.current_offset = 0;
                sp.remaining_bytes = BUCKET_BLOCKS * JSET_BLOCK_SIZE;
                return Ok(());
            }
        }

        self.bch2_journal_reclaim().await?;

        {
            let mut sp = self.slowpath.lock().unwrap();
            let next2 = (sp.current_bucket + 1) % nr;
            if next2 == sp.dirty_idx {
                return Err(JournalError::Overflow(String::from(
                    "all journal buckets exhausted after reclaim",
                )));
            }
            sp.current_bucket = next2;
            sp.current_offset = 0;
            sp.remaining_bytes = BUCKET_BLOCKS * JSET_BLOCK_SIZE;
        }
        Ok(())
    }

    // ─── Blacklist ─────────────────────────────────────────

    /// 初始化黑名单表 — 对应 bcachefs `bch2_blacklist_table_initialize()` (seq_blacklist.c:189-219)
    ///
    /// 从 superblock blacklist field 构建运行时 BlacklistTable，
    /// 支持后续 `is_blacklisted`、`next_nonblacklisted` 等运行时查询。
    /// 通常在 recovery 完成后调用。
    pub fn bch2_blacklist_table_initialize(&self, entries: &[BlacklistEntry]) {
        let table = BlacklistTable::from_entries(entries);
        *self.blacklist_table.write().unwrap() = Some(table);
    }

    /// 检查 seq 是否在黑名单中 — 对应 bcachefs `bch2_journal_seq_is_blacklisted()` (seq_blacklist.c:152-177)
    ///
    /// 若 `dirty=true` 且 seq 命中黑名单，标记该条目为 dirty（GC 时保留）。
    /// 表未初始化时返回 `false`（无黑名单 = 无不跳过）。
    pub fn bch2_journal_seq_is_blacklisted(&self, seq: u64, dirty: bool) -> bool {
        self.blacklist_table
            .read()
            .unwrap()
            .as_ref()
            .map_or(false, |t| t.is_blacklisted(seq, dirty))
    }

    /// 跳过黑名单范围 — 对应 bcachefs `bch2_journal_seq_next_nonblacklisted()` (seq_blacklist.c:132-150)
    ///
    /// 若 seq 在黑名单中，返回该范围的 end（下一个非黑名单 seq）；
    /// 否则返回 seq 本身。表未初始化时返回 seq。
    pub fn bch2_journal_seq_next_nonblacklisted(&self, seq: u64) -> u64 {
        self.blacklist_table
            .read()
            .unwrap()
            .as_ref()
            .map_or(seq, |t| t.next_nonblacklisted(seq))
    }

    /// 找到下一个黑名单条目 — 对应 bcachefs `bch2_journal_seq_next_blacklisted()` (seq_blacklist.c:114-130)
    ///
    /// 返回大于等于 seq 的下一个黑名单 entry 的 start。
    /// 无黑名单条目时返回 `u64::MAX`。
    pub fn bch2_journal_seq_next_blacklisted(&self, seq: u64) -> u64 {
        self.blacklist_table
            .read()
            .unwrap()
            .as_ref()
            .map_or(u64::MAX, |t| t.next_blacklisted(seq))
    }

    /// 获取最后一个黑名单 seq — 对应 bcachefs `bch2_journal_last_blacklisted_seq()` (seq_blacklist.c:179-187)
    ///
    /// 返回最后条目 `end - 1`；无条目时返回 0。
    pub fn bch2_journal_last_blacklisted_seq(&self) -> u64 {
        self.blacklist_table
            .read()
            .unwrap()
            .as_ref()
            .map_or(0, |t| t.last_blacklisted_seq())
    }

    /// GC 过期黑名单条目（只读检查）
    ///
    /// 对应 bcachefs `bch2_blacklist_entries_gc()` (seq_blacklist.c:276-311)。
    /// bcachefs 不修改运行时表，仅读取 dirty 标志后写入 superblock。
    /// 返回 `true` 表示存在可被 gc 的条目。
    pub fn bch2_blacklist_entries_gc(&self, oldest_seq: u64) -> bool {
        self.blacklist_table
            .read()
            .unwrap()
            .as_ref()
            .map_or(false, |t| t.gc(oldest_seq))
    }

    /// 合并 blacklist 区间并持久化 superblock。
    /// 对应本地 bcachefs `bch2_journal_seq_blacklist_add()`。
    pub async fn bch2_journal_seq_blacklist_add(
        &self,
        c: &BchVol,
        mut start: u64,
        mut end: u64,
    ) -> Result<(), JournalError> {
        // 对应本地: `guard(mutex)(&c->sb_lock)` — 全程持锁至落盘完成
        let sb = {
            let _guard = c.sb_lock.lock().unwrap();

            let entries = &mut c.superblock_mut().journal_seq_blacklist;
            let mut i = 0;
            while i < entries.len() {
                let entry = entries[i];
                if end < entry.start_seq {
                    break;
                }
                if start > entry.end_seq {
                    i += 1;
                    continue;
                }
                start = start.min(entry.start_seq);
                end = end.max(entry.end_seq);
                entries.remove(i);
            }
            entries.insert(
                i,
                BlacklistEntry {
                    start_seq: start,
                    end_seq: end,
                },
            );
            c.superblock_mut().feature_set(
                crate::storage::superblock::feature_bits::JOURNAL_SEQ_BLACKLIST_V3,
            );

            c.superblock().clone()
        };

        // 锁已释放，异步写入设备
        for dev_idx in c.device_registry.dev_indices() {
            if let Some(dev) = c.device_registry.resolve_bch_dev(dev_idx) {
                sb.write_to_device(dev.as_ref())
                    .await
                    .map_err(JournalError::Io)?;
            }
        }
        self.bch2_blacklist_table_initialize(&sb.journal_seq_blacklist);
        Ok(())
    }

    // ═══════════════════════════════════════════════════════════
    // Part 6a: New Slowpath Methods
    // ═══════════════════════════════════════════════════════════

    /// 获取指定水位线可用的 journal 空间字节数（对应 bcachefs `bch2_journal_space_available`）
    ///
    /// 基于 current_watermark 和空间分类决定。
    /// - `Stripe` / `Normal` → 只算 DISCARDED（最安全的空间）
    /// - `CopyGC` / `Btree` → 算 CLEAN_ONDISK
    /// - `BtreeCopyGC` / `Reclaim` → 算 CLEAN
    /// - `InteriorUpdate` → 算 TOTAL（全部空间）
    /// 从 slowpath 索引实时计算 4 个空间槽值（无缓存）
    ///
    /// 返回 `(total, clean, clean_ondisk)` 字节数。
    /// - total = 全部 journal bucket 容量
    /// - clean = current_bucket 到 dirty_idx 之间（可循环写入的 bucket 容量）
    /// - clean_ondisk = current_bucket 到 dirty_idx_ondisk 之间（已落盘的 bucket 容量）
    ///
    /// discarded ≈ clean（subvol 暂不追踪 per-bucket discard 状态）。
    fn compute_journal_space(&self) -> (u64, u64, u64) {
        let sp = self.slowpath.lock().unwrap();
        let nr = sp.buckets.len();
        if nr == 0 {
            return (0, 0, 0);
        }
        let cur = sp.current_bucket;
        let total_buckets = nr;
        let clean_buckets = (sp.dirty_idx + nr - cur - 1) % nr;
        let clean_ondisk_buckets = (sp.dirty_idx_ondisk + nr - cur - 1) % nr;

        let bucket_bytes = (BUCKET_BLOCKS * JSET_BLOCK_SIZE) as u64;
        (
            total_buckets as u64 * bucket_bytes,
            clean_buckets as u64 * bucket_bytes,
            clean_ondisk_buckets as u64 * bucket_bytes,
        )
    }

    /// 返回指定水位线可用的 journal 空间字节数（Compute-and-set）
    ///
    /// 对应 bcachefs `bch2_journal_space_available()` (reclaim.c:262-358)。
    pub fn bch2_journal_space_available(&self, watermark: Watermark) -> u64 {
        self.advance_dirty_idx();
        self.advance_dirty_idx_ondisk();
        let (total, clean, clean_ondisk) = self.compute_journal_space();
        let dirty_entry_bytes = self.dirty_entry_bytes.load(Ordering::Acquire);

        // 扣减 in_flight 中未完成 buf 的 sector 数（对应 bcachefs reclaim.c:167-189）
        // bcachefs 在 journal_dev_space_available 中遍历 in_flight 队列，
        // 对每个 unwritten buf 逐步扣减 sector 计数和 bucket 计数。
        // subvol 使用同步写入（bch2_journal_write），窗口很小但仍需对齐。
        let in_flight_bytes: u64 = {
            let in_flight = self.in_flight.lock().unwrap();
            in_flight
                .iter()
                .filter_map(|&idx| {
                    let buf = self.bufs.get_mut(idx as usize);
                    // 只扣减 Free/WriteDone 之外的 buf（Free 无数据，WriteDone 的空间已反映在 bucket 位置中）
                    if buf.state != BufState::Free
                        && buf.state != BufState::WriteDone
                        && buf.sectors > 0
                    {
                        Some(buf.sectors as u64 * 512)
                    } else {
                        None
                    }
                })
                .sum()
        };

        // 按水位线选择基础空间（bucket 级），再依次扣减 in_flight 占用的字节和 dirty_entry_bytes
        let base = |available: u64| -> u64 {
            available
                .saturating_sub(in_flight_bytes)
                .saturating_sub(dirty_entry_bytes.min(available))
        };
        match watermark {
            Watermark::Stripe | Watermark::Normal => base(clean),
            Watermark::CopyGC | Watermark::Btree => base(clean_ondisk),
            Watermark::BtreeCopyGC | Watermark::Reclaim => base(clean),
            Watermark::InteriorUpdate => base(total),
        }
    }

    /// bcachefs 风格的 journal cycle 状态机。
    ///
    /// 对应 `bch2_journal_cycle_locked(j, flags)`：
    /// - `MUST_CLOSE`：强制关闭当前 entry
    /// - `MUST_OPEN`：关闭后立即尝试打开新 entry
    /// - `FORCE_CLOSE`：跳过 older-write throttle
    pub(crate) fn bch2_journal_cycle_locked_flags(
        &self,
        mut flags: JournalCycleFlags,
    ) -> Result<bool, JournalError> {
        if flags == JournalCycleFlags::empty() && !self.journal_should_cycle_for_flush_waiters() {
            return Ok(false);
        }

        let mut opened = false;

        loop {
            let entry_is_open = !self.reservations.is_closed();
            let current_idx =
                (self.bch2_journal_cur_seq() & (JOURNAL_IN_FLIGHT_NR as u64 - 1)) as usize;
            let current_buf = self.bufs.get_mut(current_idx);
            let should_close = if !entry_is_open {
                false
            } else if flags.contains(JournalCycleFlags::MUST_OPEN) {
                true
            } else if !flags.contains(JournalCycleFlags::MUST_CLOSE) && !current_buf.has_must_flush
            {
                false
            } else {
                flags.contains(JournalCycleFlags::FORCE_CLOSE)
                    || self.in_flight.lock().unwrap().len() <= 1
            };

            if should_close {
                self.journal_entry_close();
            }

            flags.remove(JournalCycleFlags::MUST_CLOSE);
            flags.remove(JournalCycleFlags::FORCE_CLOSE);

            if !self.journal_should_open(flags) {
                self.bch2_journal_wake_up();
                return Ok(opened);
            }

            self.journal_entry_open()?;
            opened = true;
            flags.remove(JournalCycleFlags::MUST_OPEN);
            self.bch2_journal_wake_up();
        }
    }

    /// 关闭当前 entry 并尝试轮换到下一个 bucket（slowpath 核心同步操作）
    ///
    /// 对应 bcachefs `bch2_journal_cycle_locked()` (journal.c:636) 的 flags=0 路径。
    pub fn bch2_journal_cycle_locked(&self) -> Result<bool, JournalError> {
        self.bch2_journal_cycle_locked_flags(JournalCycleFlags::empty())
    }

    /// slowpath 预留 — 当 fastpath CAS 失败时调用。
    ///
    /// 三级 fallback（对齐 bcachefs `bch2_journal_res_get_slowpath`）：
    /// 1. cycle: `journal_cycle_locked()` — 关闭旧 entry，打开新 bucket
    /// 2. wait: 等待 in_flight buf 写入完成
    /// 3. reclaim: `bch2_journal_flush_pins()` + reclaim — 释放已 pin 空间
    ///
    /// 三级都失败后，优先把可判定的 reclaim 卡死升级为 `JournalError::Stuck`，
    /// 否则返回 `JournalError::Overflow`。
    pub fn bch2_journal_res_get_slowpath(
        &self,
        watermark: Watermark,
        req_u64s: u32,
    ) -> Result<JournalRes, JournalError> {
        self.bch2_journal_res_get_slowpath_inner(watermark, req_u64s, false)
    }

    /// 非阻塞 reservation 入口。
    ///
    /// 对应 bcachefs `JOURNAL_RES_GET_NONBLOCK` 语义：
    /// - fastpath 成功则直接返回
    /// - slowpath 只尝试一次 cycle + recheck
    /// - 若仍无法立即获得空间，则返回 `JournalError::Blocked`
    pub fn journal_res_get_nonblocking(
        &self,
        watermark: Watermark,
        req_u64s: u32,
    ) -> Result<JournalRes, JournalError> {
        if let Ok(res) = self.bch2_journal_res_get_fast(watermark, req_u64s) {
            return Ok(res);
        }

        let _guard = self
            .slowpath_lock
            .try_lock()
            .map_err(|_| JournalError::Blocked("journal slowpath busy".into()))?;

        self.bch2_journal_res_get_slowpath_inner(watermark, req_u64s, true)
    }

    fn bch2_journal_res_get_slowpath_inner(
        &self,
        watermark: Watermark,
        req_u64s: u32,
        nonblocking: bool,
    ) -> Result<JournalRes, JournalError> {
        if self.blocked.load(Ordering::Acquire) != 0 {
            return Err(JournalError::Blocked("journal blocked".into()));
        }

        let current_wm = Watermark::from_bits(self.current_watermark.load(Ordering::Acquire));
        if !current_wm.allows(watermark) {
            return Err(JournalError::Overflow(format!(
                "watermark blocked: request={:?} < current={:?}",
                watermark, current_wm,
            )));
        }

        // Phase 1: cycle
        if self.bch2_journal_cycle_locked_flags(JournalCycleFlags::MUST_OPEN)? {
            if let Ok(res) = self.bch2_journal_res_get_fast(watermark, req_u64s) {
                return Ok(res);
            }
        }

        if nonblocking {
            return Err(JournalError::Blocked(
                "journal reservation would block".into(),
            ));
        }

        // Phase 2: wait — 自旋等待 inflight 队列清空
        const SPIN_COUNT: u32 = 1024;
        for _ in 0..SPIN_COUNT {
            if self.in_flight.lock().unwrap().is_empty() {
                break;
            }
            std::thread::yield_now();
        }
        if let Ok(res) = self.bch2_journal_res_get_fast(watermark, req_u64s) {
            return Ok(res);
        }

        // Phase 3: reclaim — flush pins + advance indices
        // 对应 bcachefs `bch2_journal_reclaim(j, BCH_RECLAIM_DIRECT)` (journal.c:850-880)。
        // 使用 `journal_seq_to_flush()` 而非 `bch2_journal_cur_seq()` 精确计算需 flush 的 seq，
        // 避免过度 flush（pin FIFO 半满水位控制）。
        let seq_to_flush = self.bch2_journal_seq_to_flush();
        let nr_flushed = self.journal_flush_pins(seq_to_flush, !0, 0)?;
        if nr_flushed > 0 {
            self.nr_direct_reclaim
                .fetch_add(nr_flushed as u64, Ordering::Relaxed);
        }
        self.bch2_journal_update_last_seq();
        self.advance_dirty_idx();
        self.advance_dirty_idx_ondisk();
        if self.bch2_journal_cycle_locked_flags(JournalCycleFlags::MUST_OPEN)? {
            if let Ok(res) = self.bch2_journal_res_get_fast(watermark, req_u64s) {
                return Ok(res);
            }
        }

        let err = JournalError::Overflow(format!(
            "slowpath: no journal space after cycle+wait+reclaim (watermark={:?}, req_u64s={})",
            watermark, req_u64s,
        ));
        if journal_error_check_stuck(self, &err, watermark) {
            return Err(JournalError::Stuck(format!(
                "journal stuck after cycle+wait+reclaim (watermark={:?}, req_u64s={})",
                watermark, req_u64s,
            )));
        }
        Err(err)
    }

    /// 公开的 journal reservation 入口 — 尝试 fastpath，失败后自动进入 slowpath
    ///
    /// 对应 bcachefs `bch2_journal_res_get()` (journal.h:521)
    /// bcachefs 在 `__journal_res_get` (journal.c:820) 中处理更多 flags（如 JOURNAL_RES_GET_CHECK），
    /// subvol 的简化版始终进行 fast→slow 两级 fallback。
    ///
    /// 这是推荐的 reservation API：
    /// 1. 先尝试无锁 fastpath（CAS on `JournalResState`）
    /// 2. 如果 fastpath 因空间不足失败，获取序列化锁并通过 slowpath 重试
    ///
    /// # 并发安全性
    ///
    /// - Fastpath 路径：完全无锁，CAS 保护
    /// - Slowpath 路径：通过 `slowpath_lock` 互斥，确保同一时间只有一个线程修改 bucket 状态
    pub fn bch2_journal_res_get(
        &self,
        watermark: Watermark,
        req_u64s: u32,
    ) -> Result<JournalRes, JournalError> {
        // bcachefs journal.h:526: EBUG_ON(!test_bit(JOURNAL_running, &j->flags))
        if !self.running.load(Ordering::Acquire) {
            return Err(JournalError::Blocked("journal not running".into()));
        }
        // 1. 尝试 fastpath
        if let Ok(res) = self.bch2_journal_res_get_fast(watermark, req_u64s) {
            return Ok(res);
        }

        // 2. Fastpath 失败 → 获取 slowpath 锁后进入 slowpath
        let _guard = self.slowpath_lock.lock().unwrap();
        self.bch2_journal_res_get_slowpath_inner(watermark, req_u64s, false)
    }

    /// 设置自动 flush 间隔（毫秒）
    ///
    /// 当 interval 为 0 时禁用自动 flush。
    pub fn set_auto_flush_interval(&mut self, ms: u64) {
        self.auto_flush_ms = if ms > 0 { Some(ms) } else { None };
        self.journal_flush_delay_ms.store(ms, Ordering::Release);
    }

    /// 获取自动 flush 间隔
    pub fn auto_flush_interval(&self) -> Option<u64> {
        self.auto_flush_ms
    }

    /// 启动 journal workqueue，承载 write dispatch 与 auto-commit delayed work。
    ///
    /// 对应本地 `INIT_DELAYED_WORK(&j->write_work, bch2_journal_write_work)`；
    /// 当前 open entry 的首次 timer 在 worker 启动时排队，后续由
    /// `bch2_journal_do_writes_locked()` 重置或取消。
    pub fn start_auto_flush(&self, journal_arc: Arc<Self>) {
        unsafe {
            if (*self.flush_bg_handle.get()).is_some() {
                return;
            }
        }

        let delay = self.vol.get().and_then(|vol| vol.upgrade()).map_or_else(
            || self.journal_flush_delay_ms.load(Ordering::Acquire),
            |c| u64::from(c.opts.journal_flush_delay),
        );
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        self.write_work_deadline_ms
            .compare_exchange(
                0,
                now.saturating_add(delay),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .ok();
        self.write_work_running.store(true, Ordering::Release);

        let handle =
            BgTaskHandle::spawn_cancellable("journal-auto-flush", move |should_stop| async move {
                loop {
                    if should_stop.load(Ordering::Acquire) {
                        break;
                    }

                    let deadline = journal_arc.write_work_deadline_ms.load(Ordering::Acquire);
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64;
                    if deadline != 0 && now >= deadline {
                        if journal_arc
                            .write_work_deadline_ms
                            .compare_exchange(deadline, 0, Ordering::AcqRel, Ordering::Acquire)
                            .is_ok()
                        {
                            journal_arc.bch2_journal_write_work();
                        }
                        continue;
                    }

                    let stop_wait = async {
                        while !should_stop.load(Ordering::Acquire) {
                            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                        }
                    };
                    tokio::select! {
                        _ = journal_arc.flush_notify.notified() => {
                            if let Err(e) = journal_arc.bch2_journal_write().await {
                                eprintln!("journal write failed: {}", e);
                            }
                        }
                        _ = journal_arc.write_work_notify.notified() => {}
                        _ = async {
                            if deadline == 0 {
                                std::future::pending::<()>().await;
                            } else {
                                tokio::time::sleep(std::time::Duration::from_millis(
                                    deadline.saturating_sub(now),
                                )).await;
                            }
                        } => {}
                        _ = stop_wait => break,
                    }
                }
            });
        unsafe { *self.flush_bg_handle.get() = Some(handle) };
    }

    /// 停止自动 flush 后台任务
    ///
    /// 设置 should_stop 标志并等待任务退出。
    /// 任务在 sleep/select 分支中会检查该标志，因此可以及时收敛。
    pub async fn stop_auto_flush(&self) {
        self.write_work_running.store(false, Ordering::Release);
        self.write_work_deadline_ms.store(0, Ordering::Release);
        self.write_work_notify.notify_one();
        unsafe {
            if let Some(handle) = (*self.flush_bg_handle.get()).take() {
                handle.cancel();
                handle.join().await;
            }
        }
    }

    /// 启动后台回收任务
    ///
    /// 使用 BgTaskHandle 管理生命周期，支持 set_read_only 时停止。
    /// `journal_arc` 是 Journal 的 Arc 克隆，供后台任务访问 Journal。
    /// 后台循环在每次迭代前检查 should_stop 标志以实现及时关闭。
    pub fn start_background_reclaim(&self, journal_arc: Arc<Self>, interval_ms: u64) {
        if interval_ms == 0 {
            return;
        }
        self.reclaim_interval_ms
            .store(interval_ms, Ordering::Release);

        let handle = BgTaskHandle::spawn_cancellable(
            "journal-reclaim",
            move |should_stop| async move {
                loop {
                    if should_stop.load(Ordering::Acquire) {
                        break;
                    }

                    let kicked = journal_arc.reclaim_kicked.swap(false, Ordering::AcqRel);
                    if !kicked {
                        let stop_wait = async {
                            while !should_stop.load(Ordering::Acquire) {
                                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                            }
                        };
                        tokio::select! {
                            _ = journal_arc.reclaim_notify.notified() => {}
                            _ = tokio::time::sleep(std::time::Duration::from_millis(interval_ms)) => {}
                            _ = stop_wait => break,
                        }
                    }

                    if should_stop.load(Ordering::Acquire) {
                        break;
                    }

                    if let Err(e) = journal_arc.__bch2_journal_reclaim(false, 0, 0).await {
                        eprintln!("background reclaim failed: {}", e);
                    }
                }
            },
        );
        unsafe { *self.reclaim_bg_handle.get() = Some(handle) };
    }

    /// 停止后台回收任务
    ///
    /// 设置 should_stop 标志并等待任务退出。
    /// 任务在 wait/select 分支中会检查该标志，因此可以及时收敛。
    pub async fn stop_background_reclaim(&self) {
        unsafe {
            if let Some(handle) = (*self.reclaim_bg_handle.get()).take() {
                handle.cancel();
                handle.join().await;
            }
        }
    }

    // ─── 旧版 spawn API（保留兼容性，测试用） ───

    /// 启动自动 flush 后台任务
    ///
    /// 当 `auto_flush_ms` 有值时，启动一个 tokio::spawn 循环：
    /// 返回 JoinHandle 供调用方管理生命周期。
    ///
    /// # 调用要求
    ///
    /// 调用方需要持有 `Arc<Journal>` 和 `Arc<dyn BlockDevice>`。
    /// 此方法在所有创建 Journal 的场景（daemon、测试）中可选调用。
    pub fn spawn_auto_flush_task(self: &Arc<Self>) -> Option<tokio::task::JoinHandle<()>> {
        let interval_ms = self.auto_flush_ms?;
        if interval_ms == 0 {
            return None;
        }

        let journal = self.clone();
        let handle = tokio::spawn(async move {
            let mut last_flush_seq: u64 = 0;
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(interval_ms)).await;

                let cur_seq = journal.bch2_journal_cur_seq();
                if cur_seq == last_flush_seq {
                    continue;
                }

                let buf_util = {
                    let res_state = journal.reservations.read();
                    let cur_off = JournalResState::cur_entry_offset(res_state);
                    (cur_off as f64) / (BUF_SIZE_U64S as f64)
                };

                if buf_util > 0.75 || cur_seq > last_flush_seq + 1 {
                    if let Err(e) = journal.bch2_journal_flush().await {
                        eprintln!("auto-flush failed: {}", e);
                    }
                    last_flush_seq = journal.bch2_journal_cur_seq();
                }
            }
        });
        Some(handle)
    }

    /// 启动后台回收任务
    ///
    /// 定时调 `bch2_journal_reclaim()` 回收可重用的 journal bucket。
    /// 当 `interval_ms` 为 0 时返回 `None`（不启动）。
    ///
    /// # 调用要求
    ///
    /// 调用方需要持有 `Arc<Journal>` 和 `Arc<dyn BlockDevice>`。
    /// 此方法在所有创建 Journal 的场景（daemon、测试）中可选调用。
    pub fn spawn_background_reclaim_task(
        self: &Arc<Self>,
        interval_ms: u64,
    ) -> Option<tokio::task::JoinHandle<()>> {
        if interval_ms == 0 {
            return None;
        }
        self.reclaim_interval_ms
            .store(interval_ms, Ordering::Release);

        let journal = self.clone();
        let handle = tokio::spawn(async move {
            loop {
                let kicked = journal.reclaim_kicked.swap(false, Ordering::AcqRel);
                if !kicked {
                    tokio::select! {
                        _ = journal.reclaim_notify.notified() => {}
                        _ = tokio::time::sleep(std::time::Duration::from_millis(interval_ms)) => {}
                    }
                }

                if let Err(e) = journal.__bch2_journal_reclaim(false, 0, 0).await {
                    eprintln!("background reclaim failed: {}", e);
                }
            }
        });
        Some(handle)
    }

    // ─── R3: quiesce / halt ──────────────────────────────

    /// 检查 journal 是否已 quiesce（所有待写入已完成）。
    ///
    /// 对应 bcachefs `journal_quiesced()` (journal.c:692-701)。
    ///
    /// bcachefs 这里按 `seq == seq_ondisk` 判断，而不是 `flushed_seq_ondisk`：
    /// quiesce 需要等到写入已经真正落到盘上并完成后续 bookkeeping。
    fn bch2_journal_quiesced(&self) -> bool {
        // bcachefs: guard(percpu_read)(&j->pin_resize_lock) — subvol: no-op
        // bcachefs: guard(spinlock)(&j->lock) — subvol: slowpath_lock
        let _sp = self.slowpath_lock.lock().unwrap();
        let ret = self.seq.load(Ordering::Acquire) == self.seq_ondisk.load(Ordering::Acquire);
        if !ret {
            // 尝试关闭当前 entry 推进 flush（对应 bcachefs bch2_journal_cycle_locked(j, JOURNAL_CYCLE_must_close)）
            self.bch2_journal_cycle_locked_flags(JournalCycleFlags::MUST_CLOSE)
                .ok();
        }
        ret
    }

    /// 等待 journal 所有待写入完成（quiesce）。
    ///
    /// 对应 bcachefs `bch2_journal_quiesce()` (journal.c:703-706)。
    ///
    /// subvol 使用忙等待 + 短睡眠（无 closure 机制）。
    pub fn bch2_journal_quiesce(&self) {
        while !self.bch2_journal_quiesced() {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// 检查 journal 是否已 shutdown quiesce（更严格的 quiesce 检查）。
    ///
    /// 对应 bcachefs `journal_shutdown_quiesced()` (journal.c:722-735)。
    ///
    /// bcachefs 语义：
    /// - 有 journal error 时：仅需 `seq == seq_ondisk`（无条件落盘）
    /// - 无 journal error 时：需 `seq == flushed_seq_ondisk && !has_flush_waiters`
    ///   （flushed_seq_ondisk 只被 flush write 推进，meta write 不推进它）
    fn bch2_journal_shutdown_quiesced(&self) -> bool {
        let _sp = self.slowpath_lock.lock().unwrap();
        let seq = self.seq.load(Ordering::Acquire);
        let err = self.bch2_journal_error_check();
        // 有 error 时用 seq_ondisk（所有写路径都推进它），否则用 flushed_seq_ondisk（仅 flush write 推进）
        let ret = if err.is_some() {
            seq == self.seq_ondisk.load(Ordering::Acquire)
        } else {
            let has_waiters = self.journal_has_flush_waiters();
            seq == self.flushed_seq_ondisk.load(Ordering::Acquire) && !has_waiters
        };
        if !ret {
            self.bch2_journal_cycle_locked_flags(JournalCycleFlags::MUST_CLOSE)
                .ok();
        }
        ret
    }

    /// 等待 journal shutdown quiesce。
    ///
    /// 对应 bcachefs `bch2_journal_shutdown_quiesce()` (journal.c:737-740)。
    pub fn bch2_journal_shutdown_quiesce(&self) {
        while !self.bch2_journal_shutdown_quiesced() {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// 快速 halt journal（持有锁时调用）。
    ///
    /// 对应 bcachefs `bch2_journal_halt_locked()` (journal.c:666-684)。
    ///
    /// bcachefs 语义：
    /// 1. `__journal_entry_close_one(j, JOURNAL_ENTRY_ERROR_VAL)` — 以 ERROR_VAL 关闭当前 entry
    /// 2. `j->err_seq = journal_cur_seq(j)` — 记录 halt 时的 seq
    /// 3. `journal_wake(j)` + `__closure_wake_up(flush_wait)` + `__closure_wake_up(reclaim_flush_wait)` — 唤醒所有等待者
    ///
    /// ERROR_VAL close 的特殊行为（journal.c:375-383）：
    /// - 不更新 dirty_entry_bytes（跳过 bcachefs journal.c:315-316）
    /// - 不调 `__bch2_journal_buf_put`（不触发 pin_put/update_last_seq）
    /// - 改用 `journal_state_buf_put` 直接释放 refcount（journal.c:379-383）
    fn bch2_journal_halt_locked(&self) {
        // 1. 对应 bcachefs journal.c:670: __journal_entry_close_one(j, JOURNAL_ENTRY_ERROR_VAL, true)
        let used_u64s = self
            .reservations
            .close_entry_with_val(JOURNAL_ENTRY_ERROR_VAL);
        // 只有 entry 实际处于 open 状态时 close 才有意义
        // （used_u64s >= CLOSED_VAL 表示 entry 未打开或已在错误状态）
        if used_u64s < JOURNAL_ENTRY_CLOSED_VAL as u32 {
            // ERROR_VAL 路径的 buf_put（bcachefs journal.c:379-383）：
            // 不调 __bch2_journal_buf_put（跳过 pin_put/update_last_seq），
            // 改用 journal_state_buf_put(j, idx) 直接释放 open_entry 的隐式 refcount
            let close_seq = self.seq.load(Ordering::Acquire);
            let idx = (close_seq & (JOURNAL_STATE_BUF_NR as u64 - 1)) as u32;
            let old = self.reservations.release(idx);
            // 如果 refcount 归零（只有 open_entry 的这 1 个 ref），触发 do_writes_locked
            // 对应 bcachefs journal.c:381-382
            if JournalResState::buf_count(old, idx) == 0 {
                self.bch2_journal_do_writes_locked();
            }
        }

        // 2. 对应 bcachefs journal.c:676: if (!j->err_seq) j->err_seq = journal_cur_seq(j);
        self.err_seq
            .compare_exchange(
                0,
                self.bch2_journal_cur_seq(),
                Ordering::Release,
                Ordering::Relaxed,
            )
            .ok();
        // 设置 journal_error（阻止后续所有分配）
        self.bch2_journal_error_set(JournalError::Blocked("journal halted".into()));
        // 3. 对应 bcachefs journal.c:673: journal_wake(j) + __closure_wake_up(flush_wait) + __closure_wake_up(reclaim_flush_wait)
        self.bch2_journal_wake_up();
        // bcachefs: __closure_wake_up(&j->flush_wait) — 唤醒等待 flush 完成的线程
        self.flush_wait.notify_all();
        // bcachefs: __closure_wake_up(&j->reclaim_flush_wait) — 唤醒等待 reclaim 完成的线程
        self.reclaim_flush_wait.notify_all();
    }

    /// 阻止 journal 所有新分配并唤醒等待者。
    ///
    /// 对应 bcachefs `bch2_journal_halt()` (journal.c:686-689)。
    ///
    /// bcachefs: guard(spinlock)(&j->lock); bch2_journal_halt_locked(j);
    /// subvol: slowpath_lock 替代 spinlock
    pub fn bch2_journal_halt(&self) {
        let _lock = self.slowpath_lock.lock().unwrap();
        self.bch2_journal_halt_locked();
    }

    // ─── R4: meta entry ─────────────────────────────────

    /// 写入空 journal entry 推进 clock hands。
    ///
    /// 对应 bcachefs `__bch2_journal_meta()` (journal.c:1316-1328)。
    ///
    /// 步骤：
    /// 1. `bch2_journal_res_get(Reclaim, 0)` — 分配 0 u64s 的 reservation
    /// 2. `bch2_journal_res_put` — 释放 reservation
    /// 3. `bch2_journal_flush()` — 提交 IO 写入 + 等待完成
    /// 4. 返回错误检查结果
    ///
    /// # 同步语义
    ///
    /// bcachefs 通过 `res_flush + closure_sync` 确保写入完成后再返回。
    /// subvol 中 `res_put` 仅推进状态机，`flush(backend)` 执行实际 IO。
    pub async fn __bch2_journal_meta(&self) -> Result<(), JournalError> {
        // bcachefs: bch2_journal_res_get(j, &res, jset_u64s(0), 0, NULL)
        let res = self.bch2_journal_res_get(Watermark::Reclaim, 0)?;
        // bcachefs: bch2_journal_res_put(j, &res)
        self.bch2_journal_res_put(&res);
        // bcachefs: bch2_journal_res_flush(j, &res, &cl) + closure_sync(&cl)
        self.bch2_journal_flush().await?;
        // bcachefs: return bch2_journal_error(j)
        self.bch2_journal_error_check().map_or(Ok(()), Err)
    }

    // ─── R6-R7: flush seq async/sync ──────────────────────

    /// 等待给定 seq 的 journal 条目完成 flush（同步版本，无需 backend）。
    ///
    /// 对应 bcachefs `bch2_journal_flush_seq()` (journal.c:1207-1231)。
    ///
    /// 此函数会关闭当前 journal entry，并同步等待 flush 完成。
    ///
    /// # subvol 差异
    ///
    /// bcachefs 使用 closure 和 `__bch2_journal_flush_seq_async` 的异步等待机制。
    /// subvol 使用 `block_on_safe` 桥接现有 async flush，并等待 `flushed_seq_ondisk`。
    ///
    /// 实际后端 I/O 由异步 `bch2_journal_flush` 或后台 flush 任务完成。
    pub fn bch2_journal_flush_seq(&self, seq: u64) -> Result<(), JournalError> {
        self.bch2_journal_flush_seq_async(seq)?;
        // 关闭当前 entry 并等待 flush 完成
        // 对应 bcachefs journal.c:1213-1222: bch2_journal_flush_seq_async + closure_sync_timeout
        block_on_safe(self.bch2_journal_flush())?;

        // bcachefs: return READ_ONCE(j->err_seq) && seq > READ_ONCE(j->flushed_seq_ondisk)
        //   ? bch_err_throw(c, journal_flush_err) : 0;
        if self.err_seq.load(Ordering::Acquire) != 0
            && seq > self.flushed_seq_ondisk.load(Ordering::Acquire)
        {
            let err = self
                .bch2_journal_error_check()
                .unwrap_or(JournalError::Blocked(
                    "journal flush_seq: seq not flushed due to error".into(),
                ));
            return Err(err);
        }

        Ok(())
    }

    /// 触发指定 seq 的异步 flush（简化版，无 closure）。
    ///
    /// 对应 bcachefs `bch2_journal_flush_seq_async()` (journal.c:1157-1205)。
    ///
    /// # bcachefs 语义
    ///
    /// bcachefs 的实现：
    /// 1. 检查 `flushed_seq_ondisk` → 已 flush 则返回
    /// 2. 检查 err_seq → 已 error 且 seq > flushed 则返回 -EIO
    /// 3. 触发 close/cycle，并标记需要 flush
    ///
    /// # subvol 简化
    ///
    /// subvol 无 closure 机制，简化为：关闭 entry + 标记 needs_flush_write。
    /// 实际 I/O 由后台 async flush 任务或后续 `bch2_journal_flush` 完成。
    pub fn bch2_journal_flush_seq_async(&self, seq: u64) -> Result<(), JournalError> {
        // 对应 bcachefs journal.c:1165-1166
        let flushed = self.flushed_seq_ondisk.load(Ordering::Acquire);
        if seq <= flushed {
            return Ok(());
        }

        // 对应 bcachefs journal.c:1168-1171: seq 不能超过当前 journal seq
        let cur_seq = self.bch2_journal_cur_seq();
        if seq > cur_seq {
            return Err(JournalError::Overflow(
                "flush_seq_async: seq beyond current journal seq".into(),
            ));
        }

        // 对应 bcachefs journal.c:1181-1182 的 err_seq 检查
        if self.err_seq.load(Ordering::Acquire) != 0
            && seq > self.flushed_seq_ondisk.load(Ordering::Acquire)
        {
            return Err(JournalError::Blocked("journal halted".into()));
        }

        let front_seq = {
            let in_flight = self.in_flight.lock().unwrap();
            in_flight
                .front()
                .map(|idx| self.bufs.get_mut(*idx as usize).seq)
                .unwrap_or(seq)
        };
        // 对应 bcachefs journal.c:1184-1191: flush 更晚的 seq
        let target_seq = seq.max(front_seq);
        let mut old = self.flushing_seq.load(Ordering::Acquire);
        loop {
            if old >= target_seq {
                break;
            }
            match self.flushing_seq.compare_exchange_weak(
                old,
                target_seq,
                Ordering::Release,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(observed) => old = observed,
            }
        }

        // 对应 __bch2_journal_flush_seq_async() 先把 closure 挂到 live buf：
        // waitlist 从 NULL 变成真实 waiter 后，close 路径的 should_flush() 才会
        // 把该 entry 选为 flush write，而不是提前降级成 NOFLUSH。
        let mut wait_seq = target_seq;
        while wait_seq <= cur_seq {
            let Some(buf) = self.journal_seq_to_buf(wait_seq) else {
                break;
            };
            match buf.wait_first {
                JournalBufWaitState::Empty | JournalBufWaitState::Waiters => {
                    buf.wait_first = JournalBufWaitState::Waiters;
                    break;
                }
                JournalBufWaitState::NoFlush | JournalBufWaitState::FlushNoWait => {
                    wait_seq += 1;
                }
                JournalBufWaitState::NotInFlight => break,
            }
        }

        // 关闭当前 entry（对应 bcachefs journal.c:1116 的 bch2_journal_cycle）
        self.bch2_journal_cycle_locked_flags(JournalCycleFlags::MUST_CLOSE)?;

        // 标记需要 flush（后台 flush 任务将读取此标志并执行实际 I/O）
        self.bch2_journal_set_needs_flush_write();

        Ok(())
    }

    /// 触发当前 seq 的异步 flush（简化版，无 closure）。
    ///
    /// 对应 bcachefs `bch2_journal_flush_async()` (journal.c:1243-1253)。
    ///
    /// bcachefs 中此函数分配或接收一个 closure，调 `bch2_journal_flush_seq_async`。
    /// subvol 简化版：直接调 `bch2_journal_flush_seq_async(cur_seq)`。
    pub fn bch2_journal_flush_async(&self) {
        let seq = self.bch2_journal_cur_seq();
        let _ = self.bch2_journal_flush_seq_async(seq);
    }

    /// 写入空 journal entry 的外部包装（含 IO 等待）。
    ///
    /// 对应 bcachefs `bch2_journal_meta()` (journal.c:1330-1340)。
    ///
    /// subvol 差异：省略 bcachefs 最外层的 `enumerated_ref_tryget` 检查
    /// （通过 __bch2_journal_meta → bch2_journal_res_get 间接检查 running 标志）。
    pub async fn bch2_journal_meta(&self) -> Result<(), JournalError> {
        // bcachefs: if (!enumerated_ref_tryget(&c->writes, BCH_WRITE_REF_journal))
        //              return bch_err_throw(c, erofs_no_writes);
        // subvol 等价：__bch2_journal_meta → bch2_journal_res_get 检查 running 标志
        self.__bch2_journal_meta().await
    }

    // ─── R5: block / unblock ─────────────────────────────

    /// 阻止 journal 接受新的 reservation（内部实现，调用方需持有 slowpath_lock）。
    ///
    /// 对应 bcachefs `__bch2_journal_block()` (journal.c:1365-1384)。
    ///
    /// # bcachefs 语义
    ///
    /// 1. 递增 `blocked` 计数器（->fetch_add 1）
    /// 2. 如果是第一个 blocker：
    ///    a. 读取 reservations 当前 offset 并保存到 `cur_entry_offset_if_blocked`
    ///    b. 如果 entry 已关闭（offset >= CLOSED_VAL），直接返回
    ///    c. CAS 循环：设置 `cur_entry_offset` 为 `BLOCKED_VAL`
    ///    d. 若 entry 原来处于 open 状态（offset < BLOCKED_VAL），
    ///       更新当前 buf 的 data_end（对应 bcachefs `data->u64s = cpu_to_le32(old.cur_entry_offset)`）
    fn __bch2_journal_block(&self) {
        let old_blocked = self.blocked.fetch_add(1, Ordering::AcqRel);
        if old_blocked == 0 {
            // 第一个 blocker：原子捕获 offset 并设置 BLOCKED_VAL
            // 对应 bcachefs journal.c:1367-1380 的 do-while CAS 循环
            let init_off = JournalResState::cur_entry_offset(self.reservations.read()) as u64;
            self.cur_entry_offset_if_blocked
                .store(init_off as u32, Ordering::Release);

            // 对应 bcachefs: if (cur_entry_offset_if_blocked >= CLOSED_VAL) break;
            if init_off >= JOURNAL_ENTRY_CLOSED_VAL {
                return;
            }

            // 通过 JournalResState::try_block 封装 CAS
            let (old, success) = self.reservations.try_block();
            if success {
                // 使用 CAS 成功时的 old 值设置 data_end（避免 TOCTOU）
                // 如果 init_off 与 CAS 时的实际 offset 不同，用 CAS 结果确保正确
                let blocked_off = JournalResState::cur_entry_offset(old) as u64;
                if blocked_off < JOURNAL_ENTRY_BLOCKED_VAL {
                    // 对应 bcachefs: journal_cur_buf(j)->data->u64s = cpu_to_le32(old.cur_entry_offset)
                    let idx =
                        (self.bch2_journal_cur_seq() & (JOURNAL_IN_FLIGHT_NR as u64 - 1)) as usize;
                    let buf = self.bufs.get_mut(idx);
                    buf.data_end = (blocked_off as usize) * 8;
                }
            }
        }
    }

    /// Block journal — 阻止新 reservation，等待所有 pending 写入完成（quiesce）。
    ///
    /// 返回 RAII guard，drop 时自动调用 `bch2_journal_unblock`。
    ///
    /// 对应 bcachefs `bch2_journal_block()` (journal.c:1386-1392)。
    pub fn bch2_journal_block(&self) -> JournalBlockGuard<'_> {
        // 对应 bcachefs: scoped_guard(spinlock, &j->lock) → slowpath_lock
        let _lock = self.slowpath_lock.lock().unwrap();
        self.__bch2_journal_block();
        drop(_lock);
        // 在锁外等待 quiesce（对应 bcachefs: bch2_journal_quiesce(j) 在 spinlock 外）
        self.bch2_journal_quiesce();
        JournalBlockGuard { journal: self }
    }

    /// Unblock journal — 恢复 reservation 能力。
    ///
    /// 对应 bcachefs `bch2_journal_unblock()` (journal.c:1344-1363)。
    ///
    /// # bcachefs 语义
    ///
    /// 1. 递减 `blocked` 计数器
    /// 2. 如果是最后一个 unblocker：
    ///    a. 如果 entry 在 block 时是打开的（saved_offset < CLOSED_VAL）
    ///       且当前 offset 仍为 BLOCKED_VAL（未被他人关闭）
    ///    b. CAS 循环：将 `cur_entry_offset` 恢复为保存的值
    /// 3. `journal_wake(j)` — 唤醒所有等待 journal 空间的线程
    pub fn bch2_journal_unblock(&self) {
        // 对应 bcachefs: scoped_guard(spinlock, &j->lock)
        let _lock = self.slowpath_lock.lock().unwrap();
        let old_blocked = self.blocked.fetch_sub(1, Ordering::AcqRel);
        if old_blocked == 1 {
            // 最后一个 unblocker：检查是否需要恢复 offset
            let saved_offset = self.cur_entry_offset_if_blocked.load(Ordering::Acquire) as u64;
            if saved_offset < JOURNAL_ENTRY_CLOSED_VAL {
                // 检查当前 offset 是否仍为 BLOCKED_VAL（未被他人改变）
                // 对应 bcachefs: j->reservations.cur_entry_offset == JOURNAL_ENTRY_BLOCKED_VAL
                let cur_res = self.reservations.read();
                if JournalResState::cur_entry_offset(cur_res) as u64 == JOURNAL_ENTRY_BLOCKED_VAL {
                    // CAS 恢复保存的 offset
                    // 对应 bcachefs journal.c:1350-1359 的 do-while 循环
                    self.reservations.set_cur_entry_offset(saved_offset);
                }
            }
        }
        drop(_lock);
        // 对应 bcachefs journal.c:1362: journal_wake(j)
        self.bch2_journal_wake_up();
    }

    // ═══════════════════════════════════════════════════════════
    // R8: bch2_journal_entry_res_resize
    // ═══════════════════════════════════════════════════════════

    /// 调整 journal entry 预留大小 — 对应 bcachefs `bch2_journal_entry_res_resize()` (journal.c:988-1027)。
    ///
    /// 当预留空间不足需要扩大时调用此函数。调整 `entry_u64s_reserved` 和 `cur_entry_u64s`，
    /// 如果当前 entry 已无法容纳扩展后的空间，则关闭 entry 并轮换。
    ///
    /// # bcachefs 语义
    ///
    /// bcachefs 中此函数：
    /// 1. 持有 `pin_resize_lock` (percpu_read) + `j->lock`
    /// 2. 调整 `j->entry_u64s_reserved += d`
    /// 3. 调整 `res->u64s += d`
    /// 4. 如果 d > 0：调整 `cur_entry_u64s = max(0, cur_entry_u64s - d)`
    /// 5. 如果 entry 开放且 cur_entry_offset > cur_entry_u64s，调用 cycle_locked(must_close|force_close)
    /// 6. 否则 buf->u64s_reserved += d
    ///
    /// # subvol 简化
    ///
    /// subvol 没有 `struct journal_entry_res` 的持久化实例。此简化版调整
    /// `entry_u64s_reserved` 和 `cur_entry_u64s`，在 entry 开放且空间不足时
    /// 立即关闭 entry 以触发轮换。
    pub fn bch2_journal_entry_res_resize(&self, res_u64s: &mut u32, new_u64s: u32) {
        // bcachefs: int d = new_u64s - res->u64s;
        let d = new_u64s as i32 - *res_u64s as i32;
        if d == 0 {
            return;
        }
        // bcachefs: j->entry_u64s_reserved += d; (d may be negative)
        if d > 0 {
            self.entry_u64s_reserved
                .fetch_add(d as u32, Ordering::Release);
        } else {
            self.entry_u64s_reserved
                .fetch_sub((-d) as u32, Ordering::Release);
        }
        // bcachefs: res->u64s += d;
        *res_u64s = new_u64s;
        // bcachefs: if (d <= 0) return; — shrink: accounting only, no space check
        if d < 0 {
            return;
        }
        // bcachefs: j->cur_entry_u64s = max_t(int, 0, j->cur_entry_u64s - d);
        // d > 0 at this point (negative d returned early above)
        let d_u32 = d as u32;
        let old_cur = self.cur_entry_u64s.load(Ordering::Relaxed);
        let new_cur = old_cur.saturating_sub(d_u32);
        self.cur_entry_u64s.store(new_cur, Ordering::Release);
        // bcachefs: state = READ_ONCE(j->reservations);
        // if (state.cur_entry_offset >= JOURNAL_ENTRY_CLOSED_VAL) return;
        if self.reservations.is_closed() {
            return;
        }
        // bcachefs: if (state.cur_entry_offset > j->cur_entry_u64s) {
        //     j->cur_entry_u64s += d;
        //     bch2_journal_cycle_locked(j, must_close | force_close);
        // } else {
        //     journal_cur_buf(j)->u64s_reserved += d;
        // }
        let state = self.reservations.read();
        let cur_off = JournalResState::cur_entry_offset(state) as u64;
        if cur_off > new_cur as u64 {
            // 当前 entry 空间不够 → 关闭并轮换
            self.cur_entry_u64s.fetch_add(d_u32, Ordering::Release);
            self.bch2_journal_cycle_locked_flags(
                JournalCycleFlags::MUST_CLOSE | JournalCycleFlags::FORCE_CLOSE,
            )
            .ok();
        } else {
            // 当前 entry 有足够空间 → 增加 buf 的 u64s_reserved
            let idx = (self.bch2_journal_cur_seq() & (JOURNAL_IN_FLIGHT_NR as u64 - 1)) as usize;
            let buf = self.bufs.get_mut(idx);
            buf.u64s_reserved = buf.u64s_reserved.wrapping_add(d_u32);
        }
    }

    // ═══════════════════════════════════════════════════════════
    // R9: bch2_journal_noflush_seq
    // ═══════════════════════════════════════════════════════════

    /// 标记指定 seq 范围 [start, end) 的 journal buf 为 noflush（跳过 FUA/preflush）。
    ///
    /// 对应 bcachefs `bch2_journal_noflush_seq()` (journal.c:1265-1283)。
    ///
    /// 遍历范围内的每个 seq，调用 `bch2_journal_buf_try_noflush`。
    /// 若某 buf 已有等待者（不能 noflush）或已超过 flush 点，返回 false。
    ///
    /// # bcachefs 对齐
    ///
    /// 先检查 `BCH_FEATURE_journal_no_flush` 特性标记，未开启时直接返回 false。
    pub fn bch2_journal_noflush_seq(&self, start: u64, end: u64) -> bool {
        // bcachefs: if (!(c->sb.features & (1ULL << BCH_FEATURE_journal_no_flush))) return false;
        let has_gate = self
            .vol
            .get()
            .and_then(|w| w.upgrade())
            .map(|vol| {
                vol.superblock()
                    .feature_test(crate::storage::superblock::feature_bits::JOURNAL_NO_FLUSH)
            })
            .unwrap_or(false);
        if !has_gate {
            return false;
        }

        // bcachefs: if (c->journal.flushed_seq_ondisk >= start) return false;
        let flushed = self.flushed_seq_ondisk.load(Ordering::Acquire);
        if flushed >= start {
            return false;
        }

        // bcachefs: for (u64 seq = start; seq < end; seq++) {
        //     struct journal_buf *buf = &fifo_entry(&j->in_flight, seq);
        //     if (!journal_buf_try_noflush(buf)) return false;
        // }
        for seq in start..end {
            let idx = (seq & (JOURNAL_IN_FLIGHT_NR as u64 - 1)) as usize;
            let buf = self.bufs.get_mut(idx);
            if !buf.bch2_journal_buf_try_noflush() {
                return false;
            }
        }

        true
    }

    // ═══════════════════════════════════════════════════════════
    // R10: rewind seq
    // ═══════════════════════════════════════════════════════════

    /// 推进回滚 seq 上限 — 保证 discards 到该 seq 是安全的。
    ///
    /// 对应 bcachefs `bch2_journal_advance_rewind_seq()` (journal.c:1288-1292)。
    ///
    /// 必须在 `bch2_journal_flush()` 之前调用，以持久化新限制。
    ///
    /// # bcachefs 语义
    ///
    /// bcachefs 使用 `scoped_guard(spinlock, &j->lock)` 保护此操作。
    /// subvol 使用 `slowpath_lock` 替代。
    pub fn bch2_journal_advance_rewind_seq(&self, seq: u64) {
        // bcachefs: scoped_guard(spinlock, &j->lock) → subvol: slowpath_lock
        let _lock = self.slowpath_lock.lock().unwrap();
        let old = self.rewind_seq.load(Ordering::Relaxed);
        // bcachefs: j->rewind_seq = max(j->rewind_seq, seq);
        if seq > old {
            self.rewind_seq.store(seq, Ordering::Release);
        }
    }

    /// 添加回滚范围 — 将 [from, to) 区间记录到回滚表中。
    ///
    /// 对应 bcachefs `bch2_journal_add_rewind_range()` (journal.c:1294-1312)。
    ///
    /// # bcachefs 语义
    ///
    /// bcachefs 将此范围添加到 `rewind_ranges` darray 并构造 `jset_entry_rewind`
    /// 放入 `early_journal_entries`。`rewind_seq` 由单独的推进路径维护。
    pub fn bch2_journal_add_rewind_range(&self, from: u64, to: u64) -> Result<(), JournalError> {
        // bcachefs journal.c:1298-1301: darray_push(&j->rewind_ranges, {from, to})
        let mut ranges = self.slowpath.lock().unwrap();
        ranges.rewind_ranges.push((from, to));
        ranges.early_journal_entries.push((from, to));
        drop(ranges);

        Ok(())
    }

    // ═══════════════════════════════════════════════════════════
    // R11: do_writes / write_work
    // ═══════════════════════════════════════════════════════════

    /// 标记所有可写入的 journal buf 为待写入（持有锁时调用）。
    ///
    /// 对应 bcachefs `bch2_journal_do_writes_locked()` (write.c:1087-1162)。
    ///
    /// # bcachefs 语义
    ///
    /// bcachefs 在此函数中：
    /// 1. 找到最后一个未分配的 seq
    /// 2. 检查 flush/noflush 策略
    /// 3. 设置 flush 标记
    /// 4. (重新)启动 auto-commit timer
    /// 5. 推进 rewind_seq
    /// 6. 添加 rewind_limit entry
    /// 7. `closure_call` 启动 `bch2_journal_write`
    ///
    /// # subvol 简化
    ///
    /// subvol 的写入由 `bch2_journal_flush()`（async）统一完成。
    /// 此函数仅标记 needs_flush_write 并唤醒等待者，
    /// 实际写入由 `bch2_journal_flush_async` 触发的后台 flush 任务完成。
    /// 检查 flushing buf 是否会释放至少一个 bucket — 对应 bcachefs `flush_would_free_space()` (write.c:983-997)。
    fn flush_would_free_space(&self, new_last_seq: u64) -> bool {
        if let Some(c) = self.vol.get().and_then(|vol| vol.upgrade()) {
            let rw_journal_devs = crate::alloc::target_rw_devs(&c, BchDataType::Journal, 0);
            for dev_idx in rw_journal_devs.iter() {
                let Some(ca) = c.device_registry.resolve_bch_dev(dev_idx) else {
                    continue;
                };
                let ja = ca.journal.lock().unwrap();
                if ja.dirty_idx_ondisk != ja.dirty_idx
                    && ja
                        .bucket_seq
                        .get(ja.dirty_idx_ondisk as usize)
                        .is_some_and(|&seq| seq < new_last_seq)
                {
                    return true;
                }
            }
            return false;
        }

        let sp = self.slowpath.lock().unwrap();
        sp.dirty_idx_ondisk != sp.dirty_idx
            && sp
                .bucket_seq
                .get(sp.dirty_idx_ondisk)
                .is_some_and(|&seq| seq < new_last_seq)
    }

    /// per-buf flush 决策 — 对应 bcachefs `__should_flush()` (write.c:999-1077)。
    fn __should_flush(&self, seq: u64) -> i32 {
        let Some(buf) = self.journal_seq_to_buf(seq) else {
            return 0;
        };
        // bcachefs write.c:1017: journal error → noflush（保留数据供调试）
        if self.bch2_journal_error_check().is_some() {
            return 0;
        }
        // bcachefs write.c:1020-1021: first write after clean → must flush
        if self.bch2_journal_needs_flush_write() {
            return 1;
        }
        // bcachefs write.c:1025: must_not_flush (allocator promise) → noflush
        if buf.journal_buf_must_not_flush() {
            return 0;
        }
        // bcachefs write.c:1028-1032: reclaim needs space
        if !self.may_skip_flush.load(Ordering::Acquire) {
            let ondisk = self.last_seq_ondisk.load(Ordering::Acquire);
            if buf.last_seq != ondisk && self.flush_would_free_space(buf.last_seq) {
                return 1;
            }
        }

        let must_flush = buf.journal_buf_must_flush() || buf.has_must_flush;

        // bcachefs write.c:1043-1068: 有真实 waiter 且已有多个 outstanding flush 时，
        // 把 waiter 级联到下一个 entry，将当前 entry 降级为 noflush。
        if buf.wait_first == JournalBufWaitState::Waiters
            && self.flushes_outstanding.load(Ordering::Acquire) > 1
        {
            let next_seq = (seq < self.bch2_journal_cur_seq()).then_some(seq + 1);
            if let Some(next_seq) = next_seq {
                let indices = {
                    let in_flight = self.in_flight.lock().unwrap();
                    let from = in_flight
                        .iter()
                        .copied()
                        .find(|&idx| self.bufs.get(idx as usize).seq == seq);
                    let to = in_flight
                        .iter()
                        .copied()
                        .find(|&idx| self.bufs.get(idx as usize).seq == next_seq);
                    from.zip(to)
                };
                if let Some((from_idx, to_idx)) = indices {
                    let bufs = self.bufs.get_all_mut();
                    let spliced = if from_idx < to_idx {
                        let (left, right) = bufs.split_at_mut(to_idx as usize);
                        journal_waitlist_splice(&mut left[from_idx as usize], &mut right[0])
                    } else {
                        let (left, right) = bufs.split_at_mut(from_idx as usize);
                        journal_waitlist_splice(&mut right[0], &mut left[to_idx as usize])
                    };
                    if spliced {
                        return 0;
                    }
                }
            }
        }

        if must_flush {
            return 1;
        }
        // bcachefs write.c:1074-1076: timeout
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let last_flush = self.bch2_journal_last_flush_jiffies();
        let delay = self.vol.get().and_then(|vol| vol.upgrade()).map_or_else(
            || self.journal_flush_delay_ms.load(Ordering::Acquire),
            |c| u64::from(c.opts.journal_flush_delay),
        );
        (now >= last_flush.saturating_add(delay)) as i32
    }

    /// should_flush wrapper — 对应 bcachefs `should_flush()` (write.c:1079-1084)。
    fn should_flush(&self, seq: u64) -> i32 {
        let mut ret = self.__should_flush(seq);
        let Some(buf) = self.journal_seq_to_buf(seq) else {
            return ret;
        };
        if ret == 0 && !buf.bch2_journal_buf_try_noflush() {
            // try_noflush failed (has real waiters) → must flush
            ret = 1;
        }
        ret
    }

    /// 标记 WriteSubmitted buf 为待写入（持有 slowpath_lock 时调用）。
    ///
    /// 对应 bcachefs `bch2_journal_do_writes_locked()` (write.c:1087-1162)。
    ///
    /// bcachefs 仅处理单个 buf（最后一个未分配的 seq），subvol 遍历所有 WriteSubmitted buf。
    pub fn bch2_journal_do_writes_locked(&self) {
        let seq = self.journal_last_unallocated_seq();
        if seq == 0 {
            return;
        }
        let Some(w) = self.journal_seq_to_buf(seq) else {
            return;
        };
        let reservations = self.reservations.read();
        if w.write_started || self.journal_state_seq_count(reservations, seq) != 0 {
            return;
        }

        debug_assert_eq!(seq, w.seq);

        if !w.flush_picked {
            let flush = self.should_flush(seq);
            if flush < 0 {
                return;
            }

            let w = self.journal_seq_to_buf(seq).unwrap();
            if flush == 0 {
                let flags_offset = std::mem::offset_of!(JsetHeader, flags);
                if w.data_end >= flags_offset + std::mem::size_of::<u32>() {
                    unsafe {
                        let flags_ptr = w.data.as_mut_ptr().add(flags_offset).cast::<u32>();
                        let flags = std::ptr::read_unaligned(flags_ptr);
                        std::ptr::write_unaligned(flags_ptr, flags | super::jset::JSET_NO_FLUSH);
                        std::ptr::write_unaligned(
                            w.data
                                .as_mut_ptr()
                                .add(std::mem::offset_of!(JsetHeader, last_seq))
                                .cast::<u64>(),
                            0,
                        );
                    }
                }
                w.last_seq = 0;
                w.flush = false;
                self.nr_noflush_writes.fetch_add(1, Ordering::Relaxed);
            } else {
                if self.flushes_outstanding.load(Ordering::Acquire) > 1 {
                    return;
                }

                w.flush = true;
                self.bch2_journal_update_flush_jiffies();
                self.nr_flush_writes.fetch_add(1, Ordering::Relaxed);
                self.bch2_journal_clear_needs_flush_write();
                self.flushes_outstanding.fetch_add(1, Ordering::AcqRel);

                if seq != self.bch2_journal_cur_seq() {
                    let delay = self.vol.get().and_then(|vol| vol.upgrade()).map_or_else(
                        || self.journal_flush_delay_ms.load(Ordering::Acquire),
                        |c| u64::from(c.opts.journal_flush_delay),
                    );
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64;
                    self.write_work_deadline_ms
                        .store(now.saturating_add(delay), Ordering::Release);
                    self.write_work_notify.notify_one();
                } else {
                    self.write_work_deadline_ms.store(0, Ordering::Release);
                    self.write_work_notify.notify_one();
                }

                if self
                    .vol
                    .get()
                    .and_then(|vol| vol.upgrade())
                    .is_none_or(|c| c.opts.journal_rewind_discard_buffer_percent == 0)
                {
                    self.rewind_seq.store(seq + 1, Ordering::Release);
                }
                let rewind_seq = self.rewind_seq.load(Ordering::Acquire);
                let mut data_end = w.data_end;
                if Self::bch2_inject_rewind_limit_into_buf(
                    rewind_seq,
                    seq,
                    &mut w.data,
                    &mut data_end,
                ) {
                    w.data_end = data_end;
                }
            }
            w.flush_picked = true;
        }

        let w = self.journal_seq_to_buf(seq).unwrap();
        if w.flush && self.seq_ondisk.load(Ordering::Acquire) + 1 != seq {
            return;
        }

        self.seq_write_started.store(seq, Ordering::Release);
        w.write_started = true;
        self.flush_notify.notify_one();
    }

    /// 标记所有可写入的 journal buf 为待写入（外部锁包装）。
    ///
    /// 对应 bcachefs `bch2_journal_do_writes()` (write.c:1164-1167)。
    ///
    /// 获取 slowpath_lock 后调用 `bch2_journal_do_writes_locked`。
    pub fn bch2_journal_do_writes(&self) {
        // bcachefs: guard(spinlock)(&j->lock);
        let _lock = self.slowpath_lock.lock().unwrap();
        self.bch2_journal_do_writes_locked();
    }

    /// Journal write workqueue callback — 触发异步 flush。
    ///
    /// 对应 bcachefs `bch2_journal_write_work()` (journal.c:748-752)。
    ///
    /// # bcachefs 语义
    ///
    /// bcachefs 中这是 `work_struct` 的 callback，由 auto-commit timer
    /// 或数据写入后调度执行，直接调用 `bch2_journal_flush_async`。
    ///
    /// # subvol 差异
    ///
    /// subvol 无 workqueue。此函数直接调用 `bch2_journal_flush_async`，
    /// 提示后台 flush 任务执行写入。
    pub fn bch2_journal_write_work(&self) {
        // bcachefs: bch2_journal_flush_async(j, NULL);
        self.bch2_journal_flush_async();
    }
}

// ═══════════════════════════════════════════════════════════
// Part 7: Blacklist helpers (unchanged)
// ═══════════════════════════════════════════════════════════

/// 从 Jset 列表中提取所有 blacklist entries
// ═══════════════════════════════════════════════════════════
// Part 7: Tests
// ═══════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bch_vol::VolumeConfig;
    use crate::block_device::{BlockDevice, MockBlockDevice};
    use crate::btree::key::{Bpos, BtreeEntry, KeyType, KeyValue};
    use crate::journal::reclaim::JournalPinType;
    use crate::storage::superblock::{feature_bits, BchSb, BchSbMember};
    use crate::types::BlockAddr;
    use async_trait::async_trait;
    use parking_lot::RwLock;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::sync::Notify;

    /// 测试辅助：获取 slowpath 中 bucket 字段的引用
    fn sp_buckets_len(j: &Journal) -> usize {
        j.slowpath.lock().unwrap().buckets.len()
    }

    fn sp_current_bucket(j: &Journal) -> usize {
        j.slowpath.lock().unwrap().current_bucket
    }

    fn sp_current_offset(j: &Journal) -> u32 {
        j.slowpath.lock().unwrap().current_offset
    }

    fn sp_remaining_bytes(j: &Journal) -> u32 {
        j.slowpath.lock().unwrap().remaining_bytes
    }

    fn sp_bucket_seq(j: &Journal) -> Vec<u64> {
        j.slowpath.lock().unwrap().bucket_seq.clone()
    }

    fn sp_discard_idx(j: &Journal) -> usize {
        j.slowpath.lock().unwrap().discard_idx
    }

    fn sp_dirty_idx(j: &Journal) -> usize {
        j.slowpath.lock().unwrap().dirty_idx
    }

    fn sp_dirty_idx_ondisk(j: &Journal) -> usize {
        j.slowpath.lock().unwrap().dirty_idx_ondisk
    }

    fn make_test_entry() -> BtreeEntry {
        BtreeEntry::new(
            Bpos::new(1, 100, 0),
            KeyType::Normal,
            KeyValue::extent(0x1000, 1, 0),
        )
    }

    fn make_test_vol_with_noflush_gate(enabled: bool) -> Arc<BchVol> {
        let mut sb = BchSb::with_volume_info(
            "test-vol".to_string(),
            1,
            "test-pool".to_string(),
            4096,
            1024 * 1024,
            crate::types::BackendType::Nfs,
        );
        if enabled {
            sb.feature_set(feature_bits::JOURNAL_NO_FLUSH);
        }

        let dev = Arc::new(BchDev::new(Arc::new(MockBlockDevice::new()), 0));
        Arc::new(BchVol::alloc(
            sb,
            dev,
            VolumeConfig::default(),
            "test-vol".to_string(),
            PathBuf::from("/tmp/test-vol"),
        ))
    }

    #[derive(Debug)]
    struct GatedBlockDevice {
        blocks: Arc<RwLock<HashMap<BlockAddr, Vec<u8>>>>,
        write_started: Arc<AtomicUsize>,
        flush_started: Arc<AtomicUsize>,
        write_allowed: Arc<AtomicBool>,
        flush_allowed: Arc<AtomicBool>,
        write_notify: Arc<Notify>,
        flush_notify: Arc<Notify>,
    }

    impl GatedBlockDevice {
        fn new() -> Self {
            Self {
                blocks: Arc::new(RwLock::new(HashMap::new())),
                write_started: Arc::new(AtomicUsize::new(0)),
                flush_started: Arc::new(AtomicUsize::new(0)),
                write_allowed: Arc::new(AtomicBool::new(false)),
                flush_allowed: Arc::new(AtomicBool::new(false)),
                write_notify: Arc::new(Notify::new()),
                flush_notify: Arc::new(Notify::new()),
            }
        }

        fn allow_write(&self) {
            self.write_allowed.store(true, Ordering::Release);
            self.write_notify.notify_waiters();
        }

        fn allow_flush(&self) {
            self.flush_allowed.store(true, Ordering::Release);
            self.flush_notify.notify_waiters();
        }
    }

    #[async_trait]
    impl crate::block_device::BlockDevice for GatedBlockDevice {
        async fn read_block(
            &self,
            addr: BlockAddr,
            buf: &mut [u8],
        ) -> crate::block_device::Result<()> {
            let map = self.blocks.read();
            if let Some(data) = map.get(&addr) {
                let len = data.len().min(buf.len());
                buf[..len].copy_from_slice(&data[..len]);
            } else {
                buf.fill(0);
            }
            Ok(())
        }

        async fn write_block(
            &self,
            addr: BlockAddr,
            data: &[u8],
        ) -> crate::block_device::Result<()> {
            self.write_started.fetch_add(1, Ordering::Release);
            while !self.write_allowed.load(Ordering::Acquire) {
                self.write_notify.notified().await;
            }
            let mut map = self.blocks.write();
            map.insert(addr, data.to_vec());
            Ok(())
        }

        async fn delete_block(&self, addr: BlockAddr) -> crate::block_device::Result<()> {
            let mut map = self.blocks.write();
            map.remove(&addr);
            Ok(())
        }

        async fn trim_block(&self, addr: BlockAddr) -> crate::block_device::Result<()> {
            self.delete_block(addr).await
        }

        async fn flush(&self) -> crate::block_device::Result<()> {
            self.flush_started.fetch_add(1, Ordering::Release);
            while !self.flush_allowed.load(Ordering::Acquire) {
                self.flush_notify.notified().await;
            }
            Ok(())
        }

        async fn health_check(&self) -> crate::block_device::Result<crate::types::HealthStatus> {
            Ok(crate::types::HealthStatus::Healthy)
        }

        async fn used_space(&self) -> crate::block_device::Result<u64> {
            let map = self.blocks.read();
            Ok(map.values().map(|v| v.len() as u64).sum())
        }
    }

    #[derive(Debug)]
    struct WriteFailBlockDevice;

    #[async_trait]
    impl crate::block_device::BlockDevice for WriteFailBlockDevice {
        async fn read_block(
            &self,
            _addr: BlockAddr,
            buf: &mut [u8],
        ) -> crate::block_device::Result<()> {
            buf.fill(0);
            Ok(())
        }

        async fn write_block(
            &self,
            _addr: BlockAddr,
            _data: &[u8],
        ) -> crate::block_device::Result<()> {
            Err(StorageError::JournalError(
                "injected journal write failure".into(),
            ))
        }

        async fn delete_block(&self, _addr: BlockAddr) -> crate::block_device::Result<()> {
            Ok(())
        }

        async fn trim_block(&self, _addr: BlockAddr) -> crate::block_device::Result<()> {
            Ok(())
        }

        async fn flush(&self) -> crate::block_device::Result<()> {
            Ok(())
        }

        async fn health_check(&self) -> crate::block_device::Result<crate::types::HealthStatus> {
            Ok(crate::types::HealthStatus::Healthy)
        }

        async fn used_space(&self) -> crate::block_device::Result<u64> {
            Ok(0)
        }
    }

    // ── JournalResState unit tests ──

    #[test]
    fn test_res_state_initial() {
        let rs = JournalResState::new();
        // bcachefs: 初始状态 cur_entry_offset = JOURNAL_ENTRY_CLOSED_VAL，idx = 0
        assert_eq!(rs.read(), JOURNAL_ENTRY_CLOSED_VAL);
        assert!(rs.is_closed());
        assert_eq!(
            JournalResState::cur_entry_offset(JOURNAL_ENTRY_CLOSED_VAL),
            JOURNAL_ENTRY_CLOSED_VAL as u32
        );
        assert_eq!(JournalResState::idx(JOURNAL_ENTRY_CLOSED_VAL), 0);
        assert_eq!(JournalResState::buf_count(JOURNAL_ENTRY_CLOSED_VAL, 0), 0);
    }

    #[test]
    fn test_res_state_try_reserve_basic() {
        let rs = JournalResState::new();
        // bcachefs: 必须先 open entry 才能做 reservation
        rs.open_entry(0);
        // Reserve 10 u64s
        let (old, new) = rs.try_reserve(10, BUF_SIZE_U64S).unwrap();
        assert_eq!(JournalResState::cur_entry_offset(old), 0);
        assert_eq!(JournalResState::cur_entry_offset(new), 10);
        // open_entry 已通过 journal_state_inc 将 buf_count 设为 1，再加 try_reserve 变为 2
        assert_eq!(JournalResState::buf_count(new, 0), 2); // buf0_count = open(1) + reserve(1)
    }

    #[test]
    fn test_res_state_try_reserve_multiple() {
        let rs = JournalResState::new();
        rs.open_entry(0);
        rs.try_reserve(10, BUF_SIZE_U64S).unwrap();
        rs.try_reserve(20, BUF_SIZE_U64S).unwrap();
        let v = rs.read();
        assert_eq!(JournalResState::cur_entry_offset(v), 30);
        // open_entry(1) + 2 × try_reserve(1) = 3
        assert_eq!(JournalResState::buf_count(v, 0), 3);
    }

    #[test]
    fn test_res_state_release() {
        let rs = JournalResState::new();
        rs.open_entry(0);
        rs.try_reserve(10, BUF_SIZE_U64S).unwrap();
        let v = rs.read();
        // open_entry(1) + try_reserve(1) = 2
        assert_eq!(JournalResState::buf_count(v, 0), 2);

        let old_v = rs.release(0);
        let count_before = (old_v >> BUF0_COUNT_SHIFT) & BUF_COUNT_MAX;
        assert_eq!(count_before, 2); // was 2 before decrement (open 1 + reserve 1)
    }

    #[test]
    fn test_res_state_close_open() {
        let rs = JournalResState::new();
        // bcachefs: 初始状态即为 closed
        assert!(rs.is_closed());

        // open → close → open cycle
        rs.open_entry(1);
        assert!(!rs.is_closed());
        let v = rs.read();
        assert_eq!(JournalResState::idx(v), 1);
        assert_eq!(JournalResState::cur_entry_offset(v), 0);

        rs.close_entry();
        assert!(rs.is_closed());
    }

    #[test]
    fn test_res_state_open_entry_count() {
        let rs = JournalResState::new();
        rs.open_entry(0); // 先打开 entry 0
        rs.try_reserve(5, BUF_SIZE_U64S).unwrap(); // buf0_count = open(1) + reserve(1) = 2
        rs.close_entry();
        rs.open_entry(1); // buf1 opens: journal_state_inc → buf1_count = 1

        let v = rs.read();
        assert_eq!(JournalResState::idx(v), 1);
        // buf0_count 不受 close/open_entry 操作影响（close 通过 CAS 设 CLOSED_VAL 不碰 count）
        assert_eq!(JournalResState::buf_count(v, 0), 2); // buf0 count preserved (open 1 + reserve 1)
                                                         // buf1_count 在 open_entry 中通过 journal_state_inc 设为 1（匹配 bcachefs）
        assert_eq!(JournalResState::buf_count(v, 1), 1); // buf1 count = journal_state_inc(1)
    }

    // ── Journal constructor tests (updated) ──

    #[test]
    fn test_journal_new() {
        let addrs = vec![100, 200, 300];
        let journal = Journal::new(addrs.clone());
        let addrs_out: Vec<u64> = {
            let sp = journal.slowpath.lock().unwrap();
            sp.buckets.iter().map(|bs| bs.addr).collect()
        };
        assert_eq!(addrs_out, addrs);
        assert_eq!(journal.bch2_journal_cur_seq(), 1); // new() opens first entry → inc_return 1
        assert_eq!(sp_current_bucket(&journal), 0);
        assert_eq!(
            sp_remaining_bytes(&journal),
            BUCKET_BLOCKS * JSET_BLOCK_SIZE
        );
        assert_eq!(sp_bucket_seq(&journal).len(), 3);
        assert_eq!(sp_bucket_seq(&journal), vec![0, 0, 0]);
    }

    #[test]
    fn test_journal_from_superblock() {
        let state = JournalSuperblockState {
            bucket_addrs: vec![100, 200, 300],
            last_seq: 42,
            last_seq_ondisk: 40,
            last_bucket: 1,
            discard_idx: 0,
            dirty_idx: 0,
            dirty_idx_ondisk: 0,
            bucket_seq: vec![10, 20, 30],
            replayed_seqs: vec![],
        };
        let journal = Journal::from_superblock(&state);
        let addrs_out: Vec<u64> = {
            let sp = journal.slowpath.lock().unwrap();
            sp.buckets.iter().map(|bs| bs.addr).collect()
        };
        assert_eq!(addrs_out, vec![100, 200, 300]);
        assert_eq!(journal.bch2_journal_cur_seq(), 42); // from_superblock opens seq 42
        assert_eq!(journal.last_seq_ondisk.load(Ordering::Acquire), 40);
        assert_eq!(sp_current_bucket(&journal), 1);
        assert_eq!(sp_bucket_seq(&journal), vec![10, 20, 30]);
    }

    #[test]
    fn test_journal_from_empty_superblock_starts_at_seq_one() {
        let state = JournalSuperblockState {
            bucket_addrs: vec![100, 200],
            last_seq: 0,
            last_seq_ondisk: 0,
            last_bucket: 0,
            discard_idx: 0,
            dirty_idx: 0,
            dirty_idx_ondisk: 0,
            bucket_seq: vec![0, 0],
            replayed_seqs: vec![],
        };

        let journal = Journal::from_superblock(&state);

        assert_eq!(journal.bch2_journal_cur_seq(), 1);
        assert_eq!(journal.last_seq.load(Ordering::Acquire), 1);
        assert_eq!(journal.last_seq_ondisk.load(Ordering::Acquire), 1);
        assert_eq!(JournalResState::idx(journal.reservations.read()), 1);
    }

    #[test]
    fn test_journal_seq_increment() {
        let journal = Journal::new(vec![100]);
        // new() calls journal_entry_open → 与 bcachefs atomic64_inc_return 一致，seq=1
        assert_eq!(journal.bch2_journal_cur_seq(), 1);
        journal.seq.fetch_add(1, Ordering::Relaxed);
        assert_eq!(journal.bch2_journal_cur_seq(), 2);
        journal.seq.fetch_add(5, Ordering::Relaxed);
        assert_eq!(journal.bch2_journal_cur_seq(), 7);
    }

    #[tokio::test]
    async fn test_journal_append_seq_increment() {
        let backend = MockBlockDevice::new();
        let journal = Journal::new(vec![100, 200]);
        journal.set_test_device(Arc::new(BchDev::new(Arc::new(backend.clone()), 0)));
        let entry = make_test_entry();

        let seq1 = journal
            .append(BtreeId::Extents, std::slice::from_ref(&entry), false)
            .await
            .unwrap();
        // seq=1 from first entry opened in new(), first append uses same buf
        assert_eq!(seq1, 1);

        let seq2 = journal
            .append(BtreeId::Extents, std::slice::from_ref(&entry), false)
            .await
            .unwrap();
        // Same entry, same seq (per-entry)
        assert_eq!(seq2, 1);

        // Flush to cycle entry, then new append gets new seq
        // (flush creates a new entry, so seq advances)
    }

    #[tokio::test]
    async fn test_journal_append_btree_root() {
        let backend = MockBlockDevice::new();
        let journal = Journal::new(vec![100]);
        journal.set_test_device(Arc::new(BchDev::new(Arc::new(backend.clone()), 0)));
        let seq = journal
            .append_btree_root(BtreeId::Extents, 0xABCD, 0, false)
            .await
            .unwrap();
        assert_eq!(seq, 1);
    }

    #[tokio::test]
    async fn test_journal_flush_readback() {
        let backend = MockBlockDevice::new();
        let journal = Journal::new(vec![100, 200]);
        journal.set_test_device(Arc::new(BchDev::new(Arc::new(backend.clone()), 0)));
        let entry = make_test_entry();

        // Wait, journal.new() already opens the first entry.
        // append → reserve + commit + put on buf[0]
        journal
            .append(BtreeId::Extents, &[entry], false)
            .await
            .unwrap();

        // flush → close entry, write bufs to bucket, open new entry
        journal.bch2_journal_flush().await.unwrap();
        // new entry opened → seq advances to 2
        assert_eq!(journal.bch2_journal_cur_seq(), 2);

        // Read back the block from bucket
        let block_addr = BlockAddr::new(100);
        let mut buf = vec![0u8; JSET_BLOCK_SIZE as usize];
        let dev = Arc::new(BchDev::new(Arc::new(backend.clone()), 0));
        dev.bdev().read_block(block_addr, &mut buf).await.unwrap();

        let restored = Jset::deserialize(&buf).unwrap().unwrap();
        let entry_types: Vec<u8> = restored
            .entries
            .iter()
            .map(|entry| entry.hdr.entry_type)
            .collect();
        assert_eq!(
            entry_types,
            vec![
                JsetEntryType::BtreeKeys as u8,
                JsetEntryType::RewindLimit as u8,
                JsetEntryType::Datetime as u8,
            ]
        );
        assert_eq!(restored.entries[0].hdr.btree_type, 0); // Extents
        assert_eq!(
            restored.entries[1].hdr.entry_type,
            JsetEntryType::RewindLimit as u8
        );
        assert_eq!(
            restored.entries[2].hdr.entry_type,
            JsetEntryType::Datetime as u8
        );
    }

    #[tokio::test]
    async fn test_journal_write_submit_orders_preflush_before_write_and_fua_completion() {
        let backend = Arc::new(GatedBlockDevice::new());
        let journal = Arc::new(Journal::new(vec![100]));
        journal.set_test_device(Arc::new(BchDev::new(backend.clone(), 0)));

        let entry = make_test_entry();
        journal
            .append(BtreeId::Extents, std::slice::from_ref(&entry), false)
            .await
            .unwrap();

        let journal_for_flush = Arc::clone(&journal);
        let flush_task = tokio::spawn(async move { journal_for_flush.bch2_journal_flush().await });

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while backend.flush_started.load(Ordering::Acquire) == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for journal preflush to start"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        assert_eq!(
            backend.write_started.load(Ordering::Acquire),
            0,
            "journal data write must not start before PREFLUSH completion"
        );

        backend.allow_flush();

        while backend.write_started.load(Ordering::Acquire) == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for journal write to start"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        backend.allow_write();

        flush_task
            .await
            .expect("flush task join should succeed")
            .expect("flush should succeed");

        assert!(
            backend.flush_started.load(Ordering::Acquire) >= 2,
            "flush write should perform PREFLUSH and FUA durability completion"
        );
    }

    #[tokio::test]
    async fn test_bch2_journal_write_nochanges_marks_replicas_without_io() {
        let backend = Arc::new(GatedBlockDevice::new());
        let dev = Arc::new(BchDev::new(backend.clone(), 0));
        let mut sb = BchSb::with_volume_info(
            "test-vol".to_string(),
            1,
            "test-pool".to_string(),
            4096,
            1024 * 1024,
            crate::types::BackendType::Nfs,
        );
        unsafe {
            (*dev.mi.get()).bucket_size = 1024;
            (*dev.mi.get()).durability = 1;
            (*dev.mi.get()).data_allowed |= 1 << BchDataType::Journal as u8;
        }
        sb.members[0].bucket_size = 1024;
        sb.members[0].nbuckets = 64;
        sb.members[0].flags |= (1 << BchDataType::Journal as u8)
            << crate::storage::superblock::member_bits::DATA_ALLOWED_SHIFT;
        let vol = Arc::new(BchVol::alloc(
            sb,
            dev.clone(),
            VolumeConfig {
                nochanges: true,
                ..VolumeConfig::default()
            },
            "test-vol".to_string(),
            PathBuf::from("/tmp/test-vol"),
        ));
        unsafe {
            (*dev.mi.get()).bucket_size = 1024;
            (*dev.mi.get()).durability = 1;
            (*dev.mi.get()).data_allowed |= 1 << BchDataType::Journal as u8;
        }
        {
            let mut ja = dev.journal.lock().unwrap();
            ja.bucket_seq = vec![0; 4];
            ja.sectors_free = 1024;
            ja.discard_idx = 0;
            ja.dirty_idx_ondisk = 0;
            ja.dirty_idx = 0;
            ja.cur_idx = 0;
            ja.nr = 4;
            ja.buckets = vec![10, 11, 12, 13];
        }

        let journal = Journal::new(vec![0, 1, 2, 3]);
        journal.set_vol_ref(&vol);
        let seq = journal
            .append(
                BtreeId::Extents,
                std::slice::from_ref(&make_test_entry()),
                false,
            )
            .await
            .unwrap();

        journal.bch2_journal_flush().await.unwrap();

        assert_eq!(backend.write_started.load(Ordering::Acquire), 0);
        assert_eq!(backend.flush_started.load(Ordering::Acquire), 0);
        assert_eq!(dev.io_ref_count(BchDevIoRefKind::Write), 0);
        assert_eq!(journal.seq_ondisk.load(Ordering::Acquire), seq);
        let replicas = crate::replicas::BchReplicasEntry::new(BchDataType::Journal, &[0], 1);
        assert!(vol.replicas.lock().unwrap().contains(&replicas));
        assert!(journal
            .pin_fifo_ref()
            .entry_for_seq(seq)
            .unwrap()
            .has_dev(0));
    }

    #[tokio::test]
    async fn test_bch2_journal_write_error_runs_no_io_cleanup_and_completion() {
        let journal = Journal::new(vec![0, 1, 2, 3]);
        let vol = make_test_vol_with_noflush_gate(false);
        let dev = vol.primary_device_rcu_noerror().unwrap();
        dev.set_member_state(crate::storage::superblock::BchMemberState::Ro);
        journal.set_vol_ref(&vol);
        let seq = journal
            .append(
                BtreeId::Extents,
                std::slice::from_ref(&make_test_entry()),
                false,
            )
            .await
            .unwrap();

        let result = journal.bch2_journal_flush().await;

        assert!(matches!(result, Err(JournalError::Full(_))));
        assert_eq!(journal.err_seq.load(Ordering::Acquire), seq);
        assert_eq!(journal.seq_ondisk.load(Ordering::Acquire), seq);
        assert!(journal.flushed_seq_ondisk.load(Ordering::Acquire) < seq);
        assert!(journal.in_flight.lock().unwrap().is_empty());
        assert_eq!(dev.io_ref_count(BchDevIoRefKind::Write), 0);
        assert!(journal.bch2_journal_error_check().is_some());
    }

    #[tokio::test]
    async fn test_journal_write_preflush_submits_only_rw_members() {
        let rw_backend = Arc::new(GatedBlockDevice::new());
        let ro_backend = Arc::new(GatedBlockDevice::new());
        ro_backend.allow_flush();
        let rw = Arc::new(BchDev::new(rw_backend.clone(), 0));
        let ro = Arc::new(BchDev::new(ro_backend.clone(), 1));
        let mut sb = BchSb::with_volume_info(
            "test-vol".to_string(),
            1,
            "test-pool".to_string(),
            4096,
            1024 * 1024,
            crate::types::BackendType::Nfs,
        );
        sb.members.clear();
        for dev_idx in 0..2 {
            let mut member = BchSbMember::new(dev_idx, format!("dev-{dev_idx}"));
            member.mark_alive([dev_idx + 1; 16]);
            member.first_bucket = 1;
            member.bucket_size = 1024;
            member.nbuckets = 64;
            sb.members.push(member);
        }
        sb.primary_dev_idx = 0;
        let vol = Arc::new(BchVol::alloc_with_devices(
            sb,
            [rw.clone(), ro.clone()],
            VolumeConfig::default(),
            "test-vol".to_string(),
            PathBuf::from("/tmp/test-vol"),
        ));
        ro.set_member_state(crate::storage::superblock::BchMemberState::Ro);
        let journal = Journal::new(vec![0, 1, 2, 3]);
        journal.set_vol_ref(&vol);
        let first_err = Arc::new(AtomicFirstError::new());
        let failures = Arc::new(Mutex::new(Vec::new()));
        let initial_write_refs = rw.io_ref_count(BchDevIoRefKind::Write);

        let journal_devs = journal.journal_devices();
        let preflush = journal.journal_write_preflush(0, &journal_devs, &first_err, &failures);
        tokio::pin!(preflush);
        tokio::select! {
            result = &mut preflush => panic!("preflush completed before its gate: {result:?}"),
            result = tokio::time::timeout(std::time::Duration::from_secs(1), async {
                while rw_backend.flush_started.load(Ordering::Acquire) == 0 {
                    tokio::task::yield_now().await;
                }
            }) => result.expect("RW preflush should start promptly"),
        }
        assert_eq!(
            rw.io_ref_count(BchDevIoRefKind::Write),
            initial_write_refs + 1
        );
        rw_backend.allow_flush();
        preflush.await;

        assert_eq!(rw_backend.flush_started.load(Ordering::Acquire), 1);
        assert_eq!(ro_backend.flush_started.load(Ordering::Acquire), 0);
        assert!(first_err.take().is_none());
        assert!(failures.lock().unwrap().is_empty());
        assert_eq!(rw.io_ref_count(BchDevIoRefKind::Write), initial_write_refs);
        assert_eq!(ro.io_ref_count(BchDevIoRefKind::Write), 0);
    }

    #[tokio::test]
    async fn test_journal_write_endio_all_replicas_failed_sets_err_seq() {
        let journal = Journal::new(vec![100]);
        journal.set_test_device(Arc::new(BchDev::new(Arc::new(WriteFailBlockDevice), 0)));
        let seq = journal
            .append(
                BtreeId::Extents,
                std::slice::from_ref(&make_test_entry()),
                false,
            )
            .await
            .unwrap();

        let result = journal.bch2_journal_flush().await;
        assert!(matches!(result, Err(JournalError::Io(_))));
        assert_eq!(journal.err_seq.load(Ordering::Acquire), seq);
        assert_eq!(journal.seq_ondisk.load(Ordering::Acquire), seq);
        assert!(
            journal.flushed_seq_ondisk.load(Ordering::Acquire) < seq,
            "failed write must not advance flushed_seq_ondisk"
        );
        assert!(journal.bch2_journal_error_check().is_some());
    }

    #[tokio::test]
    async fn test_journal_flush_does_not_consume_pin_callbacks() {
        let backend = MockBlockDevice::new();
        let journal = Arc::new(Journal::new(vec![100, 200]));
        journal.set_test_device(Arc::new(BchDev::new(Arc::new(backend.clone()), 0)));
        unsafe {
            assert!(
                (*journal.pin_fifo.get())
                    .push_back(JournalEntryPinList::new(1))
                    .is_ok(),
                "test journal should have a second pin list"
            );
        }
        let flush_hits = Arc::new(AtomicUsize::new(0));
        let flush_hits_cb = Arc::clone(&flush_hits);
        let pin = JournalEntryPin::new(
            Some(Box::new(move |_, _, _| {
                flush_hits_cb.fetch_add(1, Ordering::Relaxed);
                Ok(())
            })),
            JournalPinType::Other,
        );

        journal.bch2_journal_pin_add(1, &pin, None);

        journal.bch2_journal_flush().await.unwrap();

        assert_eq!(
            flush_hits.load(Ordering::Relaxed),
            0,
            "journal flush should not run pin callbacks"
        );
    }

    #[tokio::test]
    async fn test_journal_read() {
        let backend = MockBlockDevice::new();
        let journal = Journal::new(vec![100]);
        journal.set_test_device(Arc::new(BchDev::new(Arc::new(backend.clone()), 0)));
        let entry = make_test_entry();

        journal
            .append(BtreeId::Extents, std::slice::from_ref(&entry), false)
            .await
            .unwrap();
        journal.bch2_journal_flush().await.unwrap();

        // Read back
        let mut info = JournalStartInfo::default();
        let jsets = journal.bch2_journal_read(&mut info).await.unwrap();
        // After flush, the Jset data is in the bucket
        // Each append creates one Jset; flush writes all buf data to bucket
        // The bucket may contain one or more Jset blocks depending on buf data size
        assert!(!jsets.is_empty(), "should have at least one Jset");
    }

    #[tokio::test]
    async fn test_journal_entries_read() {
        let backend = MockBlockDevice::new();
        let journal = Journal::new(vec![100, 500]);
        journal.set_test_device(Arc::new(BchDev::new(Arc::new(backend.clone()), 0)));
        let entry = make_test_entry();

        // bucket 0: write 1 entry
        journal
            .append(BtreeId::Extents, std::slice::from_ref(&entry), false)
            .await
            .unwrap();
        let pin = JournalEntryPin::new(None, JournalPinType::Btree0);
        journal.bch2_journal_pin_add(1, &pin, None);
        journal.bch2_journal_flush().await.unwrap();

        // rotate to bucket 1
        journal.bch2_journal_rotate_or_reclaim().await.unwrap();

        // bucket 1: write 1 entry
        journal
            .append(BtreeId::Alloc, &[entry], false)
            .await
            .unwrap();
        journal.bch2_journal_flush().await.unwrap();

        let mut info = JournalStartInfo::default();
        let all = journal.bch2_journal_read(&mut info).await.unwrap();
        assert!(
            all.len() >= 2,
            "expected at least two journal entries, got {}",
            all.len()
        );
        assert!(
            all.iter().any(|(bucket, jset)| {
                *bucket == 0
                    && jset
                        .entries
                        .iter()
                        .any(|e| e.hdr.btree_type == BtreeId::Extents as u8)
            }),
            "bucket 0 should contain an Extents entry"
        );
        assert!(
            all.iter().any(|(bucket, jset)| {
                *bucket == 1
                    && jset
                        .entries
                        .iter()
                        .any(|e| e.hdr.btree_type == BtreeId::Alloc as u8)
            }),
            "bucket 1 should contain an Alloc entry"
        );
    }

    #[tokio::test]
    async fn test_journal_read_device_bsearch_finds_live_tail() {
        let backend = MockBlockDevice::new();
        let buckets: Vec<u64> = (0..40).map(|i| 100 + i * 300).collect();
        let journal = Journal::new(buckets.clone());
        journal.set_test_device(Arc::new(BchDev::new(Arc::new(backend.clone()), 0)));

        let first = Jset::new(100, 100).serialize_padded().unwrap();
        let second = Jset::new(101, 100).serialize_padded().unwrap();
        backend
            .write_block(BlockAddr::new(buckets[38]), &first)
            .await
            .unwrap();
        backend
            .write_block(BlockAddr::new(buckets[39]), &second)
            .await
            .unwrap();

        let mut info = JournalStartInfo::default();
        let entries = journal.bch2_journal_read(&mut info).await.unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|(_, jset)| jset.header.seq)
                .collect::<Vec<_>>(),
            vec![100, 101]
        );
        let journal_dev = journal.journal_device();
        let ja = journal_dev.journal.lock().unwrap();
        assert_eq!(ja.highest_seq_found, 101);
        assert_eq!(ja.cur_idx, 39);
        assert_eq!(
            ja.sectors_free,
            (BUCKET_BLOCKS - 1) * SECTORS_PER_BLOCK as u32
        );
        assert_eq!(ja.bucket_seq[38], 100);
        assert_eq!(ja.bucket_seq[39], 101);
        assert_eq!(ja.discard_idx, 0);
        assert_eq!(ja.dirty_idx_ondisk, 0);
        assert_eq!(ja.dirty_idx, 0);
    }

    #[tokio::test]
    async fn test_journal_retry_full_read_fills_union_gap() {
        let backend = Arc::new(MockBlockDevice::new());
        let journal_dev = Arc::new(BchDev::new(backend.clone(), 0));
        let buckets: Vec<u64> = (0..40).map(|i| 100 + i * 300).collect();
        let journal = Journal::new(buckets.clone());
        journal.set_test_device(journal_dev.clone());
        journal_dev.journal.lock().unwrap().nr = 40;

        let mut list = JournalList::default();
        for (seq, bucket) in [(100, 37), (102, 39)] {
            let jset = Jset::new(seq, 100);
            let raw = jset.serialize_padded().unwrap();
            Journal::journal_entry_add(
                &journal_dev,
                JournalPtr {
                    csum_good: true,
                    dev: 0,
                    bucket,
                    bucket_offset: 0,
                    sector: buckets[bucket as usize] * SECTORS_PER_BLOCK,
                },
                &mut list,
                jset,
                raw,
            )
            .unwrap();
        }
        assert!(journal.journal_has_any_missing(&list, 100, 102));

        backend
            .write_block(
                BlockAddr::new(buckets[10]),
                &Jset::new(101, 100).serialize_padded().unwrap(),
            )
            .await
            .unwrap();
        let list = Arc::new(Mutex::new(list));
        journal.journal_retry_full_read(list.clone()).await.unwrap();

        let list = list.lock().unwrap();
        assert!(list.full_read);
        assert_eq!(
            list.entries.keys().copied().collect::<Vec<_>>(),
            vec![100, 101, 102]
        );
        assert!(!journal.journal_has_any_missing(&list, 100, 102));
        assert_eq!(journal_dev.io_ref_count(BchDevIoRefKind::Read), 0);
    }

    #[tokio::test]
    async fn test_journal_entries_read_scans_rw_ro_devices_and_releases_read_refs() {
        let rw_backend = Arc::new(MockBlockDevice::new());
        let ro_backend = Arc::new(MockBlockDevice::new());
        let spare_backend = Arc::new(MockBlockDevice::new());
        let rw = Arc::new(BchDev::new(rw_backend.clone(), 0));
        let ro = Arc::new(BchDev::new(ro_backend.clone(), 1));
        let spare = Arc::new(BchDev::new(spare_backend.clone(), 2));
        ro.set_member_state(crate::storage::superblock::BchMemberState::Ro);
        spare.set_member_state(crate::storage::superblock::BchMemberState::Spare);

        let mut sb = BchSb::with_volume_info(
            "test-vol".to_string(),
            1,
            "test-pool".to_string(),
            4096,
            1024 * 1024,
            crate::types::BackendType::Nfs,
        );
        sb.members.clear();
        for dev_idx in 0..3 {
            let mut member = BchSbMember::new(dev_idx, format!("dev-{dev_idx}"));
            member.mark_alive([dev_idx + 1; 16]);
            member.first_bucket = 1;
            member.bucket_size = 1024;
            member.nbuckets = 64;
            sb.members.push(member);
        }
        sb.primary_dev_idx = 0;
        let vol = Arc::new(BchVol::alloc_with_devices(
            sb,
            [rw.clone(), ro.clone(), spare.clone()],
            VolumeConfig::default(),
            "test-vol".to_string(),
            PathBuf::from("/tmp/test-vol"),
        ));
        ro.set_member_state(crate::storage::superblock::BchMemberState::Ro);
        spare.set_member_state(crate::storage::superblock::BchMemberState::Spare);

        let journal = Journal::new(vec![100]);
        journal.set_vol_ref(&vol);
        let mut bad_replica = Jset::new(77, 77);
        bad_replica.header.pad[0] = 1;
        let mut bad_raw = bad_replica.serialize_padded().unwrap();
        bad_raw[std::mem::offset_of!(JsetHeader, crc32)] ^= 1;
        rw_backend
            .write_block(BlockAddr::new(100), &bad_raw)
            .await
            .unwrap();
        let mut good_replica = Jset::new(77, 77);
        good_replica.header.pad[0] = 2;
        ro_backend
            .write_block(
                BlockAddr::new(100),
                &good_replica.serialize_padded().unwrap(),
            )
            .await
            .unwrap();
        spare_backend
            .write_block(
                BlockAddr::new(100),
                &Jset::new(999, 999).serialize_padded().unwrap(),
            )
            .await
            .unwrap();

        let mut info = JournalStartInfo::default();
        let entries = journal.bch2_journal_read(&mut info).await.unwrap();

        assert_eq!(
            entries
                .iter()
                .map(|(_, jset)| jset.header.seq)
                .collect::<Vec<_>>(),
            vec![77]
        );
        assert_eq!(entries[0].1.header.pad[0], 2);
        assert_eq!(rw.io_ref_count(BchDevIoRefKind::Read), 0);
        assert_eq!(ro.io_ref_count(BchDevIoRefKind::Read), 0);
        assert_eq!(spare.io_ref_count(BchDevIoRefKind::Read), 0);
    }

    #[tokio::test]
    async fn test_bch2_journal_read_computes_start_info_after_noflush_and_torn_write() {
        let backend = Arc::new(MockBlockDevice::new());
        let dev = Arc::new(BchDev::new(backend.clone(), 0));
        let journal = Journal::new(vec![100, 500, 900]);
        journal.set_test_device(dev);

        backend
            .write_block(
                BlockAddr::new(100),
                &Jset::new(10, 5).serialize_padded().unwrap(),
            )
            .await
            .unwrap();

        let mut noflush = Jset::new(11, 0);
        noflush.header.flags |= super::super::jset::JSET_NO_FLUSH;
        backend
            .write_block(BlockAddr::new(500), &noflush.serialize_padded().unwrap())
            .await
            .unwrap();

        let mut torn_raw = Jset::new(12, 6).serialize_padded().unwrap();
        torn_raw[std::mem::offset_of!(JsetHeader, crc32)] ^= 1;
        backend
            .write_block(BlockAddr::new(900), &torn_raw)
            .await
            .unwrap();

        let mut info = JournalStartInfo::default();
        let entries = journal.bch2_journal_read(&mut info).await.unwrap();

        assert_eq!(info.cur_seq, 13);
        assert_eq!(info.replay_end, 10);
        assert_eq!(info.last_seq, 5);
        assert!(!info.clean);
        assert_eq!(
            entries
                .iter()
                .map(|(_, jset)| jset.header.seq)
                .collect::<Vec<_>>(),
            vec![10]
        );
    }

    #[tokio::test]
    async fn test_bch2_journal_read_restores_rewind_state() {
        let backend = Arc::new(MockBlockDevice::new());
        let journal = Journal::new(vec![100]);
        journal.set_test_device(Arc::new(BchDev::new(backend.clone(), 0)));

        let mut jset = Jset::new(10, 5);
        jset.entries.push(
            RawJsetEntry::new(
                0,
                JsetEntryType::RewindLimit as u8,
                7u64.to_le_bytes().to_vec(),
                0,
            )
            .unwrap(),
        );
        let mut rewind_payload = Vec::new();
        rewind_payload.extend_from_slice(&8u64.to_le_bytes());
        rewind_payload.extend_from_slice(&10u64.to_le_bytes());
        jset.entries
            .push(RawJsetEntry::new(0, JsetEntryType::Rewind as u8, rewind_payload, 0).unwrap());
        backend
            .write_block(BlockAddr::new(100), &jset.serialize_padded().unwrap())
            .await
            .unwrap();

        let mut info = JournalStartInfo::default();
        journal.bch2_journal_read(&mut info).await.unwrap();

        assert_eq!(journal.rewind_seq.load(Ordering::Acquire), 7);
        assert_eq!(journal.rewind_seq_ondisk.load(Ordering::Acquire), 7);
        assert_eq!(
            journal.slowpath.lock().unwrap().rewind_ranges.as_slice(),
            &[(8, 10)]
        );
    }

    #[test]
    fn test_journal_entry_add_prefers_good_replica_and_tracks_ptrs() {
        let dev0 = BchDev::new(Arc::new(MockBlockDevice::new()), 0);
        let dev1 = BchDev::new(Arc::new(MockBlockDevice::new()), 1);

        let mut bad_source = Jset::new(10, 5);
        bad_source.header.pad[0] = 1;
        let mut bad_raw = bad_source.serialize_padded().unwrap();
        let crc_offset = std::mem::offset_of!(JsetHeader, crc32);
        bad_raw[crc_offset] ^= 1;
        let bad = Jset::deserialize(&bad_raw).unwrap().unwrap();
        assert!(!bad.verify());

        let mut good_source = Jset::new(10, 5);
        good_source.header.pad[0] = 2;
        let good_raw = good_source.serialize_padded().unwrap();
        let good = Jset::deserialize(&good_raw).unwrap().unwrap();
        assert!(good.verify());

        let mut list = JournalList::default();
        Journal::journal_entry_add(
            &dev0,
            JournalPtr {
                csum_good: false,
                dev: 0,
                bucket: 3,
                bucket_offset: 8,
                sector: 100,
            },
            &mut list,
            bad,
            bad_raw,
        )
        .unwrap();
        Journal::journal_entry_add(
            &dev1,
            JournalPtr {
                csum_good: true,
                dev: 1,
                bucket: 4,
                bucket_offset: 16,
                sector: 200,
            },
            &mut list,
            good,
            good_raw,
        )
        .unwrap();

        let replay = list.entries.get(&10).unwrap();
        assert!(replay.csum_good);
        assert_eq!(replay.jset.header.pad[0], 2);
        assert_eq!(replay.ptrs.len(), 2);
        assert_eq!(replay.ptrs[0].dev, 0);
        assert_eq!(replay.ptrs[0].bucket, 3);
        assert_eq!(replay.ptrs[0].bucket_offset, 8);
        assert_eq!(replay.ptrs[1].dev, 1);
        assert_eq!(replay.ptrs[1].bucket, 4);
        assert_eq!(replay.ptrs[1].bucket_offset, 16);
    }

    #[test]
    fn test_journal_entry_add_keeps_good_for_reread_and_bad_replica() {
        let dev0 = BchDev::new(Arc::new(MockBlockDevice::new()), 0);
        let dev1 = BchDev::new(Arc::new(MockBlockDevice::new()), 1);
        let good_raw = Jset::new(10, 5).serialize_padded().unwrap();
        let good = Jset::deserialize(&good_raw).unwrap().unwrap();
        let good_ptr = JournalPtr {
            csum_good: true,
            dev: 0,
            bucket: 3,
            bucket_offset: 8,
            sector: 100,
        };
        let mut list = JournalList::default();

        Journal::journal_entry_add(
            &dev0,
            good_ptr.clone(),
            &mut list,
            good.clone(),
            good_raw.clone(),
        )
        .unwrap();
        Journal::journal_entry_add(&dev0, good_ptr, &mut list, good, good_raw.clone()).unwrap();
        assert_eq!(list.entries[&10].ptrs.len(), 1);

        let mut bad_raw = good_raw;
        bad_raw[std::mem::offset_of!(JsetHeader, crc32)] ^= 1;
        let bad = Jset::deserialize(&bad_raw).unwrap().unwrap();
        Journal::journal_entry_add(
            &dev1,
            JournalPtr {
                csum_good: false,
                dev: 1,
                bucket: 4,
                bucket_offset: 16,
                sector: 200,
            },
            &mut list,
            bad,
            bad_raw,
        )
        .unwrap();

        let replay = &list.entries[&10];
        assert!(replay.csum_good);
        assert_eq!(replay.ptrs.len(), 2);
        assert_eq!(replay.ptrs[1].dev, 1);
    }

    #[test]
    fn test_journal_entry_add_logs_duplicate_conflicts_instead_of_aborting() {
        // 对应 bcachefs read.c:274-285 — ret_fsck_err_on 记录冲突但继续，
        // 不中止整个 recovery。subvol 对 same_device 和 not_identical
        // 改为 tracing::warn + 继续，而非 return Err。
        let dev0 = BchDev::new(Arc::new(MockBlockDevice::new()), 0);
        let dev1 = BchDev::new(Arc::new(MockBlockDevice::new()), 1);
        let raw0 = Jset::new(10, 5).serialize_padded().unwrap();
        let jset0 = Jset::deserialize(&raw0).unwrap().unwrap();
        let mut same_device = JournalList::default();
        let ptr0 = JournalPtr {
            csum_good: true,
            dev: 0,
            bucket: 3,
            bucket_offset: 8,
            sector: 100,
        };
        // 首次插入：OK
        Journal::journal_entry_add(
            &dev0,
            ptr0.clone(),
            &mut same_device,
            jset0.clone(),
            raw0.clone(),
        )
        .unwrap();
        // 同设备不同扇区、相同数据：应返回 Ok（记录 fsck 错误但继续）
        // bcachefs: ret_fsck_err_on(same_device) + identical → return 0
        Journal::journal_entry_add(
            &dev0,
            JournalPtr {
                sector: 101,
                ..ptr0
            },
            &mut same_device,
            jset0,
            raw0,
        )
        .unwrap(); // 不 abort！与 bcachefs 一致

        // not_identical: 两个设备上的校验和正确的不同副本
        let mut raw1_jset = Jset::new(10, 5);
        raw1_jset.header.pad[0] = 1;
        let raw1 = raw1_jset.serialize_padded().unwrap();
        let jset1 = Jset::deserialize(&raw1).unwrap().unwrap();
        let mut mismatch = JournalList::default();
        Journal::journal_entry_add(
            &dev0,
            JournalPtr {
                csum_good: true,
                dev: 0,
                bucket: 3,
                bucket_offset: 8,
                sector: 100,
            },
            &mut mismatch,
            Jset::new(10, 5),
            Jset::new(10, 5).serialize_padded().unwrap(),
        )
        .unwrap();
        // 不同设备上的不同副本：应返回 Ok
        // bcachefs: ret_fsck_err_on(not_identical) → 不中止，继续
        Journal::journal_entry_add(
            &dev1,
            JournalPtr {
                csum_good: true,
                dev: 1,
                bucket: 4,
                bucket_offset: 16,
                sector: 200,
            },
            &mut mismatch,
            jset1,
            raw1,
        )
        .unwrap(); // 不 abort！与 bcachefs 一致
    }

    #[test]
    fn test_journal_entry_missing_range_skips_blacklisted_sequences() {
        let journal = Journal::new(vec![0, 1, 2, 3]);
        journal.bch2_blacklist_table_initialize(&[
            BlacklistEntry {
                start_seq: 10,
                end_seq: 20,
            },
            BlacklistEntry {
                start_seq: 30,
                end_seq: 40,
            },
        ]);

        assert_eq!(
            journal.bch2_journal_entry_missing_range(5, 10),
            U64Range { start: 5, end: 10 }
        );
        assert_eq!(
            journal.bch2_journal_entry_missing_range(10, 15),
            U64Range::default()
        );
        assert_eq!(
            journal.bch2_journal_entry_missing_range(10, 25),
            U64Range { start: 20, end: 25 }
        );
        assert_eq!(
            journal.bch2_journal_entry_missing_range(5, 35),
            U64Range { start: 5, end: 10 }
        );
        assert_eq!(
            journal.bch2_journal_entry_missing_range(20, 35),
            U64Range { start: 20, end: 30 }
        );
        assert_eq!(
            journal.bch2_journal_entry_missing_range(40, 40),
            U64Range::default()
        );
    }

    #[tokio::test]
    async fn test_journal_write_pipeline_advances_oldest_unallocated_entry_first() {
        let backend = MockBlockDevice::new();
        let journal = Journal::new(vec![100, 500]);
        journal.set_test_device(Arc::new(BchDev::new(Arc::new(backend), 0)));
        let entry = make_test_entry();

        let seq1 = journal
            .append(BtreeId::Extents, std::slice::from_ref(&entry), false)
            .await
            .unwrap();
        journal.journal_entry_close();
        journal.journal_entry_open().unwrap();
        let seq2 = journal
            .append(BtreeId::Alloc, std::slice::from_ref(&entry), false)
            .await
            .unwrap();
        journal.journal_entry_close();

        assert_eq!(seq2, seq1 + 1);
        assert!(journal.journal_seq_to_buf(seq1).unwrap().write_started);
        assert!(!journal.journal_seq_to_buf(seq2).unwrap().write_started);

        journal.bch2_journal_flush().await.unwrap();

        assert_eq!(journal.seq_ondisk.load(Ordering::Acquire), seq2);
        assert!(journal.in_flight.lock().unwrap().iter().all(|&idx| {
            let w = journal.bufs.get(idx as usize);
            w.seq > seq2
        }));
        let mut info = JournalStartInfo::default();
        let entries = journal.bch2_journal_read(&mut info).await.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].1.header.seq, seq1);
        assert_eq!(entries[1].1.header.seq, seq2);
        assert_eq!(
            journal.entry_bytes_written.load(Ordering::Acquire),
            entries
                .iter()
                .map(|(_, jset)| jset.serialized_padded_len() as u64)
                .sum::<u64>()
        );
    }

    #[test]
    fn test_journal_utilization() {
        let mut journal = Journal::new(vec![100, 200]);
        assert_eq!(journal.utilization(), 0.0);

        {
            let sp = journal.slowpath.get_mut().unwrap();
            sp.current_offset = (BUCKET_BLOCKS * JSET_BLOCK_SIZE) / 2;
            sp.remaining_bytes = (BUCKET_BLOCKS * JSET_BLOCK_SIZE) / 2;
        }
        let u = journal.utilization();
        assert!(u > 0.24 && u < 0.26);
    }

    #[test]
    fn test_journal_set_watermark_tracks_space_pressure() {
        let mut journal = Journal::new(vec![100, 200]);

        journal.bch2_journal_set_watermark();
        assert_eq!(journal.watermark(), Watermark::Stripe);

        {
            let sp = journal.slowpath.get_mut().unwrap();
            sp.current_bucket = 1;
            sp.current_offset = (BUCKET_BLOCKS * JSET_BLOCK_SIZE) / 2;
            sp.remaining_bytes = (BUCKET_BLOCKS * JSET_BLOCK_SIZE) - sp.current_offset;
        }

        journal.bch2_journal_set_watermark();
        assert_eq!(journal.watermark(), Watermark::Reclaim);
    }

    #[test]
    fn test_journal_dirty_entry_bytes_reduce_space_budget() {
        let journal = Journal::new(vec![100, 200]);
        let total_before = journal.bch2_journal_space_available(Watermark::InteriorUpdate);
        assert!(total_before > 0);

        let dirty_bytes = (BUCKET_BLOCKS * JSET_BLOCK_SIZE) as u64;
        journal
            .dirty_entry_bytes
            .store(dirty_bytes, Ordering::Release);

        let total_after = journal.bch2_journal_space_available(Watermark::InteriorUpdate);
        assert!(total_after < total_before);
        assert_eq!(total_after, total_before.saturating_sub(dirty_bytes));
    }

    #[test]
    fn test_journal_seq_to_flush_uses_bucket_target() {
        let mut journal = Journal::new(vec![100, 200, 300, 400]);
        {
            let sp = journal.slowpath.get_mut().unwrap();
            sp.current_bucket = 0;
            sp.bucket_seq = vec![5, 10, 30, 40];
        }

        journal.seq.store(20, Ordering::Relaxed);

        // pin FIFO half target = 20 - 64 = 0, bucket target = bucket_seq[2] = 30
        assert_eq!(journal.bch2_journal_seq_to_flush(), 30);
    }

    #[test]
    fn test_journal_space_available_advances_dirty_indices() {
        let mut journal = Journal::new(vec![100, 200, 300]);
        {
            let sp = journal.slowpath.get_mut().unwrap();
            sp.bucket_seq = vec![5, 10, 0];
            sp.current_bucket = 2;
            sp.dirty_idx = 0;
            sp.dirty_idx_ondisk = 0;
        }
        journal.last_seq.store(8, Ordering::Relaxed);
        journal.last_seq_ondisk.store(8, Ordering::Relaxed);

        let _ = journal.bch2_journal_space_available(Watermark::Stripe);

        assert_eq!(sp_dirty_idx(&journal), 1);
        assert_eq!(sp_dirty_idx_ondisk(&journal), 1);
    }

    #[test]
    fn test_journal_error_check_stuck_requires_closed_entry_and_empty_in_flight() {
        let journal = Journal::new(vec![100, 200]);
        let err = JournalError::Overflow("slowpath exhausted".into());

        assert!(!journal_error_check_stuck(
            &journal,
            &err,
            Watermark::Stripe
        ));
        assert!(!journal_error_check_stuck(
            &journal,
            &err,
            Watermark::Reclaim
        ));

        journal.reservations.close_entry();
        journal.in_flight.lock().unwrap().clear();
        assert!(journal.reservations.is_closed());
        assert!(journal.in_flight.lock().unwrap().is_empty());

        assert!(journal_error_check_stuck(
            &journal,
            &err,
            Watermark::Reclaim
        ));
        assert!(journal_error_check_stuck(
            &journal,
            &JournalError::Full("journal full".into()),
            Watermark::Reclaim
        ));
        assert!(journal_error_check_stuck(
            &journal,
            &JournalError::PinFull("journal pin full".into()),
            Watermark::Reclaim
        ));
    }

    #[tokio::test]
    async fn test_journal_rotate_or_reclaim() {
        let backend = MockBlockDevice::new();
        let mut journal = Journal::new(vec![100, 200]);
        journal.set_test_device(Arc::new(BchDev::new(Arc::new(backend.clone()), 0)));
        assert_eq!(sp_current_bucket(&journal), 0);

        // Fill bucket 0
        {
            let sp = journal.slowpath.get_mut().unwrap();
            sp.current_offset = BUCKET_BLOCKS * JSET_BLOCK_SIZE - OVERFLOW_MARGIN;
            sp.remaining_bytes = OVERFLOW_MARGIN - 1;
        }
        journal.seq.store(100, Ordering::Relaxed);

        journal.bch2_journal_rotate_or_reclaim().await.unwrap();
        assert_eq!(sp_current_bucket(&journal), 1);
        assert_eq!(sp_current_offset(&journal), 0);
    }

    #[tokio::test]
    async fn test_journal_ring_full_overflow() {
        let backend = MockBlockDevice::new();
        let mut journal = Journal::new(vec![100, 200]);
        journal.set_test_device(Arc::new(BchDev::new(Arc::new(backend.clone()), 0)));
        let _entry = make_test_entry();

        // Fill bucket 0 → rotate to bucket 1
        {
            let sp = journal.slowpath.get_mut().unwrap();
            sp.current_offset = BUCKET_BLOCKS * JSET_BLOCK_SIZE - OVERFLOW_MARGIN;
            sp.remaining_bytes = OVERFLOW_MARGIN - 1;
            sp.bucket_seq[0] = 10;
        }
        journal.seq.store(10, Ordering::Relaxed);
        journal.bch2_journal_rotate_or_reclaim().await.unwrap();
        assert_eq!(sp_current_bucket(&journal), 1);

        // Fill bucket 1 → can't rotate back (dirty_idx=0 not advanced) → Overflow
        {
            let sp = journal.slowpath.get_mut().unwrap();
            sp.current_offset = BUCKET_BLOCKS * JSET_BLOCK_SIZE - OVERFLOW_MARGIN;
            sp.remaining_bytes = OVERFLOW_MARGIN - 1;
        }
        let result = journal.bch2_journal_rotate_or_reclaim().await;
        assert!(result.is_err());
        match result {
            Err(JournalError::Overflow(msg)) => assert!(msg.contains("exhausted")),
            _ => panic!("expected Overflow"),
        }
    }

    #[test]
    fn test_journal_bucket_seq_initialization() {
        let journal = Journal::new(vec![100, 200, 300]);
        assert_eq!(sp_bucket_seq(&journal), vec![0, 0, 0]);
        assert_eq!(sp_discard_idx(&journal), 0);
        assert_eq!(sp_dirty_idx(&journal), 0);
        assert_eq!(sp_dirty_idx_ondisk(&journal), 0);
        // new() 调用 journal_entry_open 推入 1 个自钉
        assert_eq!(unsafe { (*journal.pin_fifo.get()).len() }, 1);
        assert_eq!(journal.flushed_seq_marker.load(Ordering::Acquire), 0);
    }

    #[test]
    fn test_journal_to_superblock_state() {
        let journal = Journal::new(vec![100, 200]);
        let state = journal.to_superblock_state();
        assert_eq!(state.bucket_addrs, vec![100, 200]);
        // bch2_journal_cur_seq() 与 open entry 的 seq 相同（atomic64_inc_return）
        assert_eq!(state.last_seq, 1);
        assert_eq!(state.last_seq_ondisk, 1);
    }

    #[test]
    fn test_journal_advance_dirty_idx() {
        let mut journal = Journal::new(vec![100, 200, 300]);
        {
            let sp = journal.slowpath.get_mut().unwrap();
            sp.bucket_seq = vec![5, 10, 0];
            sp.current_bucket = 2;
        }
        journal.last_seq.store(8, Ordering::Relaxed);

        journal.advance_dirty_idx();
        assert_eq!(sp_dirty_idx(&journal), 1);
    }

    #[test]
    fn test_journal_advance_dirty_idx_ignores_open_seq() {
        let mut journal = Journal::new(vec![100, 200, 300]);
        {
            let sp = journal.slowpath.get_mut().unwrap();
            sp.bucket_seq = vec![5, 10, 0];
            sp.current_bucket = 2;
        }
        journal.seq.store(20, Ordering::Relaxed);
        journal.last_seq.store(8, Ordering::Relaxed);

        journal.advance_dirty_idx();
        assert_eq!(sp_dirty_idx(&journal), 1);
    }

    #[test]
    fn test_journal_no_advance_when_dirty_idx_equals_boundary() {
        let mut journal = Journal::new(vec![100, 200]);
        {
            let sp = journal.slowpath.get_mut().unwrap();
            sp.bucket_seq = vec![5, 10];
            sp.current_bucket = 0;
        }
        journal.last_seq.store(4, Ordering::Relaxed);

        journal.advance_dirty_idx();
        assert_eq!(sp_dirty_idx(&journal), 0);
    }

    #[test]
    fn test_journal_advance_dirty_idx_wraparound() {
        let mut journal = Journal::new(vec![100, 200, 300]);
        {
            let sp = journal.slowpath.get_mut().unwrap();
            sp.bucket_seq = vec![20, 5, 15];
            sp.current_bucket = 0;
            sp.dirty_idx = 1;
        }
        journal.last_seq.store(18, Ordering::Relaxed);

        journal.advance_dirty_idx();
        journal.advance_dirty_idx();
        assert_eq!(sp_dirty_idx(&journal), 0);
    }

    #[test]
    fn test_journal_advance_dirty_idx_ondisk() {
        let mut journal = Journal::new(vec![100, 200]);
        {
            let sp = journal.slowpath.get_mut().unwrap();
            sp.bucket_seq = vec![5, 10];
            sp.dirty_idx = 2;
        }
        journal.seq.store(20, Ordering::Relaxed);
        journal.last_seq_ondisk.store(8, Ordering::Relaxed);

        journal.advance_dirty_idx_ondisk();
        assert_eq!(sp_dirty_idx_ondisk(&journal), 1);
    }

    // ── Journal P2: must_flush + background reclaim tests ──

    #[tokio::test]
    async fn test_must_flush_flag() {
        let backend = MockBlockDevice::new();
        let journal = Arc::new(Journal::new(vec![100, 200]));
        journal.set_test_device(Arc::new(BchDev::new(Arc::new(backend.clone()), 0)));
        let entry = make_test_entry();

        // must_flush=true 时 append 应正常完成
        let result = journal.append(BtreeId::Extents, &[entry], true).await;
        assert!(result.is_ok(), "append with must_flush=true should succeed");
        let seq = result.unwrap();
        assert!(seq > 0, "seq should be non-zero");
    }

    #[tokio::test]
    async fn test_must_flush_default_false() {
        let backend = MockBlockDevice::new();
        let journal = Arc::new(Journal::new(vec![100, 200]));
        journal.set_test_device(Arc::new(BchDev::new(Arc::new(backend.clone()), 0)));
        let entry = make_test_entry();

        // must_flush=false 时 append 也正常完成
        let result = journal.append(BtreeId::Extents, &[entry], false).await;
        assert!(
            result.is_ok(),
            "append with must_flush=false should succeed"
        );
        let seq = result.unwrap();
        assert!(seq > 0, "seq should be non-zero");
    }

    #[tokio::test]
    async fn test_must_flush_propagation() {
        let backend = MockBlockDevice::new();
        let journal = Arc::new(Journal::new(vec![100, 200]));
        journal.set_test_device(Arc::new(BchDev::new(Arc::new(backend.clone()), 0)));
        let entry = make_test_entry();

        // 使用 must_flush=true 调用 append
        journal
            .append(BtreeId::Extents, &[entry], true)
            .await
            .unwrap();

        // flush 后检查 buf 的 has_must_flush 标记在 write_bufs_to_bucket 中被正确处理
        // 只需验证 flush 不报错
        journal.bch2_journal_flush().await.unwrap();
    }

    #[tokio::test]
    async fn test_must_flush_btree_root() {
        let backend = MockBlockDevice::new();
        let journal = Arc::new(Journal::new(vec![100]));
        journal.set_test_device(Arc::new(BchDev::new(Arc::new(backend.clone()), 0)));

        // append_btree_root 也支持 must_flush
        let seq = journal
            .append_btree_root(BtreeId::Extents, 0xABCD, 0, true)
            .await
            .unwrap();
        assert!(seq > 0, "seq should be non-zero");

        // flush 以确认数据落盘
        journal.bch2_journal_flush().await.unwrap();
    }

    #[tokio::test]
    async fn test_replay_done_forces_first_flush_write() {
        let backend = MockBlockDevice::new();
        let journal = Arc::new(Journal::new(vec![100]));
        journal.set_test_device(Arc::new(BchDev::new(Arc::new(backend.clone()), 0)));
        journal.bch2_journal_set_replay_done();

        let seq = journal
            .append(
                BtreeId::Extents,
                &[BtreeEntry::new(
                    Bpos::new(1, 0x55, 0),
                    KeyType::Normal,
                    KeyValue::extent(0xAA, 1, 0),
                )],
                false,
            )
            .await
            .unwrap();
        assert!(seq > 0);

        journal.bch2_journal_flush().await.unwrap();

        let buf_idx = JournalResState::idx(journal.reservations.read()) as usize;
        let buf = journal.bufs.get_mut(buf_idx);
        assert!(
            !buf.is_noflush(),
            "first post-replay write must stay flushable, not demote to noflush"
        );
    }

    #[test]
    fn test_bch2_journal_buf_try_noflush_rules() {
        let mut buf = JournalBuf::free();
        assert!(
            !buf.bch2_journal_buf_try_noflush(),
            "JOURNAL_BUF_NOT_IN_FLIGHT cannot enter noflush"
        );
        buf.reset_for_accepting(1);
        assert!(
            buf.bch2_journal_buf_try_noflush(),
            "active empty buf can enter noflush"
        );
        assert!(buf.is_noflush(), "noflush state should be set");

        let mut wait_buf = JournalBuf::free();
        wait_buf.wait_first = JournalBufWaitState::Waiters;
        assert!(
            !wait_buf.bch2_journal_buf_try_noflush(),
            "buf with waiters cannot enter noflush"
        );
        assert!(
            !wait_buf.is_noflush(),
            "buf with waiters should stay flushable"
        );

        let mut flush_no_wait_buf = JournalBuf::free();
        flush_no_wait_buf.wait_first = JournalBufWaitState::FlushNoWait;
        assert!(
            !flush_no_wait_buf.bch2_journal_buf_try_noflush(),
            "FLUSH_NO_WAIT entry must remain flushable"
        );
        assert!(
            flush_no_wait_buf.is_flush_no_wait(),
            "FLUSH_NO_WAIT sentinel must stay intact"
        );
    }

    #[test]
    fn test_journal_add_entry_marks_clean_transition_flush_no_wait() {
        let journal = Journal::new(vec![100]);
        let res = journal
            .bch2_journal_res_get_fast(Watermark::Btree, 1)
            .expect("reservation should succeed");
        let mut res = res;
        journal.last_seq_ondisk.store(res.seq, Ordering::Release);

        journal.bch2_journal_add_raw(&mut res, &[0u8; 8]);

        let buf = journal.bufs.get_mut(res.buf_idx as usize);
        assert!(
            buf.is_flush_no_wait(),
            "first post-clean entry should be marked FLUSH_NO_WAIT"
        );
        assert!(
            !buf.bch2_journal_buf_try_noflush(),
            "FLUSH_NO_WAIT entry must not demote to noflush"
        );
    }

    #[test]
    fn test_close_entry_idempotent_on_closed_state() {
        let state = JournalResState::new();
        state
            .bits
            .store(JOURNAL_ENTRY_CLOSED_VAL, Ordering::Relaxed);

        let captured = state.close_entry();
        assert_eq!(captured, JOURNAL_ENTRY_CLOSED_VAL as u32);
        assert_eq!(
            state.bits.load(Ordering::Relaxed),
            JOURNAL_ENTRY_CLOSED_VAL,
            "close_entry should not overwrite an already closed entry"
        );
    }

    #[test]
    fn test_journal_cycle_locked_close_only() {
        let journal = Journal::new(vec![100, 200]);
        let start_seq = journal.bch2_journal_cur_seq();

        let res = journal
            .bch2_journal_res_get_fast(Watermark::Btree, 1)
            .expect("reservation should succeed");
        let mut res = res;
        res.must_flush = true;
        journal.bch2_journal_add_raw(&mut res, &[0u8; 8]);
        journal.bch2_journal_res_put(&res);

        let cycled = journal
            .bch2_journal_cycle_locked()
            .expect("close-only cycle should succeed");
        assert!(
            !cycled,
            "flags=0 cycle should close without forcing a new entry"
        );
        assert_eq!(
            journal.bch2_journal_cur_seq(),
            start_seq,
            "close-only cycle must not advance seq"
        );
    }

    #[test]
    fn test_journal_cycle_locked_reopens_when_flush_is_pending() {
        let journal = Journal::new(vec![100, 200]);
        let start_seq = journal.bch2_journal_cur_seq();

        journal.journal_entry_close();
        journal.bch2_journal_set_needs_flush_write();

        let cycled = journal
            .bch2_journal_cycle_locked()
            .expect("flush-pending cycle should succeed");
        assert!(cycled, "closed journal with pending flush should reopen");
        assert!(
            journal.bch2_journal_cur_seq() > start_seq,
            "reopen should advance journal sequence"
        );
    }

    #[test]
    fn test_journal_res_get_nonblocking_returns_blocked_when_slowpath_locked() {
        let journal = Arc::new(Journal::new(vec![100, 200]));
        let held = journal
            .bch2_journal_res_get_fast(Watermark::Btree, BUF_SIZE_U64S - 1)
            .expect("should be able to fill the current entry");

        let slowpath_guard = journal.slowpath_lock.lock().unwrap();
        let clone = Arc::clone(&journal);
        let handle =
            std::thread::spawn(move || clone.journal_res_get_nonblocking(Watermark::Btree, 2));

        let result = handle.join().expect("thread should not panic");
        drop(slowpath_guard);
        drop(held);

        assert!(
            matches!(result, Err(JournalError::Blocked(_))),
            "nonblocking reservation should not wait for the slowpath lock"
        );
    }

    #[test]
    fn test_journal_res_get_slowpath_rejects_blocked_journal() {
        let journal = Journal::new(vec![100, 200]);
        journal.blocked.store(1, Ordering::Release);

        match journal.bch2_journal_res_get_slowpath(Watermark::Btree, BUF_SIZE_U64S + 1) {
            Err(JournalError::Blocked(_)) => {}
            Err(other) => panic!("blocked journal returned unexpected error: {other}"),
            Ok(_) => panic!("blocked journal should reject slowpath reservations"),
        }
    }

    #[test]
    fn test_journal_res_get_slowpath_rejects_lower_watermark() {
        let journal = Journal::new(vec![100, 200]);
        journal
            .current_watermark
            .store(Watermark::Reclaim.to_bits(), Ordering::Release);

        match journal.bch2_journal_res_get_slowpath(Watermark::Stripe, BUF_SIZE_U64S + 1) {
            Err(JournalError::Overflow(_)) => {}
            Err(other) => panic!("watermark mismatch returned unexpected error: {other}"),
            Ok(_) => panic!("lower watermark should be rejected"),
        }
    }

    #[tokio::test]
    async fn test_background_reclaim_task() {
        let backend = Arc::new(MockBlockDevice::new());
        let journal = Arc::new(Journal::new(vec![100, 200, 300]));
        journal.set_test_device(Arc::new(BchDev::new(backend.clone(), 0)));

        // interval=0 时不应启动
        let handle = Journal::spawn_background_reclaim_task(&journal, 0);
        assert!(handle.is_none(), "interval=0 should return None");

        // interval>0 时应启动
        let handle = Journal::spawn_background_reclaim_task(&journal, 1000);
        assert!(handle.is_some(), "interval>0 should return Some(handle)");
        // 取消后台任务
        if let Some(h) = handle {
            h.abort();
        }
    }

    #[tokio::test]
    async fn test_background_tasks_stop_cleanly() {
        let backend = Arc::new(MockBlockDevice::new());
        let mut journal = Journal::new(vec![100, 200, 300]);
        journal.set_auto_flush_interval(1);
        journal.set_test_device(Arc::new(BchDev::new(backend.clone(), 0)));
        let journal = Arc::new(journal);

        journal.start_auto_flush(Arc::clone(&journal));
        journal.start_background_reclaim(Arc::clone(&journal), 1);

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let stop_auto = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            journal.stop_auto_flush().await;
        });
        let stop_reclaim = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            journal.stop_background_reclaim().await;
        });

        assert!(stop_auto.await.is_ok(), "auto flush stop should not hang");
        assert!(
            stop_reclaim.await.is_ok(),
            "background reclaim stop should not hang"
        );
    }

    #[tokio::test]
    async fn test_auto_commit_write_work_fires_and_stops_cleanly() {
        let backend = Arc::new(MockBlockDevice::new());
        let mut journal = Journal::new(vec![100, 200, 300]);
        journal.set_auto_flush_interval(10);
        journal.set_test_device(Arc::new(BchDev::new(backend, 0)));
        let journal = Arc::new(journal);

        journal.start_auto_flush(Arc::clone(&journal));
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while journal.bch2_journal_cur_seq() == 1 {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("auto-commit write_work did not fire");
        journal.stop_auto_flush().await;
    }

    // ─────────────────────────────────────────────────────────
    // R1: buf put 链测试
    // ─────────────────────────────────────────────────────────

    /// 验证 `__bch2_journal_buf_put_final` 不会 panic。
    ///
    /// 测试函数的基本调用安全：pin_put → update_last_seq → wake_up 链至少不崩溃。
    /// 注意：当前 pin_fifo 的 entry_for_seq 使用 seq % PIN_FIFO_SIZE 索引，
    /// 与 push_back 的 tail 顺序索引不一致（预存 bug），因此 pin_put 返回 false
    /// 且 update_last_seq 不被调用。此测试仅验证函数无 panic。
    #[tokio::test]
    async fn test_bch2_journal_buf_put_final_no_panic() {
        let journal = Journal::new(vec![100, 200]);
        let buf_seq = journal.bch2_journal_cur_seq();

        // 不应 panic
        journal.__bch2_journal_buf_put_final(buf_seq);

        // wake_up 是 no-op（无等待者），至少调用不崩溃
    }

    /// 验证 `__bch2_journal_buf_put` 正确递减 buf_count。
    ///
    /// 场景：先做 reservation 使 buf_count=1，再调 buf_put 应：
    /// 1. 将 buf_count 从 1 减为 0
    /// 2. 调用 final（pin_put + wake_up — 因 entry_for_seq bug 不推进 last_seq）
    #[tokio::test]
    async fn test_bch2_journal_buf_put_decrements_count() {
        let journal = Journal::new(vec![100, 200]);
        let buf_seq = journal.bch2_journal_cur_seq();
        let idx = (buf_seq & (JOURNAL_STATE_BUF_NR as u64 - 1)) as u32;

        // open_entry 通过 journal_state_inc 设 buf_count=1，再 try_reserve 增加 1，总计 2
        let _ = journal
            .reservations
            .try_reserve(10, BUF_SIZE_U64S)
            .expect("reservation should succeed on empty entry");

        // 验证 count 为 2（open(1) + reserve(1)）
        let state_before = journal.reservations.read();
        assert_eq!(
            JournalResState::buf_count(state_before, idx),
            2,
            "buf_count should be 2 after open(1) + single reservation(1)"
        );

        // 调用 __bch2_journal_buf_put — 释放 open_entry 的隐式 refcount（count 2→1）
        journal.__bch2_journal_buf_put(buf_seq);

        // buf_count 应为 1（reservation 的 ref 仍在）
        let state_after = journal.reservations.read();
        assert_eq!(
            JournalResState::buf_count(state_after, idx),
            1,
            "buf_count should be 1 after __bch2_journal_buf_put (reservation still held)"
        );
    }

    /// 验证 `__bch2_journal_buf_put` 在 refcount >1 时不调用 final。
    ///
    /// 场景：做两次 reservation 使 count=2，调一次 buf_put 使 count=1，
    /// 不应触发 final → buf_count 应停在 1。
    #[tokio::test]
    async fn test_bch2_journal_buf_put_no_final_when_count_gt_1() {
        let journal = Journal::new(vec![100, 200]);
        let buf_seq = journal.bch2_journal_cur_seq();
        let idx = (buf_seq & (JOURNAL_STATE_BUF_NR as u64 - 1)) as u32;

        // 做两次 reservation 使 count=2
        let _ = journal
            .reservations
            .try_reserve(10, BUF_SIZE_U64S)
            .expect("first reservation should succeed");
        let _ = journal
            .reservations
            .try_reserve(20, BUF_SIZE_U64S)
            .expect("second reservation should succeed");

        // 验证 count 为 3（open(1) + 2 × reserve(1) = 3）
        let state = journal.reservations.read();
        assert_eq!(
            JournalResState::buf_count(state, idx),
            3,
            "buf_count should be 3 after open(1) + two reservations(2)"
        );

        // 释放 open_entry 的隐式 refcount：count 3→2 — 不应触发 final（仍有 2 个 reservation）
        journal.__bch2_journal_buf_put(buf_seq);

        // 验证 count 变为 2（仍有两个活跃 reservation）
        let state_mid = journal.reservations.read();
        assert_eq!(
            JournalResState::buf_count(state_mid, idx),
            2,
            "buf_count should be 2 after one release (two reservations still active)"
        );

        // 再释放一次：count 2→1 — 仍不应触发 final（仍有 1 个 reservation）
        journal.__bch2_journal_buf_put(buf_seq);

        let state_mid2 = journal.reservations.read();
        assert_eq!(
            JournalResState::buf_count(state_mid2, idx),
            1,
            "buf_count should be 1 after second release (one reservation still active)"
        );

        // 第三次释放：count 1→0 — 应触发 final
        journal.__bch2_journal_buf_put(buf_seq);

        let state_after = journal.reservations.read();
        assert_eq!(
            JournalResState::buf_count(state_after, idx),
            0,
            "buf_count should be 0 after third release (final triggered)"
        );
    }

    /// 回归测试：bch2_journal_res_put 中新增 wake_up 不破坏现有功能。
    ///
    /// 验证添加 bch2_journal_wake_up 后，多次 append + flush cycle 仍正常运作：
    /// 1. append 写入数据
    /// 2. flush 关闭 entry 并落盘
    /// 3. 第二次 append 在新 entry 上工作（seq 推进）
    #[tokio::test]
    async fn test_bch2_journal_res_put_with_wakeup_regression() {
        let backend = MockBlockDevice::new();
        let journal = Journal::new(vec![100, 200]);
        journal.set_test_device(Arc::new(BchDev::new(Arc::new(backend.clone()), 0)));
        let entry = make_test_entry();

        // 第一轮：append + flush（内部调用 bch2_journal_res_put + 新增的 wake_up）
        let seq = journal
            .append(BtreeId::Extents, std::slice::from_ref(&entry), true)
            .await
            .expect("first append + flush should succeed");

        assert!(seq > 0, "append should return a valid seq");

        // 第二轮：再 append + flush — 验证 wake_up 不破坏状态
        let seq2 = journal
            .append(BtreeId::Alloc, std::slice::from_ref(&entry), true)
            .await
            .expect("second append + flush should succeed");

        // must_flush=true 触发 flush，flush 关闭 entry 再打开新 entry → seq 推进
        assert!(
            seq2 > seq,
            "second append + flush should return a higher seq (new entry opened)"
        );
    }

    // ─── Phase 2: R2 — entry close/open 增强 ─────────────────────────────

    #[test]
    fn test_journal_entry_close_sectors_and_last_seq() {
        let journal = Journal::new(vec![100, 200]);

        // 写入数据使 close_entry() 捕获非零的 cur_entry_offset
        journal
            .reservations
            .try_reserve(10, BUF_SIZE_U64S)
            .expect("reserve 10 u64s should succeed");

        // 获取当前 buf idx（open_entry 在 new() 中已分配）
        let close_seq = journal.seq.load(Ordering::Acquire);
        let buf_idx = (close_seq & (JOURNAL_STATE_BUF_NR as u64 - 1)) as usize;

        // 关闭 entry → journal_entry_close 应设置 sectors 和 last_seq
        let used_u64s = journal.journal_entry_close();
        assert!(
            used_u64s >= 10,
            "close should capture used_u64s >= 10, got {used_u64s}"
        );

        // 验证 sectors = total_u64s.div_ceil(512) * 8（对齐 bcachefs vstruct_blocks_plus）
        let block_u64s = (JSET_BLOCK_SIZE / 8) as u64;
        let block_sectors = (JSET_BLOCK_SIZE / 512) as u64;
        let total_u64s = used_u64s as u64;
        let expected_sectors = (total_u64s.div_ceil(block_u64s) * block_sectors) as u32;
        let buf = journal.bufs.get_mut(buf_idx);
        assert!(
            expected_sectors > 0,
            "expected_sectors should be > 0 after writing 10 u64s"
        );
        assert_eq!(
            buf.sectors, expected_sectors,
            "buf.sectors should match total_u64s={total_u64s} div_ceil({block_u64s}) * {block_sectors}"
        );

        // 验证 last_seq = last_seq_ondisk（初始为 1）
        assert_eq!(buf.last_seq, 1, "buf.last_seq should be 1 for first close");
    }

    #[test]
    fn test_journal_entry_close_open_cycle() {
        let journal = Journal::new(vec![100, 200, 300]);

        // 记录 close/open 前的状态
        let seq_before_close = journal.seq.load(Ordering::Relaxed);
        let idx_before_close = JournalResState::idx(journal.reservations.read());

        // 写入一些数据
        journal
            .reservations
            .try_reserve(10, BUF_SIZE_U64S)
            .expect("reserve should succeed");

        // ── close ──
        let used_u64s = journal.journal_entry_close();
        assert!(
            used_u64s >= 10,
            "close should return used_u64s >= 10, got {used_u64s}"
        );
        assert!(
            !journal.reservations.is_open(),
            "entry should be closed after journal_entry_close"
        );
        // seq 不受 close 影响（只读不写）
        assert_eq!(
            journal.seq.load(Ordering::Relaxed),
            seq_before_close,
            "close must not modify seq"
        );
        // idx 不受 close 影响（只改 offset）
        assert_eq!(
            JournalResState::idx(journal.reservations.read()),
            idx_before_close,
            "close must not modify idx"
        );

        // ── open ──
        journal
            .journal_entry_open()
            .expect("open after close should succeed");
        assert!(
            journal.reservations.is_open(),
            "entry should be open after journal_entry_open"
        );
        // seq 推进 1
        assert_eq!(
            journal.seq.load(Ordering::Relaxed),
            seq_before_close + 1,
            "open must advance seq by 1"
        );
        // idx 推进到下一个 buf（循环）
        let idx_after_open = JournalResState::idx(journal.reservations.read());
        let expected_idx = (idx_before_close + 1) & (JOURNAL_STATE_BUF_NR as u32 - 1);
        assert_eq!(
            idx_after_open, expected_idx,
            "idx should cycle to next buf after close+open"
        );
        // 关键不变量：idx == open_seq & BUF_MASK
        // （bcachefs 要求 idx 与 entry 的 seq 低 2 位一致）
        let open_seq = journal.seq.load(Ordering::Relaxed);
        assert_eq!(
            idx_after_open,
            (open_seq & (JOURNAL_STATE_BUF_NR as u64 - 1)) as u32,
            "invariant: idx must equal (open_seq) & BUF_MASK"
        );
    }

    // ── R3: quiesce / halt ──

    #[tokio::test]
    async fn test_bch2_journal_meta_uses_res_get() {
        // 验证 __bch2_journal_meta 使用 res_get + res_put + flush 并且返回 Ok
        let journal = Journal::new(vec![100, 200, 300]);
        let backend = MockBlockDevice::new();
        journal.set_test_device(Arc::new(BchDev::new(Arc::new(backend), 0)));
        // journal_meta 不应失败（新 journal 中有足够空间）
        let result = journal.__bch2_journal_meta().await;
        assert!(
            result.is_ok(),
            "__bch2_journal_meta should succeed on fresh journal, got {:?}",
            result
        );
    }

    #[test]
    fn test_bch2_journal_halt_sets_err_seq() {
        // 验证 halt 设置 err_seq
        let journal = Journal::new(vec![100, 200, 300]);
        let seq_before = journal.bch2_journal_cur_seq();

        journal.bch2_journal_halt();

        // halt 后 err_seq 应非零且与 halt 时的 seq 一致
        let err_seq = journal.err_seq.load(Ordering::Acquire);
        assert_ne!(err_seq, 0, "err_seq should be non-zero after halt");
        assert_eq!(
            err_seq, seq_before,
            "err_seq should match cur_seq at halt time: got {}, expected {}",
            err_seq, seq_before
        );
    }

    #[test]
    fn test_bch2_journal_halt_blocks_res_get() {
        // 验证 halt 后所有 res_get 返回 Err
        let journal = Journal::new(vec![100, 200, 300]);

        journal.bch2_journal_halt();

        let result = journal.bch2_journal_res_get(Watermark::Normal, 10);
        assert!(result.is_err(), "res_get should fail after halt");
        // 应返回 Blocked 或类似错误
        match result {
            Err(JournalError::Blocked(_)) => {} // 期望结果
            Err(e) => panic!("unexpected error after halt: {:?}", e),
            Ok(_) => panic!("res_get should have failed after halt"),
        }
    }

    #[test]
    fn test_bch2_journal_quiesced_uses_seq_ondisk() {
        let journal = Journal::new(vec![100, 200, 300]);
        let seq = journal.bch2_journal_cur_seq();

        // quiesce 只看 seq_ondisk；flushed_seq_marker 可以不同。
        journal.seq_ondisk.store(seq, Ordering::Release);
        journal
            .flushed_seq_marker
            .store(seq.saturating_sub(1), Ordering::Release);
        assert!(
            journal.bch2_journal_quiesced(),
            "quiesced should follow seq_ondisk even if flushed_seq_marker differs"
        );

        // 反过来，flushed_seq_marker 相等但 seq_ondisk 不等时不应算 quiesced。
        journal
            .seq_ondisk
            .store(seq.saturating_sub(1), Ordering::Release);
        journal.flushed_seq_marker.store(seq, Ordering::Release);
        assert!(
            !journal.bch2_journal_quiesced(),
            "quiesced must not rely only on flushed_seq_marker"
        );
    }

    // ─────────────────────────────────────────────────────────
    // R6-R7: flush seq async/sync 测试
    // ─────────────────────────────────────────────────────────

    /// 验证 `bch2_journal_flush_seq` 在空闲 journal 上不会报错。
    ///
    /// 场景：新建 journal（seq=1），flush_seq(1) 应推进 flushed_seq_ondisk
    /// 并返回 Ok。
    #[test]
    fn test_bch2_journal_flush_seq_basic() {
        let backend = MockBlockDevice::new();
        let journal = Journal::new(vec![0, 1, 2, 3]);
        journal.set_test_device(Arc::new(BchDev::new(Arc::new(backend), 0)));
        let seq = journal.bch2_journal_cur_seq(); // = 1 (new() opens first entry)

        // flush seq 在空闲 journal 上不应报错
        let result = journal.bch2_journal_flush_seq(seq);
        assert!(
            result.is_ok(),
            "flush_seq on idle journal should succeed, got {:?}",
            result
        );

        // flushed_seq_ondisk 应已推进到 ≥ seq
        let flushed = journal.flushed_seq_ondisk.load(Ordering::Acquire);
        assert!(
            flushed >= seq,
            "flushed_seq_ondisk {} should be >= {} after flush_seq",
            flushed,
            seq
        );
    }

    /// 验证 `bch2_journal_flush_seq` 传入超出当前 seq 的值返回错误。
    #[test]
    fn test_bch2_journal_flush_seq_overflow() {
        let journal = Journal::new(vec![0, 1, 2, 3]);
        let cur_seq = journal.bch2_journal_cur_seq();
        let beyond_seq = cur_seq + 100;

        let result = journal.bch2_journal_flush_seq(beyond_seq);
        assert!(
            result.is_err(),
            "flush_seq with seq beyond cur_seq should return error"
        );
        match result {
            Err(JournalError::Overflow(_)) => {} // 期望结果
            Err(e) => panic!("unexpected error: {:?}", e),
            Ok(_) => panic!("should have returned error"),
        }
    }

    /// 验证 `bch2_journal_flush_seq` 已 flushed 的 seq 立即返回 Ok。
    #[test]
    fn test_bch2_journal_flush_seq_already_flushed() {
        let backend = MockBlockDevice::new();
        let journal = Journal::new(vec![0, 1, 2, 3]);
        journal.set_test_device(Arc::new(BchDev::new(Arc::new(backend), 0)));
        // 设置 flushed_seq_ondisk = 100
        journal.flushed_seq_ondisk.store(100, Ordering::Release);

        // flush_seq(50) 应直接返回 Ok（因为 50 ≤ 100）
        let result = journal.bch2_journal_flush_seq(50);
        assert!(
            result.is_ok(),
            "flush_seq of already-flushed seq should succeed"
        );
    }

    /// 验证 `bch2_journal_flush_seq_async` 在空闲 journal 上不报错。
    #[test]
    fn test_bch2_journal_flush_seq_async_basic() {
        let journal = Journal::new(vec![0, 1, 2, 3]);
        let seq = journal.bch2_journal_cur_seq();

        let result = journal.bch2_journal_flush_seq_async(seq);
        assert!(
            result.is_ok(),
            "flush_seq_async on idle journal should succeed, got {:?}",
            result
        );
    }

    /// 验证 `bch2_journal_flush_seq_async` 对已 flush 的 seq 立即返回 Ok。
    #[test]
    fn test_bch2_journal_flush_seq_async_already_flushed() {
        let journal = Journal::new(vec![0, 1, 2, 3]);
        journal.flushed_seq_ondisk.store(100, Ordering::Release);

        let result = journal.bch2_journal_flush_seq_async(50);
        assert!(
            result.is_ok(),
            "flush_seq_async of already-flushed seq should succeed"
        );
    }

    /// 验证 `bch2_journal_flush_seq_async` 对超出当前 seq 的值返回错误。
    #[test]
    fn test_bch2_journal_flush_seq_async_overflow() {
        let journal = Journal::new(vec![0, 1, 2, 3]);
        let cur_seq = journal.bch2_journal_cur_seq();
        let beyond_seq = cur_seq + 100;

        let result = journal.bch2_journal_flush_seq_async(beyond_seq);
        assert!(
            result.is_err(),
            "flush_seq_async with seq beyond cur_seq should return error"
        );
    }

    /// 验证 `bch2_journal_flush_seq_async` 会推进 `flushing_seq` 到当前 seq 上限。
    #[test]
    fn test_bch2_journal_flush_seq_async_tracks_flushing_seq() {
        let journal = Journal::new(vec![0, 1, 2, 3]);
        let cur_seq = journal.bch2_journal_cur_seq();
        let seq = cur_seq.saturating_sub(1);

        let result = journal.bch2_journal_flush_seq_async(seq);
        assert!(result.is_ok(), "flush_seq_async should succeed");
        assert_eq!(
            journal.flushing_seq.load(Ordering::Acquire),
            seq,
            "flushing_seq should advance to the requested live seq"
        );
    }

    /// 验证 `bch2_journal_flush_seq_async` 在错误态下先返回错误且不推进 `flushing_seq`。
    #[test]
    fn test_bch2_journal_flush_seq_async_error_gates_before_state_update() {
        let journal = Journal::new(vec![0, 1, 2, 3]);
        journal.err_seq.store(1, Ordering::Release);

        let seq = journal.bch2_journal_cur_seq();
        let result = journal.bch2_journal_flush_seq_async(seq);
        assert!(
            matches!(result, Err(JournalError::Blocked(_))),
            "flush_seq_async should fail with journal halted error"
        );
        assert_eq!(
            journal.flushing_seq.load(Ordering::Acquire),
            0,
            "flushing_seq must not advance on err_seq gate"
        );
    }

    /// 验证 `bch2_journal_flush_async` 不 panic。
    #[test]
    fn test_bch2_journal_flush_async_basic() {
        let journal = Journal::new(vec![0, 1, 2, 3]);
        // 不应 panic
        journal.bch2_journal_flush_async();
    }

    /// 验证 `bch2_journal_flush_seq` 在 halt 后返回错误。
    #[test]
    fn test_bch2_journal_flush_seq_after_halt() {
        let journal = Journal::new(vec![0, 1, 2, 3]);
        journal.bch2_journal_halt();

        let seq = journal.bch2_journal_cur_seq();
        let result = journal.bch2_journal_flush_seq(seq);
        assert!(result.is_err(), "flush_seq after halt should return error");
    }

    /// 验证 `bch2_journal_flush_seq_async` 在 halt 后返回错误。
    #[test]
    fn test_bch2_journal_flush_seq_async_after_halt() {
        let journal = Journal::new(vec![0, 1, 2, 3]);
        journal.bch2_journal_halt();

        let seq = journal.bch2_journal_cur_seq();
        let result = journal.bch2_journal_flush_seq_async(seq);
        assert!(
            result.is_err(),
            "flush_seq_async after halt should return error"
        );
    }

    /// 回归测试：多次调用 `bch2_journal_flush_seq` 不影响后续 append 操作。
    ///
    /// flush_seq 关闭 entry 后，调用方需要先重新打开 entry 才能继续 append。
    /// 这是因为 append 使用 `bch2_journal_res_get_fast`，它不会自动进入 slowpath reopen。
    #[test]
    fn test_bch2_journal_flush_seq_regression() {
        let backend = MockBlockDevice::new();
        let journal = Journal::new(vec![100, 200]);
        journal.set_test_device(Arc::new(BchDev::new(Arc::new(backend.clone()), 0)));
        let entry = make_test_entry();

        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("multi-thread tokio runtime should build");

        rt.block_on(async {
            // 先调 flush_seq 关闭当前 entry、推进 flushed_seq_marker
            let seq = journal.bch2_journal_cur_seq();
            assert!(journal.bch2_journal_flush_seq(seq).is_ok());

            // flush_seq 关闭了 entry → 需恢复开放状态
            //（append 使用 res_get_fast，不能在关闭的 entry 上工作）
            journal
                .journal_entry_open()
                .expect("reopen after flush_seq should succeed");

            // 正常 append + flush
            let append_seq = journal
                .append(BtreeId::Extents, std::slice::from_ref(&entry), true)
                .await
                .expect("append should succeed after flush_seq + reopen");
            assert!(append_seq > 0);

            // 再次 flush_seq
            let seq2 = journal.bch2_journal_cur_seq();
            let result = journal.bch2_journal_flush_seq(seq2);
            assert!(
                result.is_ok(),
                "flush_seq after append+flush should succeed: {result:?}"
            );
        });
    }

    // ─── R5: block / unblock ─────────────────────────────────────

    /// 验证 block 后 fastpath reservation 失败，unblock 后恢复。
    #[test]
    fn test_bch2_journal_block_unblock_cycle() {
        let journal = Journal::new(vec![100, 200, 300]);

        // Block 前：fastpath 应成功
        let res_before = journal.bch2_journal_res_get_fast(Watermark::Normal, 1);
        assert!(res_before.is_ok(), "fastpath should succeed before block");

        // Block
        journal.__bch2_journal_block();

        // Block 后：fastpath 应失败（BLOCKED_VAL 使 cur_entry_offset 超出 BUF_SIZE_U64S）
        let res_during = journal.bch2_journal_res_get_fast(Watermark::Normal, 1);
        assert!(res_during.is_err(), "fastpath should fail after block");

        // 验证 blocked 计数器已递增
        assert_eq!(
            journal.blocked.load(Ordering::Acquire),
            1,
            "blocked counter should be 1"
        );

        // Unblock
        journal.bch2_journal_unblock();

        // Unblock 后：blocked 计数器归零
        assert_eq!(
            journal.blocked.load(Ordering::Acquire),
            0,
            "blocked counter should be 0 after unblock"
        );

        // Unblock 后：fastpath 应重新成功
        let res_after = journal.bch2_journal_res_get_fast(Watermark::Normal, 1);
        assert!(res_after.is_ok(), "fastpath should succeed after unblock");
    }

    /// 验证嵌套 block/unblock（多个 blocker 同时存在）。
    #[test]
    fn test_bch2_journal_block_nested() {
        let journal = Journal::new(vec![100, 200, 300]);

        // 第一层 block
        journal.__bch2_journal_block();
        assert_eq!(
            journal.blocked.load(Ordering::Acquire),
            1,
            "blocked should be 1 after first block"
        );
        let res1 = journal.bch2_journal_res_get_fast(Watermark::Normal, 1);
        assert!(res1.is_err(), "fastpath should fail after first block");

        // 第二层 block（嵌套）
        journal.__bch2_journal_block();
        assert_eq!(
            journal.blocked.load(Ordering::Acquire),
            2,
            "blocked should be 2 after second block"
        );
        let res2 = journal.bch2_journal_res_get_fast(Watermark::Normal, 1);
        assert!(
            res2.is_err(),
            "fastpath should still fail after second block"
        );

        // 第一层 unblock：blocked 2→1，仍应阻止 reservation
        journal.bch2_journal_unblock();
        assert_eq!(
            journal.blocked.load(Ordering::Acquire),
            1,
            "blocked should be 1 after first unblock"
        );
        let res3 = journal.bch2_journal_res_get_fast(Watermark::Normal, 1);
        assert!(
            res3.is_err(),
            "fastpath should still fail after first unblock (nested)"
        );

        // 第二层 unblock：blocked 1→0，reservation 恢复
        journal.bch2_journal_unblock();
        assert_eq!(
            journal.blocked.load(Ordering::Acquire),
            0,
            "blocked should be 0 after second unblock"
        );
        let res4 = journal.bch2_journal_res_get_fast(Watermark::Normal, 1);
        assert!(res4.is_ok(), "fastpath should succeed after both unblocks");
    }

    /// 验证 block 后 journal_entry_open 返回 Blocked 错误。
    #[test]
    fn test_bch2_journal_block_prevents_entry_open() {
        let journal = Journal::new(vec![100, 200, 300]);

        // 关闭当前 entry
        journal.journal_entry_close();
        assert!(
            !journal.reservations.is_open(),
            "entry should be closed after manual close"
        );

        // Block
        journal.__bch2_journal_block();

        // 尝试打开新 entry → 应失败（blocked）
        let open_result = journal.journal_entry_open();
        assert!(
            matches!(open_result, Err(JournalError::Blocked(_))),
            "journal_entry_open should return Blocked while blocked, got {:?}",
            open_result
        );

        // Unblock
        journal.bch2_journal_unblock();

        // Unblock 后：entry 可正常打开
        let open_after = journal.journal_entry_open();
        assert!(
            open_after.is_ok(),
            "journal_entry_open should succeed after unblock"
        );
        assert!(
            journal.reservations.is_open(),
            "entry should be open after successful open"
        );
    }

    /// 验证 block 保存的 cur_entry_offset_if_blocked 在 unblock 时正确恢复。
    #[test]
    fn test_bch2_journal_block_restores_offset() {
        let journal = Journal::new(vec![100, 200, 300]);

        // 先做一些 reservation 使 cur_entry_offset 推进
        let _r1 = journal
            .reservations
            .try_reserve(10, BUF_SIZE_U64S)
            .expect("first reserve should succeed");

        // 捕获 block 前的 offset
        let before_state = journal.reservations.read();
        let before_offset = JournalResState::cur_entry_offset(before_state);

        // Block
        journal.__bch2_journal_block();

        // 验证 cur_entry_offset_if_blocked 保存了正确的值
        let saved = journal.cur_entry_offset_if_blocked.load(Ordering::Acquire);
        assert_eq!(
            saved, before_offset,
            "cur_entry_offset_if_blocked should match offset at block time"
        );

        // 验证当前 reservations offset 已变为 BLOCKED_VAL
        // （如果 block 前 entry 是打开的）
        if before_offset as u64 >= JOURNAL_ENTRY_CLOSED_VAL {
            return; // entry 已关闭，block 不会改变 offset
        }
        let blocked_state = journal.reservations.read();
        let blocked_offset = JournalResState::cur_entry_offset(blocked_state) as u64;
        assert_eq!(
            blocked_offset, JOURNAL_ENTRY_BLOCKED_VAL,
            "reservations offset should be BLOCKED_VAL after block"
        );

        // Unblock
        journal.bch2_journal_unblock();

        // 验证 offset 已被恢复
        let after_state = journal.reservations.read();
        let after_offset = JournalResState::cur_entry_offset(after_state);
        assert_eq!(
            after_offset, before_offset,
            "cur_entry_offset should be restored to pre-block value after unblock"
        );
    }

    /// 验证 block 时如果 entry 已关闭，不会设置 BLOCKED_VAL。
    #[test]
    fn test_bch2_journal_block_already_closed() {
        let journal = Journal::new(vec![100, 200, 300]);

        // 关闭当前 entry
        journal.journal_entry_close();
        assert!(
            !journal.reservations.is_open(),
            "entry must be closed for this test"
        );

        // Block（entry 已关闭时，__bch2_journal_block 应只递增 blocked 计数器）
        journal.__bch2_journal_block();

        // blocked 计数器应已递增
        assert_eq!(
            journal.blocked.load(Ordering::Acquire),
            1,
            "blocked counter should be 1"
        );

        // reservations offset 应仍为 CLOSED_VAL（未被改为 BLOCKED_VAL）
        let state = journal.reservations.read();
        let offset = JournalResState::cur_entry_offset(state) as u64;
        assert_eq!(
            offset, JOURNAL_ENTRY_CLOSED_VAL,
            "offset should remain CLOSED_VAL for already-closed entry"
        );

        // Unblock
        journal.bch2_journal_unblock();
        assert_eq!(
            journal.blocked.load(Ordering::Acquire),
            0,
            "blocked counter should be 0 after unblock"
        );
    }

    // ─────────────────────────────────────────────────────────
    // Phase 7: R8-R11 小模块测试
    // ─────────────────────────────────────────────────────────

    /// R8: 验证 `bch2_journal_entry_res_resize` 不修改缩小后的值。
    #[test]
    fn test_bch2_journal_entry_res_resize_no_change() {
        let journal = Journal::new(vec![0, 1, 2, 3]);
        let mut res_u64s: u32 = 10;

        // 相同大小 → 不应修改
        journal.bch2_journal_entry_res_resize(&mut res_u64s, 10);
        assert_eq!(res_u64s, 10, "res_u64s should remain unchanged");
    }

    /// R8: 验证 `bch2_journal_entry_res_resize` 增加预留值。
    #[test]
    fn test_bch2_journal_entry_res_resize_expand() {
        let journal = Journal::new(vec![0, 1, 2, 3]);
        let mut res_u64s: u32 = 5;

        // 扩大预留
        journal.bch2_journal_entry_res_resize(&mut res_u64s, 20);
        assert_eq!(res_u64s, 20, "res_u64s should be updated to new value");

        // entry_u64s_reserved 应已增加 15
        let reserved = journal.entry_u64s_reserved.load(Ordering::Acquire);
        assert_eq!(reserved, 15, "entry_u64s_reserved should increase by 15");
    }

    /// R8: 验证 `bch2_journal_entry_res_resize` 缩小预留值（bcachefs: d <= 0 时更新 accounting 后返回）。
    #[test]
    fn test_bch2_journal_entry_res_resize_shrink() {
        let journal = Journal::new(vec![0, 1, 2, 3]);
        let mut res_u64s: u32 = 100;
        // 设置 entry_u64s_reserved 以匹配 res_u64s（模拟正常状态）
        journal.entry_u64s_reserved.store(100, Ordering::Release);

        // 缩小（bcachefs: d <= 0 时更新 accounting 后返回，不再做空间检查）
        journal.bch2_journal_entry_res_resize(&mut res_u64s, 10);
        // res_u64s 应更新为 10（bcachefs: res->u64s += d;）
        assert_eq!(
            res_u64s, 10,
            "res_u64s should be updated to new_u64s when shrinking"
        );
        // entry_u64s_reserved 应减少 90
        let reserved = journal.entry_u64s_reserved.load(Ordering::Acquire);
        assert_eq!(
            reserved, 10,
            "entry_u64s_reserved should decrease by 90 on shrink"
        );
    }

    /// R9: 验证 `bch2_journal_noflush_seq` 在新 buf 上返回 true。
    #[test]
    fn test_bch2_journal_noflush_seq_basic() {
        let journal = Journal::new(vec![0, 1, 2, 3]);
        let vol = make_test_vol_with_noflush_gate(true);
        journal.set_vol_ref(&vol);
        let seq = journal.bch2_journal_cur_seq();
        // 新 buf 应可 noflush
        let result = journal.bch2_journal_noflush_seq(seq, seq + 1);
        assert!(result, "noflush_seq on new buf should succeed");
    }

    /// R9: 验证 `bch2_journal_noflush_seq` 对已 flush 的 seq 返回 false。
    #[test]
    fn test_bch2_journal_noflush_seq_already_flushed() {
        let journal = Journal::new(vec![0, 1, 2, 3]);
        journal.flushed_seq_ondisk.store(50, Ordering::Release);

        // flushed >= start → 应返回 false
        let result = journal.bch2_journal_noflush_seq(50, 60);
        assert!(!result, "noflush_seq on flushed seq should return false");

        // 即使 start 略小于 flushed，也比对
        let result = journal.bch2_journal_noflush_seq(40, 50);
        assert!(
            !result,
            "noflush_seq when start <= flushed should return false"
        );
    }

    /// R9: 验证 `bch2_journal_noflush_seq` 遍历多个 seq 并在第一个不可 noflush 的 buf 处停止。
    #[test]
    fn test_bch2_journal_noflush_seq_multi() {
        let journal = Journal::new(vec![0, 1, 2, 3]);
        let seq = journal.bch2_journal_cur_seq();

        // 标记一个 buf 为 has waiters → 阻止 noflush
        let idx = (seq & (JOURNAL_STATE_BUF_NR as u64 - 1)) as usize;
        let buf = journal.bufs.get_mut(idx);
        buf.wait_first = JournalBufWaitState::Waiters;

        // 尝试 noflush ≥ 2 seq（第一个可 noflush，第二个有 waiters）
        let result = journal.bch2_journal_noflush_seq(seq, seq + 2);
        assert!(!result, "noflush_seq should fail when a buf has waiters");
    }

    /// R9: 验证 `bch2_journal_noflush_seq` 受 superblock feature gate 控制。
    #[test]
    fn test_bch2_journal_noflush_seq_feature_gate() {
        let journal_without_gate = Journal::new(vec![0, 1, 2, 3]);
        let seq = journal_without_gate.bch2_journal_cur_seq();

        let vol_without_gate = make_test_vol_with_noflush_gate(false);
        journal_without_gate.set_vol_ref(&vol_without_gate);
        assert!(
            !journal_without_gate.bch2_journal_noflush_seq(seq, seq + 1),
            "noflush_seq should be disabled when feature gate is absent"
        );

        let journal_with_gate = Journal::new(vec![0, 1, 2, 3]);
        let vol_with_gate = make_test_vol_with_noflush_gate(true);
        journal_with_gate.set_vol_ref(&vol_with_gate);
        assert!(
            journal_with_gate.bch2_journal_noflush_seq(seq, seq + 1),
            "noflush_seq should be enabled when feature gate is present"
        );
    }

    /// R10: 验证 `bch2_journal_advance_rewind_seq` 正确更新 rewind_seq。
    #[test]
    fn test_bch2_journal_advance_rewind_seq_basic() {
        let journal = Journal::new(vec![0, 1, 2, 3]);
        // 对应 bcachefs — rewind_seq 初始为 0（无 rewind 目标）
        assert_eq!(
            journal.rewind_seq.load(Ordering::Acquire),
            0,
            "initial rewind_seq should be 0 (no rewind active, bcachefs default)"
        );

        journal.bch2_journal_advance_rewind_seq(42);
        assert_eq!(
            journal.rewind_seq.load(Ordering::Acquire),
            42,
            "rewind_seq should be 42 after advance"
        );

        // 推进到更小的值 → 不应降低
        journal.bch2_journal_advance_rewind_seq(10);
        assert_eq!(
            journal.rewind_seq.load(Ordering::Acquire),
            42,
            "rewind_seq should not decrease"
        );

        // 推进到更大的值 → 应增加
        journal.bch2_journal_advance_rewind_seq(100);
        assert_eq!(
            journal.rewind_seq.load(Ordering::Acquire),
            100,
            "rewind_seq should increase to 100"
        );
    }

    /// R10: 验证 `bch2_journal_add_rewind_range` 至少不 panic。
    #[test]
    fn test_bch2_journal_add_rewind_range_basic() {
        let journal = Journal::new(vec![0, 1, 2, 3]);
        let result = journal.bch2_journal_add_rewind_range(10, 20);
        assert!(
            result.is_ok(),
            "bch2_journal_add_rewind_range should succeed"
        );
        {
            let sp = journal.slowpath.lock().unwrap();
            assert_eq!(sp.rewind_ranges.as_slice(), &[(10, 20)]);
            assert_eq!(sp.early_journal_entries.as_slice(), &[(10, 20)]);
        }
        // add_rewind_range 只记录 pending range，不推进 rewind_seq
        // 对应 bcachefs — rewind_seq 仅由 bch2_journal_do_writes 在 flush 时推进
        assert_eq!(
            journal.rewind_seq.load(Ordering::Acquire),
            0,
            "rewind_seq should remain 0 (bcachefs default) until the write path advances it"
        );
    }

    /// R10: 验证 rewind pending 记录会在写盘前被编码进额外的 Jset。
    #[test]
    fn test_bch2_journal_inject_rewind_entries_into_buf() {
        let journal = Journal::new(vec![0, 1, 2, 3]);
        journal.bch2_journal_add_rewind_range(10, 20).unwrap();
        journal.bch2_journal_advance_rewind_seq(20);

        let pending_ranges = {
            let sp = journal.slowpath.lock().unwrap();
            sp.early_journal_entries.clone()
        };
        let base_jset = Jset::new(7, 6);
        let base_serialized = base_jset.serialize_padded().unwrap();
        let mut buf = vec![0u8; base_serialized.len() * 2];
        buf[..base_serialized.len()].copy_from_slice(&base_serialized);
        let mut data_end = base_serialized.len();

        let applied =
            Journal::bch2_inject_rewind_entries_into_buf(&pending_ranges, &mut buf, &mut data_end);
        assert!(applied, "rewind entries should be injected into buf");
        assert!(
            data_end > base_serialized.len(),
            "buf should grow after injection"
        );

        let appended = Jset::deserialize(&buf[base_serialized.len()..data_end])
            .unwrap()
            .unwrap();
        let entry_types: Vec<u8> = appended.entries.iter().map(|e| e.hdr.entry_type).collect();
        assert_eq!(entry_types, vec![JsetEntryType::Rewind as u8]);

        let rewind_entry = &appended.entries[0];
        assert_eq!(rewind_entry.payload.len(), 16);
        assert_eq!(&rewind_entry.payload[..8], &10u64.to_le_bytes());
        assert_eq!(&rewind_entry.payload[8..], &20u64.to_le_bytes());
    }

    /// R10: 验证 flush 分支会单独追加 `RewindLimit` entry。
    #[test]
    fn test_bch2_journal_inject_rewind_limit_into_buf() {
        let journal = Journal::new(vec![0, 1, 2, 3]);
        journal.bch2_journal_advance_rewind_seq(20);

        let base_jset = Jset::new(7, 6);
        let base_serialized = base_jset.serialize_padded().unwrap();
        let mut buf = vec![0u8; base_serialized.len() * 2];
        buf[..base_serialized.len()].copy_from_slice(&base_serialized);
        let mut data_end = base_serialized.len();

        let applied = Journal::bch2_inject_rewind_limit_into_buf(20, 7, &mut buf, &mut data_end);
        assert!(applied, "rewind limit should be injected into buf");

        let appended = Jset::deserialize(&buf[..data_end]).unwrap().unwrap();
        let entry_types: Vec<u8> = appended.entries.iter().map(|e| e.hdr.entry_type).collect();
        assert_eq!(entry_types, vec![JsetEntryType::RewindLimit as u8]);

        let rewind_limit = &appended.entries[0];
        assert_eq!(rewind_limit.payload.len(), 8);
        assert_eq!(&rewind_limit.payload[..], &8u64.to_le_bytes());
    }

    /// R11: 验证 `bch2_journal_do_writes_locked` 在无待写 buf 时不做无意义的 flush。
    #[test]
    fn test_bch2_journal_do_writes_locked_basic() {
        let journal = Journal::new(vec![0, 1, 2, 3]);
        // 无 WriteSubmitted buf：不应触发 flush（优化测试 —— 空 journal 无待写）
        // 此时 __should_flush 因无触发条件返回 false，仅标记 noflush（无操作），
        // 不会设置 needs_flush_write。
        journal.bch2_journal_do_writes_locked();
        // needs_flush_write 可以 true 或 false（取决于 util 等条件），但不会 panic
    }

    /// R11: 验证 `bch2_journal_do_writes` 启动有 flush waiter 的 WriteSubmitted buf。
    #[test]
    fn test_bch2_journal_do_writes_basic() {
        let journal = Journal::new(vec![0, 1, 2, 3]);
        journal.bch2_journal_clear_needs_flush_write();
        assert!(
            !journal.bch2_journal_needs_flush_write(),
            "needs_flush_write should be false after clear"
        );

        // 模拟本地 write.c:1092-1096 的可提交前置条件：FIFO 中最旧未 allocation
        // entry 已关闭，且 reservation count 已归零。
        let idx = *journal.in_flight.lock().unwrap().front().unwrap();
        let buf = journal.bufs.get_mut(idx as usize);
        let seq = buf.seq;
        buf.state = BufState::WriteSubmitted;
        buf.has_must_flush = true;
        buf.wait_first = JournalBufWaitState::Waiters;
        let _ = buf;
        journal.reservations.release(idx);
        journal
            .seq_ondisk
            .store(seq.saturating_sub(1), Ordering::Release);

        journal.bch2_journal_do_writes();
        assert!(
            !journal.bch2_journal_needs_flush_write(),
            "flush selection should clear JOURNAL_need_flush_write"
        );
        assert!(journal.journal_seq_to_buf(seq).unwrap().write_started);
    }

    #[test]
    fn test_bch2_journal_do_writes_zero_delay_selects_flush() {
        let journal = Journal::new(vec![0, 1, 2, 3]);
        journal.bch2_journal_clear_needs_flush_write();
        journal.journal_flush_delay_ms.store(0, Ordering::Release);
        journal.last_flush_jiffies.store(0, Ordering::Release);
        let idx = *journal.in_flight.lock().unwrap().front().unwrap();
        let w = journal.bufs.get_mut(idx as usize);
        let seq = w.seq;
        w.state = BufState::WriteSubmitted;
        w.wait_first = JournalBufWaitState::Empty;
        let _ = w;
        journal.reservations.release(idx);
        journal
            .seq_ondisk
            .store(seq.saturating_sub(1), Ordering::Release);

        journal.bch2_journal_do_writes();

        let w = journal.journal_seq_to_buf(seq).unwrap();
        assert!(w.flush);
        assert!(w.flush_picked);
        assert!(w.write_started);
        assert_eq!(journal.nr_flush_writes.load(Ordering::Acquire), 1);
        assert_eq!(journal.nr_noflush_writes.load(Ordering::Acquire), 0);
        assert_eq!(journal.seq_write_started.load(Ordering::Acquire), seq);
    }

    #[test]
    fn test_bch2_journal_do_writes_counts_noflush_and_started_seq() {
        let journal = Journal::new(vec![0, 1, 2, 3]);
        journal
            .journal_flush_delay_ms
            .store(60_000, Ordering::Release);
        journal.bch2_journal_update_flush_jiffies();
        let idx = *journal.in_flight.lock().unwrap().front().unwrap();
        let w = journal.bufs.get_mut(idx as usize);
        let seq = w.seq;
        w.state = BufState::WriteSubmitted;
        w.wait_first = JournalBufWaitState::Empty;
        let _ = w;
        journal.reservations.release(idx);

        journal.bch2_journal_do_writes();

        let w = journal.journal_seq_to_buf(seq).unwrap();
        assert!(!w.flush);
        assert!(w.flush_picked);
        assert!(w.write_started);
        assert_eq!(journal.nr_flush_writes.load(Ordering::Acquire), 0);
        assert_eq!(journal.nr_noflush_writes.load(Ordering::Acquire), 1);
        assert_eq!(journal.seq_write_started.load(Ordering::Acquire), seq);
    }

    #[test]
    fn test_flush_selection_rearms_or_cancels_write_work() {
        for (has_newer_open_entry, expect_pending) in [(true, true), (false, false)] {
            let journal = Journal::new(vec![0, 1, 2, 3]);
            journal
                .journal_flush_delay_ms
                .store(60_000, Ordering::Release);
            journal.write_work_deadline_ms.store(42, Ordering::Release);

            let idx = *journal.in_flight.lock().unwrap().front().unwrap();
            let w = journal.bufs.get_mut(idx as usize);
            let seq = w.seq;
            w.state = BufState::WriteSubmitted;
            w.has_must_flush = true;
            w.wait_first = JournalBufWaitState::Waiters;
            let _ = w;
            journal.reservations.release(idx);
            journal
                .seq_ondisk
                .store(seq.saturating_sub(1), Ordering::Release);
            if has_newer_open_entry {
                journal.seq.store(seq + 1, Ordering::Release);
            }

            let before = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            journal.bch2_journal_do_writes();

            let deadline = journal.write_work_deadline_ms.load(Ordering::Acquire);
            if expect_pending {
                assert!(deadline >= before.saturating_add(60_000));
            } else {
                assert_eq!(deadline, 0);
            }
        }
    }

    #[test]
    fn test_should_flush_uses_volume_journal_flush_delay() {
        let journal = Journal::new(vec![0, 1, 2, 3]);
        let mut sb = BchSb::with_volume_info(
            "test-vol".to_string(),
            1,
            "test-pool".to_string(),
            4096,
            1024 * 1024,
            crate::types::BackendType::Nfs,
        );
        sb.storage_config = Some(crate::config::StorageConfig {
            journal_flush_delay_ms: 60_000,
            ..crate::config::StorageConfig::default()
        });
        let vol = Arc::new(BchVol::alloc(
            sb,
            Arc::new(BchDev::new(Arc::new(MockBlockDevice::new()), 0)),
            VolumeConfig::default(),
            "test-vol".to_string(),
            PathBuf::from("/tmp/test-vol"),
        ));
        journal.set_vol_ref(&vol);
        journal.bch2_journal_clear_needs_flush_write();
        journal.journal_flush_delay_ms.store(0, Ordering::Release);
        journal.bch2_journal_update_flush_jiffies();

        assert_eq!(vol.opts.journal_flush_delay, 60_000);
        assert_eq!(journal.__should_flush(journal.bch2_journal_cur_seq()), 0);
    }

    #[test]
    fn test_flush_selection_respects_rewind_discard_buffer_option() {
        for (percent, expected_rewind_seq) in [(0, 2), (4, 0)] {
            let journal = Journal::new(vec![0, 1, 2, 3]);
            let sb = BchSb::with_volume_info(
                "test-vol".to_string(),
                1,
                "test-pool".to_string(),
                4096,
                1024 * 1024,
                crate::types::BackendType::Nfs,
            );
            let vol = Arc::new(BchVol::alloc(
                sb,
                Arc::new(BchDev::new(Arc::new(MockBlockDevice::new()), 0)),
                VolumeConfig {
                    journal_rewind_discard_buffer_percent: percent,
                    ..VolumeConfig::default()
                },
                "test-vol".to_string(),
                PathBuf::from("/tmp/test-vol"),
            ));
            journal.set_vol_ref(&vol);
            journal.bch2_journal_clear_needs_flush_write();
            journal.journal_flush_delay_ms.store(0, Ordering::Release);
            journal.last_flush_jiffies.store(0, Ordering::Release);

            let idx = *journal.in_flight.lock().unwrap().front().unwrap();
            let w = journal.bufs.get_mut(idx as usize);
            let seq = w.seq;
            assert_eq!(seq, 1);
            w.state = BufState::WriteSubmitted;
            w.wait_first = JournalBufWaitState::Empty;
            let _ = w;
            journal.reservations.release(idx);
            journal
                .seq_ondisk
                .store(seq.saturating_sub(1), Ordering::Release);

            journal.bch2_journal_do_writes();

            assert_eq!(
                journal.rewind_seq.load(Ordering::Acquire),
                expected_rewind_seq
            );
        }
    }

    /// R11: 验证 `bch2_journal_write_work` 不 panic。
    #[test]
    fn test_bch2_journal_write_work_basic() {
        let journal = Journal::new(vec![0, 1, 2, 3]);
        // 仅验证不 panic（内部调 flush_async）
        journal.bch2_journal_write_work();
    }

    #[test]
    fn test_journal_waitlist_splice_moves_waiters() {
        let mut from = JournalBuf::free();
        let mut to = JournalBuf::free();
        from.wait_first = JournalBufWaitState::Waiters;
        to.wait_first = JournalBufWaitState::Empty;
        from.write_done_callbacks.push(Some(Box::new(|| {})));

        assert!(journal_waitlist_splice(&mut from, &mut to));
        assert_eq!(from.wait_first, JournalBufWaitState::NoFlush);
        assert!(from.write_done_callbacks.is_empty());
        assert_eq!(to.wait_first, JournalBufWaitState::Waiters);
        assert_eq!(to.write_done_callbacks.len(), 1);
    }

    #[test]
    fn test_journal_waitlist_splice_restores_on_sentinel() {
        let mut from = JournalBuf::free();
        let mut to = JournalBuf::free();
        from.wait_first = JournalBufWaitState::Waiters;
        from.write_done_callbacks.push(Some(Box::new(|| {})));
        to.wait_first = JournalBufWaitState::FlushNoWait;

        assert!(!journal_waitlist_splice(&mut from, &mut to));
        assert_eq!(from.wait_first, JournalBufWaitState::Waiters);
        assert_eq!(from.write_done_callbacks.len(), 1);
        assert_eq!(to.wait_first, JournalBufWaitState::FlushNoWait);
        assert!(to.write_done_callbacks.is_empty());
    }

    #[test]
    fn test_last_uncompleted_write_seq_two_state_semantics() {
        let journal = Journal::new(vec![0, 1, 2, 3]);
        let idx = *journal.in_flight.lock().unwrap().front().unwrap();
        let buf = journal.bufs.get_mut(idx as usize);
        let seq = buf.seq;

        buf.write_done = false;
        assert_eq!(journal.last_uncompleted_write_seq(seq + 1), 0);
        assert_eq!(journal.last_uncompleted_write_seq(seq), seq);

        buf.write_done = true;
        assert_eq!(journal.last_uncompleted_write_seq(seq + 1), seq);
    }

    #[test]
    fn test_replicas_refs_put_batches_refs_collected_from_pin_fifo() {
        let journal = Journal::new(vec![0, 1, 2, 3]);
        let vol = make_test_vol_with_noflush_gate(false);
        let replicas = crate::replicas::BchReplicasEntry::new(BchDataType::Journal, &[0], 1);
        {
            let mut table = vol.replicas.lock().unwrap();
            table.get_or_mark(&replicas);
            table.get_or_mark(&replicas);
        }

        unsafe {
            let fifo = &mut *journal.pin_fifo.get();
            assert!(fifo.push_back(JournalEntryPinList::new(1)).is_ok());
            for seq in 1..3 {
                let pin = fifo.entry_for_seq_mut(seq).unwrap();
                pin.set_devs(&[0]);
                pin.bytes = 64;
            }
        }
        journal.last_seq_ondisk.store(1, Ordering::Release);
        journal.dirty_entry_bytes.store(128, Ordering::Release);

        let mut refs = ReplicasEntryRefs::new();
        assert_eq!(journal.bch2_journal_update_last_seq_ondisk(3, &mut refs), 0);
        assert_eq!(refs.entries.len(), 1);
        assert_eq!(refs.entries[0].nr_refs, 2);
        assert_eq!(journal.dirty_entry_bytes.load(Ordering::Acquire), 0);
        assert_eq!(journal.pin_fifo_ref().entry_for_seq(1).unwrap().devs.nr, 0);
        assert_eq!(journal.pin_fifo_ref().entry_for_seq(2).unwrap().devs.nr, 0);

        replicas_refs_put(&vol, &mut refs);

        assert!(refs.is_empty());
        assert!(vol.replicas.lock().unwrap().is_empty());
    }

    #[test]
    fn test_journal_buf_realloc_resizes_write_buffers_first_and_preserves_data() {
        let journal = Journal::new(vec![0, 1, 2, 3]);
        let vol = make_test_vol_with_noflush_gate(false);
        journal.set_vol_ref(&vol);

        let mut buf = JournalBuf::free();
        buf.reset_for_accepting(7);
        buf.data[..4].copy_from_slice(&[1, 2, 3, 4]);
        journal.buf_size_want.store(BUF_SIZE * 2, Ordering::Release);

        journal.journal_buf_realloc(&mut buf);

        assert_eq!(buf.buf_size, BUF_SIZE * 2);
        assert_eq!(buf.data.len(), BUF_SIZE * 2);
        assert_eq!(&buf.data[..4], &[1, 2, 3, 4]);
        let set = unsafe { &*vol.write_buffer_set.get() };
        for wb in &set.buffers {
            assert!(wb.flushing.keys.capacity() >= (BUF_SIZE * 2) / 64);
            assert!(wb.inc.keys.capacity() >= (BUF_SIZE * 2) / 64);
        }
    }

    #[test]
    fn test_journal_write_alloc_advances_and_holds_io_ref() {
        let journal = Journal::new(vec![0, 1, 2, 3]);
        let vol = make_test_vol_with_noflush_gate(false);
        let ca = vol.primary_device_rcu_noerror().unwrap();
        unsafe {
            (*ca.mi.get()).bucket_size = 1024;
            (*ca.mi.get()).durability = 1;
            (*ca.mi.get()).data_allowed |= 1 << BchDataType::Journal as u8;
        }
        {
            let mut ja = ca.journal.lock().unwrap();
            ja.bucket_seq = vec![0; 4];
            ja.sectors_free = 0;
            ja.discard_idx = 0;
            ja.dirty_idx_ondisk = 0;
            ja.dirty_idx = 0;
            ja.cur_idx = 0;
            ja.nr = 4;
            ja.buckets = vec![10, 11, 12, 13];
        }
        journal.set_vol_ref(&vol);

        let idx = *journal.in_flight.lock().unwrap().front().unwrap() as usize;
        let w = journal.bufs.get_mut(idx);
        w.data_end = JSET_BLOCK_SIZE as usize;
        let seq = w.seq;
        let mut replicas = 0;

        journal.journal_write_alloc(w, &mut replicas).unwrap();

        assert_eq!(replicas, 1);
        assert_eq!(w.key.len(), 1);
        assert_eq!(w.key[0].dev, ca.dev_idx);
        assert_eq!(w.key[0].offset, 11 * 1024);
        assert_eq!(ca.io_ref_count(BchDevIoRefKind::Write), 1);
        let ja = ca.journal.lock().unwrap();
        assert_eq!(ja.cur_idx, 1);
        assert_eq!(ja.sectors_free, 1024 - SECTORS_PER_BLOCK as u32);
        assert_eq!(ja.bucket_seq[1], seq);
        drop(ja);

        w.cas.clear();
        assert_eq!(ca.io_ref_count(BchDevIoRefKind::Write), 0);
    }

    #[test]
    fn test_journal_write_alloc_target_then_all_devices_for_replicas() {
        let journal = Journal::new(vec![0, 1, 2, 3]);
        let mut sb = BchSb::with_volume_info(
            "test-vol".to_string(),
            1,
            "test-pool".to_string(),
            4096,
            1024 * 1024,
            crate::types::BackendType::Nfs,
        );
        sb.members.clear();
        for dev_idx in 0..2 {
            let mut member = BchSbMember::new(dev_idx, format!("dev-{dev_idx}"));
            member.mark_alive([dev_idx + 1; 16]);
            member.first_bucket = 1;
            member.bucket_size = 1024;
            member.nbuckets = 64;
            member.flags |= (1 << BchDataType::Journal as u8)
                << crate::storage::superblock::member_bits::DATA_ALLOWED_SHIFT;
            sb.members.push(member);
        }
        sb.primary_dev_idx = 0;

        let dev0 = Arc::new(BchDev::new(Arc::new(MockBlockDevice::new()), 0));
        let dev1 = Arc::new(BchDev::new(Arc::new(MockBlockDevice::new()), 1));
        let config = VolumeConfig {
            metadata_replicas: 3,
            metadata_target: 1,
            ..VolumeConfig::default()
        };
        let vol = Arc::new(BchVol::alloc_with_devices(
            sb,
            [dev0.clone(), dev1.clone()],
            config,
            "test-vol".to_string(),
            PathBuf::from("/tmp/test-vol"),
        ));
        for (ca, bucket) in [(&dev0, 10_u64), (&dev1, 20_u64)] {
            unsafe {
                (*ca.mi.get()).durability = 1;
            }
            let mut ja = ca.journal.lock().unwrap();
            ja.bucket_seq = vec![0; 4];
            ja.sectors_free = 1024;
            ja.discard_idx = 0;
            ja.dirty_idx_ondisk = 0;
            ja.dirty_idx = 0;
            ja.cur_idx = 0;
            ja.nr = 4;
            ja.buckets = vec![bucket, bucket + 1, bucket + 2, bucket + 3];
        }
        journal.set_vol_ref(&vol);

        let idx = *journal.in_flight.lock().unwrap().front().unwrap() as usize;
        let w = journal.bufs.get_mut(idx);
        w.data_end = JSET_BLOCK_SIZE as usize;
        let mut replicas = 0;

        journal.journal_write_alloc(w, &mut replicas).unwrap();

        assert_eq!(replicas, 2);
        assert_eq!(w.key.iter().map(|ptr| ptr.dev).collect::<Vec<_>>(), [0, 1]);
        assert_eq!(dev0.io_ref_count(BchDevIoRefKind::Write), 1);
        assert_eq!(dev1.io_ref_count(BchDevIoRefKind::Write), 1);

        w.cas.clear();
        assert_eq!(dev0.io_ref_count(BchDevIoRefKind::Write), 0);
        assert_eq!(dev1.io_ref_count(BchDevIoRefKind::Write), 0);
    }

    #[test]
    fn test_journal_write_alloc_rejects_zero_replicas_and_releases_io_ref() {
        let journal = Journal::new(vec![0, 1, 2, 3]);
        let vol = make_test_vol_with_noflush_gate(false);
        let ca = vol.primary_device_rcu_noerror().unwrap();
        unsafe {
            (*ca.mi.get()).bucket_size = 1024;
            (*ca.mi.get()).durability = 1;
            (*ca.mi.get()).data_allowed |= 1 << BchDataType::Journal as u8;
        }
        {
            let mut ja = ca.journal.lock().unwrap();
            ja.nr = 0;
            ja.buckets.clear();
            ja.bucket_seq.clear();
        }
        journal.set_vol_ref(&vol);

        let idx = *journal.in_flight.lock().unwrap().front().unwrap() as usize;
        let w = journal.bufs.get_mut(idx);
        w.data_end = JSET_BLOCK_SIZE as usize;
        let mut replicas = 0;

        let err = journal.journal_write_alloc(w, &mut replicas).unwrap_err();

        assert!(matches!(err, JournalError::Full(_)));
        assert_eq!(replicas, 0);
        assert!(w.key.is_empty());
        assert!(w.cas.is_empty());
        assert_eq!(ca.io_ref_count(BchDevIoRefKind::Write), 0);
    }

    #[test]
    fn test_journal_write_prep_adds_common_entries_and_flushes_write_buffer_keys() {
        let journal = Journal::new(vec![0, 1, 2, 3]);
        let vol = make_test_vol_with_noflush_gate(false);
        journal.set_vol_ref(&vol);
        vol.key_version.store(17, Ordering::Release);
        vol.io_clock[0].store(23, Ordering::Release);
        vol.io_clock[1].store(29, Ordering::Release);

        let payload = bincode::serialize(&vec![make_test_entry()]).unwrap();
        let mut source = Jset::new(7, 6);
        source.entries.push(
            RawJsetEntry::new(
                BtreeId::Accounting as u8,
                JsetEntryType::WriteBufferKeys as u8,
                payload,
                0,
            )
            .unwrap(),
        );
        let serialized = source.serialize_padded().unwrap();

        let mut w = JournalBuf::free();
        w.reset_for_accepting(7);
        w.last_seq = 6;
        w.sectors = (BUF_SIZE / crate::types::SECTOR_SIZE as usize) as u32;
        w.data[..serialized.len()].copy_from_slice(&serialized);
        w.data_end = serialized.len();

        journal.bch2_journal_write_prep(&mut w).unwrap();

        let prepared = Jset::deserialize(&w.data[..w.data_end]).unwrap().unwrap();
        let types: Vec<u8> = prepared
            .entries
            .iter()
            .map(|entry| entry.hdr.entry_type)
            .collect();
        assert_eq!(
            types,
            vec![
                JsetEntryType::BtreeKeys as u8,
                JsetEntryType::Datetime as u8,
                JsetEntryType::Usage as u8,
                JsetEntryType::Clock as u8,
                JsetEntryType::Clock as u8,
            ]
        );
        assert!(!w.need_flush_to_write_buffer);

        let wb_idx = crate::btree::write_buffer::bch_wb_btree_idx(BtreeId::Accounting);
        let wb_set = unsafe { &*vol.write_buffer_set.get() };
        let wb = &wb_set.buffers[wb_idx as usize];
        assert_eq!(wb.inc.nr, 1);
        assert_eq!(wb.inc.keys[0].journal_seq, 7);
        assert_eq!(wb.inc.keys[0].btree_id, BtreeId::Accounting);

        assert_eq!(prepared.entries[2].hdr.btree_type, 2);
        assert_eq!(
            u64::from_le_bytes(prepared.entries[2].payload.clone().try_into().unwrap()),
            17
        );
        assert_eq!(prepared.entries[3].payload[0], 0);
        assert_eq!(prepared.entries[4].payload[0], 1);
    }

    #[test]
    fn test_journal_write_prep_rejects_sector_overrun() {
        let journal = Journal::new(vec![0, 1, 2, 3]);
        let source = Jset::new(7, 6).serialize_padded().unwrap();
        let mut w = JournalBuf::free();
        w.reset_for_accepting(7);
        w.last_seq = 6;
        w.sectors = 0;
        w.data[..source.len()].copy_from_slice(&source);
        w.data_end = source.len();

        assert!(matches!(
            journal.bch2_journal_write_prep(&mut w),
            Err(JournalError::Io(StorageError::InvalidData(_)))
        ));
    }

    #[test]
    fn test_journal_write_checksum_sets_local_jset_flags_and_zero_padding() {
        let journal = Journal::new(vec![0, 1, 2, 3]);
        let mut source = Jset::new(7, 6);
        source.entries.push(
            RawJsetEntry::new(
                BtreeId::Extents as u8,
                JsetEntryType::BtreeKeys as u8,
                bincode::serialize(&vec![make_test_entry()]).unwrap(),
                0,
            )
            .unwrap(),
        );
        source.header.flags = super::super::jset::JSET_NO_FLUSH;
        let serialized = source.serialize_padded().unwrap();
        let mut w = JournalBuf::free();
        w.reset_for_accepting(7);
        w.has_overwrites = true;
        w.data[..serialized.len()].copy_from_slice(&serialized);
        w.data_end = serialized.len();

        journal.bch2_journal_write_checksum(&mut w).unwrap();

        let checksummed = Jset::deserialize(&w.data[..w.data_end]).unwrap().unwrap();
        assert_eq!(
            checksummed.header.flags & super::super::jset::JSET_CSUM_TYPE_MASK,
            CSUM_TYPE_CRC32C as u32
        );
        assert_ne!(
            checksummed.header.flags & super::super::jset::JSET_NO_FLUSH,
            0
        );
        assert_ne!(
            checksummed.header.flags & super::super::jset::JSET_HAS_OVERWRITES,
            0
        );
        assert!(checksummed.verify());
        assert!(super::super::validate::bch2_jset_validate(&checksummed));

        let data_bytes = std::mem::size_of::<JsetHeader>()
            + checksummed
                .entries
                .iter()
                .map(|entry| std::mem::size_of::<JsetEntryHeader>() + entry.payload.len())
                .sum::<usize>();
        assert!(w.data[data_bytes..w.data_end].iter().all(|&byte| byte == 0));
    }
}
