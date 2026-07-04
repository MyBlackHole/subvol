//! Journal entry (Jset) — 对应 bcachefs `struct jset`
//!
//! Jset 是 journal 中的基本条目单位。每个 Jset 包含一个或多个 entries，
//! 每个 entry 记录对某个 btree type 的批量修改。
//!
//! # 格式（v2，repr(C) 固定布局）
//!
//! ```text
//! ┌────────────────────────────────────┐
//! │ JsetHeader        (64 B fixed)     │
//! ├────────────────────────────────────┤
//! │ JsetEntryHeader 0 (8 B)           │
//! │ JsetEntryHeader 0 payload (变长)   │
//! │ JsetEntryHeader 1 (8 B)           │
//! │ JsetEntryHeader 1 payload (变长)   │
//! │ ...                                │
//! ├────────────────────────────────────┤
//! │ 零填充到 JSET_BLOCK_SIZE (4096)    │
//! └────────────────────────────────────┘
//! ```
//!
//! CRC32C 覆盖：从 JsetHeader（crc32 字段置 0）到最后一个 entry payload 末尾。

use serde::{Deserialize, Serialize};
use std::mem::size_of;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::types::StorageError;
use crc::Crc;

/// CRC32C 算法（bcachefs 对齐）：Castagnoli 多项式（0x1EDC6F41，lsb 0x82F63B78）
pub(crate) const CRC32C: Crc<u32> = Crc::<u32>::new(&crc::CRC_32_ISCSI);

// ═══════════════════════════════════════════════════════════════
// CRC32C 硬件加速 + 自动调度
// ═══════════════════════════════════════════════════════════════

/// CRC32C Castagnoli 查表（反射形式 0x82F63B78）
const CRC32C_TABLE: [u32; 256] = {
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
};

/// CRC32C 纯软件实现（Castagnoli 多项式 0x1EDC6F41，反射 0x82F63B78）
///
/// `crc` 为初始 seed（0 表示从头开始，非零用于分块连续计算）。
/// 对应 bcachefs `crc32c_le_bch(crc, buf, len)` 语义。
pub fn crc32c_sw(data: &[u8], crc: u32) -> u32 {
    let mut crc = !crc;
    for &byte in data {
        let idx = ((crc as u8) ^ byte) as usize;
        crc = CRC32C_TABLE[idx] ^ (crc >> 8);
    }
    !crc
}

/// CRC32C SSE4.2 硬件加速（x86_64 only）
///
/// 使用 `_mm_crc32_u64` 一次处理 8 字节，剩余用 `_mm_crc32_u8`。
/// 调用方必须确保 SSE4.2 可用（通过 is_x86_feature_detected 或 compile-time feature gate）。
///
/// **重要**: 硬件 CRC32 指令不做标准 CRC32 的初始补码（!crc）和最终补码（!result）。
/// 因此我们在进入指令前将 `crc` 取补，在返回前将结果取补，与 `crc32c_sw` 语义保持一致。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse4.2")]
unsafe fn crc32c_hw_impl(data: &[u8], crc: u32) -> u32 {
    // `!crc` 是标准 CRC32 初始值（seed=0 → 0xFFFFFFFF，链式调用 seed=X → !X）
    let mut crc64 = (!crc) as u64;
    for chunk in data.chunks_exact(8) {
        let val: u64 = u64::from_le_bytes(chunk.try_into().unwrap());
        crc64 = core::arch::x86_64::_mm_crc32_u64(crc64, val);
    }
    // _mm_crc32_u8 操作低 32 位
    let mut ret = crc64 as u32;
    for &b in data.chunks_exact(8).remainder() {
        ret = core::arch::x86_64::_mm_crc32_u8(ret, b);
    }
    // 最终取补，与 crc32c_sw 的 !crc 末尾语义一致
    !ret
}

/// CRC32C 自动选择硬件/软件路径
///
/// x86_64: 运行时检测 SSE4.2，有则用硬件路径，否则回退软件路径。
/// 非 x86_64: 始终使用软件路径。
///
/// `crc` 为初始 seed（用于分块连续计算），单次调用传 0。
/// 对应 bcachefs `crc32c_le_bch(0, buf, len)`。
pub fn crc32c(data: &[u8], crc: u32) -> u32 {
    #[cfg(target_arch = "x86_64")]
    {
        #[cfg(target_feature = "sse4.2")]
        {
            // 编译时已知 SSE4.2 可用（如 RUSTFLAGS="-C target-feature=+sse4.2"）
            unsafe { crc32c_hw_impl(data, crc) }
        }
        #[cfg(not(target_feature = "sse4.2"))]
        {
            // 运行时检测：std crate 中 is_x86_feature_detected! 始终可用
            if std::is_x86_feature_detected!("sse4.2") {
                unsafe { crc32c_hw_impl(data, crc) }
            } else {
                crc32c_sw(data, crc)
            }
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        crc32c_sw(data, crc)
    }
}

/// Journal 魔数（原始 subvol 格式）
pub const JOURNAL_MAGIC: [u8; 8] = *b"VOLM_JNL";

/// 新 volatile 魔数（subvol + 时间戳版本，对应 bcachefs `JSET_MAGIC` / `VMNT_JSET_MAGIC`）
pub const VMNT_JSET_MAGIC: [u8; 8] = *b"VMNTJNL0";

/// Jset padding 对齐块大小（对齐 backend block size）
pub const JSET_BLOCK_SIZE: u32 = 4096;

/// 当前 Jset 格式版本号（对应 bcachefs `bcachefs_metadata_version`）
/// v1: bincode 序列化格式
/// v2: repr(C) 固定布局
pub const JSET_VERSION: u32 = 2;

/// 校验和类型：无校验
pub const CSUM_TYPE_NONE: u8 = 0;
/// 校验和类型：crc32c
pub const CSUM_TYPE_CRC32C: u8 = 1;

/// 对应本地 bcachefs `JSET_CSUM_TYPE` (`bcachefs_format.h:1836`)。
pub const JSET_CSUM_TYPE_MASK: u32 = 0x0f;
/// 对应本地 bcachefs `JSET_BIG_ENDIAN` (`bcachefs_format.h:1837`)。
pub const JSET_BIG_ENDIAN: u32 = 1 << 4;
/// 对应本地 bcachefs `JSET_NO_FLUSH` (`bcachefs_format.h:1839`)。
pub const JSET_NO_FLUSH: u32 = 1 << 5;
/// 对应本地 bcachefs `JSET_HAS_OVERWRITES` (`bcachefs_format.h:1840`)。
pub const JSET_HAS_OVERWRITES: u32 = 1 << 6;

/// 当前 Jset entry header 格式版本。
///
/// `JsetHeader::version` 描述整个 Jset 的磁盘布局；entry version 描述单个
/// `JsetEntryHeader` 的局部布局，避免未来 entry header 扩展时只能依赖外层版本。
pub const JSET_ENTRY_VERSION: u8 = 1;

const JSET_HEADER_CRC32_OFFSET: usize = 24;

// ═══════════════════════════════════════════════════════════════
// Jset 固定布局数据结构（repr(C)，直接 ptr 读写）
// ═══════════════════════════════════════════════════════════════

/// Jset 头部（64 字节固定，repr(C)），对应 bcachefs `struct jset`
///
/// 磁盘布局：
/// - magic:      [0..8)    魔数
/// - seq:        [8..16)   递增序列号
/// - last_seq:   [16..24)  最老未 flush seq
/// - crc32:      [24..28)  CRC32C 校验和（计算时此字段置 0）
/// - entry_count: [28..32)  包含的 entry 数量
/// - version:    [32..36)  格式版本
/// - flags:      [36..40)  checksum/endian/noflush/overwrite flags
/// - pad:        [40..64)  填充到 64 字节
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

/// Jset entry 头部（8 字节固定，repr(C)），对应 bcachefs `struct jset_entry`
///
/// 磁盘布局：
/// - btree_type:  [0]    btree 类型
/// - entry_type:  [1]    entry 类型（JsetEntryType 的 u8 值）
/// - version:     [2]    entry header 格式版本
/// - flags:       [3]    保留 flags（未来用于 has_last/has_prev bit 扩展）
/// - payload_len: [4..6) payload 字节数
/// - has_last:    [6]    是否有上一 Jset（journal 链表遍历）
/// - has_prev:    [7]    是否有下一 Jset
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct JsetEntryHeader {
    pub btree_type: u8,
    pub entry_type: u8,
    pub version: u8,
    /// 对应 bcachefs jset_entry.level — btree root level（非 root entry 为 0）
    pub level: u8,
    pub payload_len: u16,
    pub has_last: u8,
    pub has_prev: u8,
}

/// Jset entry 的高层表示（header + 反序列化后的 payload）
///
/// `payload` 以 `Vec<u8>` 存储序列化的 btree keys（bincode 格式）。
#[derive(Debug, Clone)]
pub struct RawJsetEntry {
    pub hdr: JsetEntryHeader,
    pub payload: Vec<u8>,
}

impl RawJsetEntry {
    /// 创建新的 RawJsetEntry
    ///
    /// `level`: btree root 条目的 level（非 root entry 传 0），对应 bcachefs jset_entry.level。
    pub fn new(
        btree_type: u8,
        entry_type: u8,
        payload: Vec<u8>,
        level: u8,
    ) -> Result<Self, StorageError> {
        let payload_len = u16::try_from(payload.len()).map_err(|_| {
            StorageError::InvalidData(format!(
                "jset entry payload too large: {} > {}",
                payload.len(),
                u16::MAX
            ))
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

// ═══════════════════════════════════════════════════════════════
// 旧格式保留类型
// ═══════════════════════════════════════════════════════════════

/// JsetEntry 的类型（对齐 bcachefs `enum journal_entry_type`）
///
/// 值定义与 bcachefs 一致以保证格式兼容性：
/// - 0:  BtreeKeys — btree insert/delete keys
/// - 1:  BtreeRoot — root pointer update
/// - 4:  Blacklist — 标记已完成 journal flush 的 seq 范围（blacklist_v2）
/// - 5:  Usage — key version 等全局 usage 值
/// - 6:  DataUsage — legacy data usage
/// - 7:  Clock — IO clock
/// - 8:  DevUsage — legacy per-device usage
/// - 10: Overwrite — overwrite entry
/// - 11: WriteBufferKeys — write buffer deferred keys
/// - 12: Datetime — journal write wall clock
/// - 14: RewindLimit — 最旧可安全 rewind 的 journal seq
/// - 15: Rewind — rewind 进行中范围
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum JsetEntryType {
    /// btree insert/delete keys
    BtreeKeys = 0,
    /// root pointer update
    BtreeRoot = 1,
    /// blacklist 条目：标记已完成 journal flush 的 seq 范围（recovery 时跳过）
    Blacklist = 4,
    /// 全局 usage 值（本地 `BCH_JSET_ENTRY_usage`）
    Usage = 5,
    /// legacy data usage（本地 `BCH_JSET_ENTRY_data_usage`）
    DataUsage = 6,
    /// IO clock（本地 `BCH_JSET_ENTRY_clock`）
    Clock = 7,
    /// legacy per-device usage（本地 `BCH_JSET_ENTRY_dev_usage`）
    DevUsage = 8,
    /// overwrite entry：覆盖式写入（bcachefs BCH_JSET_ENTRY_overwrite）
    Overwrite = 10,
    /// write buffer 累积的 deferred btree keys（bcachefs BCH_JSET_ENTRY_write_buffer_keys）
    WriteBufferKeys = 11,
    /// journal write wall clock（本地 `BCH_JSET_ENTRY_datetime`）
    Datetime = 12,
    /// 最旧可安全 rewind 的 journal seq（bcachefs BCH_JSET_ENTRY_rewind_limit）
    RewindLimit = 14,
    /// rewind 进行中范围（bcachefs BCH_JSET_ENTRY_rewind）
    Rewind = 15,
}

impl JsetEntryType {
    /// 从 u8 转换到 JsetEntryType，未知值返回 None
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(JsetEntryType::BtreeKeys),
            1 => Some(JsetEntryType::BtreeRoot),
            4 => Some(JsetEntryType::Blacklist),
            5 => Some(JsetEntryType::Usage),
            6 => Some(JsetEntryType::DataUsage),
            7 => Some(JsetEntryType::Clock),
            8 => Some(JsetEntryType::DevUsage),
            10 => Some(JsetEntryType::Overwrite),
            11 => Some(JsetEntryType::WriteBufferKeys),
            12 => Some(JsetEntryType::Datetime),
            14 => Some(JsetEntryType::RewindLimit),
            15 => Some(JsetEntryType::Rewind),
            _ => None,
        }
    }
}

/// 分块 CRC32C 计算（对齐 bcachefs 的 crc32c 分块校验）
///
/// bcachefs 将 Jset 数据按 4KB block 分块后分别计算 CRC 再合并。
///
/// 使用 `crc::Digest` 的 `update()` 方法实现多块追加计算，
/// 与 bcachefs 的 `crc32c_le_bch()` 分块语义一致。
///
/// # 示例
///
/// ```text
/// let mut hasher = Crc32CHasher::new();
/// hasher.update(&block1);
/// hasher.update(&block2);
/// let result = hasher.finalize();
/// ```
pub struct Crc32CHasher {
    digest: crc::Digest<'static, u32>,
}

impl Crc32CHasher {
    /// 创建新的 CRC32C 计算器（初始值 0）
    pub fn new() -> Self {
        Self {
            digest: CRC32C.digest(),
        }
    }

    /// 追加数据块到 CRC 计算
    pub fn update(&mut self, data: &[u8]) {
        self.digest.update(data);
    }

    /// 完成 CRC 计算，返回最终的 32 位校验值
    pub fn finalize(&self) -> u32 {
        self.digest.clone().finalize()
    }

    /// 从单个数据块计算 CRC32C（自动选择硬件/软件路径）
    pub fn hash(data: &[u8]) -> u32 {
        crc32c(data, 0)
    }
}

impl Default for Crc32CHasher {
    fn default() -> Self {
        Self::new()
    }
}

/// Blacklist entry — 对应 bcachefs `struct jset_entry_blacklist`
///
/// journal flush 时将当前已落盘的 seq 范围写入 blacklist entries。
/// recovery 时 journal_read pass 跳过 blacklist 范围内的 seq。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlacklistEntry {
    /// blacklist 覆盖的最旧 seq
    pub start_seq: u64,
    /// blacklist 覆盖的最新 seq（exclusive）
    pub end_seq: u64,
}

// ═══════════════════════════════════════════════════════════════
// BlacklistTable — 运行时黑名单查询
// ═══════════════════════════════════════════════════════════════

/// BlacklistTableEntry — 对应 bcachefs `struct journal_seq_blacklist_table_entry`
///
/// 运行时黑名单表条目。`dirty` 用 `AtomicBool` 实现无锁标志位更新，
/// 对应 bcachefs 中无锁的 `t->entries[i].dirty` 写入。
/// bcachefs 中 dirty 标志位在同一对齐字节内用普通赋值即可生效（单字节写入原子性），
/// Rust 需要 `AtomicBool` 保证安全并发访问。
#[derive(Debug)]
pub struct BlacklistTableEntry {
    /// 黑名单起始 seq（inclusive）
    pub start: u64,
    /// 黑名单结束 seq（exclusive）
    pub end: u64,
    /// 是否在运行期间被命中（GC 时保留 dirty 条目）
    /// AtomicBool 实现无锁写入，对应 bcachefs 的直接字段赋值。
    pub dirty: AtomicBool,
}

/// BlacklistTable — 对应 bcachefs `struct journal_seq_blacklist_table`
///
/// 运行时黑名单表。entries 按 `start` 升序排列，支持二分查找。
/// 使用 `partition_point` 实现 O(log n) 查询，等价于 bcachefs 的
/// eytzinger0 树二分查找。
#[derive(Debug)]
pub struct BlacklistTable {
    /// 按 start 升序排列的黑名单条目
    entries: Vec<BlacklistTableEntry>,
}

impl BlacklistTable {
    /// 从 `&[BlacklistEntry]` 构建运行时黑名单表
    ///
    /// 对应 bcachefs `bch2_blacklist_table_initialize()` (seq_blacklist.c:189-219)。
    /// 将 journal 中读取的 BlacklistEntry 转换为运行时 BlacklistTableEntry，
    /// 并按 `start` 升序排列以便二分查找。
    pub fn from_entries(entries: &[BlacklistEntry]) -> Self {
        let mut tbl_entries: Vec<BlacklistTableEntry> = entries
            .iter()
            .map(|e| BlacklistTableEntry {
                start: e.start_seq,
                end: e.end_seq,
                dirty: AtomicBool::new(false),
            })
            .collect();
        // 对应 bcachefs eytzinger0_sort — 按 start 排序
        tbl_entries.sort_by_key(|e| e.start);
        Self {
            entries: tbl_entries,
        }
    }

    /// 检查 seq 是否在黑名单范围内
    ///
    /// 对应 bcachefs `bch2_journal_seq_is_blacklisted()` (seq_blacklist.c:152-177)。
    /// 使用 `partition_point` 二分查找，O(log n)。
    /// 若 `dirty=true` 且 seq 在黑名单中，用 `AtomicBool::store(Release)` 标记 dirty。
    /// bcachefs 中 dirty 标志位为普通 bool 写入（同一 cache line 字节内赋值在 x86 原子），
    /// 无锁。subvol 用 `AtomicBool` 匹配语义。
    pub fn is_blacklisted(&self, seq: u64, dirty: bool) -> bool {
        // partition_point 找第一个 start > seq 的位置，减 1 得 start <= seq 的最大条目
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

    /// 跳过黑名单范围：若 seq 在黑名单中则返回该范围的 end（下一个非黑名单 seq）
    ///
    /// 对应 bcachefs `bch2_journal_seq_next_nonblacklisted()` (seq_blacklist.c:132-150)。
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

    /// 找到大于等于 seq 的下一个黑名单 entry 的 start
    ///
    /// 对应 bcachefs `bch2_journal_seq_next_blacklisted()` (seq_blacklist.c:114-130)。
    /// 返回 `max(seq, 第一个 end > seq 的 entry.start)`。
    /// 无匹配则返回 `u64::MAX`。
    pub fn next_blacklisted(&self, seq: u64) -> u64 {
        // partition_point 找第一个 end > seq 的 entry
        let idx = self.entries.partition_point(|e| e.end <= seq);
        if idx >= self.entries.len() {
            return u64::MAX;
        }
        std::cmp::max(seq, self.entries[idx].start)
    }

    /// 返回最后一个黑名单条目的 `end - 1`（最大黑名单 seq）
    ///
    /// 对应 bcachefs `bch2_journal_last_blacklisted_seq()` (seq_blacklist.c:179-187)。
    /// 无条目返回 0。
    pub fn last_blacklisted_seq(&self) -> u64 {
        self.entries
            .last()
            .map_or(0, |e| if e.end > 0 { e.end - 1 } else { 0 })
    }

    /// 垃圾回收过期黑名单条目（只读）
    ///
    /// 对应 bcachefs `bch2_blacklist_entries_gc()` (seq_blacklist.c:276-311)。
    /// bcachefs 的 gc 从运行时表读取 dirty 标志，写入 superblock；
    /// **不修改运行时表本身**。subvol 对齐为只读检查，返回 `true`
    /// 表示有可被 gc 的条目（需要调用方处理 superblock 写入）。
    pub fn gc(&self, oldest_seq: u64) -> bool {
        self.entries
            .iter()
            .any(|e| !e.dirty.load(Ordering::Acquire) && e.end < oldest_seq)
    }
}

// ═══════════════════════════════════════════════════════════════
// Jset — 高层封装
// ═══════════════════════════════════════════════════════════════

/// Journal entry — 对应 bcachefs `struct jset`
///
/// 一次提交的所有 btree 修改被打包成一个 Jset 写入 journal。
/// v2 格式使用 repr(C) 固定布局序列化。
#[derive(Debug, Clone)]
pub struct Jset {
    /// Jset header（repr(C)，64 字节固定）
    pub header: JsetHeader,
    /// 本 Jset 包含的 entries
    pub entries: Vec<RawJsetEntry>,
}

impl Jset {
    /// 创建新的 Jset（使用原始魔数）
    pub fn new(seq: u64, last_seq: u64) -> Self {
        Self {
            header: JsetHeader {
                magic: JOURNAL_MAGIC,
                seq,
                last_seq,
                crc32: 0,
                entry_count: 0,
                version: JSET_VERSION as u32,
                flags: CSUM_TYPE_NONE as u32,
                pad: [0u8; 24],
            },
            entries: Vec::new(),
        }
    }

    /// 创建新的 Jset（使用 volatile 魔数 VMNT_JSET_MAGIC）
    pub fn new_volatile(seq: u64, last_seq: u64) -> Self {
        Self {
            header: JsetHeader {
                magic: VMNT_JSET_MAGIC,
                seq,
                last_seq,
                crc32: 0,
                entry_count: 0,
                version: JSET_VERSION as u32,
                flags: CSUM_TYPE_NONE as u32,
                pad: [0u8; 24],
            },
            entries: Vec::new(),
        }
    }

    /// 计算序列化数据的字节数（不含 padding）
    fn data_size(&self) -> usize {
        let mut sz = size_of::<JsetHeader>();
        for entry in &self.entries {
            sz += size_of::<JsetEntryHeader>();
            sz += entry.payload.len();
        }
        sz
    }

    /// 返回序列化后按 `JSET_BLOCK_SIZE` 填充的字节数。
    ///
    /// append 路径用它预估 journal reservation，避免为了计算大小先构造一份完整 buffer。
    pub fn serialized_padded_len(&self) -> usize {
        let data_size = self.data_size();
        let block_size = JSET_BLOCK_SIZE as usize;
        let pad = (block_size - (data_size % block_size)) % block_size;
        data_size + pad
    }

    fn crc32_over_entries(&self) -> u32 {
        let mut header_zero = self.header;
        header_zero.crc32 = 0;
        let header_bytes = unsafe {
            std::slice::from_raw_parts(
                &header_zero as *const JsetHeader as *const u8,
                size_of::<JsetHeader>(),
            )
        };
        let mut crc = crc32c(header_bytes, 0);

        for entry in &self.entries {
            let entry_bytes = unsafe {
                std::slice::from_raw_parts(
                    &entry.hdr as *const JsetEntryHeader as *const u8,
                    size_of::<JsetEntryHeader>(),
                )
            };
            crc = crc32c(entry_bytes, crc);
            if !entry.payload.is_empty() {
                crc = crc32c(&entry.payload, crc);
            }
        }

        crc
    }

    /// 验证 Jset 的 CRC32 和 magic
    ///
    /// CRC32C 覆盖完整 Jset header（crc32 字段置 0）+ 所有 entries。
    /// 支持两种魔数：JOURNAL_MAGIC（原始格式）和 VMNT_JSET_MAGIC（volatile 格式）。
    pub fn verify(&self) -> bool {
        if self.header.magic != JOURNAL_MAGIC && self.header.magic != VMNT_JSET_MAGIC {
            return false;
        }

        self.crc32_over_entries() == self.header.crc32
    }

    /// 序列化 + CRC32 计算（覆盖完整 header + entries）+ padding 到 JSET_BLOCK_SIZE
    ///
    /// 1. 分配 buf，写 header（crc32=0）+ entries
    /// 2. crc32c(0, &buf[..data_end]) 计算完整数据的 CRC
    /// 3. 写 crc 回 header
    /// 4. 零填充到 JSET_BLOCK_SIZE
    pub fn serialize_padded(&self) -> Result<Vec<u8>, StorageError> {
        let data_size = self.data_size();
        let total_size = self.serialized_padded_len();

        let mut buf = vec![0u8; total_size];

        // 写 header（crc32=0）
        let mut header = self.header;
        header.crc32 = 0;
        header.entry_count = self.entries.len() as u32;

        unsafe {
            let ptr = buf.as_mut_ptr();
            ptr::copy_nonoverlapping(
                &header as *const JsetHeader as *const u8,
                ptr,
                size_of::<JsetHeader>(),
            );

            let mut off = size_of::<JsetHeader>();
            for entry in &self.entries {
                ptr::copy_nonoverlapping(
                    &entry.hdr as *const JsetEntryHeader as *const u8,
                    ptr.add(off),
                    size_of::<JsetEntryHeader>(),
                );
                off += size_of::<JsetEntryHeader>();

                if !entry.payload.is_empty() {
                    ptr::copy_nonoverlapping(
                        entry.payload.as_ptr(),
                        ptr.add(off),
                        entry.payload.len(),
                    );
                    off += entry.payload.len();
                }
            }
            debug_assert_eq!(off, data_size);
        }

        // 计算 CRC（覆盖 header + entries，crc32 字段已置 0）
        let crc = crc32c(&buf[..data_size], 0);

        // 写 CRC 回 header 的 crc32 字段。Vec<u8> 只保证 1 字节对齐，必须使用 unaligned write。
        unsafe {
            ptr::write_unaligned(
                buf.as_mut_ptr().add(JSET_HEADER_CRC32_OFFSET).cast::<u32>(),
                crc,
            );
        }

        Ok(buf)
    }

    /// 从字节反序列化 Jset
    ///
    /// 格式检测逻辑：
    /// 1. 读取 magic 字段，不匹配则返回 None
    /// 2. 读取 version 字段（以 v2 JsetHeader offset=32 的 u32 值）：
    ///    - 如果 2 ≤ version ≤ JSET_VERSION → 以 v2 repr(C) 固定布局读取
    ///    - 如果 version 超出范围（含旧 v1 bincode 格式，其 version 字段是 u16 + csum_type，
    ///      读取为 u32 后可能 > JSET_VERSION）→ 尝试 bincode 回退
    /// 3. v1 bincode 失败 → 返回 None
    pub fn deserialize(data: &[u8]) -> Result<Option<Self>, StorageError> {
        if data.len() < size_of::<JsetHeader>() {
            return Ok(None);
        }

        // 输入 &[u8] 不保证 JsetHeader 对齐，必须使用 read_unaligned。
        let header: JsetHeader = unsafe { ptr::read_unaligned(data.as_ptr().cast::<JsetHeader>()) };

        if header.magic != JOURNAL_MAGIC && header.magic != VMNT_JSET_MAGIC {
            return Ok(None);
        }

        // v2+ 固定布局：version 字段在 JsetHeader 的 u32 偏移位置，
        // 且值必须在 [2, JSET_VERSION] 范围内
        if (2..=JSET_VERSION).contains(&header.version) {
            return Self::parse_v2(data, &header);
        }

        // version > JSET_VERSION → 无法识别的格式
        Ok(None)
    }

    /// 以 v2+ repr(C) 固定布局读取 Jset。
    ///
    /// `header` 必须来自 `ptr::read_unaligned` 读取的有效数据，
    /// 且已确认 `header.version` 在 2..=JSET_VERSION 范围内。
    fn parse_v2(data: &[u8], header: &JsetHeader) -> Result<Option<Self>, StorageError> {
        let entry_count = header.entry_count as usize;

        let mut entries = Vec::with_capacity(entry_count);
        let mut off = size_of::<JsetHeader>();

        for _ in 0..entry_count {
            if off + size_of::<JsetEntryHeader>() > data.len() {
                return Ok(None);
            }

            let entry_hdr: JsetEntryHeader =
                unsafe { ptr::read_unaligned(data.as_ptr().add(off).cast::<JsetEntryHeader>()) };
            off += size_of::<JsetEntryHeader>();

            if entry_hdr.version > JSET_ENTRY_VERSION {
                return Ok(None);
            }

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

        Ok(Some(Jset {
            header: *header,
            entries,
        }))
    }
}

// ═══════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btree::key::{Bpos, BtreeEntry, KeyType, KeyValue};

    fn make_test_jset() -> Jset {
        let payload = bincode::serialize(&vec![BtreeEntry::new(
            Bpos::new(1, 100, 0),
            KeyType::Normal,
            KeyValue::extent(0x1000, 1, 0),
        )])
        .unwrap();
        let entry = RawJsetEntry::new(0, JsetEntryType::BtreeKeys as u8, payload, 0).unwrap();
        let mut jset = Jset::new(1, 0);
        jset.entries.push(entry);
        jset.header.entry_count = 1;
        jset
    }

    #[test]
    fn test_jset_roundtrip() {
        let jset = make_test_jset();
        let data = jset.serialize_padded().unwrap();

        // 验证 padding
        assert_eq!(data.len() % JSET_BLOCK_SIZE as usize, 0);

        let restored = Jset::deserialize(&data).unwrap().unwrap();
        assert_eq!(restored.header.magic, JOURNAL_MAGIC);
        assert_eq!(restored.header.seq, 1);
        assert_eq!(restored.header.entry_count, 1);
        assert_eq!(restored.entries.len(), 1);
        assert_eq!(restored.entries[0].hdr.btree_type, 0);
        assert_eq!(
            restored.entries[0].hdr.entry_type,
            JsetEntryType::BtreeKeys as u8
        );
        assert_eq!(restored.entries[0].hdr.version, JSET_ENTRY_VERSION);

        // 验证 CRC32（非零）
        assert_ne!(restored.header.crc32, 0);
        assert!(restored.verify());
    }

    #[test]
    fn test_jset_crc32_verify() {
        let jset = make_test_jset();
        let data = jset.serialize_padded().unwrap();
        let restored = Jset::deserialize(&data).unwrap().unwrap();

        // 正常情况：通过
        assert!(restored.verify());

        // 篡改 crc32 字段 → 不匹配
        let mut tampered = restored.clone();
        tampered.header.crc32 = 0xDEAD_BEEF;
        assert!(!tampered.verify());

        // 篡改 header 字段（seq）→ 全 Jset CRC 覆盖检测到
        let mut tampered_seq = restored.clone();
        tampered_seq.header.seq = 999;
        assert!(!tampered_seq.verify());

        // 篡改 header 字段（last_seq）
        let mut tampered_ls = restored.clone();
        tampered_ls.header.last_seq = 999;
        assert!(!tampered_ls.verify());

        // 篡改 magic
        let mut tampered_magic = restored.clone();
        tampered_magic.header.magic = [0; 8];
        // magic 不匹配在 verify 入口直接返回 false，不经过 CRC 检查
        assert!(!tampered_magic.verify());
    }

    #[test]
    fn test_jset_invalid_magic() {
        let data = vec![0u8; JSET_BLOCK_SIZE as usize];
        let result = Jset::deserialize(&data).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_jset_empty_entries() {
        let jset = Jset::new(42, 10);
        let data = jset.serialize_padded().unwrap();
        let restored = Jset::deserialize(&data).unwrap().unwrap();
        assert_eq!(restored.header.seq, 42);
        assert_eq!(restored.header.last_seq, 10);
        assert!(restored.entries.is_empty());
        assert!(restored.verify());
    }

    #[test]
    fn test_jset_header_size() {
        // 验证 JsetHeader 是精确 64 字节
        assert_eq!(size_of::<JsetHeader>(), 64);
    }

    #[test]
    fn test_jset_entry_header_size() {
        // 验证 JsetEntryHeader 是精确 8 字节
        assert_eq!(size_of::<JsetEntryHeader>(), 8);
    }

    #[test]
    fn test_jset_entry_unknown_version_rejected() {
        let jset = make_test_jset();
        let mut data = jset.serialize_padded().unwrap();
        let entry_version_offset = size_of::<JsetHeader>() + 2;
        data[entry_version_offset] = JSET_ENTRY_VERSION + 1;
        assert!(Jset::deserialize(&data).unwrap().is_none());
    }

    #[test]
    fn test_jset_entry_type_from_u8() {
        assert_eq!(JsetEntryType::from_u8(0), Some(JsetEntryType::BtreeKeys));
        assert_eq!(JsetEntryType::from_u8(1), Some(JsetEntryType::BtreeRoot));
        assert_eq!(JsetEntryType::from_u8(4), Some(JsetEntryType::Blacklist));
        assert_eq!(JsetEntryType::from_u8(10), Some(JsetEntryType::Overwrite));
        assert_eq!(
            JsetEntryType::from_u8(11),
            Some(JsetEntryType::WriteBufferKeys)
        );
        assert_eq!(JsetEntryType::from_u8(12), Some(JsetEntryType::Datetime));
        assert_eq!(JsetEntryType::from_u8(99), None);
    }

    #[test]
    fn test_raw_jset_entry_new() {
        let payload = vec![1, 2, 3, 4];
        let entry = RawJsetEntry::new(5, 1, payload.clone(), 0).unwrap();
        assert_eq!(entry.hdr.btree_type, 5);
        assert_eq!(entry.hdr.entry_type, 1);
        assert_eq!(entry.hdr.version, JSET_ENTRY_VERSION);
        assert_eq!(entry.hdr.level, 0);
        assert_eq!(entry.hdr.payload_len, 4);
        assert_eq!(entry.payload, payload);
    }

    #[test]
    fn test_raw_jset_entry_rejects_payload_len_overflow() {
        let payload = vec![0u8; u16::MAX as usize + 1];
        assert!(RawJsetEntry::new(0, JsetEntryType::BtreeKeys as u8, payload, 0).is_err());
    }

    // ─── CRC32C 向量测试 ─────────────────────────────────────

    /// Castagnoli CRC-32C 标准验证向量
    const CRC32C_CHECK_VALUE: u32 = 0xE3069283;

    #[test]
    fn test_crc32c_known_vector() {
        // CRC-32C 标准验证："123456789" -> 0xE3069283
        let data = b"123456789";
        assert_eq!(crc32c_sw(data, 0), CRC32C_CHECK_VALUE);
        assert_eq!(crc32c(data, 0), CRC32C_CHECK_VALUE);
        assert_eq!(Crc32CHasher::hash(data), CRC32C_CHECK_VALUE);
    }

    #[test]
    fn test_crc32c_empty() {
        assert_eq!(crc32c_sw(b"", 0), 0);
        assert_eq!(crc32c(b"", 0), 0);
    }

    #[test]
    fn test_crc32c_chaining() {
        // 分块计算应等于一次性计算
        let large = b"Hello, World! This is a test of CRC32C chaining across multiple blocks.";
        let full = crc32c_sw(large, 0);

        // 分两块
        let mid = large.len() / 2;
        let c1 = crc32c_sw(&large[..mid], 0);
        let c2 = crc32c_sw(&large[mid..], c1);
        assert_eq!(c2, full, "chained CRC must match single-pass");

        // 分三块
        let third = large.len() / 3;
        let c1 = crc32c_sw(&large[..third], 0);
        let c2 = crc32c_sw(&large[third..2 * third], c1);
        let c3 = crc32c_sw(&large[2 * third..], c2);
        assert_eq!(c3, full, "three-chunk CRC must match single-pass");
    }

    #[test]
    fn test_crc32c_hw_sw_consistent() {
        // 软件和硬件路径（如果 SSE4.2 可用）结果一致
        let data = b"Consistency test data for CRC32C hardware and software paths.";
        let sw = crc32c_sw(data, 0);

        #[cfg(all(target_arch = "x86_64", target_feature = "sse4.2"))]
        {
            let hw = unsafe { super::crc32c_hw_impl(data, 0) };
            assert_eq!(hw, sw, "hardware CRC must match software CRC");
        }

        let dispatch = crc32c(data, 0);
        assert_eq!(dispatch, sw, "auto-dispatch CRC must match software CRC");
    }

    #[test]
    fn test_crc32c_nonzero_seed() {
        // 非零 seed 测试
        let seed = 0xDEADBEEFu32;
        let data = b"non-zero seed test";
        // 链式调用：先用 seed 作初始值计算
        let result = crc32c_sw(data, seed);
        // 重新计算验证
        let recheck = crc32c_sw(data, seed);
        assert_eq!(result, recheck, "CRC with same seed must be deterministic");
    }

    #[test]
    fn test_jset_serialize_verify_deserialize_multiple_entries() {
        // 多个 entry 的完整 roundtrip
        let payload1 = bincode::serialize(&vec![BtreeEntry::new(
            Bpos::new(1, 100, 0),
            KeyType::Normal,
            KeyValue::extent(0x1000, 1, 0),
        )])
        .unwrap();
        let payload2 = bincode::serialize(&vec![BtreeEntry::new(
            Bpos::new(2, 200, 0),
            KeyType::Normal,
            KeyValue::Raw(vec![10, 20]),
        )])
        .unwrap();

        let mut jset = Jset::new_volatile(42, 10);
        jset.header.flags = CSUM_TYPE_CRC32C as u32;
        jset.entries
            .push(RawJsetEntry::new(0, JsetEntryType::BtreeKeys as u8, payload1, 0).unwrap());
        jset.entries
            .push(RawJsetEntry::new(1, JsetEntryType::BtreeRoot as u8, payload2, 0).unwrap());
        jset.header.entry_count = 2;

        let data = jset.serialize_padded().unwrap();
        assert_eq!(data.len() % JSET_BLOCK_SIZE as usize, 0);

        let restored = Jset::deserialize(&data).unwrap().unwrap();
        assert_eq!(restored.header.seq, 42);
        assert_eq!(restored.header.last_seq, 10);
        assert_eq!(restored.header.entry_count, 2);
        assert_eq!(restored.entries.len(), 2);
        assert_eq!(restored.entries[0].hdr.btree_type, 0);
        assert_eq!(
            restored.entries[0].hdr.entry_type,
            JsetEntryType::BtreeKeys as u8
        );
        assert_eq!(restored.entries[1].hdr.btree_type, 1);
        assert_eq!(
            restored.entries[1].hdr.entry_type,
            JsetEntryType::BtreeRoot as u8
        );

        assert!(restored.verify());
    }

    #[test]
    fn test_jset_volatile_magic() {
        let mut jset = Jset::new_volatile(1, 0);
        let payload = bincode::serialize(&vec![BtreeEntry::new(
            Bpos::new(1, 100, 0),
            KeyType::Normal,
            KeyValue::extent(0x1000, 1, 0),
        )])
        .unwrap();
        jset.entries
            .push(RawJsetEntry::new(0, JsetEntryType::BtreeKeys as u8, payload, 0).unwrap());
        jset.header.entry_count = 1;

        let data = jset.serialize_padded().unwrap();
        let restored = Jset::deserialize(&data).unwrap().unwrap();
        assert_eq!(restored.header.magic, VMNT_JSET_MAGIC);
        assert!(restored.verify());
    }

    // ─── BlacklistTable 测试 ───────────────────────────────────

    fn make_table(ranges: &[(u64, u64)]) -> BlacklistTable {
        let entries: Vec<BlacklistEntry> = ranges
            .iter()
            .map(|&(s, e)| BlacklistEntry {
                start_seq: s,
                end_seq: e,
            })
            .collect();
        BlacklistTable::from_entries(&entries)
    }

    #[test]
    fn test_is_blacklisted() {
        let table = make_table(&[(10, 20), (30, 40)]);
        assert!(!table.is_blacklisted(5, false));
        assert!(table.is_blacklisted(10, false));
        assert!(table.is_blacklisted(15, false));
        assert!(!table.is_blacklisted(20, false));
        assert!(!table.is_blacklisted(25, false));
        assert!(table.is_blacklisted(30, false));
        assert!(!table.is_blacklisted(40, false));
    }

    #[test]
    fn test_is_blacklisted_dirty() {
        let table = make_table(&[(10, 20)]);
        assert!(table.is_blacklisted(15, true));
        assert!(
            table.entries[0].dirty.load(Ordering::Relaxed),
            "应标记 dirty"
        );
    }

    #[test]
    fn test_next_nonblacklisted() {
        let table = make_table(&[(10, 20), (30, 40)]);
        assert_eq!(table.next_nonblacklisted(5), 5);
        assert_eq!(table.next_nonblacklisted(10), 20);
        assert_eq!(table.next_nonblacklisted(15), 20);
        assert_eq!(table.next_nonblacklisted(25), 25);
        assert_eq!(table.next_nonblacklisted(30), 40);
        assert_eq!(table.next_nonblacklisted(50), 50);
    }

    #[test]
    fn test_next_blacklisted() {
        let table = make_table(&[(10, 20), (30, 40)]);
        assert_eq!(table.next_blacklisted(0), 10);
        assert_eq!(table.next_blacklisted(10), 10); // seq 恰在 start 上
        assert_eq!(table.next_blacklisted(15), 15); // seq 在范围内，返回 seq 本身
        assert_eq!(table.next_blacklisted(20), 30); // seq 恰在 end 边界，跳到下一范围
        assert_eq!(table.next_blacklisted(50), u64::MAX);
    }

    #[test]
    fn test_last_blacklisted_seq() {
        let table = make_table(&[(10, 20), (30, 40)]);
        assert_eq!(table.last_blacklisted_seq(), 39); // end=40 → 40-1=39
    }

    #[test]
    fn test_empty_table() {
        let table = BlacklistTable::from_entries(&[]);
        assert_eq!(table.last_blacklisted_seq(), 0);
        assert_eq!(table.next_blacklisted(0), u64::MAX);
        assert_eq!(table.next_nonblacklisted(100), 100);
    }

    #[test]
    fn test_blacklist_gc() {
        let table = make_table(&[(10, 20), (30, 40), (50, 60)]);
        table.entries[1].dirty.store(true, Ordering::Relaxed); // (30,40) dirty, 应保留
                                                               // oldest_seq=45:
                                                               //   (10,20): dirty=false, end=20 < 45 → 可删除
                                                               //   (30,40): dirty=true → 保留
                                                               //   (50,60): end=60 >= 45 → 保留
                                                               // gc 是只读检查，不修改表
        let changed = table.gc(45);
        assert!(changed, "存在可 gc 条目");
        assert_eq!(table.entries.len(), 3); // gc 不修改运行时表
    }
}
