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
//!
//! 详细架构规范见 `.trellis/spec/architecture.md`

mod init;
mod jset;
pub(crate) mod reclaim;
mod replay;
mod types;
pub(crate) mod validate;

pub use init::{bch2_dev_journal_alloc, bch2_fs_journal_alloc};
pub use jset::{
    crc32c, BlacklistEntry, BlacklistTable, BlacklistTableEntry, Crc32CHasher, Jset,
    JsetEntryHeader, JsetEntryType, JsetHeader, RawJsetEntry, CSUM_TYPE_CRC32C, CSUM_TYPE_NONE,
    JOURNAL_MAGIC, JSET_BLOCK_SIZE, JSET_ENTRY_VERSION, JSET_VERSION, VMNT_JSET_MAGIC,
};
pub use replay::{JournalReplayer, ReplayedEntry};
pub use types::{
    BufState, Journal, JournalBlockGuard, JournalError, JournalRes, JournalSpace, JournalStartInfo,
    JournalSuperblockState, U64Range, BUF_SIZE, DEFAULT_JOURNAL_BUCKETS, JOURNAL_NEEDS_FLUSH_WRITE,
    JOURNAL_SPACE_CLEAN, JOURNAL_SPACE_CLEAN_ONDISK, JOURNAL_SPACE_DISCARDED, JOURNAL_SPACE_NR,
    JOURNAL_SPACE_TOTAL, JOURNAL_STATE_BUF_NR, MAX_PIN_ENTRIES,
};
pub use validate::bch2_jset_validate;
