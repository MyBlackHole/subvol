//! B-tree node with 256KB buffer, 3 bsets, packed variable-length entries
//!
//! ## 存储格式
//!
//! Data region 存储 bset header 与 packed entries：
//! ```text
//! [BsetHeader(24B)] [BkeyPacked header(3B)] [packed key fields + value bytes]
//! ```
//! 每个 entry 的大小由 `entry.u64s * 8` 决定。
//! 辅助搜索树存储在独立的 `aux_data` buffer 中。

use crate::bch_vol::BchVol;
use crate::block_device::BchDev;
#[cfg(test)]
use crate::btree::key::KeyValue;
use crate::btree::key::BtreeEntry;
use crate::btree::key::{
    bch2_bkey_pack_pos_lossy, bkey_cmp_packed, bkey_cmp_packed_vs_bpos, bkey_pack_extent,
    bkey_pack_raw, bkey_unpack, bkey_unpack_bytes, bkey_unpack_extent, entry_packed_size, BchVal,
    BkeyFormat, BkeyPackPosRet, BkeyPacked, Bpos, BtreeKey, ExtentValue, KeyType,
    BKEY_FORMAT_CURRENT, KEY_FORMAT_CURRENT,
};
use crate::journal::crc32c;
use crate::journal::reclaim::JournalEntryPin;
use crate::journal::Journal;
use crate::lock::six::SixLock;
use crate::types::StorageError;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering as CmpOrdering;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};

pub const DEFAULT_NODE_SIZE: u32 = 256 * 1024;
pub const BSET_COUNT: usize = 3;
/// bcachefs 对齐: `MAX_BSETS`（对应 `#define MAX_BSETS 3U`）
pub const MAX_BSETS: usize = BSET_COUNT;
pub const BSET_HEADER_U64S: u16 =
    (std::mem::size_of::<BsetHeader>() / std::mem::size_of::<u64>()) as u16;

/// BtreeNodeHeader magic
pub const BTREE_NODE_MAGIC: u32 = 0x56544E42; // "BNTV" (BTree Node Volume)
/// BtreeNodeDiskEntry magic（未来用于 log-structured append）
pub const BTREE_NODE_ENTRY_MAGIC: u32 = 0x45544E42; // "BNTE" (BTree Node Entry)
/// 当前 node record 格式；旧格式不提供 fallback。
pub const BTREE_NODE_VERSION: u16 = 3;
/// 1 block = 4KB（I/O 最小单元）
pub const BLOCK_SIZE: usize = 4096;
/// 持久 pointer 与 BSET_OFFSET 使用 512-byte sector（源自中央常量）。
pub(crate) const SECTOR_SIZE: usize = crate::types::SECTOR_SIZE as usize;
pub(crate) const SECTORS_PER_BLOCK: u16 = (BLOCK_SIZE / SECTOR_SIZE) as u16;

/// 磁盘 bset header — 对齐 bcachefs `struct bset`
///
/// bcachefs C: `struct bset { __le64 seq; __le64 journal_seq; __le32 flags;
///                            __le16 version; __le16 u64s; } __packed __aligned(8)`
///
/// 每个 bset（sorted key 集合）前面的 header：
/// - `seq`: 最近一次写入此 bset 的 journal seq（恢复排序用）
/// - `journal_seq`: 此 bset 被写入时的 journal seq
/// - `flags`: BSET_* 标志位（csum type、offset 等）
/// - `version`: bset 格式版本
/// - `u64s`: 整个 bset 的 u64 数（含 header）。data = u64s * 8 - sizeof(BsetHeader)
///
/// 注意：当前 subvol 每个节点只有一个活跃 bset（bset_count=1），
///       后续增量更新会使用更多 bset（对应 bcachefs 的 3 级 bset 结构）。
#[derive(Debug, Clone, Copy)]
#[repr(C, packed)]
pub struct BsetHeader {
    /// journal seq of most recent write to this bset
    pub seq: u64, // 0-7
    /// journal seq when this bset was written
    pub journal_seq: u64, // 8-15
    /// BSET_* flags (csum type offset, etc.)
    pub flags: u32, // 16-19
    /// format version
    pub version: u16, // 20-21
    /// packed key payload size in u64s（不含 BsetHeader）
    pub u64s: u16, // 22-23
} // 24 bytes — 精确对齐 bcachefs struct bset

impl BsetHeader {
    /// bset 中 packed entries 的数据字节数
    pub fn data_bytes(&self) -> usize {
        (self.u64s as usize) * 8
    }
}

/// 磁盘 btree node header（写入每个 bucket 第一块）
/// `#[repr(C, packed)]` — 固定 88 字节布局
///
/// bcachefs C 的 `struct btree_node` 是 152 字节（含 inline bset、format、ptr）。
/// subvol 用更精简的 88 字节头部，因为：
/// - 独立的 BsetHeader（24B）放在每个 bset 前（而非 inline）
/// - 缺省 bkey_format 从 BKEY_FORMAT_CURRENT 获取
/// - ptr 信息从 bucket_addr 和 level 隐含
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[repr(C, packed)]
pub struct BtreeNodeHeader {
    pub magic: u32,        // 0-3    magic number
    pub version: u16,      // 4-5    格式版本 (v3 = record log)
    pub level: u8,         // 6      btree level
    pub node_type: u8,     // 7      BtreeId as u8
    pub packed_keys: u16,  // 8-9    压缩格式的键数量
    pub unpacked_keys: u16,// 10-11  解压格式的键数量
    pub bset_count: u16,   // 12-13  bset headers 数量
    pub crc32: u32,        // 14-17  CRC over header + bsets + entries
    pub seq: u64,          // 18-25  journal seq
    pub bucket_addr: u64,  // 26-33  extent 起始 block 地址
    pub generation: u32,   // 34-37  replacement generation
    pub record_bytes: u32, // 38-41 block-aligned record 长度

    // Bpos 拆为内联字段 → packed 上下文中精确对齐，无 padding
    pub min_key_inode: u64,    // 42-49  subtree min key — inode
    pub min_key_offset: u64,   // 50-57  subtree min key — offset
    pub min_key_snapshot: u32, // 58-61  subtree min key — snapshot

    pub max_key_inode: u64,    // 62-69  subtree max key — inode
    pub max_key_offset: u64,   // 70-77  subtree max key — offset
    pub max_key_snapshot: u32, // 78-81  subtree max key — snapshot
    pub _pad: [u8; 6],         // 82-87  使后续 BsetHeader 8-byte 对齐
} // 88 bytes

impl BtreeNodeHeader {
    pub fn min_key(&self) -> Bpos {
        Bpos::new(
            self.min_key_inode,
            self.min_key_offset,
            self.min_key_snapshot,
        )
    }
    pub fn set_min_key(&mut self, key: Bpos) {
        self.min_key_inode = key.inode;
        self.min_key_offset = key.offset;
        self.min_key_snapshot = key.snapshot;
    }
    pub fn max_key(&self) -> Bpos {
        Bpos::new(
            self.max_key_inode,
            self.max_key_offset,
            self.max_key_snapshot,
        )
    }
    pub fn set_max_key(&mut self, key: Bpos) {
        self.max_key_inode = key.inode;
        self.max_key_offset = key.offset;
        self.max_key_snapshot = key.snapshot;
    }
}

/// 磁盘 btree node append record header。
///
/// 对应 bcachefs `struct btree_node_entry` 的 checksum + bset 角色；额外的
/// magic/generation/record_bytes 用于 Rust 后端做严格边界验证。
#[derive(Debug, Clone, Copy)]
#[repr(C, packed)]
pub struct BtreeNodeDiskEntry {
    pub magic: u32,
    pub version: u16,
    pub _flags: u16,
    pub seq: u64,
    pub generation: u32,
    pub record_bytes: u32,
    pub crc32: u32,
    pub _pad: u32,
} // 32 bytes

// 备注：bcachefs 对齐: 辅助搜索树类型（对应 `enum bset_aux_tree_type`）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum BsetAuxTreeType {
    NoAuxTree = 0,
    RoAuxTree = 1,
    RwAuxTree = 2,
}

/// bcachefs 对齐: `BSET_NO_AUX_TREE_VAL` / `BSET_RW_AUX_TREE_VAL`
pub const BSET_NO_AUX_TREE_VAL: u16 = u16::MAX;
pub const BSET_RW_AUX_TREE_VAL: u16 = u16::MAX - 1;

/// bcachefs 对齐: `BSET_CACHELINE` — 每 BSET_CACHELINE 字节对应一个 aux tree 节点
pub const BSET_CACHELINE: u32 = 256;

const L1_CACHE_BYTES: u32 = 64;
const BFLOAT_FAILED: u8 = u8::MAX;
const BKEY_MANTISSA_BITS: u32 = 16;

#[derive(Debug, Clone, Copy, Default)]
#[repr(C, packed)]
struct BkeyFloat {
    exponent: u8,
    key_offset: u8,
    mantissa: u16,
}

fn eytzinger1_first(size: usize) -> usize {
    if size != 0 {
        1usize << (usize::BITS - 1 - size.leading_zeros())
    } else {
        0
    }
}

fn eytzinger1_next(mut i: usize, size: usize) -> usize {
    if i * 2 + 1 <= size {
        i = i * 2 + 1;
        i <<= (usize::BITS - 1 - size.leading_zeros()) - (usize::BITS - 1 - i.leading_zeros());
        i >>= usize::from(i > size);
    } else {
        i >>= i.trailing_ones() + 1;
    }
    i
}

fn eytzinger1_prev(mut i: usize, size: usize) -> usize {
    if i * 2 <= size {
        i = i * 2 + 1;
        i <<= (usize::BITS - 1 - size.leading_zeros()) - (usize::BITS - 1 - i.leading_zeros());
        i -= 1;
        i >>= usize::from(i > size);
    } else {
        i >>= i.trailing_zeros() + 1;
    }
    i
}

fn eytzinger1_extra(size: usize) -> usize {
    if size != 0 {
        (size + 1 - (1usize << (usize::BITS - 1 - size.leading_zeros()))) << 1
    } else {
        0
    }
}

fn __eytzinger1_to_inorder(mut i: usize, size: usize, extra: usize) -> usize {
    let b = usize::BITS - 1 - i.leading_zeros();
    let shift = usize::BITS - 1 - size.leading_zeros() - b;
    i ^= 1usize << b;
    i <<= 1;
    i |= 1;
    i <<= shift;
    if i > extra {
        i -= (i - extra) >> 1;
    }
    i
}

#[inline]
fn prefetch_four_cachelines(ptr: *const u8) {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        use std::arch::x86_64::{_mm_prefetch, _MM_HINT_T0};
        _mm_prefetch(ptr.cast(), _MM_HINT_T0);
        _mm_prefetch(
            ptr.wrapping_add(L1_CACHE_BYTES as usize).cast(),
            _MM_HINT_T0,
        );
        _mm_prefetch(
            ptr.wrapping_add((L1_CACHE_BYTES * 2) as usize).cast(),
            _MM_HINT_T0,
        );
        _mm_prefetch(
            ptr.wrapping_add((L1_CACHE_BYTES * 3) as usize).cast(),
            _MM_HINT_T0,
        );
    }

    #[cfg(not(target_arch = "x86_64"))]
    let _ = ptr;
}

/// bcachefs `struct rw_aux_tree` on little-endian hosts.
#[derive(Debug, Clone, Copy)]
#[repr(C, packed(4))]
struct RwAuxTree {
    offset: u16,
    _pad: u16,
    snapshot: u32,
    key_offset: u64,
    inode: u64,
}

impl RwAuxTree {
    const ZERO: Self = Self {
        offset: 0,
        _pad: 0,
        snapshot: 0,
        key_offset: 0,
        inode: 0,
    };

    fn pos(self) -> Bpos {
        Bpos {
            inode: self.inode,
            offset: self.key_offset,
            snapshot: self.snapshot,
        }
    }

    fn set_pos(&mut self, pos: Bpos) {
        self.snapshot = pos.snapshot;
        self.key_offset = pos.offset;
        self.inode = pos.inode;
    }
}

#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct BsetTree {
    pub size: u16,
    pub extra: u16,
    pub data_offset: u16,
    pub aux_data_offset: u16,
    pub end_offset: u16,
}

impl BsetTree {
    pub fn data_bytes(&self) -> u32 {
        u32::from(
            self.end_offset
                .saturating_sub(self.data_offset + BSET_HEADER_U64S),
        ) * 8
    }

    /// bcachefs 对齐: `bset_aux_tree_type()` — 获取辅助树类型
    ///
    /// 区分三种情况：
    /// - `BSET_NO_AUX_TREE_VAL` → `NoAuxTree`
    /// - `BSET_RW_AUX_TREE_VAL` → `RwAuxTree`（inc set 有可写 aux tree）
    /// - 其他 extra 值 → `RoAuxTree`
    pub fn aux_tree_type(&self) -> BsetAuxTreeType {
        match self.extra {
            BSET_NO_AUX_TREE_VAL => {
                debug_assert_eq!(self.size, 0);
                BsetAuxTreeType::NoAuxTree
            }
            BSET_RW_AUX_TREE_VAL => {
                debug_assert_ne!(self.size, 0);
                BsetAuxTreeType::RwAuxTree
            }
            _ => {
                debug_assert_ne!(self.size, 0);
                BsetAuxTreeType::RoAuxTree
            }
        }
    }

    /// bcachefs 对齐: `bset_has_ro_aux_tree()`
    pub fn has_ro_aux_tree(&self) -> bool {
        self.aux_tree_type() == BsetAuxTreeType::RoAuxTree
    }

    /// bcachefs 对齐: `bset_has_rw_aux_tree()`
    pub fn has_rw_aux_tree(&self) -> bool {
        self.aux_tree_type() == BsetAuxTreeType::RwAuxTree
    }

    /// bcachefs `btree_bkey_first_offset()`，单位为 u64。
    pub fn first_key_offset(&self) -> u16 {
        self.data_offset + BSET_HEADER_U64S
    }
}

/// bcachefs 对齐: struct btree_node 的 state 字段 — 节点生命周期状态
///
/// 对应 bcachefs `enum btree_node_state` 的简化映射:
/// - `Alive` = BKEY_TYPE_BTREE_CACHED_CLEAN/... — 节点在 clean/dirty/pending_flush 列表中
/// - `Deleting` = 节点被标记为删除
/// - `InFlight` = BKEY_TYPE_BTREE_IN_FLIGHT — IO 正在进行
/// - `Reclaim` = BKEY_TYPE_BTREE_RECLAIM — 正在被 shrink/evict
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum NodeState {
    Alive = 0,
    Deleting = 1,
    InFlight = 2,
    Reclaim = 3,
}

/// 节点标志位常量 — bcachefs 对齐: BTREE_NODE_* flags
///
/// 用于两阶段 shrinker 和生命周期管理：
/// - NODE_ACCESSED: 节点最近被访问过（shrinker 第一遍清除、第二遍才驱逐）
/// - NODE_NEED_REWRITE: 节点需要重写（写回后再释放旧版本）
pub const NODE_ACCESSED: u8 = 0x01;
pub const NODE_NEED_REWRITE: u8 = 0x02;
/// bcachefs 对齐: BTREE_NODE_write_in_flight — 节点写入正在进行
pub const NODE_WRITE_IN_FLIGHT: u8 = 0x04;
/// bcachefs 对齐: BTREE_NODE_read_in_flight — 节点读取正在进行
pub const NODE_READ_IN_FLIGHT: u8 = 0x08;
/// bcachefs 对齐: BTREE_NODE_just_written — 节点刚被写入
pub const NODE_JUST_WRITTEN: u8 = 0x10;
/// bcachefs 对齐: BTREE_NODE_write_in_flight_inner
pub const NODE_WRITE_IN_FLIGHT_INNER: u8 = 0x20;
/// bcachefs 对齐: BTREE_NODE_dirty — 节点有未落盘的修改
///
/// 在 bkey 插入/删除/修改时设置，写入 CAS 中原子清除。
/// 对应 bcachefs `BTREE_NODE_dirty` (1U << 5)，subvol 使用 bit 6 (0x40)。
pub const NODE_DIRTY: u8 = 0x40;

/// bcachefs 对齐: `for_each_bset` — 遍历 BtreeNode 的所有活跃 bset
/// 返回 (set_index, &BsetTree) 的迭代器
pub fn for_each_bset(b: &BtreeNode) -> impl Iterator<Item = (usize, &BsetTree)> {
    let nsets = b.nsets() as usize;
    b.sets[..nsets].iter().enumerate()
}

/// bcachefs 对齐: `struct btree_node_iter_set` — 一个 bset 上的迭代状态
#[derive(Debug, Clone, Copy)]
pub struct BtreeNodeIterSet {
    /// 当前 key 在节点 data buffer 中的 u64 偏移。
    pub k: u16,
    /// bset 结束的 u64 偏移。
    pub end: u16,
}

/// bcachefs 对齐: `struct btree_node_iter` — 节点内跨 bset 迭代器
#[derive(Debug, Clone)]
pub struct BtreeNodeIter {
    pub data: [BtreeNodeIterSet; MAX_BSETS],
}

impl Default for BtreeNodeIter {
    fn default() -> Self {
        Self {
            data: [BtreeNodeIterSet { k: 0, end: 0 }; MAX_BSETS],
        }
    }
}

impl BtreeNodeIter {
    /// bcachefs 对齐: `__btree_node_iter_set_end()` — 检查 set 是否迭代结束
    pub fn set_end(&self, i: usize) -> bool {
        self.data[i].k == self.data[i].end
    }

    /// bcachefs 对齐: `bch2_btree_node_iter_end()` — 全部 set 是否迭代结束
    pub fn is_end(&self) -> bool {
        self.set_end(0)
    }

    /// bcachefs 对齐: 重置所有 set 的迭代状态
    pub fn reset(&mut self) {
        for d in &mut self.data {
            d.k = 0;
            d.end = 0;
        }
    }
}

pub struct BtreeNode {
    pub lock: SixLock,
    pub level: u8,
    pub packed_keys: u16,
    pub unpacked_keys: u16,
    pub whiteout_u64s: u16,
    pub node_size: u32,
    pub data: Vec<u8>,
    pub aux_data: Vec<u8>,
    pub nsets: u8,
    pub sets: [BsetTree; BSET_COUNT],
    pub refcount: AtomicU32,
    pub state: AtomicU8,
    /// 节点标志位 — bcachefs 对齐: btree->flags
    ///
    /// - NODE_ACCESSED: shrinker 两阶段 clock 用（设置 = 刚被访问，清除后可驱逐）
    /// - NODE_NEED_REWRITE: 节点需要写回重写
    pub flags: AtomicU8,
    /// bcachefs 对齐: `btree->written` — 已提交到节点 extent 的扇区数。
    ///
    /// 单位固定为 512-byte sector；新节点为 0，读盘后从
    /// `bch_btree_ptr_v2.sectors_written` 恢复，写请求排队时推进。
    pub written: u16,
    /// 该节点子树覆盖的最小 key（空节点 = Bpos::MAX）
    pub min_key: Bpos,
    /// 该节点子树覆盖的最大 key（空节点 = Bpos::MIN）
    pub max_key: Bpos,
    /// bcachefs 对齐: sib_u64s[2] — 左右兄弟节点的 u64 大小估计
    ///
    /// - sib_u64s[0] = btree_prev_sib: 左兄弟的 live_u64s 估计值
    /// - sib_u64s[1] = btree_next_sib: 右兄弟的 live_u64s 估计值
    /// - u16::MAX 表示边界节点（无兄弟）
    /// 在 btree_node_reset_sib_u64s() 中初始化。
    pub sib_u64s: [u16; 2],
    /// 该节点所处的 journal seq（标记脏数据对应的 journal entry）。
    /// 0 = 未关联任何 journal entry（读入/重建/未修改）。
    /// 非零 = 该节点被 journal_seq 对应的 entry 修改过。
    /// 序列化时写入 header.seq，反序列化时恢复。
    /// 在节点写回后端后，调用 `journal.bch2_journal_pin_put(journal_seq)` 释放 pin。
    pub journal_seq: u64,
    /// bcachefs 对齐: will_make_reachable — 新节点在被首次写入前不可驱逐
    ///
    /// 在 btree split/merge/increase_depth 中创建的节点，在首次落盘前
    /// 设置此标志，阻止 cannibalize eviction。节点写入后端后清除。
    ///
    /// 对应 bcachefs `b->will_make_reachable` 标记指针 + 位标志。
    /// subvol 用 AtomicBool 简化（bcachefs 用标记指针持 interior update 闭包引用）。
    pub will_make_reachable: AtomicBool,

    /// bcachefs 对齐: `struct btree::write_blocked` 非空状态。
    /// 节点级阻塞不串行化不同节点的 interior update。
    pub write_blocked: Arc<AtomicBool>,

    /// bcachefs 对齐: pin_count — 显式 pin 计数防止驱逐
    ///
    /// 在 shrink/evict 中跳过 `pin_count > 0` 的节点。
    /// 对应 bcachefs `bch2_node_pin` / `bch2_btree_cache_unpin`。
    pub pin_count: AtomicU32,

    /// Journal pin — 侵入式嵌入，对应 bcachefs `struct btree.journal_pin`
    ///
    /// bcachefs 将 `journal_entry_pin` 直接嵌入在 `struct btree` 中，
    /// 没有额外 mutex 保护。subvol 同样直接嵌入 `JournalEntryPin`，
    /// 该类型内部用 `UnsafeCell` + `AtomicU64` 实现，可变性由 journal 的
    /// pin_list lock 和 btree 的 SixLock 分层保护。
    ///
    /// 首次 `bch2_btree_add_journal_pin` 调用时注册到 journal，
    /// `bch2_btree_node_write_done` 完成时释放。
    /// 对应 bcachefs `struct btree.journal_pin`。
    pub journal_pin: JournalEntryPin,

    // ─── bcachefs 对齐: read_in_flight Condvar 等待 ───────────────────
    //
    // 对应 bcachefs `clear_btree_node_read_in_flight` 中的 `wake_up_bit()`。
    //
    // AtomicBool 与 flags 中的 NODE_READ_IN_FLIGHT 位始终保持同步：
    // - set_read_in_flight() 设置 flags 位 + store(true)
    // - clear_read_in_flight() 清除 flags 位 + store(false) + notify_all()
    // - try_lock_read_in_flight() 成功时也 store(true)
    //
    // 参考: bcachefs-tools/fs/btree/cache.c:1174 (set_btree_node_read_in_flight)
    //       bcachefs-tools/fs/btree/cache.c:1180 (clear_btree_node_read_in_flight)
    /// bcachefs 对齐: btree_node_read_in_flight — 读取进行中标志（独立于 flags 位，用于 Condvar 等待）
    pub read_in_flight: AtomicBool,
    /// Condvar 锁（仅用于 wait/notify，不保护实际数据）
    pub read_wait_mutex: Mutex<()>,
    /// bcachefs 对齐: wake_up_bit(&b->flags, BTREE_NODE_read_in_flight)
    pub read_condvar: Condvar,

    // ─── bcachefs 对齐: write_in_flight Condvar 等待 ──────────────────
    //
    // 写入进行中的等待语义与 read_in_flight 对称：驱逐/回收前需要等写完。
    pub write_wait_mutex: Mutex<()>,
    pub write_condvar: Condvar,

    /// bcachefs 对齐: filesystem 反向指针（对应 `b->c`）
    ///
    /// 通过 `BchVol` 访问后端设备。IO 函数从 `vol` 获取 device，
    /// 不通过参数传递 `backend`。
    pub vol: OnceLock<Weak<BchVol>>,
    /// bcachefs 对齐: journal 反向指针（对应 `b->c->journal`）
    ///
    /// IO 完成后 `bch2_btree_node_write_done` 通过此引用访问 journal
    /// 以释放 journal pin，无需外部回调传递 journal 参数。
    pub journal: OnceLock<Weak<Journal>>,
    #[cfg(test)]
    /// 测试用设备（无 BchVol 时使用）。
    pub test_device: OnceLock<Arc<BchDev>>,

    /// bcachefs 对齐: `b->key` — 节点磁盘地址，首次分配后终身不变。
    ///
    /// 0 = 未分配。`bch2_btree_node_read` 或首次写入时设置。
    /// 所有写（包括 flush 回调写）都使用此固定地址。
    pub block_addr: AtomicU64,
}

/// Entry max size for CURRENT packed format: (BKEY_U64S + 1 value u64) * 8 = 32 bytes
///
/// 用于保守的空间检查（insert/delete 前确定是否有足够空间）。
/// 实际 packed size 可能因 key format 不同而变化，可通过 `key::entry_packed_size()` 获得。
pub fn entry_size() -> u32 {
    // ExtentValue::to_bytes() 固定 16 字节 → 2 u64s
    (super::key::BKEY_U64S as u32 + 2) * 8
}

// ---------------------------------------------------------------------------
// Split/Merge 阈值常量 — 对齐 bcachefs cache.h:189 / interior.h:195-199
// ---------------------------------------------------------------------------

/// 分裂阈值：live_u64s > btree_max_u64s * 3/4 (75%) 触发分裂
/// 对应 bcachefs BTREE_SPLIT_THRESHOLD (cache.h:189)
pub const SPLIT_THRESHOLD_NUM: u32 = 3;
pub const SPLIT_THRESHOLD_DEN: u32 = 4;

/// 合并阈值：live_u64s < btree_max_u64s / 3 (33%) 触发合并
/// 对应 bcachefs BTREE_FOREGROUND_MERGE_THRESHOLD (interior.h:195)
pub const MERGE_THRESHOLD_NUM: u32 = 1;
pub const MERGE_THRESHOLD_DEN: u32 = 3;

/// 合并后节点大小上限：合并后节点应 ≤ btree_max_u64s * 3/5 (60%)
/// 对应 bcachefs MERGE_HIGHER (interior.h:197)
pub const MERGE_HIGHER_NUM: u32 = 3;
pub const MERGE_HIGHER_DEN: u32 = 5;

/// 合并滞后退避阈值：sib_u64s > MERGE_HYSTERESIS 时膨胀 sib_u64s
/// MERGE_HYSTERESIS = MERGE_THRESHOLD + MERGE_THRESHOLD/4 ≈ 41%
/// 对应 bcachefs MERGE_HYSTERESIS (interior.h:199)
pub const MERGE_HYSTERESIS_NUM: u32 = 5;
pub const MERGE_HYSTERESIS_DEN: u32 = 12;

/// 平衡分裂目标：左节点获得约 60% 的 live_u64s
/// 对应 bcachefs find_balanced_split 3/5 目标 (interior.c:2022)
pub const BALANCE_TARGET_NUM: u32 = 3;
pub const BALANCE_TARGET_DEN: u32 = 5;

/// 计算节点的 btree_max_u64s
/// btree_max_u64s = node_size / 8（subvol 没有 BtreeNodeHeader 额外开销）
pub fn btree_max_u64s(node_size: u32) -> u32 {
    node_size / 8
}

/// 检查节点的 live_u64s 是否超过分裂阈值
pub fn should_split(live_u64s: u32, node_size: u32) -> bool {
    let max_u64s = btree_max_u64s(node_size);
    live_u64s > max_u64s * SPLIT_THRESHOLD_NUM / SPLIT_THRESHOLD_DEN
}

/// 检查节点的 live_u64s 是否低于合并阈值
pub fn should_merge(live_u64s: u32, node_size: u32) -> bool {
    let max_u64s = btree_max_u64s(node_size);
    live_u64s < max_u64s * MERGE_THRESHOLD_NUM / MERGE_THRESHOLD_DEN
}

/// 检查合并后节点是否不超过 MERGE_HIGHER 上限
pub fn merge_fits_high_mark(live_u64s: u32, node_size: u32) -> bool {
    let max_u64s = btree_max_u64s(node_size);
    live_u64s <= max_u64s * MERGE_HIGHER_NUM / MERGE_HIGHER_DEN
}

/// 平衡分裂目标值（左节点期望的 live_u64s 占比）
pub fn balance_target_u64s(total_u64s: u32) -> u32 {
    total_u64s * BALANCE_TARGET_NUM / BALANCE_TARGET_DEN
}

impl BtreeNode {
    pub fn new_leaf() -> Self {
        Self::new(0)
    }
    pub fn new_internal() -> Self {
        Self::new(1)
    }

    /// 设置 BchVol 引用（对齐 bcachefs `b->c`）。
    /// IO 函数通过 `vol` 获取后端设备，不通过参数传递。
    pub fn set_vol_ref(&self, vol: &Arc<BchVol>) {
        self.vol.set(Arc::downgrade(vol)).ok();
    }

    pub(crate) fn vol_arc(&self) -> Option<Arc<BchVol>> {
        self.vol.get().and_then(|w| w.upgrade())
    }

    pub(crate) fn vol_device(&self) -> Arc<BchDev> {
        if let Some(vol) = self.vol_arc() {
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
        panic!("BtreeNode: vol not set — call set_vol_ref before IO")
    }

    /// 设置 journal 引用（对齐 bcachefs `b->c->journal`）。
    /// IO 回调通过此引用释放 journal pin，无需外部闭包传递 journal 参数。
    pub fn set_journal_ref(&self, journal: &Arc<Journal>) {
        self.journal.set(Arc::downgrade(journal)).ok();
    }

    /// 通过 `journal` 反向引用获取 Arc<Journal>。
    pub(crate) fn vol_journal(&self) -> Option<Arc<Journal>> {
        self.journal.get().and_then(|w| w.upgrade())
    }

    /// bcachefs 对齐: 设置节点磁盘地址（仅首次可设置）。
    ///
    /// 对应 bcachefs `b->key` — 分配后不变。
    /// 返回 true 表示本次调用成功设置了地址（首次设置）。
    pub fn try_set_block_addr(&self, addr: u64) -> bool {
        self.block_addr
            .compare_exchange(0, addr, Ordering::Release, Ordering::Relaxed)
            .is_ok()
    }

    /// bcachefs 对齐: 读取节点磁盘地址。
    ///
    /// 0 表示尚未分配。
    pub fn block_addr(&self) -> u64 {
        self.block_addr.load(Ordering::Acquire)
    }

    #[cfg(test)]
    /// 设置测试用设备（当 BchVol 不可用时使用）。
    pub fn set_test_device(&self, dev: Arc<BchDev>) {
        self.test_device.set(dev).ok();
    }

    /// bcachefs 对齐: `btree_node_will_make_reachable()` — 检查节点是否设置了 will_make_reachable
    pub fn will_make_reachable(&self) -> bool {
        self.will_make_reachable.load(Ordering::Acquire)
    }

    /// bcachefs 对齐: `set_btree_node_will_make_reachable()` — 设置 will_make_reachable 标志
    pub fn set_will_make_reachable(&self) {
        self.will_make_reachable.store(true, Ordering::Release);
    }

    /// bcachefs 对齐: `clear_btree_node_will_make_reachable()` — 清除 will_make_reachable 标志
    pub fn clear_will_make_reachable(&self) {
        self.will_make_reachable.store(false, Ordering::Release);
    }

    /// bcachefs 对齐: `set_btree_node_write_blocked()`。
    pub(crate) fn set_btree_node_write_blocked(&self) -> bool {
        self.write_blocked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
    }

    /// bcachefs 对齐: `clear_btree_node_write_blocked()`。
    pub(crate) fn clear_btree_node_write_blocked(&self) {
        self.write_blocked.store(false, Ordering::Release);
    }

    /// bcachefs 对齐: `btree_node_write_blocked()`。
    pub(crate) fn btree_node_write_blocked(&self) -> bool {
        self.write_blocked.load(Ordering::Acquire)
    }

    /// 取得节点级 `write_blocked` 状态的独立所有权。
    ///
    /// bcachefs 的 bit 位于节点对象中；Rust 需要允许 interior update
    /// 在 IO 完成回调之后才释放该 bit，因此只共享状态 Arc，不复制节点。
    pub(crate) fn write_blocked_state(&self) -> Arc<AtomicBool> {
        self.write_blocked.clone()
    }

    /// 缓存驱逐时清理嵌入的 journal pin
    ///
    /// 调用 `j.bch2_journal_pin_drop` 将 pin 从 journal 链表中移除并置 seq=0。
    /// 等同于旧版从 `Mutex<Option<JournalEntryPin>>` 中 take 后调用 drop。
    pub fn evict_journal_pin(&self, j: &Journal) {
        if self.journal_pin.is_active() {
            j.bch2_journal_pin_drop(&self.journal_pin);
        }
    }

    /// bcachefs 对齐: `bch2_btree_node_transition_state()` — 转换节点生命周期状态
    ///
    /// 对应 bcachefs `bch2_btree_node_transition_state(node, new_state)` (cache.c):
    /// 更新节点的 state 字段并在各列表之间移动。
    /// subvol 中使用 `state: AtomicU8` 标记，列表归属由调用者维护。
    pub fn bch2_btree_node_transition_state(&self, new_state: NodeState) {
        self.state.store(new_state as u8, Ordering::Release);
    }

    /// bcachefs 对齐 — 读取当前节点生命周期状态
    ///
    /// 返回 `state` 字段中存储的 `NodeState` 值。
    pub(crate) fn btree_node_state(&self) -> NodeState {
        match self.state.load(Ordering::Acquire) {
            0 => NodeState::Alive,
            1 => NodeState::Deleting,
            2 => NodeState::InFlight,
            3 => NodeState::Reclaim,
            _ => NodeState::Alive,
        }
    }

    pub(crate) fn new(level: u8) -> Self {
        let size = DEFAULT_NODE_SIZE; // 256 * 1024
        let data = vec![0u8; size as usize];
        let aux_data = vec![0u8; (size / BSET_CACHELINE * 8) as usize];
        let sets = [
            BsetTree {
                size: 0,
                extra: BSET_NO_AUX_TREE_VAL,
                data_offset: 0,
                aux_data_offset: u16::MAX,
                end_offset: BSET_HEADER_U64S,
            },
            BsetTree {
                size: 0,
                extra: BSET_NO_AUX_TREE_VAL,
                data_offset: u16::MAX,
                aux_data_offset: u16::MAX,
                end_offset: u16::MAX,
            },
            BsetTree {
                size: 0,
                extra: BSET_NO_AUX_TREE_VAL,
                data_offset: u16::MAX,
                aux_data_offset: u16::MAX,
                end_offset: u16::MAX,
            },
        ];
        let mut node = Self {
            lock: SixLock::new(),
            level,
            packed_keys: 0,
            unpacked_keys: 0,
            whiteout_u64s: 0,
            node_size: size,
            data,
            aux_data,
            nsets: 1,
            sets,
            refcount: AtomicU32::new(1),
            state: AtomicU8::new(NodeState::Alive as u8),
            flags: AtomicU8::new(0),
            written: 0,
            min_key: Bpos::MAX, // 空节点 = 无下界
            max_key: Bpos::MIN, // 空节点 = 无上界（min > max 表示空）
            sib_u64s: [u16::MAX; 2],
            journal_seq: 0,
            will_make_reachable: AtomicBool::new(false),
            write_blocked: Arc::new(AtomicBool::new(false)),
            pin_count: AtomicU32::new(0),
            journal_pin: JournalEntryPin::new(
                None,
                match level {
                    0 => crate::journal::reclaim::JournalPinType::Btree0,
                    1 => crate::journal::reclaim::JournalPinType::Btree1,
                    2 => crate::journal::reclaim::JournalPinType::Btree2,
                    _ => crate::journal::reclaim::JournalPinType::Btree3,
                },
            ),
            read_in_flight: AtomicBool::new(false),
            read_wait_mutex: Mutex::new(()),
            read_condvar: Condvar::new(),
            write_wait_mutex: Mutex::new(()),
            write_condvar: Condvar::new(),
            vol: OnceLock::new(),
            journal: OnceLock::new(),
            #[cfg(test)]
            test_device: OnceLock::new(),
            block_addr: AtomicU64::new(0),
        };
        node.bch2_bset_build_aux_tree(0, true);
        node
    }

    /// bcachefs 对齐: `btree_node_nsets()`
    pub fn nsets(&self) -> u8 {
        self.nsets
    }

    /// bcachefs 对齐: `btree_current_write()` — 获取当前写的 bset_tree
    /// set[nsets-1] 是当前可写入的增量 bset
    pub fn current_bset(&self) -> &BsetTree {
        &self.sets[self.nsets() as usize - 1]
    }

    /// bcachefs 对齐: `write_block()` — 当前 append record 的字节起点。
    pub(crate) fn write_block_offset(&self) -> usize {
        usize::from(self.written) << 9
    }

    /// bcachefs 对齐: `bset_written()` — bset 是否位于已提交前缀内。
    pub(crate) fn bset_written(&self, si: usize) -> bool {
        usize::from(self.sets[si].data_offset) * std::mem::size_of::<u64>()
            < self.write_block_offset()
    }

    /// bcachefs 对齐: `__bch2_btree_u64s_remaining()`。
    ///
    /// `end_u64s` 是相对 node buffer 起点的末尾偏移；与本地实现相同，
    /// whiteout suffix 计入已用空间，并始终为 varint 解码额外保留一个 u64。
    pub(crate) fn __bch2_btree_u64s_remaining(&self, end_u64s: u32) -> isize {
        let mut used = isize::try_from(end_u64s).expect("btree end offset exceeds isize")
            + self.whiteout_u64s as isize;
        let total = isize::try_from(self.node_size / 8).expect("btree size exceeds isize");

        used += 1;

        total - used
    }

    /// bcachefs 对齐: `bch2_btree_keys_u64s_remaining()`。
    pub(crate) fn bch2_btree_keys_u64s_remaining(&self) -> u32 {
        let last = self.nsets as usize - 1;
        let remaining = self.__bch2_btree_u64s_remaining(u32::from(self.sets[last].end_offset));

        assert!(remaining >= 0, "btree node key space underflow");

        if self.bset_written(last) {
            return 0;
        }

        remaining as u32
    }

    /// bcachefs 对齐: `bch2_btree_node_insert_fits()` (`interior.h:371-377`).
    pub(crate) fn bch2_btree_node_insert_fits(&self, u64s: u32) -> bool {
        if self.need_rewrite() {
            return false;
        }

        u64s <= self.bch2_btree_keys_u64s_remaining()
    }

    pub fn is_alive(&self) -> bool {
        self.state.load(Ordering::Acquire) == NodeState::Alive as u8
    }

    pub fn refcount(&self) -> u32 {
        self.refcount.load(Ordering::Acquire)
    }

    // ─── bcachefs 对齐: 节点生命周期标志位 ──────────────────────

    /// bcachefs 对齐: `btree_node_accessed()` — 节点最近是否被访问过
    ///
    /// 两阶段 shrinker 使用：第一遍见到 accessed 置位 → 清除标志（给第二次机会）；
    /// 第二遍见到 accessed 清除 → 驱逐节点。
    pub fn is_accessed(&self) -> bool {
        self.flags.load(Ordering::Acquire) & NODE_ACCESSED != 0
    }

    /// bcachefs 对齐: `set_btree_node_accessed()` — 标记节点为最近被访问
    pub fn set_accessed(&self) {
        self.flags.fetch_or(NODE_ACCESSED, Ordering::Release);
    }

    /// bcachefs 对齐: `clear_btree_node_accessed()` — 清除 accessed 标志
    pub fn clear_accessed(&self) {
        self.flags.fetch_and(!NODE_ACCESSED, Ordering::Release);
    }

    /// bcachefs 对齐: `btree_node_need_rewrite()` — 节点是否需要重写
    pub fn need_rewrite(&self) -> bool {
        self.flags.load(Ordering::Acquire) & NODE_NEED_REWRITE != 0
    }

    /// bcachefs 对齐: `set_btree_node_need_rewrite()` — 标记节点需要重写
    pub fn set_need_rewrite(&self) {
        self.flags.fetch_or(NODE_NEED_REWRITE, Ordering::Release);
    }

    /// bcachefs 对齐: `clear_btree_node_need_rewrite()` — 清除节点重写标记
    pub fn clear_need_rewrite(&self) {
        self.flags.fetch_and(!NODE_NEED_REWRITE, Ordering::Release);
    }

    /// bcachefs 对齐: `btree_node_dirty()` — 节点是否有未落盘的修改
    pub fn is_dirty(&self) -> bool {
        self.flags.load(Ordering::Acquire) & NODE_DIRTY != 0
    }

    /// bcachefs 对齐: `set_btree_node_dirty()` — 标记节点有未落盘的修改
    pub fn set_dirty(&self) {
        self.flags.fetch_or(NODE_DIRTY, Ordering::Release);
    }

    /// bcachefs 对齐: `clear_btree_node_dirty()` — 清除脏标记（CAS 成功后由写入路径调用）
    pub fn clear_dirty(&self) {
        self.flags.fetch_and(!NODE_DIRTY, Ordering::Release);
    }

    // ─── bcachefs 对齐: IO 标志位 ────────────────────────────────

    /// bcachefs 对齐: `btree_node_write_in_flight()` — 节点是否有写入进行中
    pub fn is_write_in_flight(&self) -> bool {
        self.flags.load(Ordering::Acquire) & NODE_WRITE_IN_FLIGHT != 0
    }

    /// bcachefs 对齐: `set_btree_node_write_in_flight()` — 设置写入进行中标志
    pub fn set_write_in_flight(&self) {
        self.flags.fetch_or(NODE_WRITE_IN_FLIGHT, Ordering::Release);
    }

    /// bcachefs 对齐: `clear_btree_node_write_in_flight()` — 清除写入进行中标志
    pub fn clear_write_in_flight(&self) {
        self.flags
            .fetch_and(!NODE_WRITE_IN_FLIGHT, Ordering::Release);
        self.write_condvar.notify_all();
    }

    pub fn set_write_in_flight_inner(&self) {
        self.flags
            .fetch_or(NODE_WRITE_IN_FLIGHT_INNER, Ordering::Release);
    }

    pub fn clear_write_in_flight_inner(&self) {
        self.flags
            .fetch_and(!NODE_WRITE_IN_FLIGHT_INNER, Ordering::Release);
    }

    /// bcachefs 对齐: `btree_node_read_in_flight()` — 节点是否有读取进行中
    pub fn is_read_in_flight(&self) -> bool {
        self.flags.load(Ordering::Acquire) & NODE_READ_IN_FLIGHT != 0
    }

    /// bcachefs 对齐: `set_btree_node_read_in_flight()` — 设置读取进行中标志
    ///
    /// 同步设置 flags 位和 AtomicBool，对应 cache.c:1174。
    pub fn set_read_in_flight(&self) {
        self.flags.fetch_or(NODE_READ_IN_FLIGHT, Ordering::Release);
        self.read_in_flight.store(true, Ordering::Release);
    }

    /// bcachefs 对齐: `clear_btree_node_read_in_flight()` — 清除读取进行中标志
    ///
    /// 同步清除 flags 位和 AtomicBool，并通知等待者。
    /// 对应 cache.c:1180 — `clear_bit(BTREE_NODE_read_in_flight, &b->flags); wake_up_bit(...)`。
    pub fn clear_read_in_flight(&self) {
        self.flags
            .fetch_and(!NODE_READ_IN_FLIGHT, Ordering::Release);
        self.read_in_flight.store(false, Ordering::Release);
        self.read_condvar.notify_all();
    }

    /// bcachefs 对齐: `btree_node_just_written()` — 节点是否刚被写入
    pub fn is_just_written(&self) -> bool {
        self.flags.load(Ordering::Acquire) & NODE_JUST_WRITTEN != 0
    }

    /// bcachefs 对齐: `set_btree_node_just_written()` — 标记节点刚被写入
    pub fn set_just_written(&self) {
        self.flags.fetch_or(NODE_JUST_WRITTEN, Ordering::Release);
    }

    /// bcachefs 对齐: `clear_btree_node_just_written()` — 清除 just_written 标志
    pub fn clear_just_written(&self) {
        self.flags.fetch_and(!NODE_JUST_WRITTEN, Ordering::Release);
    }

    /// 原子尝试获取 write_in_flight 锁
    /// 类似 bcachefs 的 CAS 协议: 如果 flag 为 false，设为 true 并返回 true
    pub fn try_lock_write_in_flight(&self) -> bool {
        let mut old = self.flags.load(Ordering::Relaxed);
        loop {
            if old & NODE_WRITE_IN_FLIGHT != 0 {
                return false; // 已被锁
            }
            match self.flags.compare_exchange_weak(
                old,
                old | NODE_WRITE_IN_FLIGHT,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(actual) => old = actual,
            }
        }
    }

    /// bcachefs 对齐: wait_on_write — 等待正在进行的写入完成
    ///
    /// 对应 bcachefs `wait_on_bit(&b->flags, BTREE_NODE_write_in_flight, ...)`。
    /// 与 `wait_on_read()` 对称，用于在驱逐/回收节点前等待写回完成。
    pub fn wait_on_write(&self, timeout: Option<std::time::Duration>) -> bool {
        let guard = self.write_wait_mutex.lock().unwrap();
        if !self.is_write_in_flight() {
            return true;
        }
        match timeout {
            Some(dur) => {
                let (_unused, result) = self.write_condvar.wait_timeout(guard, dur).unwrap();
                !result.timed_out()
            }
            None => {
                let mut guard = guard;
                while self.is_write_in_flight() {
                    guard = self.write_condvar.wait(guard).unwrap();
                }
                true
            }
        }
    }

    /// 原子尝试获取 read_in_flight 锁
    ///
    /// CAS 成功时同步设置 AtomicBool 标志，确保 flags 位与 AtomicBool 一致。
    pub fn try_lock_read_in_flight(&self) -> bool {
        let mut old = self.flags.load(Ordering::Relaxed);
        loop {
            if old & NODE_READ_IN_FLIGHT != 0 {
                return false;
            }
            match self.flags.compare_exchange_weak(
                old,
                old | NODE_READ_IN_FLIGHT,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    self.read_in_flight.store(true, Ordering::Release);
                    return true;
                }
                Err(actual) => old = actual,
            }
        }
    }

    /// bcachefs 对齐: wait_on_read — 等待正在进行的读取完成
    ///
    /// 对应 bcachefs `wait_on_bit(&b->flags, BTREE_NODE_read_in_flight, ...)`。
    /// 使用 Condvar 等待直到 `clear_read_in_flight()` 被调用（通知或轮询到完成）。
    ///
    /// - `timeout = None`：无限等待直到读取完成
    /// - `timeout = Some(dur)`：最多等待 dur 时间，超时返回 false
    ///
    /// 返回 true 表示读取已完成，false 表示超时。
    /// 参考: bcachefs-tools/fs/btree/cache.c:1178 (wake_up_bit)
    pub fn wait_on_read(&self, timeout: Option<std::time::Duration>) -> bool {
        let guard = self.read_wait_mutex.lock().unwrap();
        // 快速路径：如果没有读取进行中，立即返回
        if !self.read_in_flight.load(Ordering::Acquire) {
            return true;
        }
        match timeout {
            Some(dur) => {
                let (_unused, result) = self.read_condvar.wait_timeout(guard, dur).unwrap();
                !result.timed_out()
            }
            None => {
                let mut guard = guard;
                while self.read_in_flight.load(Ordering::Acquire) {
                    guard = self.read_condvar.wait(guard).unwrap();
                }
                true
            }
        }
    }

    // ─── bcachefs 对齐: bset 操作 ─────────────────────────────────

    fn bset_header(&self, si: usize) -> BsetHeader {
        let offset = self.sets[si].data_offset as usize * 8;
        unsafe {
            self.data
                .as_ptr()
                .add(offset)
                .cast::<BsetHeader>()
                .read_unaligned()
        }
    }

    fn write_bset_header(&mut self, si: usize, header: BsetHeader) {
        let offset = self.sets[si].data_offset as usize * 8;
        unsafe {
            self.data
                .as_mut_ptr()
                .add(offset)
                .cast::<BsetHeader>()
                .write_unaligned(header);
        }
    }

    fn set_bset_u64s(&mut self, si: usize, u64s: u16) {
        let mut header = self.bset_header(si);
        header.u64s = u64s;
        self.write_bset_header(si, header);
    }

    /// bcachefs 对齐: `bch2_btree_keys_init()` — 初始化节点的所有 bset
    pub fn bch2_btree_keys_init(&mut self) {
        self.nsets = 0;
        for t in &mut self.sets {
            t.data_offset = u16::MAX;
        }
        self.bch2_bset_set_no_aux_tree(0);
        self.packed_keys = 0;
        self.unpacked_keys = 0;
    }

    fn bch2_bset_set_no_aux_tree(&mut self, si: usize) {
        for set in &mut self.sets[si..] {
            set.size = 0;
            set.extra = BSET_NO_AUX_TREE_VAL;
            set.aux_data_offset = u16::MAX;
        }
    }

    fn bset_aux_tree_buf_end(&self, si: usize) -> u16 {
        let set = &self.sets[si];
        debug_assert_ne!(set.aux_data_offset, u16::MAX);

        match set.aux_tree_type() {
            BsetAuxTreeType::NoAuxTree => set.aux_data_offset,
            BsetAuxTreeType::RoAuxTree => {
                set.aux_data_offset + (set.size as u32 * 4).div_ceil(8) as u16
            }
            BsetAuxTreeType::RwAuxTree => {
                set.aux_data_offset
                    + (set.size as usize * std::mem::size_of::<RwAuxTree>()).div_ceil(8) as u16
            }
        }
    }

    fn bset_aux_tree_buf_start(&self, si: usize) -> u16 {
        if si == 0 {
            0
        } else {
            self.bset_aux_tree_buf_end(si - 1)
        }
    }

    fn bset_alloc_tree(&mut self, si: usize) {
        self.bch2_bset_set_no_aux_tree(si);
        let start = self.bset_aux_tree_buf_start(si);
        self.sets[si].aux_data_offset = (start + 7) & !7;
    }

    fn __bset_tree_capacity(&self, si: usize) -> usize {
        self.aux_data.len() - self.sets[si].aux_data_offset as usize * 8
    }

    fn bset_ro_tree_capacity(&self, si: usize) -> usize {
        self.__bset_tree_capacity(si) / std::mem::size_of::<BkeyFloat>()
    }

    fn ro_aux_tree(&self, si: usize, idx: usize) -> BkeyFloat {
        let base = self.sets[si].aux_data_offset as usize * 8;
        unsafe {
            self.aux_data
                .as_ptr()
                .add(base + idx * std::mem::size_of::<BkeyFloat>())
                .cast::<BkeyFloat>()
                .read_unaligned()
        }
    }

    fn ro_aux_tree_write(&mut self, si: usize, idx: usize, entry: BkeyFloat) {
        let base = self.sets[si].aux_data_offset as usize * 8;
        unsafe {
            self.aux_data
                .as_mut_ptr()
                .add(base + idx * std::mem::size_of::<BkeyFloat>())
                .cast::<BkeyFloat>()
                .write_unaligned(entry);
        }
    }

    fn bset_cacheline_offset(&self, si: usize, cacheline: usize) -> isize {
        let first = self.sets[si].first_key_offset() as usize * 8;
        let absolute = self.data.as_ptr() as usize + first;
        (absolute & !(L1_CACHE_BYTES as usize - 1)) as isize - self.data.as_ptr() as isize
            + (cacheline * BSET_CACHELINE as usize) as isize
    }

    fn bkey_to_cacheline(&self, si: usize, key: u16) -> usize {
        ((key as isize * 8 - self.bset_cacheline_offset(si, 0)) / BSET_CACHELINE as isize) as usize
    }

    fn cacheline_to_bkey(&self, si: usize, cacheline: usize, offset: u8) -> u16 {
        ((self.bset_cacheline_offset(si, cacheline) + offset as isize * 8) / 8) as u16
    }

    fn tree_to_bkey(&self, si: usize, idx: usize) -> u16 {
        let set = &self.sets[si];
        let inorder = __eytzinger1_to_inorder(idx, set.size as usize - 1, set.extra as usize);
        self.cacheline_to_bkey(si, inorder, self.ro_aux_tree(si, idx).key_offset)
    }

    fn bkey_mantissa(packed: &BkeyPacked, float: BkeyFloat) -> u16 {
        let ptr = std::ptr::addr_of!(*packed).cast::<u8>();
        let value = unsafe {
            ptr.add((float.exponent >> 3) as usize)
                .cast::<u64>()
                .read_unaligned()
        };
        (value >> (float.exponent & 7)) as u16
    }

    /// bcachefs `bch2_bkey_greatest_differing_bit()` (bkey.c:1106).
    /// Walks packed key u64 words from high to low; matches bcachefs LE path.
    fn bkey_greatest_differing_bit(
        format: &BkeyFormat,
        left: &BkeyPacked,
        right: &BkeyPacked,
    ) -> u32 {
        let key_u64s = format.key_u64s as usize;
        let l_base = std::ptr::addr_of!(*left).cast::<u8>();
        let r_base = std::ptr::addr_of!(*right).cast::<u8>();

        let mut word_idx = key_u64s - 1;
        let mut nr_key_bits = format.bpos_key_bits();
        let mut word_bits: u32 = 64;

        let mut l_v = unsafe { l_base.add(word_idx * 8).cast::<u64>().read_unaligned() };
        let mut r_v = unsafe { r_base.add(word_idx * 8).cast::<u64>().read_unaligned() };

        loop {
            if nr_key_bits < word_bits {
                let shift = word_bits - nr_key_bits;
                l_v >>= shift;
                r_v >>= shift;
            }
            let xor = l_v ^ r_v;
            if xor != 0 {
                let bit_in_word = 63u32.wrapping_sub(xor.leading_zeros());
                let remaining = nr_key_bits.saturating_sub(word_bits);
                return bit_in_word + remaining;
            }
            if nr_key_bits <= word_bits || word_idx == 0 {
                break;
            }
            nr_key_bits -= word_bits;
            word_idx -= 1;
            l_v = unsafe { l_base.add(word_idx * 8).cast::<u64>().read_unaligned() };
            r_v = unsafe { r_base.add(word_idx * 8).cast::<u64>().read_unaligned() };
            word_bits = 64;
        }
        0
    }

    fn make_bfloat(&mut self, si: usize, idx: usize, min_key: &BkeyPacked, max_key: &BkeyPacked) {
        let middle_off = self.tree_to_bkey(si, idx);
        let middle = unsafe {
            &*self
                .data
                .as_ptr()
                .add(middle_off as usize * 8)
                .cast::<BkeyPacked>()
        };
        let left = if idx.is_power_of_two() {
            min_key
        } else {
            let off = self.tree_to_bkey(si, idx >> (idx.trailing_zeros() + 1));
            unsafe {
                &*self
                    .data
                    .as_ptr()
                    .add(off as usize * 8)
                    .cast::<BkeyPacked>()
            }
        };
        let right = if (idx + 1).is_power_of_two() {
            max_key
        } else {
            let off = self.tree_to_bkey(si, idx >> ((idx + 1).trailing_zeros() + 1));
            unsafe {
                &*self
                    .data
                    .as_ptr()
                    .add(off as usize * 8)
                    .cast::<BkeyPacked>()
            }
        };
        if !left.is_packed() || !right.is_packed() || !middle.is_packed() {
            let mut float = self.ro_aux_tree(si, idx);
            float.exponent = BFLOAT_FAILED;
            self.ro_aux_tree_write(si, idx, float);
            return;
        }
        let high_bit = Self::bkey_greatest_differing_bit(&BKEY_FORMAT_CURRENT, left, right)
            .max(BKEY_MANTISSA_BITS.min(BKEY_FORMAT_CURRENT.bpos_key_bits()) - 1);
        let exponent = high_bit as i32 - (BKEY_MANTISSA_BITS as i32 - 1);
        let shift = (BKEY_FORMAT_CURRENT.key_u64s as u32 * 64 - BKEY_FORMAT_CURRENT.bpos_key_bits())
            as i32
            + exponent;
        let mut float = self.ro_aux_tree(si, idx);
        if shift < 0 || shift >= BFLOAT_FAILED as i32 {
            float.exponent = BFLOAT_FAILED;
            self.ro_aux_tree_write(si, idx, float);
            return;
        }
        float.exponent = shift as u8;
        float.mantissa = Self::bkey_mantissa(middle, float);
        if exponent < 0 {
            float.mantissa |= !(!0u16 << (-exponent as u32));
        }
        self.ro_aux_tree_write(si, idx, float);
    }

    fn __build_rw_aux_tree(&mut self, si: usize) {
        self.sets[si].size = 1;
        self.sets[si].extra = BSET_RW_AUX_TREE_VAL;
        let first = self.sets[si].first_key_offset();
        let mut entry = RwAuxTree::ZERO;
        entry.offset = first;
        self.rw_aux_tree_write(si, 0, entry);

        let mut key = first;
        while key < self.sets[si].end_offset {
            if self.sets[si].size as usize == self.bset_rw_tree_capacity(si) {
                break;
            }

            if u32::from(key - self.rw_aux_to_bkey(si, self.sets[si].size as usize - 1)) * 8
                > L1_CACHE_BYTES
            {
                let idx = self.sets[si].size as usize;
                self.sets[si].size += 1;
                self.rw_aux_tree_set(si, idx, key);
            }

            key += self.read_entry_u64s(key as usize * 8) as u16;
        }
    }

    fn __build_ro_aux_tree(&mut self, si: usize) {
        let last = self.sets[si].end_offset;
        self.sets[si].size = self
            .bkey_to_cacheline(si, last)
            .min(self.bset_ro_tree_capacity(si)) as u16;

        loop {
            if self.sets[si].size < 2 {
                self.sets[si].size = 0;
                self.sets[si].extra = BSET_NO_AUX_TREE_VAL;
                return;
            }

            self.sets[si].extra = eytzinger1_extra(self.sets[si].size as usize - 1) as u16;
            let mut key = self.sets[si].first_key_offset();
            let mut cacheline = 1usize;
            let mut idx = eytzinger1_first(self.sets[si].size as usize - 1);
            let mut retry = false;
            while idx != 0 {
                while self.bkey_to_cacheline(si, key) < cacheline {
                    key += self.read_entry_u64s(key as usize * 8) as u16;
                }
                if key >= last {
                    self.sets[si].size -= 1;
                    retry = true;
                    break;
                }

                let line_base = self.bset_cacheline_offset(si, cacheline);
                let key_offset = (key as isize * 8 - line_base) / 8;
                let mut float = BkeyFloat::default();
                float.key_offset = key_offset as u8;
                self.ro_aux_tree_write(si, idx, float);
                cacheline += 1;
                idx = eytzinger1_next(idx, self.sets[si].size as usize - 1);
            }
            if retry {
                continue;
            }

            let mut min_buf = [0u64; 8];
            let min_key = unsafe { &mut *min_buf.as_mut_ptr().cast::<BkeyPacked>() };
            if !crate::btree::key::bkey_pack_pos(min_key, self.min_key, &BKEY_FORMAT_CURRENT) {
                min_key.set_format(KEY_FORMAT_CURRENT);
            }
            let mut max_buf = [0u64; 8];
            let max_key = unsafe { &mut *max_buf.as_mut_ptr().cast::<BkeyPacked>() };
            if !crate::btree::key::bkey_pack_pos(max_key, self.max_key, &BKEY_FORMAT_CURRENT) {
                max_key.set_format(KEY_FORMAT_CURRENT);
            }

            let mut idx = eytzinger1_first(self.sets[si].size as usize - 1);
            while idx != 0 {
                self.make_bfloat(si, idx, min_key, max_key);
                idx = eytzinger1_next(idx, self.sets[si].size as usize - 1);
            }
            return;
        }
    }

    /// bcachefs `bch2_bset_build_aux_tree()` (`bset.c:1028`).
    pub fn bch2_bset_build_aux_tree(&mut self, si: usize, writeable: bool) {
        if if writeable {
            self.sets[si].has_rw_aux_tree()
        } else {
            self.sets[si].has_ro_aux_tree()
        } {
            return;
        }

        self.bset_alloc_tree(si);

        if self.__bset_tree_capacity(si) == 0 {
            return;
        }

        if writeable {
            self.__build_rw_aux_tree(si);
        } else {
            self.__build_ro_aux_tree(si);
        }
        self.bset_aux_tree_verify();
    }

    /// bcachefs `bset_aux_tree_verify()` (bset.c:791).
    /// Verifies aux tree invariants for every bset in the node.
    fn bset_aux_tree_verify(&self) {
        for si in 0..self.nsets as usize {
            if self.sets[si].aux_data_offset == u16::MAX {
                continue;
            }

            // bcachefs: BUG_ON(t != b->set && t[-1].aux_data_offset == U16_MAX)
            debug_assert!(
                si == 0 || self.sets[si - 1].aux_data_offset != u16::MAX,
                "non-first bset aux present but previous bset has none"
            );

            let aux_data_u64s = (self.aux_data.len() / 8) as u16;
            let buf_start = self.bset_aux_tree_buf_start(si);
            let buf_end = self.bset_aux_tree_buf_end(si);

            // bcachefs: BUG_ON(t->aux_data_offset < bset_aux_tree_buf_start(b, t))
            debug_assert!(
                self.sets[si].aux_data_offset >= buf_start,
                "bset[{}] aux_data_offset {} < buf_start {}",
                si,
                self.sets[si].aux_data_offset,
                buf_start
            );
            // bcachefs: BUG_ON(t->aux_data_offset > btree_aux_data_u64s(b))
            debug_assert!(
                self.sets[si].aux_data_offset <= aux_data_u64s,
                "bset[{}] aux_data_offset {} > aux_data_u64s {}",
                si,
                self.sets[si].aux_data_offset,
                aux_data_u64s
            );
            // bcachefs: BUG_ON(bset_aux_tree_buf_end(t) > btree_aux_data_u64s(b))
            debug_assert!(
                buf_end <= aux_data_u64s,
                "bset[{}] buf_end {} > aux_data_u64s {}",
                si,
                buf_end,
                aux_data_u64s
            );

            match self.sets[si].aux_tree_type() {
                BsetAuxTreeType::RoAuxTree => self.__bset_aux_tree_verify_ro(si),
                BsetAuxTreeType::RwAuxTree => self.__bset_aux_tree_verify_rw(si),
                BsetAuxTreeType::NoAuxTree => {}
            }
        }
    }

    /// bcachefs `__bset_aux_tree_verify_ro()` (bset.c:752).
    /// Verifies `tree_to_bkey` round-trips for every RO aux tree entry.
    fn __bset_aux_tree_verify_ro(&self, si: usize) {
        if self.sets[si].size < 2 {
            return;
        }
        let last = self.sets[si].end_offset;
        let mut k = self.sets[si].first_key_offset();
        let mut idx = eytzinger1_first(self.sets[si].size as usize - 1);
        while idx != 0 {
            while self.tree_to_bkey(si, idx) > k && k != last {
                k += self.read_entry_u64s(k as usize * 8) as u16;
            }
            debug_assert!(
                self.tree_to_bkey(si, idx) == k,
                "ro_aux_tree_verify: tree_to_bkey({}) = {} but key offset = {}",
                idx,
                self.tree_to_bkey(si, idx),
                k
            );
            idx = eytzinger1_next(idx, self.sets[si].size as usize - 1);
        }
    }

    /// bcachefs `__bch2_bset_verify_rw_aux_tree()` (bset.c:713).
    /// Verifies offset ordering and position correctness of the RW aux tree.
    fn __bset_aux_tree_verify_rw(&self, si: usize) {
        debug_assert!(!self.sets[si].has_ro_aux_tree());
        if !self.sets[si].has_rw_aux_tree() {
            return;
        }

        let size = self.sets[si].size as usize;
        debug_assert!(size >= 1);

        let last = self.sets[si].end_offset;
        let mut k = self.sets[si].first_key_offset();
        let mut j = 1usize;

        // bcachefs: BUG_ON(rw_aux_to_bkey(b, t, 0) != k)
        debug_assert!(self.rw_aux_to_bkey(si, 0) == k);

        if j >= size {
            return;
        }
        // bcachefs offset monotonic check after goto start (j becomes 1)
        debug_assert!(
            self.rw_aux_tree(si, 1).offset > self.rw_aux_tree(si, 0).offset,
            "rw aux tree offsets not strictly increasing"
        );

        loop {
            // bcachefs: if (rw_aux_to_bkey(b, t, j) == k) { ... }
            if self.rw_aux_to_bkey(si, j) == k {
                let rw = self.rw_aux_tree(si, j);
                let packed =
                    unsafe { &*self.data.as_ptr().add(k as usize * 8).cast::<BkeyPacked>() };
                let (pos, _, _, _) = bkey_unpack(&BKEY_FORMAT_CURRENT, packed);
                // bcachefs: BUG_ON(!bpos_eq(rw_aux_tree(b, t)[j].k, bkey_unpack_pos(b, k)))
                debug_assert!(
                    rw.pos() == pos,
                    "rw aux tree entry {} pos mismatch: tree={:?} key={:?}",
                    j,
                    rw.pos(),
                    pos
                );

                j += 1;
                if j == size {
                    break;
                }
                // bcachefs offset monotonic check
                debug_assert!(
                    self.rw_aux_tree(si, j).offset > self.rw_aux_tree(si, j - 1).offset,
                    "rw aux tree offsets not strictly increasing at index {}",
                    j
                );
            }

            // bcachefs: k = bkey_p_next(k);
            k += self.read_entry_u64s(k as usize * 8) as u16;
            // bcachefs: BUG_ON(k >= btree_bkey_last(b, t));
            debug_assert!(k < last, "rw aux tree walk past bset end");
        }
    }

    /// 从 data buffer 的指定字节偏移处读取 u64s 字段
    pub(crate) fn read_entry_u64s(&self, offset: usize) -> u8 {
        unsafe {
            std::ptr::addr_of!(self.data[offset])
                .cast::<u8>()
                .read_unaligned()
        }
    }

    /// 计算节点中所有 packed entry 的实际总字节数
    ///
    /// 遍历所有 bset 的 data buffer，对每个 entry 读取 u64s 字段
    /// 累加 `u64s * 8`。用于 `can_absorb()`、`node_underfull()` 等
    /// 需要真实空间占用的判断。
    pub fn total_data_bytes(&self) -> u32 {
        let mut total = 0u32;
        for set in &self.sets[..self.nsets as usize] {
            let mut cur = u32::from(set.first_key_offset()) * 8;
            while cur < u32::from(set.end_offset) * 8 {
                let u64s = self.read_entry_u64s(cur as usize);
                let entry_bytes = (u64s as u32) * 8;
                cur += entry_bytes;
                total += entry_bytes;
            }
        }
        total
    }

    /// 排除 KeyType::Deleted 的 live data bytes。
    /// bcachefs 对齐: `bch2_btree_node_iter_and` 中检查 `k->type != KEY_TYPE_deleted` 后再计数。
    pub fn live_data_bytes(&self) -> u32 {
        let mut total = 0u32;
        for set in &self.sets[..self.nsets as usize] {
            let mut cur = u32::from(set.first_key_offset()) * 8;
            while cur < u32::from(set.end_offset) * 8 {
                let base = cur as usize;
                let u64s = self.read_entry_u64s(base);
                let entry_bytes = (u64s as u32) * 8;
                unsafe {
                    let pk = &*(self.data.as_ptr().add(base) as *const BkeyPacked);
                    if pk.type_ != KeyType::Deleted as u8 {
                        total += entry_bytes;
                    }
                }
                cur += entry_bytes;
            }
        }
        total
    }

    /// Compare a packed entry's bpos against a target BtreeKey.
    /// Uses field-by-field comparison to avoid fully unpacking all fields.
    /// Returns Ordering consistent with BtreeKey::cmp.
    pub fn compare_packed_entry(&self, offset: usize, target: &BtreeKey) -> CmpOrdering {
        unsafe {
            let pk = &*(self.data.as_ptr().add(offset) as *const BkeyPacked);
            let t_bpos = Bpos {
                inode: std::ptr::addr_of!(target.inode).read_unaligned(),
                offset: std::ptr::addr_of!(target.vaddr).read_unaligned(),
                snapshot: std::ptr::addr_of!(target.snapshot_id).read_unaligned(),
            };
            bkey_cmp_packed_vs_bpos(&BKEY_FORMAT_CURRENT, pk, &t_bpos)
        }
    }

    /// 从 data buffer 的指定字节偏移处读取一个 packed entry（ExtentValue 感知）
    pub(crate) fn read_packed_entry(&self, offset: usize) -> (BtreeKey, ExtentValue) {
        unsafe {
            let pk = &*(self.data.as_ptr().add(offset) as *const BkeyPacked);
            let (bpos, key_type, extent) = bkey_unpack_extent(&BKEY_FORMAT_CURRENT, pk);

            let key = BtreeKey::from_bpos_size(bpos, KeyType::from_u8(key_type), extent.size);
            (key, extent)
        }
    }

    /// Read the idx-th entry (1-indexed) from a bset.
    ///
    /// For compacted set[0] (aux_data_offset > 0): reads from aux array to get data_offset,
    /// then reads the packed entry at that offset.
    ///
    /// For incremental sets[1..] (no aux): sequential scan from data_offset to reach
    /// the idx-th entry by reading each entry's u64s header field.
    pub fn read_entry(&self, set: &BsetTree, idx: usize) -> (BtreeKey, ExtentValue) {
        assert!(idx >= 1);
        let mut cur = u32::from(set.first_key_offset()) * 8;
        for _ in 1..idx {
            assert!(cur < u32::from(set.end_offset) * 8);
            cur += self.read_entry_u64s(cur as usize) as u32 * 8;
        }
        assert!(cur < u32::from(set.end_offset) * 8);
        self.read_packed_entry(cur as usize)
    }

    /// Write a packed entry at the given byte offset in the data buffer.
    /// Returns the actual packed size in bytes (entry.u64s * 8).
    /// 统一写入 ExtentValue（不再区分 Single/Range）。
    pub(crate) fn write_entry(
        &mut self,
        offset: u32,
        key: &BtreeKey,
        value: &BchVal,
        dev_idx: u8,
    ) -> u32 {
        let bpos = Bpos {
            inode: unsafe { std::ptr::addr_of!(key.inode).read_unaligned() },
            offset: unsafe { std::ptr::addr_of!(key.vaddr).read_unaligned() },
            snapshot: unsafe { std::ptr::addr_of!(key.snapshot_id).read_unaligned() },
        };
        let size = unsafe { std::ptr::addr_of!(key.size).read_unaligned() };
        let extent = ExtentValue {
            paddr: value.paddr.get(),
            size,
            ver: value.ver,
            dev_idx,
            crc32c: 0,
            crc_offset_blocks: 0,
        };
        unsafe {
            let pk = &mut *(self.data.as_mut_ptr().add(offset as usize) as *mut BkeyPacked);
            bkey_pack_extent(pk, bpos, key.key_type as u8, &extent, &BKEY_FORMAT_CURRENT);
            pk.u64s as u32 * 8
        }
    }

    /// Write a packed entry with ExtentValue directly at the given byte offset.
    /// Returns the actual packed size in bytes (entry.u64s * 8).
    /// 保留 dev_idx 信息。
    pub(crate) fn write_entry_extent(
        &mut self,
        offset: u32,
        key: &BtreeKey,
        extent: &ExtentValue,
    ) -> u32 {
        let bpos = Bpos {
            inode: unsafe { std::ptr::addr_of!(key.inode).read_unaligned() },
            offset: unsafe { std::ptr::addr_of!(key.vaddr).read_unaligned() },
            snapshot: unsafe { std::ptr::addr_of!(key.snapshot_id).read_unaligned() },
        };
        unsafe {
            let pk = &mut *(self.data.as_mut_ptr().add(offset as usize) as *mut BkeyPacked);
            bkey_pack_extent(pk, bpos, key.key_type as u8, extent, &BKEY_FORMAT_CURRENT);
            pk.u64s as u32 * 8
        }
    }

    /// Write a packed entry with raw value bytes at the given byte offset.
    /// Returns the actual packed size in bytes.
    pub(crate) fn write_entry_bytes(&mut self, offset: u32, entry: &BtreeEntry) -> u32 {
        let bpos = &entry.pos;
        let value_bytes = entry.value.to_bytes();
        unsafe {
            let pk = &mut *(self.data.as_mut_ptr().add(offset as usize) as *mut BkeyPacked);
            bkey_pack_raw(
                pk,
                *bpos,
                entry.key_type as u8,
                &value_bytes,
                &BKEY_FORMAT_CURRENT,
            );
            pk.set_needs_whiteout(entry.needs_whiteout);
            pk.u64s as u32 * 8
        }
    }

    /// Read a packed entry as BtreeEntry (supports both Extent and Raw values).
    pub(crate) fn read_packed_entry_raw(&self, offset: usize) -> BtreeEntry {
        unsafe {
            let pk = &*(self.data.as_ptr().add(offset) as *const BkeyPacked);
            let (bpos, key_type, value_bytes) = bkey_unpack_bytes(&BKEY_FORMAT_CURRENT, pk);
            let value = if self.level > 0
                && value_bytes.len() == crate::btree::types::BtreePtrV2::DISK_BYTES
            {
                match crate::btree::types::BtreePtrV2::from_bytes(value_bytes) {
                    Ok(ptr) => crate::btree::key::KeyValue::BtreePtr(ptr),
                    Err(_) => crate::btree::key::KeyValue::Raw(value_bytes.to_vec()),
                }
            } else {
                crate::btree::key::KeyValue::Raw(value_bytes.to_vec())
            };
            let mut entry = BtreeEntry::new(bpos, KeyType::from_u8(key_type), value);
            entry.needs_whiteout = pk.needs_whiteout();
            entry
        }
    }

    /// Read the idx-th entry (1-indexed) from a bset, returning BtreeEntry.
    ///
    /// Like [`read_entry`] but works with `BtreeEntry` / `KeyValue` (supports
    /// both Extent and Raw value types).
    pub fn read_entry_raw(&self, set: &BsetTree, idx: usize) -> BtreeEntry {
        assert!(idx >= 1);
        let mut cur = u32::from(set.first_key_offset()) * 8;
        for _ in 1..idx {
            assert!(cur < u32::from(set.end_offset) * 8);
            cur += self.read_entry_u64s(cur as usize) as u32 * 8;
        }
        assert!(cur < u32::from(set.end_offset) * 8);
        self.read_packed_entry_raw(cur as usize)
    }

    /// Search across all bsets.
    /// Set[0] with aux array uses binary search on aux keys
    /// (standard binary on sorted aux, or Eytzinger on Eytzinger aux).
    /// Sets[1..] and set[0] without aux use linear scan.
    pub fn search(&self, target: &BtreeKey) -> Option<(BtreeKey, ExtentValue)> {
        let pos = Bpos::from_key(target);
        let mut iter = BtreeNodeIter::default();
        bch2_btree_node_iter_init(&mut iter, self, &pos);
        let packed = bch2_btree_node_iter_peek(&mut iter, self)?;
        let key = unsafe { &*packed.as_ptr().cast::<BkeyPacked>() };
        let (found, _, _, _) = bkey_unpack(&BKEY_FORMAT_CURRENT, key);
        (found == pos).then(|| self.read_packed_entry(iter.data[0].k as usize * 8))
    }

    /// Build 1-indexed Eytzinger array from sorted keys
    pub fn build_eytzinger(keys: &[(BtreeKey, BchVal)]) -> Vec<(BtreeKey, BchVal)> {
        let n = keys.len();
        if n == 0 {
            return Vec::new();
        }
        let mut result = vec![(BtreeKey::MIN_KEY, BchVal::new(0, 0)); n + 1];
        Self::build_rec(keys, &mut result, 0, n, 1);
        result
    }

    fn build_rec(
        keys: &[(BtreeKey, BchVal)],
        out: &mut [(BtreeKey, BchVal)],
        l: usize,
        r: usize,
        i: usize,
    ) {
        if l >= r || i >= out.len() {
            return;
        }
        let mid = l + (r - l) / 2;
        out[i] = keys[mid];
        Self::build_rec(keys, out, l, mid, i * 2);
        Self::build_rec(keys, out, mid + 1, r, i * 2 + 1);
    }

    // ── RW aux tree 辅助方法 ───────────────────────────────────────────────

    fn rw_aux_tree(&self, si: usize, idx: usize) -> RwAuxTree {
        let base = self.sets[si].aux_data_offset as usize * 8;
        unsafe {
            self.aux_data
                .as_ptr()
                .add(base + idx * std::mem::size_of::<RwAuxTree>())
                .cast::<RwAuxTree>()
                .read_unaligned()
        }
    }

    fn rw_aux_tree_write(&mut self, si: usize, idx: usize, entry: RwAuxTree) {
        let base = self.sets[si].aux_data_offset as usize * 8;
        unsafe {
            self.aux_data
                .as_mut_ptr()
                .add(base + idx * std::mem::size_of::<RwAuxTree>())
                .cast::<RwAuxTree>()
                .write_unaligned(entry);
        }
    }

    fn rw_aux_to_bkey(&self, si: usize, idx: usize) -> u16 {
        self.rw_aux_tree(si, idx).offset
    }

    fn rw_aux_tree_set(&mut self, si: usize, idx: usize, key: u16) {
        debug_assert!(key < self.sets[si].end_offset);
        let packed = unsafe {
            &*self
                .data
                .as_ptr()
                .add(key as usize * 8)
                .cast::<BkeyPacked>()
        };
        let (pos, _, _, _) = bkey_unpack(&BKEY_FORMAT_CURRENT, packed);
        let mut entry = RwAuxTree::ZERO;
        entry.offset = key;
        entry.set_pos(pos);
        self.rw_aux_tree_write(si, idx, entry);
    }

    fn bset_rw_tree_capacity(&self, si: usize) -> usize {
        let start = self.sets[si].aux_data_offset as usize * 8;
        self.aux_data.len().saturating_sub(start) / std::mem::size_of::<RwAuxTree>()
    }

    fn rw_aux_tree_bsearch(&self, si: usize, offset: u16) -> usize {
        let set = &self.sets[si];
        let bset_offs = offset - set.first_key_offset();
        let bset_u64s = set.end_offset - set.first_key_offset();
        let mut idx = if bset_u64s != 0 {
            bset_offs as usize * set.size as usize / bset_u64s as usize
        } else {
            0
        };

        debug_assert!(set.has_rw_aux_tree());
        debug_assert_ne!(set.size, 0);
        debug_assert!(idx <= set.size as usize);

        while idx < set.size as usize && self.rw_aux_to_bkey(si, idx) < offset {
            idx += 1;
        }
        while idx != 0 && self.rw_aux_to_bkey(si, idx - 1) >= offset {
            idx -= 1;
        }
        idx
    }

    fn bset_search_write_set(&self, si: usize, search: &Bpos) -> u16 {
        let set = &self.sets[si];
        let mut l = 0usize;
        let mut r = set.size as usize;

        while l + 1 != r {
            let m = (l + r) >> 1;
            if self.rw_aux_tree(si, m).pos() < *search {
                l = m;
            } else {
                r = m;
            }
        }

        self.rw_aux_to_bkey(si, l)
    }

    fn bset_search_tree(&self, si: usize, search: &Bpos, packed_search: &BkeyPacked) -> u16 {
        let set = &self.sets[si];
        let mut n = 1usize;
        let mut float;

        loop {
            float = self.ro_aux_tree(si, n);
            if float.exponent < BFLOAT_FAILED {
                let left = float.mantissa;
                let right = Self::bkey_mantissa(packed_search, float);
                let bits_dropped = float.exponent
                    > BKEY_FORMAT_CURRENT.key_u64s as u8 * 64
                        - BKEY_FORMAT_CURRENT.bpos_key_bits() as u8;
                if left != right || !bits_dropped {
                    n = n * 2 + usize::from(left < right);
                    if n >= set.size as usize {
                        break;
                    }
                    continue;
                }
            }

            let key = self.tree_to_bkey(si, n);
            let packed = unsafe {
                &*self
                    .data
                    .as_ptr()
                    .add(key as usize * 8)
                    .cast::<BkeyPacked>()
            };
            let cmp = bkey_cmp_packed_vs_bpos(&BKEY_FORMAT_CURRENT, packed, search);
            if cmp == std::cmp::Ordering::Equal {
                return key;
            }
            n = n * 2 + usize::from(cmp == std::cmp::Ordering::Less);
            if n >= set.size as usize {
                break;
            }
        }

        let mut inorder =
            __eytzinger1_to_inorder(n >> 1, set.size as usize - 1, set.extra as usize);
        if n & 1 == 0 {
            inorder -= 1;
            if inorder == 0 {
                return set.first_key_offset();
            }
            float = self.ro_aux_tree(si, eytzinger1_prev(n >> 1, set.size as usize - 1));
        }
        self.cacheline_to_bkey(si, inorder, float.key_offset)
    }

    fn __bch2_bset_search(&self, si: usize, search: &Bpos, packed_search: &BkeyPacked) -> u16 {
        let set = &self.sets[si];
        match set.aux_tree_type() {
            BsetAuxTreeType::NoAuxTree => set.first_key_offset(),
            BsetAuxTreeType::RwAuxTree => self.bset_search_write_set(si, search),
            BsetAuxTreeType::RoAuxTree => self.bset_search_tree(si, search, packed_search),
        }
    }

    fn rw_aux_tree_insert_entry(&mut self, si: usize, idx: usize) {
        debug_assert_ne!(idx, 0);
        debug_assert!(idx <= self.sets[si].size as usize);
        let start = self.rw_aux_to_bkey(si, idx - 1);
        let end = if idx < self.sets[si].size as usize {
            self.rw_aux_to_bkey(si, idx)
        } else {
            self.sets[si].end_offset
        };

        if (self.sets[si].size as usize) < self.bset_rw_tree_capacity(si)
            && u32::from(end - start) * 8 > L1_CACHE_BYTES
        {
            let mut key = start;
            loop {
                key += self.read_entry_u64s(key as usize * 8) as u16;
                if key == end {
                    break;
                }

                if u32::from(key - start) * 8 >= L1_CACHE_BYTES {
                    let base = self.sets[si].aux_data_offset as usize * 8;
                    let entry_size = std::mem::size_of::<RwAuxTree>();
                    let count = self.sets[si].size as usize - idx;
                    unsafe {
                        std::ptr::copy(
                            self.aux_data.as_ptr().add(base + idx * entry_size),
                            self.aux_data
                                .as_mut_ptr()
                                .add(base + (idx + 1) * entry_size),
                            count * entry_size,
                        );
                    }
                    self.sets[si].size += 1;
                    self.rw_aux_tree_set(si, idx, key);
                    break;
                }
            }
        }
    }

    // ── bcachefs 对齐: `__bch2_bset_fix_lookup_table` ──────────────────

    /// bcachefs `__bch2_bset_fix_lookup_table()` (`bset.c:1190`).
    fn __bch2_bset_fix_lookup_table(
        &mut self,
        si: usize,
        where_off: u16,
        clobber_u64s: u16,
        new_u64s: u16,
    ) {
        let shift = new_u64s as i32 - clobber_u64s as i32;
        let mut idx;

        debug_assert!(!self.sets[si].has_ro_aux_tree());

        if where_off > self.rw_aux_to_bkey(si, self.sets[si].size as usize - 1) {
            self.rw_aux_tree_insert_entry(si, self.sets[si].size as usize);
            return;
        }

        idx = self.rw_aux_tree_bsearch(si, where_off);

        if self.rw_aux_to_bkey(si, idx) == where_off {
            if idx == 0 {
                idx += 1;
            } else if where_off < self.sets[si].end_offset {
                self.rw_aux_tree_set(si, idx, where_off);
                idx += 1;
            } else {
                debug_assert_eq!(where_off, self.sets[si].end_offset);
                self.sets[si].size -= 1;
                self.rw_aux_tree_insert_entry(si, self.sets[si].size as usize);
                return;
            }
        }

        debug_assert!(
            idx >= self.sets[si].size as usize || self.rw_aux_to_bkey(si, idx) > where_off
        );

        if idx < self.sets[si].size as usize
            && self.rw_aux_to_bkey(si, idx) as i32 + shift
                == self.rw_aux_to_bkey(si, idx - 1) as i32
        {
            let base = self.sets[si].aux_data_offset as usize * 8;
            let entry_size = std::mem::size_of::<RwAuxTree>();
            let count = self.sets[si].size as usize - idx - 1;
            unsafe {
                std::ptr::copy(
                    self.aux_data.as_ptr().add(base + (idx + 1) * entry_size),
                    self.aux_data.as_mut_ptr().add(base + idx * entry_size),
                    count * entry_size,
                );
            }
            self.sets[si].size -= 1;
        }

        for j in idx..self.sets[si].size as usize {
            let mut entry = self.rw_aux_tree(si, j);
            entry.offset = (entry.offset as i32 + shift) as u16;
            self.rw_aux_tree_write(si, j, entry);
        }

        debug_assert!(
            idx >= self.sets[si].size as usize
                || self.rw_aux_to_bkey(si, idx) != self.rw_aux_to_bkey(si, idx - 1)
        );

        self.rw_aux_tree_insert_entry(si, idx);
    }

    fn bch2_bset_fix_lookup_table(
        &mut self,
        si: usize,
        where_off: u16,
        clobber_u64s: u16,
        new_u64s: u16,
    ) {
        if self.sets[si].has_rw_aux_tree() {
            self.__bch2_bset_fix_lookup_table(si, where_off, clobber_u64s, new_u64s);
        }
    }

    // ── bcachefs 对齐: `bch2_bset_insert` ──────────────────────────────

    /// bcachefs `bch2_bset_insert()` (`bset.c:1255`).
    pub fn bch2_bset_insert(&mut self, where_off: u16, insert: &BtreeEntry, clobber_u64s: u16) {
        let si = self.nsets as usize - 1;
        let new_u64s = entry_packed_size(insert) as u16 / 8;

        if new_u64s != clobber_u64s {
            let src = (where_off + clobber_u64s) as usize * 8;
            let dst = (where_off + new_u64s) as usize * 8;
            let count = (self.sets[si].end_offset - where_off - clobber_u64s) as usize * 8;
            unsafe {
                std::ptr::copy(
                    self.data.as_ptr().add(src),
                    self.data.as_mut_ptr().add(dst),
                    count,
                );
            }
            self.sets[si].end_offset =
                (self.sets[si].end_offset as i32 + new_u64s as i32 - clobber_u64s as i32) as u16;
            let u64s = self.bset_header(si).u64s;
            self.set_bset_u64s(
                si,
                (u64s as i32 + new_u64s as i32 - clobber_u64s as i32) as u16,
            );
        }

        let actual = self.write_entry_bytes(where_off as u32 * 8, insert);
        debug_assert_eq!(actual, new_u64s as u32 * 8);

        if new_u64s != clobber_u64s {
            self.bch2_bset_fix_lookup_table(si, where_off, clobber_u64s, new_u64s);
        }
    }

    // ── bcachefs 对齐: `bch2_bset_delete` ──────────────────────────────

    /// bcachefs `bch2_bset_delete()` (`bset.c:1296`).
    pub fn bch2_bset_delete(&mut self, where_off: u16, clobber_u64s: u16) {
        let si = self.nsets as usize - 1;
        let src = (where_off + clobber_u64s) as usize * 8;
        let dst = where_off as usize * 8;
        let count = (self.sets[si].end_offset - where_off - clobber_u64s) as usize * 8;

        unsafe {
            std::ptr::copy(
                self.data.as_ptr().add(src),
                self.data.as_mut_ptr().add(dst),
                count,
            );
        }
        self.sets[si].end_offset -= clobber_u64s;
        let u64s = self.bset_header(si).u64s;
        debug_assert!(u64s >= clobber_u64s);
        self.set_bset_u64s(si, u64s - clobber_u64s);

        self.bch2_bset_fix_lookup_table(si, where_off, clobber_u64s, 0);
    }

    /// bcachefs `bch2_push_whiteout()` (`interior.c:44`).
    pub fn bch2_push_whiteout(&mut self, pos: Bpos) {
        let mut packed = [0u8; super::key::BKEY_U64S as usize * 8];
        let key = unsafe { &mut *packed.as_mut_ptr().cast::<BkeyPacked>() };
        bkey_pack_raw(key, pos, KeyType::Deleted as u8, &[], &BKEY_FORMAT_CURRENT);
        key.set_needs_whiteout(true);

        let new_whiteout_u64s = self.whiteout_u64s + key.u64s as u16;
        let data_end = self.node_size as usize - new_whiteout_u64s as usize * 8;
        let last_set_end = self.sets[self.nsets as usize - 1].end_offset as usize * 8;
        assert!(last_set_end <= data_end);

        self.whiteout_u64s = new_whiteout_u64s;
        self.data[data_end..data_end + key.u64s as usize * 8]
            .copy_from_slice(&packed[..key.u64s as usize * 8]);
    }

    /// bcachefs 对齐: `bch2_set_bset_needs_whiteout()` (`sort.c:274-280`).
    pub(crate) fn bch2_set_bset_needs_whiteout(&mut self, si: usize, value: bool) {
        let mut offset = usize::from(self.sets[si].first_key_offset()) * 8;
        let end = usize::from(self.sets[si].end_offset) * 8;

        while offset < end {
            let packed = unsafe { &mut *self.data.as_mut_ptr().add(offset).cast::<BkeyPacked>() };
            let bytes = usize::from(packed.u64s) * 8;
            packed.set_needs_whiteout(value);
            offset += bytes;
        }
    }

    /// Insert a key-value into the incremental bset
    pub fn insert(&mut self, key: BtreeKey, value: BchVal) -> bool {
        self.insert_entry(&BtreeEntry::from((key, value)))
    }

    /// bcachefs `bch2_btree_bset_insert_key()` (`commit.c:182`).
    pub fn bch2_btree_bset_insert_key(
        &mut self,
        node_iter: &mut BtreeNodeIter,
        insert: &BtreeEntry,
    ) -> bool {
        let mut insert = insert.clone();
        let mut key = if !node_iter.is_end() {
            let offset = node_iter.data[0].k;
            let packed = unsafe {
                &*self
                    .data
                    .as_ptr()
                    .add(offset as usize * 8)
                    .cast::<BkeyPacked>()
            };
            let (pos, _, _, _) = bkey_unpack(&BKEY_FORMAT_CURRENT, packed);
            (pos == insert.pos).then_some(offset)
        } else {
            None
        };

        if insert.key_type == KeyType::Deleted && key.is_none() {
            return false;
        }

        let last = self.nsets as usize - 1;
        let key_writeable = key.is_some_and(|offset| offset >= self.sets[last].first_key_offset());

        if let Some(offset) = key {
            let (needs_whiteout, old_u64s) = unsafe {
                let packed = &mut *self
                    .data
                    .as_mut_ptr()
                    .add(offset as usize * 8)
                    .cast::<BkeyPacked>();
                debug_assert_ne!(packed.type_, KeyType::Deleted as u8);
                let needs_whiteout = packed.needs_whiteout();
                let old_u64s = packed.u64s as u16;
                packed.type_ = KeyType::Deleted as u8;
                packed.set_needs_whiteout(false);
                (needs_whiteout, old_u64s)
            };
            self.packed_keys = self.packed_keys.saturating_sub(1);

            if needs_whiteout {
                if insert.key_type == KeyType::Deleted {
                    self.bch2_push_whiteout(insert.pos);
                } else {
                    insert.needs_whiteout = true;
                }
            }

            if !key_writeable {
                bch2_btree_node_iter_sort(node_iter, self);
            }

            debug_assert_eq!(old_u64s, self.read_entry_u64s(offset as usize * 8) as u16);
        }

        let clobber_u64s = if key_writeable {
            self.read_entry_u64s(key.unwrap() as usize * 8) as u16
        } else {
            0
        };

        if insert.key_type == KeyType::Deleted {
            if key_writeable {
                self.bch2_bset_delete(key.unwrap(), clobber_u64s);
                // bcachefs: bset_delete 物理移动数据后 rw_aux_tree 失效，
                // 清除并重建 aux tree
                self.sets[last].size = 0;
                self.sets[last].extra = BSET_NO_AUX_TREE_VAL;
                self.sets[last].aux_data_offset = u16::MAX;
                self.bch2_bset_build_aux_tree(last, true);
            }
        } else {
            if !key_writeable {
                key = Some(bch2_btree_node_iter_bset_pos(
                    node_iter,
                    self,
                    &self.sets[last],
                ));
            }

            self.bch2_bset_insert(key.unwrap(), &insert, clobber_u64s);
            self.packed_keys += 1;
        }

        let new_u64s = if insert.key_type != KeyType::Deleted {
            self.read_entry_u64s(key.unwrap() as usize * 8) as u16
        } else {
            0
        };
        if clobber_u64s != new_u64s {
            bch2_btree_node_iter_init(node_iter, self, &insert.pos);
        }

        true
    }

    pub fn insert_entry(&mut self, entry: &BtreeEntry) -> bool {
        let insert_u64s = entry_packed_size(entry) / 8;
        if !self.bch2_btree_node_insert_fits(insert_u64s) {
            return false;
        }

        let last = self.nsets as usize - 1;
        self.bch2_bset_build_aux_tree(last, true);
        let mut node_iter = BtreeNodeIter::default();
        bch2_btree_node_iter_init(&mut node_iter, self, &entry.pos);
        self.bch2_btree_bset_insert_key(&mut node_iter, entry)
    }

    pub fn delete_key(&mut self, target: &BtreeKey) -> bool {
        let pos = Bpos::from_key(target);
        self.insert_entry(&BtreeEntry::raw(pos, KeyType::Deleted, Vec::new()))
    }

    /// 从所有 bsets 收集所有条目（不写入 buffer），按 (offset, snapshot DESC) 排序去重
    ///
    /// 与 compact() 一样使用 BtreeEntry（read_packed_entry_raw）以保留 KeyValue::Raw 数据。
    fn collect_all_entries(&self) -> Vec<BtreeEntry> {
        let mut all: Vec<BtreeEntry> = Vec::new();
        for si in 0..self.nsets as usize {
            let s = &self.sets[si];
            let mut cur = u32::from(s.first_key_offset()) * 8;
            while cur < u32::from(s.end_offset) * 8 {
                all.push(self.read_packed_entry_raw(cur as usize));
                let u64s = self.read_entry_u64s(cur as usize);
                cur += (u64s as u32) * 8;
            }
        }
        // 排序 + 去重 + 过滤 whiteout（与 compact 相同的逻辑）
        all.sort_by(|a, b| match a.pos.offset.cmp(&b.pos.offset) {
            std::cmp::Ordering::Equal => b.pos.snapshot.cmp(&a.pos.snapshot),
            other => other,
        });
        all.reverse();
        all.dedup_by(|a, b| a.pos.offset == b.pos.offset && a.pos.snapshot == b.pos.snapshot);
        all.reverse();
        all.retain(|e| e.key_type != KeyType::Deleted);
        all
    }

    /// 收集本次 node write 应写入的 key。
    ///
    /// 对应本地 `__bch2_btree_node_write()` 添加所有未写 bset 与 unwritten-whiteout
    /// suffix 后调用 `bch2_sort_keys_keep_unwritten_whiteouts()`：普通 bset 中的 Deleted
    /// 被丢弃；suffix whiteout 仅在没有同位置真实 key 时保留；同位置真实 key 取内存中
    /// 最后一个版本。
    fn collect_unwritten_entries(&self) -> Vec<BtreeEntry> {
        let mut real = std::collections::BTreeMap::<Bpos, BtreeEntry>::new();
        let mut whiteouts = std::collections::BTreeMap::<Bpos, BtreeEntry>::new();

        for si in 0..self.nsets as usize {
            if self.bset_written(si) {
                continue;
            }

            let set = &self.sets[si];
            let mut offset = usize::from(set.first_key_offset()) * 8;
            let end = usize::from(set.end_offset) * 8;
            while offset < end {
                let entry = self.read_packed_entry_raw(offset);
                let u64s = self.read_entry_u64s(offset);
                if entry.key_type != KeyType::Deleted {
                    real.insert(entry.pos, entry);
                }
                offset += usize::from(u64s) * 8;
            }
        }

        let mut offset = self.node_size as usize - usize::from(self.whiteout_u64s) * 8;
        while offset < self.node_size as usize {
            let entry = self.read_packed_entry_raw(offset);
            let u64s = self.read_entry_u64s(offset);
            whiteouts.insert(entry.pos, entry);
            offset += usize::from(u64s) * 8;
        }

        for (pos, entry) in whiteouts {
            real.entry(pos).or_insert(entry);
        }

        real.into_values().collect()
    }

    /// Bcachefs‑aligned split using find_balanced_split + pack_entries_into
    ///
    /// 1. Collect all entries from all bsets (without writing to buffer)
    /// 2. Sort + dedup + filter whiteout
    /// 3. Use find_balanced_split to find ~60%/40% split point with shard alignment
    /// 4. Self keeps the left half, returns a new node with the right half
    /// 5. Returns (median_key, new_node)
    ///
    /// Like the old split(), does NOT call compact() because aux array may not fit.
    pub fn split(&mut self) -> Option<(BtreeKey, Self)> {
        let all = self.collect_all_entries();
        let n = all.len();
        if n < 2 {
            return None;
        }

        let (mid, median_key) = Self::find_balanced_split(&all, self.node_size)?;
        let old_min = if self.min_key <= self.max_key {
            self.min_key
        } else {
            Bpos::MIN
        };
        let old_max = if self.min_key <= self.max_key {
            self.max_key
        } else {
            Bpos::MAX
        };

        let mut r_node = Self::new(self.level);
        Self::pack_entries_into(self, all, mid, &mut r_node);
        self.min_key = old_min;
        self.max_key = median_key.to_bpos().predecessor();
        r_node.min_key = median_key.to_bpos();
        r_node.max_key = old_max;

        Some((median_key, r_node))
    }

    // -----------------------------------------------------------------------
    // Balanced split helpers  (bcachefs‑aligned)
    // -----------------------------------------------------------------------

    /// 估算 split 后两侧的总 packed 字节数（含 aux 开销）
    ///
    /// bcachefs `predict_split()` 对齐。对每个候选分界点计算：
    /// - 左侧：`left_data_bytes + left_count * aux_per_entry`
    /// - 右侧：`right_data_bytes + right_count * aux_per_entry`
    /// 用于在 `find_balanced_split` 中选择最平衡的分界点。
    fn predict_split_bias(entries: &[BtreeEntry], split_idx: usize) -> (u32, u32) {
        let aes = std::mem::size_of::<BtreeKey>() + 4; // aux entry 字节数
        let mut left_data = 0u32;
        let mut right_data = 0u32;
        for (i, e) in entries.iter().enumerate() {
            let value_bytes = e.value.to_bytes();
            let value_u64s = value_bytes.len().div_ceil(8);
            let entry_bytes = (super::key::BKEY_U64S as u32 + value_u64s as u32) * 8;
            if i < split_idx {
                left_data += entry_bytes;
            } else {
                right_data += entry_bytes;
            }
        }
        let left_aux = split_idx as u32 * aes as u32;
        let right_aux = (entries.len() - split_idx) as u32 * aes as u32;
        (left_data + left_aux, right_data + right_aux)
    }

    /// Find the balanced split point.
    ///
    /// - Targets 60 %/40 % distribution (left gets 3/5 of total u64s)
    /// - Prefers shard boundaries (find_shard_split)
    /// - Uses `predict_split_bias` to validate format-adjusted packing balance
    /// - Returns (mid_index, median_key) where entries[..mid] → left, entries[mid..] → right
    pub fn find_balanced_split(
        entries: &[BtreeEntry],
        node_size: u32,
    ) -> Option<(usize, BtreeKey)> {
        let n = entries.len();
        if n < 2 {
            return None;
        }

        let entry_eff_u64s: Vec<u32> = entries
            .iter()
            .map(|e| {
                let value_bytes = e.value.to_bytes();
                let value_u64s = value_bytes.len().div_ceil(8);
                // A3: 加入每个 entry 的 aux 开销（3 u64s = 24 B）到有效大小中
                // 使平衡分裂时考虑 per-entry aux 数组的 overhead
                let aux_overhead = (std::mem::size_of::<BtreeKey>() + 4).div_ceil(8) as u32; // 20B → 3 u64s
                super::key::BKEY_U64S as u32 + value_u64s as u32 + aux_overhead
            })
            .collect();

        let total_eff: u32 = entry_eff_u64s.iter().sum();
        let target_eff = balance_target_u64s(total_eff);

        // Find split point: accumulate effective sizes until reaching 3/5 of total
        let mut accumulated = 0u32;
        let mut mid = n / 2; // fallback: entry-count midpoint
        for (i, &u) in entry_eff_u64s.iter().enumerate() {
            if accumulated >= target_eff && i > 0 {
                mid = i;
                break;
            }
            accumulated += u;
        }

        // Shard alignment: prefer splitting at a shard boundary
        if let Some(shard_idx) = Self::find_shard_split(entries) {
            mid = shard_idx;
        }

        // Ensure mid is at least 20% from each end
        let min_split = n / 5;
        let max_split = n - n / 5;
        mid = mid.clamp(min_split, max_split);

        // A3: format-aware validation — 验证两侧 packed size 不会导致某侧超出容量
        // 如果左侧预测大小超过 node_size * 3/4，向右偏移 split 点
        if node_size > 0 {
            let max_side = node_size * 3 / 4;
            let (left_total, _) = Self::predict_split_bias(entries, mid);
            if left_total > max_side && mid < max_split {
                // 左侧过大，尝试右移 split 点
                for new_mid in (mid + 1)..=max_split {
                    let (new_left, _) = Self::predict_split_bias(entries, new_mid);
                    if new_left <= max_side {
                        mid = new_mid;
                        break;
                    }
                }
            }
            // 如果还是超出，保持 mid 不变（split 仍能工作，只是 left 节点较满）
        }

        let median_key = BtreeKey::from_bpos(entries[mid].pos, entries[mid].key_type);
        Some((mid, median_key))
    }

    /// Check if the node's key span covers a shard boundary and return
    /// the preferred split index. A shard boundary exists at positions
    /// that are multiples of SHARD_FACTOR (1024).
    fn find_shard_split(entries: &[BtreeEntry]) -> Option<usize> {
        const SHARD_FACTOR: u64 = 1024;
        let n = entries.len();
        if n < 3 {
            return None;
        }

        for (i, entry) in entries.iter().enumerate() {
            if i == 0 {
                continue;
            }
            let prev_off = entries[i - 1].pos.offset;
            let curr_off = entry.pos.offset;
            // Crosses a shard boundary: prev < N*SHARD_FACTOR <= curr
            if prev_off / SHARD_FACTOR < curr_off / SHARD_FACTOR {
                // Only use if it's a reasonable split (>20% from each end)
                if i > n / 5 && i < n * 4 / 5 {
                    return Some(i);
                }
            }
        }
        None
    }

    /// Pack entries into left (self) and right nodes according to the balanced split.
    pub(crate) fn pack_entries_into(
        &mut self,
        entries: Vec<BtreeEntry>,
        mid: usize,
        right: &mut Self,
    ) {
        // Write self (left half)
        let mut l_cur = u32::from(BSET_HEADER_U64S) * 8;
        for entry in entries.iter().take(mid) {
            l_cur += self.write_entry_bytes(l_cur, entry);
        }
        self.sets[0] = BsetTree {
            size: 0,
            extra: BSET_NO_AUX_TREE_VAL,
            data_offset: 0,
            aux_data_offset: u16::MAX,
            end_offset: (l_cur / 8) as u16,
        };
        self.set_bset_u64s(0, self.sets[0].end_offset - self.sets[0].first_key_offset());
        self.write_bset_header(
            0,
            BsetHeader {
                seq: 0,
                journal_seq: 0,
                flags: 0,
                version: 0,
                u64s: self.sets[0].end_offset - self.sets[0].first_key_offset(),
            },
        );
        self.nsets = 1;
        self.packed_keys = mid as u16;
        self.unpacked_keys = 0;
        for i in 1..BSET_COUNT {
            self.sets[i] = BsetTree {
                size: 0,
                extra: BSET_NO_AUX_TREE_VAL,
                data_offset: u16::MAX,
                aux_data_offset: u16::MAX,
                end_offset: u16::MAX,
            };
        }
        // 设置左节点的 key 范围
        if mid > 0 {
            self.min_key = entries[0].pos;
            self.max_key = entries[mid - 1].pos;
        }

        // Write right node
        let mut r_cur = u32::from(BSET_HEADER_U64S) * 8;
        for entry in entries.iter().skip(mid) {
            r_cur += right.write_entry_bytes(r_cur, entry);
        }
        right.sets[0] = BsetTree {
            size: 0,
            extra: BSET_NO_AUX_TREE_VAL,
            data_offset: 0,
            aux_data_offset: u16::MAX,
            end_offset: (r_cur / 8) as u16,
        };
        right.set_bset_u64s(
            0,
            right.sets[0].end_offset - right.sets[0].first_key_offset(),
        );
        right.write_bset_header(
            0,
            BsetHeader {
                seq: 0,
                journal_seq: 0,
                flags: 0,
                version: 0,
                u64s: right.sets[0].end_offset - right.sets[0].first_key_offset(),
            },
        );
        right.nsets = 1;
        right.packed_keys = (entries.len() - mid) as u16;
        right.unpacked_keys = 0;
        for i in 1..BSET_COUNT {
            right.sets[i] = BsetTree {
                size: 0,
                extra: BSET_NO_AUX_TREE_VAL,
                data_offset: u16::MAX,
                aux_data_offset: u16::MAX,
                end_offset: u16::MAX,
            };
        }
        // 设置右节点的 key 范围
        let right_count = entries.len() - mid;
        if right_count > 0 {
            right.min_key = entries[mid].pos;
            right.max_key = entries[entries.len() - 1].pos;
        }
    }

    /// Check if this node can absorb entries from another node
    ///
    /// Both nodes should be compacted first.
    /// Returns true if combined entries fit within node_size.
    /// 使用实际 data bytes（支持变长 KeyValue::Raw）。
    pub fn can_absorb(&self, other: &Self) -> bool {
        let self_bytes = self.total_data_bytes();
        let other_bytes = other.total_data_bytes();
        let es = entry_size(); // 一个 entry 的最小预留空间（保守估计）
                               // 吸收后节点必须留出至少一个 entry 的写入空间（用于 Deleted 墓碑）。
                               // 注意：吸收后的 compact 可能因 aux 数组（每 entry ~20B）放不下而跳过 aux，
                               // 但数据本身必须在节点容量内，且还需保留 1 个 entry 的空间给后续写入。
        self_bytes + other_bytes + es + 64 <= self.node_size
    }

    /// Absorb all entries from `other` into `self`
    ///
    /// Both nodes must be compacted first.
    /// After absorb, `other` is left empty.
    pub fn absorb(&mut self, other: &mut Self) {
        self.compact();
        other.compact();
        let self_count = (self.packed_keys + self.unpacked_keys) as usize;
        let other_count = (other.packed_keys + other.unpacked_keys) as usize;

        // Collect all self entries first (before we overwrite the buffer)
        let mut all: Vec<(BtreeKey, ExtentValue)> = Vec::with_capacity(self_count + other_count);
        for i in 1..=self_count {
            all.push(self.read_entry(&self.sets[0], i));
        }
        for i in 1..=other_count {
            let (k, v) = other.read_entry(&other.sets[0], i);
            all.push((k, v));
        }

        // Re-pack all entries into self's buffer using write_entry_extent (preserve dev_idx)
        let mut cur = u32::from(BSET_HEADER_U64S) * 8;
        for (k, v) in &all {
            cur += self.write_entry_extent(cur, k, v);
        }

        let total = (self_count + other_count) as u32;
        self.sets[0] = BsetTree {
            size: 0,
            extra: BSET_NO_AUX_TREE_VAL,
            data_offset: 0,
            aux_data_offset: u16::MAX,
            end_offset: (cur / 8) as u16,
        };
        self.set_bset_u64s(0, self.sets[0].end_offset - self.sets[0].first_key_offset());
        self.write_bset_header(
            0,
            BsetHeader {
                seq: 0,
                journal_seq: 0,
                flags: 0,
                version: 0,
                u64s: self.sets[0].end_offset - self.sets[0].first_key_offset(),
            },
        );
        self.nsets = 1;
        self.packed_keys = total as u16;
        self.unpacked_keys = 0;

        // re-sort
        self.compact();

        // 更新 key 范围：吸收后 min_key = min(self.min, other.min)，max_key 取所有 entries 的最大值
        // 由于双方 compact 过且合并后再次 compact，直接取 set[0] 首尾即可
        if (self.packed_keys + self.unpacked_keys) > 0 {
            let (first_k, _) = self.read_entry(&self.sets[0], 1);
            self.min_key = Bpos::from_key(&first_k);
            let last_idx = (self.packed_keys + self.unpacked_keys) as usize;
            let (last_k, _) = self.read_entry(&self.sets[0], last_idx);
            self.max_key = Bpos::from_key(&last_k);
        }

        // clear other
        other.sets[0] = BsetTree {
            size: 0,
            extra: BSET_NO_AUX_TREE_VAL,
            data_offset: 0,
            end_offset: BSET_HEADER_U64S,
            aux_data_offset: u16::MAX,
        };
        other.set_bset_u64s(0, 0);
        other.write_bset_header(
            0,
            BsetHeader {
                seq: 0,
                journal_seq: 0,
                flags: 0,
                version: 0,
                u64s: 0,
            },
        );
        other.nsets = 1;
        other.packed_keys = 0;
        other.unpacked_keys = 0;
    }

    /// 对应本地 `bch2_btree_node_compact_fits()`（`interior.h:390-403`）。
    ///
    /// compact 后的完整 node record 和承载新 key 的 follow-on record 必须分别加上
    /// varint read-past-end 余量并按 filesystem block 向上取整，再比较 node sector 预算。
    pub(crate) fn bch2_btree_node_compact_fits(&self, new_key_u64s: u32) -> bool {
        let block_bytes = self
            .vol
            .get()
            .and_then(Weak::upgrade)
            .map(|c| c.block_size() as usize)
            .unwrap_or(BLOCK_SIZE);

        let initial_bytes = std::mem::size_of::<BtreeNodeHeader>()
            + std::mem::size_of::<BsetHeader>()
            + self.live_data_bytes() as usize
            + 8;
        let followon_bytes = std::mem::size_of::<BtreeNodeDiskEntry>()
            + std::mem::size_of::<BsetHeader>()
            + new_key_u64s as usize * std::mem::size_of::<u64>()
            + 8;

        let initial_sectors = initial_bytes.div_ceil(block_bytes) * block_bytes / SECTOR_SIZE;
        let followon_sectors = followon_bytes.div_ceil(block_bytes) * block_bytes / SECTOR_SIZE;

        initial_sectors + followon_sectors <= self.node_size as usize / SECTOR_SIZE
    }

    /// Compact all bsets into a single sorted set[0] with aux array.
    ///
    /// After compact:
    /// - Packed entries are stored sequentially starting at buffer offset 0
    /// - Aux array `(BtreeKey, u32 data_offset)` is placed after packed data
    /// - `aux_data_offset` points to the start of the aux array
    /// - set[0] can use binary search via aux keys
    pub fn compact(&mut self) {
        let mut all: Vec<BtreeEntry> = Vec::new();
        for si in 0..self.nsets as usize {
            let s = &self.sets[si];
            let mut cur = u32::from(s.first_key_offset()) * 8;
            while cur < u32::from(s.end_offset) * 8 {
                all.push(self.read_packed_entry_raw(cur as usize));
                let u64s = self.read_entry_u64s(cur as usize);
                cur += (u64s as u32) * 8;
            }
        }
        all.sort_by(|a, b| a.pos.cmp(&b.pos));
        all.reverse();
        all.dedup_by(|a, b| a.pos == b.pos);
        all.reverse();
        all.retain(|e| e.key_type != KeyType::Deleted);
        let n = all.len();

        let mut cur = u32::from(BSET_HEADER_U64S) * 8;
        for entry in &all {
            cur += self.write_entry_bytes(cur, entry);
        }

        self.sets[0] = BsetTree {
            size: 0,
            extra: BSET_NO_AUX_TREE_VAL,
            data_offset: 0,
            aux_data_offset: u16::MAX,
            end_offset: (cur / 8) as u16,
        };
        self.set_bset_u64s(0, self.sets[0].end_offset - self.sets[0].first_key_offset());
        self.write_bset_header(
            0,
            BsetHeader {
                seq: 0,
                journal_seq: 0,
                flags: 0,
                version: 0,
                u64s: self.sets[0].end_offset - self.sets[0].first_key_offset(),
            },
        );
        for i in 1..BSET_COUNT {
            self.sets[i] = BsetTree {
                size: 0,
                extra: BSET_NO_AUX_TREE_VAL,
                data_offset: u16::MAX,
                aux_data_offset: u16::MAX,
                end_offset: u16::MAX,
            };
        }
        self.nsets = 1;
        self.packed_keys = n as u16;
        self.unpacked_keys = 0;
        self.whiteout_u64s = 0;
        self.bch2_bset_build_aux_tree(0, true);
    }

    // ─── 磁盘序列化 ─────────────────────────────────────────

    /// 序列化当前 node 到字节数组（固定 C 布局，bcachefs 对齐）
    ///
    /// 布局（version >= 2）:
    /// ```text
    /// [BtreeNodeHeader(82B)] [BsetHeader(24B)] [packed entries] [zeros to BLOCK_SIZE]
    /// ```
    /// CRC32C 覆盖 header + BsetHeader + packed entries。
    ///
    /// bcachefs 参考: `struct btree_node` (bcachefs_format.h:1944),
    ///               `struct bset` (bcachefs_format.h:1905)
    ///
    /// 与 bcachefs C 的关键差异:
    /// - subvol 使用精简 82B header（bcachefs `struct btree_node` 是 152B）
    /// - bset header 放在每个 bset 数据前（非 inline）
    /// - bkey_format 从 BKEY_FORMAT_CURRENT 获取（非每节点指定）
    /// 将节点的全部 key 序列化为首个 record（忽略 written 状态）。
    ///
    /// 仅用于测试 roundtrip — bcachefs 生产路径通过
    /// `serialize_initial_record`（`written==0`）和
    /// `serialize_append_record`（`written != 0`）区分。
    pub fn serialize_to_bucket(&self, bucket_addr: u64) -> Result<Vec<u8>, StorageError> {
        let entries = self.collect_all_entries();
        let payload = Self::pack_entries(&entries);
        let header_size = std::mem::size_of::<BtreeNodeHeader>();
        let bset_size = std::mem::size_of::<BsetHeader>();
        debug_assert_eq!(header_size, 88);
        debug_assert_eq!(header_size + bset_size, 112);

        let record_bytes = Self::record_bytes(header_size + bset_size + payload.len())?;
        if record_bytes > self.node_size as usize {
            return Err(StorageError::InvalidBlockSize(record_bytes as u64));
        }
        let mut record = vec![0u8; record_bytes];
        let header = BtreeNodeHeader {
            magic: BTREE_NODE_MAGIC,
            version: BTREE_NODE_VERSION,
            level: self.level,
            node_type: 0,
            packed_keys: entries.len() as u16,
            unpacked_keys: 0,
            bset_count: u16::from(!entries.is_empty()),
            crc32: 0,
            seq: self.journal_seq,
            bucket_addr,
            generation: 1,
            record_bytes: record_bytes as u32,
            min_key_inode: self.min_key.inode,
            min_key_offset: self.min_key.offset,
            min_key_snapshot: self.min_key.snapshot,
            max_key_inode: self.max_key.inode,
            max_key_offset: self.max_key.offset,
            max_key_snapshot: self.max_key.snapshot,
            _pad: [0; 6],
        };
        let bset = Self::make_bset_header(self.journal_seq, 0, payload.len())?;
        unsafe {
            std::ptr::write_unaligned(record.as_mut_ptr().cast::<BtreeNodeHeader>(), header);
            std::ptr::write_unaligned(
                record.as_mut_ptr().add(header_size).cast::<BsetHeader>(),
                bset,
            );
        }
        record[header_size + bset_size..header_size + bset_size + payload.len()]
            .copy_from_slice(&payload);
        Self::write_record_crc(&mut record, 14);
        Ok(record)
    }

    /// 编码 extent 的首个 node record。
    ///
    /// 对应 bcachefs `__bch2_btree_node_write()` 首次写使用
    /// `struct btree_node`（`fs/btree/write.c:440-475`）。
    pub fn serialize_initial_record(
        &self,
        bucket_addr: u64,
        generation: u32,
    ) -> Result<Vec<u8>, StorageError> {
        if self.written != 0 {
            return Err(StorageError::InvalidData(
                "initial btree record requested after node was written".into(),
            ));
        }
        let entries = self.collect_unwritten_entries();
        let payload = Self::pack_entries(&entries);
        let header_size = std::mem::size_of::<BtreeNodeHeader>();
        let bset_size = std::mem::size_of::<BsetHeader>();
        debug_assert_eq!(header_size, 88);
        debug_assert_eq!(header_size + bset_size, 112);

        let record_bytes = Self::record_bytes(header_size + bset_size + payload.len())?;
        if record_bytes > self.node_size as usize {
            return Err(StorageError::InvalidBlockSize(record_bytes as u64));
        }
        let mut record = vec![0u8; record_bytes];
        let header = BtreeNodeHeader {
            magic: BTREE_NODE_MAGIC,
            version: BTREE_NODE_VERSION,
            level: self.level,
            node_type: 0,
            packed_keys: entries.len() as u16,
            unpacked_keys: 0,
            bset_count: u16::from(!entries.is_empty()),
            crc32: 0,
            seq: self.journal_seq,
            bucket_addr,
            generation,
            record_bytes: record_bytes as u32,
            min_key_inode: self.min_key.inode,
            min_key_offset: self.min_key.offset,
            min_key_snapshot: self.min_key.snapshot,
            max_key_inode: self.max_key.inode,
            max_key_offset: self.max_key.offset,
            max_key_snapshot: self.max_key.snapshot,
            _pad: [0; 6],
        };
        let bset = Self::make_bset_header(self.journal_seq, 0, payload.len())?;

        unsafe {
            std::ptr::write_unaligned(record.as_mut_ptr().cast::<BtreeNodeHeader>(), header);
            std::ptr::write_unaligned(
                record.as_mut_ptr().add(header_size).cast::<BsetHeader>(),
                bset,
            );
        }
        record[header_size + bset_size..header_size + bset_size + payload.len()]
            .copy_from_slice(&payload);
        Self::write_record_crc(&mut record, 14);
        Ok(record)
    }

    /// 编码稳定 extent 内的增量 node entry record。
    ///
    /// 对应 bcachefs 后续写使用 `struct btree_node_entry` 并设置
    /// `BSET_OFFSET(i, b->written)`（`fs/btree/write.c:440-527`）。
    pub fn serialize_append_record(
        &self,
        generation: u32,
        sector_offset: u16,
    ) -> Result<Vec<u8>, StorageError> {
        if sector_offset == 0 || sector_offset % SECTORS_PER_BLOCK != 0 {
            return Err(StorageError::InvalidData(format!(
                "append offset {} is not block aligned",
                sector_offset
            )));
        }
        if sector_offset != self.written {
            return Err(StorageError::InvalidData(format!(
                "append offset {} does not match written {}",
                sector_offset, self.written
            )));
        }

        let entries = self.collect_unwritten_entries();
        if entries.is_empty() {
            return Ok(Vec::new());
        }
        let payload = Self::pack_entries(&entries);
        let header_size = std::mem::size_of::<BtreeNodeDiskEntry>();
        let bset_size = std::mem::size_of::<BsetHeader>();
        debug_assert_eq!(header_size + bset_size, 56);

        let record_bytes = Self::record_bytes(header_size + bset_size + payload.len())?;
        let end_sectors = sector_offset as usize + record_bytes / SECTOR_SIZE;
        if end_sectors > self.node_size as usize / SECTOR_SIZE {
            return Err(StorageError::InvalidBlockSize(record_bytes as u64));
        }

        let mut record = vec![0u8; record_bytes];
        let header = BtreeNodeDiskEntry {
            magic: BTREE_NODE_ENTRY_MAGIC,
            version: BTREE_NODE_VERSION,
            _flags: 0,
            seq: self.journal_seq,
            generation,
            record_bytes: record_bytes as u32,
            crc32: 0,
            _pad: 0,
        };
        let bset = Self::make_bset_header(self.journal_seq, sector_offset, payload.len())?;
        unsafe {
            std::ptr::write_unaligned(record.as_mut_ptr().cast::<BtreeNodeDiskEntry>(), header);
            std::ptr::write_unaligned(
                record.as_mut_ptr().add(header_size).cast::<BsetHeader>(),
                bset,
            );
        }
        record[header_size + bset_size..header_size + bset_size + payload.len()]
            .copy_from_slice(&payload);
        Self::write_record_crc(&mut record, 24);
        Ok(record)
    }

    /// 按持久 pointer 的 `sectors_written` 边界解析完整 node record 日志。
    pub fn deserialize_from_extent(
        data: &[u8],
        ptr: crate::btree::types::BtreePtrV2,
    ) -> Result<Self, StorageError> {
        if !ptr.is_valid() {
            return Err(StorageError::InvalidData("invalid btree pointer".into()));
        }
        if ptr.sectors_written % SECTORS_PER_BLOCK != 0 {
            return Err(StorageError::InvalidData(
                "btree pointer sectors_written is not block aligned".into(),
            ));
        }
        let committed_bytes = ptr.sectors_written as usize * SECTOR_SIZE;
        if data.len() < committed_bytes {
            return Err(StorageError::InvalidData(format!(
                "btree extent has {} bytes, pointer commits {}",
                data.len(),
                committed_bytes
            )));
        }

        let mut offset = 0usize;
        let mut latest = std::collections::HashMap::<Bpos, BtreeEntry>::new();
        let initial_header_size = std::mem::size_of::<BtreeNodeHeader>();
        if committed_bytes < initial_header_size {
            return Err(StorageError::InvalidData(
                "btree extent is shorter than initial header".into(),
            ));
        }
        let initial: BtreeNodeHeader =
            unsafe { std::ptr::read_unaligned(data.as_ptr().cast::<BtreeNodeHeader>()) };
        let initial_magic = { initial.magic };
        let initial_version = { initial.version };
        let initial_level = { initial.level };
        let initial_addr = { initial.bucket_addr };
        let initial_generation = { initial.generation };
        let initial_record_bytes = { initial.record_bytes as usize };
        if initial_magic != BTREE_NODE_MAGIC
            || initial_version != BTREE_NODE_VERSION
            || initial_level != ptr.level
            || initial_addr != ptr.block_addr
            || initial_generation != ptr.generation
        {
            return Err(StorageError::InvalidData(
                "btree initial record does not match pointer".into(),
            ));
        }
        Self::validate_record_len(initial_record_bytes, committed_bytes)?;
        Self::verify_record_crc(&data[..initial_record_bytes], 14, initial.crc32)?;
        let initial_entries = Self::parse_record_bset(
            &data[..initial_record_bytes],
            initial_header_size,
            0,
            ptr.level,
        )?;
        if initial_entries.len() != (initial.packed_keys as usize) + (initial.unpacked_keys as usize) {
            return Err(StorageError::InvalidData(
                "btree initial key count mismatch".into(),
            ));
        }
        for entry in initial_entries {
            latest.insert(entry.pos, entry);
        }
        let min_key = initial.min_key();
        let max_key = initial.max_key();
        let mut max_seq = initial.seq;
        offset += initial_record_bytes;

        while offset < committed_bytes {
            if committed_bytes - offset < std::mem::size_of::<BtreeNodeDiskEntry>() {
                return Err(StorageError::InvalidData(
                    "truncated btree append header".into(),
                ));
            }
            let header: BtreeNodeDiskEntry = unsafe {
                std::ptr::read_unaligned(data.as_ptr().add(offset).cast::<BtreeNodeDiskEntry>())
            };
            let magic = { header.magic };
            let version = { header.version };
            let generation = { header.generation };
            let record_bytes = { header.record_bytes as usize };
            let stored_crc = { header.crc32 };
            if magic != BTREE_NODE_ENTRY_MAGIC
                || version != BTREE_NODE_VERSION
                || generation != ptr.generation
            {
                return Err(StorageError::InvalidData(
                    "invalid btree append record header".into(),
                ));
            }
            Self::validate_record_len(record_bytes, committed_bytes - offset)?;
            let record = &data[offset..offset + record_bytes];
            Self::verify_record_crc(record, 24, stored_crc)?;
            let entries = Self::parse_record_bset(
                record,
                std::mem::size_of::<BtreeNodeDiskEntry>(),
                (offset / SECTOR_SIZE) as u16,
                ptr.level,
            )?;
            for entry in entries {
                latest.insert(entry.pos, entry);
            }
            max_seq = max_seq.max(header.seq);
            offset += record_bytes;
        }
        if offset != committed_bytes {
            return Err(StorageError::InvalidData(
                "btree records do not end at sectors_written".into(),
            ));
        }

        let mut entries: Vec<BtreeEntry> = latest.into_values().collect();
        entries.retain(|entry| entry.key_type != KeyType::Deleted);
        entries.sort_by(|a, b| {
            a.pos
                .offset
                .cmp(&b.pos.offset)
                .then_with(|| b.pos.snapshot.cmp(&a.pos.snapshot))
        });

        let mut node = BtreeNode::new(ptr.level);
        node.min_key = min_key;
        node.max_key = max_key;
        node.journal_seq = max_seq;
        node.written = ptr.sectors_written;
        let mut total_written = u32::from(BSET_HEADER_U64S) * 8;
        for entry in &entries {
            let entry_bytes = entry_packed_size(entry);
            if total_written + entry_bytes > node.node_size {
                return Err(StorageError::InvalidData(
                    "decoded btree entries exceed node capacity".into(),
                ));
            }
            total_written += node.write_entry_bytes(total_written, entry);
        }
        if !entries.is_empty() {
            node.sets[0] = BsetTree {
                size: 0,
                extra: BSET_NO_AUX_TREE_VAL,
                data_offset: 0,
                aux_data_offset: u16::MAX,
                end_offset: (total_written / 8) as u16,
            };
            node.set_bset_u64s(0, node.sets[0].end_offset - node.sets[0].first_key_offset());
            node.packed_keys = entries.len() as u16;
            node.unpacked_keys = 0;
        }
        Ok(node)
    }

    /// 首 record 兼容 wrapper；生产恢复使用 `deserialize_from_extent()`。
    pub fn deserialize_from_bucket(data: &[u8]) -> Result<Self, StorageError> {
        if data.len() < std::mem::size_of::<BtreeNodeHeader>() {
            return Err(StorageError::InvalidData(
                "data too short for btree node header".into(),
            ));
        }
        let header: BtreeNodeHeader =
            unsafe { std::ptr::read_unaligned(data.as_ptr().cast::<BtreeNodeHeader>()) };
        let magic = { header.magic };
        let version = { header.version };
        if magic != BTREE_NODE_MAGIC || version != BTREE_NODE_VERSION {
            return Err(StorageError::InvalidData(format!(
                "unsupported btree node format version {}",
                version
            )));
        }
        let record_bytes = header.record_bytes as usize;
        Self::validate_record_len(record_bytes, data.len())?;
        let ptr = crate::btree::types::BtreePtrV2 {
            block_addr: header.bucket_addr,
            sectors_written: (record_bytes / SECTOR_SIZE) as u16,
            level: header.level,
            dev_idx: 0,
            generation: header.generation,
        };
        Self::deserialize_from_extent(&data[..record_bytes], ptr)
    }

    fn record_bytes(content_bytes: usize) -> Result<usize, StorageError> {
        let record_bytes = content_bytes.div_ceil(BLOCK_SIZE) * BLOCK_SIZE;
        if record_bytes > u32::MAX as usize {
            return Err(StorageError::InvalidBlockSize(record_bytes as u64));
        }
        Ok(record_bytes)
    }

    fn make_bset_header(
        seq: u64,
        sector_offset: u16,
        payload_bytes: usize,
    ) -> Result<BsetHeader, StorageError> {
        if payload_bytes % 8 != 0 || payload_bytes / 8 > u16::MAX as usize {
            return Err(StorageError::InvalidBlockSize(payload_bytes as u64));
        }
        Ok(BsetHeader {
            seq,
            journal_seq: seq,
            flags: (sector_offset as u32) << 16,
            version: BTREE_NODE_VERSION,
            u64s: (payload_bytes / 8) as u16,
        })
    }

    fn pack_entries(entries: &[BtreeEntry]) -> Vec<u8> {
        let total_size: usize = entries.iter().map(|e| entry_packed_size(e) as usize).sum();
        let mut payload = vec![0u8; total_size];
        let mut offset = 0usize;
        for entry in entries {
            let value_bytes = entry.value.to_bytes();
            unsafe {
                let packed = &mut *payload.as_mut_ptr().add(offset).cast::<BkeyPacked>();
                bkey_pack_raw(
                    packed,
                    entry.pos,
                    entry.key_type as u8,
                    &value_bytes,
                    &BKEY_FORMAT_CURRENT,
                );
                offset += packed.u64s as usize * 8;
            }
        }
        payload
    }

    fn write_record_crc(record: &mut [u8], crc_offset: usize) {
        record[crc_offset..crc_offset + 4].fill(0);
        let crc = crc32c(record, 0);
        record[crc_offset..crc_offset + 4].copy_from_slice(&crc.to_le_bytes());
    }

    fn verify_record_crc(
        record: &[u8],
        crc_offset: usize,
        stored_crc: u32,
    ) -> Result<(), StorageError> {
        let mut crc_record = record.to_vec();
        crc_record[crc_offset..crc_offset + 4].fill(0);
        let actual = crc32c(&crc_record, 0);
        if actual != stored_crc {
            return Err(StorageError::ChecksumMismatch {
                expected: stored_crc,
                actual,
            });
        }
        Ok(())
    }

    fn validate_record_len(record_bytes: usize, available: usize) -> Result<(), StorageError> {
        if record_bytes == 0 || record_bytes % BLOCK_SIZE != 0 || record_bytes > available {
            return Err(StorageError::InvalidData(format!(
                "invalid btree record length {} (available {})",
                record_bytes, available
            )));
        }
        Ok(())
    }

    fn parse_record_bset(
        record: &[u8],
        bset_offset: usize,
        expected_sector_offset: u16,
        level: u8,
    ) -> Result<Vec<BtreeEntry>, StorageError> {
        let bset_size = std::mem::size_of::<BsetHeader>();
        if bset_offset + bset_size > record.len() {
            return Err(StorageError::InvalidData("truncated bset header".into()));
        }
        let bset: BsetHeader = unsafe {
            std::ptr::read_unaligned(record.as_ptr().add(bset_offset).cast::<BsetHeader>())
        };
        if bset.version != BTREE_NODE_VERSION || (bset.flags >> 16) as u16 != expected_sector_offset
        {
            return Err(StorageError::InvalidData(
                "btree bset version/offset mismatch".into(),
            ));
        }

        let entries_offset = bset_offset + bset_size;
        let entries_end = entries_offset
            .checked_add(bset.data_bytes())
            .ok_or_else(|| StorageError::InvalidData("btree bset length overflow".into()))?;
        if entries_end > record.len() {
            return Err(StorageError::InvalidData(
                "btree bset extends beyond record".into(),
            ));
        }

        let mut entries = Vec::new();
        let mut offset = entries_offset;
        while offset < entries_end {
            if entries_end - offset < 3 {
                return Err(StorageError::InvalidData("truncated packed bkey".into()));
            }
            let u64s = record[offset] as usize;
            if u64s == 0 {
                return Err(StorageError::InvalidData("zero-sized packed bkey".into()));
            }
            let entry_bytes = u64s * 8;
            if offset + entry_bytes > entries_end {
                return Err(StorageError::InvalidData(
                    "packed bkey extends beyond bset".into(),
                ));
            }
            let packed = unsafe { &*record.as_ptr().add(offset).cast::<BkeyPacked>() };
            let (pos, key_type, value_bytes) = bkey_unpack_bytes(&BKEY_FORMAT_CURRENT, packed);
            let value =
                if level > 0 && value_bytes.len() == crate::btree::types::BtreePtrV2::DISK_BYTES {
                    crate::btree::key::KeyValue::BtreePtr(
                        crate::btree::types::BtreePtrV2::from_bytes(value_bytes)?,
                    )
                } else {
                    crate::btree::key::KeyValue::Raw(value_bytes.to_vec())
                };
            entries.push(BtreeEntry::new(pos, KeyType::from_u8(key_type), value));
            offset += entry_bytes;
        }
        Ok(entries)
    }

    /// 将当前 node 的所有 live entries 打包成 bset 字节
    ///（无 header，仅 packed entries）
    ///
    /// 与 `collect_all_entries()` 同样逻辑：排序去重过滤 whiteout，
    /// 用 `bkey_pack_raw` 重新打包。用于 Wave 3 的 log-structured append。
    pub fn pack_bset(&self) -> Result<Vec<u8>, StorageError> {
        let entries = self.collect_all_entries();
        if entries.is_empty() {
            return Ok(Vec::new());
        }

        let total_size: usize = entries.iter().map(|e| entry_packed_size(e) as usize).sum();
        let mut buf = vec![0u8; total_size];
        let mut cur = 0u32;
        for entry in &entries {
            // Safety: buf 已预分配，cur 始终在范围内
            unsafe {
                let pk = &mut *(buf.as_mut_ptr().add(cur as usize) as *mut BkeyPacked);
                let bpos = &entry.pos;
                let value_bytes = entry.value.to_bytes();
                bkey_pack_raw(
                    pk,
                    *bpos,
                    entry.key_type as u8,
                    &value_bytes,
                    &BKEY_FORMAT_CURRENT,
                );
                cur += pk.u64s as u32 * 8;
            }
        }
        Ok(buf)
    }
}

// =========================================================================
// bcachefs 对齐: 自由函数 — bset 工具
// =========================================================================

/// bcachefs 对齐: `bset_next_set()` — 计算下一个 bset 的起始位置
/// 当前 bset 之后按 block_bytes 对齐
pub fn bset_next_set(_b: &BtreeNode, t: &BsetTree) -> u32 {
    let end = u32::from(t.end_offset) * 8;
    let block_bytes = BLOCK_SIZE as u32;
    debug_assert!(block_bytes.is_power_of_two());
    ((end + block_bytes - 1) / block_bytes) * block_bytes
}

/// bcachefs 对齐: `__btree_keys_cachelines()` — 计算 keys 占用的 cacheline 数
pub fn btree_keys_cachelines(byte_order: u32) -> u32 {
    (1u32 << byte_order) / BSET_CACHELINE
}

/// bcachefs 对齐: `__btree_aux_data_bytes()` — 计算 aux data 的字节数
pub fn btree_aux_data_bytes(byte_order: u32) -> u32 {
    btree_keys_cachelines(byte_order) * 8
}

/// bcachefs 对齐: `bset()` — 获取 bset_tree 对应的数据起始地址
/// 返回指向 data buffer 中该 bset 起始位置的指针
pub fn bset<'a>(b: &'a BtreeNode, t: &BsetTree) -> &'a [u8] {
    let start = t.data_offset as usize * 8;
    let end = t.end_offset as usize * 8;
    &b.data[start..end]
}

/// bcachefs 对齐: `bset_u64s()` — bset 中 u64 数量（不含 Bset 头部）
pub fn bset_u64s(t: &BsetTree) -> u32 {
    u32::from(t.end_offset - t.data_offset - BSET_HEADER_U64S)
}

/// bcachefs 对齐: `btree_bset_first()` — 首个 bset 的数据切片
pub fn btree_bset_first(b: &BtreeNode) -> &[u8] {
    bset(b, &b.sets[0])
}

/// bcachefs 对齐: `btree_bset_last()` — 最后一个 bset 的数据切片
pub fn btree_bset_last(b: &BtreeNode) -> &[u8] {
    let last_idx = b.nsets() as usize - 1;
    bset(b, &b.sets[last_idx])
}

// =========================================================================
// bcachefs 对齐: btree_node_iter 操作 — 节点内跨 bset 迭代
// =========================================================================

/// bcachefs `bkey_iter_cmp_p_or_unp()` (bset.c).
/// Compare a packed key against either a packed search key or an unpacked bpos.
fn bkey_iter_cmp_p_or_unp(
    format: &BkeyFormat,
    left: &BkeyPacked,
    right_packed: Option<&BkeyPacked>,
    right_pos: &Bpos,
) -> std::cmp::Ordering {
    let cmp = match right_packed {
        Some(rp) => bkey_cmp_packed(format, left, rp),
        None => bkey_cmp_packed_vs_bpos(format, left, right_pos),
    };
    cmp.then_with(|| {
        if left.type_ == KeyType::Deleted as u8 {
            std::cmp::Ordering::Less
        } else {
            std::cmp::Ordering::Equal
        }
    })
}

/// bcachefs `bch2_bset_search_linear()` (bset.c).
/// Linear scan within a bset from `start` to find the first key >= search.
fn bch2_bset_search_linear(
    b: &BtreeNode,
    si: usize,
    search: &Bpos,
    packed_search: Option<&BkeyPacked>,
    lossy_packed_search: &BkeyPacked,
    start: u16,
) -> u16 {
    let end = b.sets[si].end_offset;
    let mut cur = start;
    while cur < end {
        let packed = unsafe { &*b.data.as_ptr().add(cur as usize * 8).cast::<BkeyPacked>() };
        let cmp = bkey_iter_cmp_p_or_unp(
            &BKEY_FORMAT_CURRENT,
            packed,
            Some(lossy_packed_search),
            search,
        );
        if cmp != std::cmp::Ordering::Less {
            break;
        }
        cur += packed.u64s as u16;
    }

    if packed_search.is_none() {
        while cur < end {
            let packed = unsafe { &*b.data.as_ptr().add(cur as usize * 8).cast::<BkeyPacked>() };
            let cmp = bkey_iter_cmp_p_or_unp(&BKEY_FORMAT_CURRENT, packed, None, search);
            if cmp != std::cmp::Ordering::Less {
                break;
            }
            cur += packed.u64s as u16;
        }
    }
    cur
}

/// bcachefs 对齐: `bch2_btree_node_iter_init()` — 从指定 pos 初始化迭代器
///
/// 在所有 bset 中定位第一个 >= pos 的 key，设置各 set 的迭代范围。
///
/// 对齐 bcachefs `bch2_btree_node_iter_init()` 的两阶段搜索：
/// 1. `__bch2_bset_search` — cacheline 级粗定位（含 aux tree）
/// 2. `bch2_bset_search_linear` — cacheline 内线性精确定位
pub fn bch2_btree_node_iter_init(iter: &mut BtreeNodeIter, b: &BtreeNode, pos: &Bpos) {
    if (b.packed_keys > 0 || b.unpacked_keys > 0) && b.min_key <= b.max_key {
        if !(*pos >= b.min_key && *pos <= b.max_key) {
            // pos 超出节点范围，回退到 start 初始化
            bch2_btree_node_iter_init_from_start(iter, b);
            return;
        }
    }

    iter.reset();
    let mut packed_buf = [0u64; 8];
    let packed_search = unsafe { &mut *packed_buf.as_mut_ptr().cast::<BkeyPacked>() };
    let pack_ret = bch2_bkey_pack_pos_lossy(packed_search, pos, &BKEY_FORMAT_CURRENT);

    let packed = match pack_ret {
        BkeyPackPosRet::Exact => Some(&*packed_search),
        BkeyPackPosRet::Smaller => None,
        BkeyPackPosRet::Fail => {
            bch2_btree_node_iter_init_from_start(iter, b);
            while let Some(raw) = bch2_btree_node_iter_peek(iter, b) {
                let pk = unsafe { &*raw.as_ptr().cast::<BkeyPacked>() };
                let cmp = bkey_cmp_packed_vs_bpos(&BKEY_FORMAT_CURRENT, pk, pos).then_with(|| {
                    if pk.type_ == KeyType::Deleted as u8 {
                        std::cmp::Ordering::Less
                    } else {
                        std::cmp::Ordering::Equal
                    }
                });
                if cmp != std::cmp::Ordering::Less {
                    break;
                }
                bch2_btree_node_iter_advance(iter, b);
            }
            return;
        }
    };

    let mut candidates = [0u16; MAX_BSETS];
    for (si, candidate) in candidates.iter_mut().enumerate().take(b.nsets as usize) {
        *candidate = b.__bch2_bset_search(si, pos, packed_search);
        prefetch_four_cachelines(unsafe { b.data.as_ptr().add(*candidate as usize * 8) });
    }

    let mut next = 0;
    for (si, set) in b.sets[..b.nsets as usize].iter().enumerate() {
        let cur = bch2_bset_search_linear(b, si, pos, packed, packed_search, candidates[si]);
        if cur != set.end_offset {
            iter.data[next] = BtreeNodeIterSet {
                k: cur,
                end: set.end_offset,
            };
            next += 1;
        }
    }
    bch2_btree_node_iter_sort(iter, b);
}

/// bcachefs 对齐: `bch2_btree_node_iter_init_from_start()` — 从头初始化
pub fn bch2_btree_node_iter_init_from_start(iter: &mut BtreeNodeIter, b: &BtreeNode) {
    iter.reset();
    let mut next = 0;
    for set in &b.sets[..b.nsets as usize] {
        let first = set.first_key_offset();
        if first != set.end_offset {
            iter.data[next] = BtreeNodeIterSet {
                k: first,
                end: set.end_offset,
            };
            next += 1;
        }
    }
    bch2_btree_node_iter_sort(iter, b);
}

fn btree_node_iter_set_find(iter: &BtreeNodeIter, end_offset: u16) -> Option<&BtreeNodeIterSet> {
    iter.data
        .iter()
        .take_while(|set| set.k != set.end)
        .find(|set| set.end == end_offset)
}

/// bcachefs `bch2_btree_node_iter_bset_pos()` (`bset.c:1668`).
pub(crate) fn bch2_btree_node_iter_bset_pos(
    iter: &BtreeNodeIter,
    _b: &BtreeNode,
    set: &BsetTree,
) -> u16 {
    btree_node_iter_set_find(iter, set.end_offset)
        .map(|iter_set| iter_set.k)
        .unwrap_or(set.end_offset)
}

/// bcachefs 对齐: `bch2_btree_node_iter_sort()` — 排序 set 数据，使 data[0] 为最小值
pub fn bch2_btree_node_iter_sort(iter: &mut BtreeNodeIter, b: &BtreeNode) {
    if !iter.set_end(2) {
        btree_node_iter_sort_two(iter, b, 0);
        btree_node_iter_sort_two(iter, b, 1);
    }

    if !iter.set_end(1) {
        btree_node_iter_sort_two(iter, b, 0);
    }
}

fn btree_node_iter_sort_two(iter: &mut BtreeNodeIter, b: &BtreeNode, first: usize) -> bool {
    let swap = btree_node_iter_cmp(b, iter.data[first], iter.data[first + 1]).is_gt();
    if swap {
        iter.data.swap(first, first + 1);
    }
    swap
}

/// bcachefs 对齐: `bkey_iter_cmp()` — 比较两个 packed key
fn btree_node_iter_cmp(
    b: &BtreeNode,
    l: BtreeNodeIterSet,
    r: BtreeNodeIterSet,
) -> std::cmp::Ordering {
    if l.k >= l.end && r.k >= r.end {
        return std::cmp::Ordering::Equal;
    }
    if l.k >= l.end {
        return std::cmp::Ordering::Greater;
    }
    if r.k >= r.end {
        return std::cmp::Ordering::Less;
    }
    let lp = unsafe { &*b.data.as_ptr().add(l.k as usize * 8).cast::<BkeyPacked>() };
    let rp = unsafe { &*b.data.as_ptr().add(r.k as usize * 8).cast::<BkeyPacked>() };
    let (lpos, _, _, _) = bkey_unpack(&BKEY_FORMAT_CURRENT, lp);
    let (rpos, _, _, _) = bkey_unpack(&BKEY_FORMAT_CURRENT, rp);
    lpos.cmp(&rpos)
        .then_with(|| {
            (rp.type_ == KeyType::Deleted as u8).cmp(&(lp.type_ == KeyType::Deleted as u8))
        })
        .then_with(|| l.k.cmp(&r.k))
}

/// bcachefs 对齐: `bch2_btree_node_iter_peek_all()` — 窥视（含 deleted）
pub fn bch2_btree_node_iter_peek_all<'a>(
    iter: &BtreeNodeIter,
    b: &'a BtreeNode,
) -> Option<&'a [u8]> {
    if iter.data[0].k < iter.data[0].end {
        let start = iter.data[0].k as usize * 8;
        let u64s = b.read_entry_u64s(start) as u32;
        Some(&b.data[start..start + (u64s as usize) * 8])
    } else {
        None
    }
}

/// bcachefs 对齐: `bch2_btree_node_iter_peek()` — 窥视（跳过 deleted）
pub fn bch2_btree_node_iter_peek<'a>(
    iter: &mut BtreeNodeIter,
    b: &'a BtreeNode,
) -> Option<&'a [u8]> {
    while iter.data[0].k < iter.data[0].end {
        let start = iter.data[0].k as usize * 8;
        let pk = unsafe { &*(b.data.as_ptr().add(start) as *const BkeyPacked) };
        if pk.type_ != KeyType::Deleted as u8 {
            let u64s = pk.u64s as u32;
            return Some(&b.data[start..start + (u64s as usize) * 8]);
        }
        bch2_btree_node_iter_advance(iter, b);
    }
    None
}

/// bcachefs 对齐: `bch2_btree_node_iter_advance()` — 前进
pub fn bch2_btree_node_iter_advance(iter: &mut BtreeNodeIter, b: &BtreeNode) {
    if iter.data[0].k < iter.data[0].end {
        let u64s = b.read_entry_u64s(iter.data[0].k as usize * 8);
        iter.data[0].k += u64s as u16;
    }

    if iter.set_end(0) {
        iter.data[0] = iter.data[1];
        iter.data[1] = iter.data[2];
        iter.data[2] = BtreeNodeIterSet { k: 0, end: 0 };
        return;
    }

    if iter.set_end(1) {
        return;
    }

    if !btree_node_iter_sort_two(iter, b, 0) {
        return;
    }

    if iter.set_end(2) {
        return;
    }

    btree_node_iter_sort_two(iter, b, 1);
}

/// bcachefs 对齐: `bch2_btree_node_iter_next_all()` — 下一个（含 deleted）
pub fn bch2_btree_node_iter_next_all<'a>(
    iter: &mut BtreeNodeIter,
    b: &'a BtreeNode,
) -> Option<&'a [u8]> {
    let ret = bch2_btree_node_iter_peek_all(iter, b);
    if ret.is_some() {
        bch2_btree_node_iter_advance(iter, b);
    }
    ret
}

/// bcachefs 对齐: `bch2_btree_node_iter_set_drop()` — 丢弃指定 set
pub fn bch2_btree_node_iter_set_drop(iter: &mut BtreeNodeIter, _b: &BtreeNode, idx: usize) {
    if idx < iter.data.len() {
        let last = iter.data.len() - 1;
        if idx < last {
            iter.data.copy_within(idx + 1.., idx);
        }
        iter.data[last] = BtreeNodeIterSet { k: 0, end: 0 };
    }
}

impl Clone for BtreeNode {
    fn clone(&self) -> Self {
        Self {
            lock: SixLock::new(),
            level: self.level,
            packed_keys: self.packed_keys,
            unpacked_keys: self.unpacked_keys,
            whiteout_u64s: self.whiteout_u64s,
            node_size: self.node_size,
            data: self.data.clone(),
            aux_data: self.aux_data.clone(),
            nsets: self.nsets,
            sets: self.sets,
            refcount: AtomicU32::new(self.refcount.load(Ordering::Acquire)),
            state: AtomicU8::new(self.state.load(Ordering::Acquire)),
            flags: AtomicU8::new(self.flags.load(Ordering::Acquire)),
            // Clone creates a distinct, unallocated node (`block_addr` is reset below),
            // matching a freshly allocated bcachefs node rather than sharing an extent.
            written: 0,
            min_key: self.min_key,
            max_key: self.max_key,
            sib_u64s: self.sib_u64s,
            journal_seq: self.journal_seq,
            will_make_reachable: AtomicBool::new(self.will_make_reachable.load(Ordering::Acquire)),
            write_blocked: Arc::new(AtomicBool::new(false)),
            pin_count: AtomicU32::new(0),
            journal_pin: JournalEntryPin::new(
                None,
                match self.level {
                    0 => crate::journal::reclaim::JournalPinType::Btree0,
                    1 => crate::journal::reclaim::JournalPinType::Btree1,
                    2 => crate::journal::reclaim::JournalPinType::Btree2,
                    _ => crate::journal::reclaim::JournalPinType::Btree3,
                },
            ),
            read_in_flight: AtomicBool::new(self.read_in_flight.load(Ordering::Acquire)),
            read_wait_mutex: Mutex::new(()),
            read_condvar: Condvar::new(),
            write_wait_mutex: Mutex::new(()),
            write_condvar: Condvar::new(),
            vol: OnceLock::new(),
            journal: OnceLock::new(),
            #[cfg(test)]
            test_device: OnceLock::new(),
            block_addr: AtomicU64::new(0),
        }
    }
}

impl std::fmt::Debug for BtreeNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BtreeNode")
            .field("level", &self.level)
            .field("packed_keys", &self.packed_keys)
            .field("unpacked_keys", &self.unpacked_keys)
            .field("whiteout", &self.whiteout_u64s)
            .field("written", &self.written)
            .field("node_size", &self.node_size)
            .field("alive", &self.is_alive())
            .field("refcount", &self.refcount())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btree::writer::NoopWriter;
    use crate::btree::Btree;

    #[test]
    fn test_new() {
        let n = BtreeNode::new_leaf();
        assert_eq!(n.level, 0);
        assert_eq!(n.node_size, DEFAULT_NODE_SIZE);
        assert!(n.is_alive());
    }

    #[test]
    fn test_internal() {
        assert_eq!(BtreeNode::new_internal().level, 1);
    }

    #[test]
    fn test_btree_node_iter_init_handles_out_of_range_key() {
        let btree = Btree::new();
        assert!(futures::executor::block_on(btree.bch2_btree_insert(
            &NoopWriter,
            BtreeKey::new(10, 1, KeyType::Normal),
            BchVal::new(100, 0),
            0,
        ))
        .unwrap_or(false));

        let root = btree.root();
        let mut node = root.node.as_ref().clone();
        node.min_key = Bpos::from_key(&BtreeKey::new(10, 1, KeyType::Normal));
        node.max_key = Bpos::from_key(&BtreeKey::new(10, 1, KeyType::Normal));
        let mut iter = BtreeNodeIter::default();
        bch2_btree_node_iter_init(
            &mut iter,
            &node,
            &Bpos::from_key(&BtreeKey::new(9, 1, KeyType::Normal)),
        );
    }

    #[test]
    fn test_write_read() {
        let mut n = BtreeNode::new_leaf();
        let k = BtreeKey::new(100, 1, KeyType::Normal);
        let _v = BchVal::new(0xABCD, 42);
        let ev = ExtentValue {
            paddr: 0xABCD,
            size: 1,
            ver: 42,
            dev_idx: 0,
            crc32c: 0,
            crc_offset_blocks: 0,
        };
        let first_key_off = u32::from(n.sets[0].first_key_offset()) * 8;
        let written = n.write_entry(first_key_off, &k, &ev.to_bchval(), 0);
        n.sets[0].end_offset = ((first_key_off + written) / 8) as u16;
        assert_eq!(n.read_entry(&n.sets[0], 1), (k, ev));
    }

    #[test]
    fn test_eytzinger_build() {
        let keys = vec![
            (BtreeKey::new(10, 1, KeyType::Normal), BchVal::new(1, 0)),
            (BtreeKey::new(20, 1, KeyType::Normal), BchVal::new(2, 0)),
            (BtreeKey::new(30, 1, KeyType::Normal), BchVal::new(3, 0)),
        ];
        let eyt = BtreeNode::build_eytzinger(&keys);
        assert_eq!(eyt.len(), 4);
        assert_eq!(eyt[1].0.get_vaddr(), 20);
        assert_eq!(eyt[2].0.get_vaddr(), 10);
        assert_eq!(eyt[3].0.get_vaddr(), 30);
    }

    #[test]
    fn test_search() {
        let mut n = BtreeNode::new_leaf();
        let key = BtreeKey::new(42, 1, KeyType::Normal);
        let _val = BchVal::new(0xFF, 1);
        let ev = ExtentValue {
            paddr: 0xFF,
            size: 1,
            ver: 1,
            dev_idx: 0,
            crc32c: 0,
            crc_offset_blocks: 0,
        };
        let first_key_off = u32::from(n.sets[0].first_key_offset()) * 8;
        let written = n.write_entry(first_key_off, &key, &ev.to_bchval(), 0);
        n.sets[0].end_offset = ((first_key_off + written) / 8) as u16;
        n.sets[0].size = 0;
        n.sets[0].extra = BSET_NO_AUX_TREE_VAL;
        n.sets[0].aux_data_offset = u16::MAX;
        assert_eq!(n.search(&key), Some((key, ev)));
        assert!(n.search(&BtreeKey::new(99, 1, KeyType::Normal)).is_none());
    }

    #[test]
    fn test_insert() {
        let mut n = BtreeNode::new_leaf();
        assert!(n.insert(BtreeKey::new(100, 1, KeyType::Normal), BchVal::new(1, 0)));
        assert!(n.search(&BtreeKey::new(100, 1, KeyType::Normal)).is_some());
    }

    #[test]
    fn test_insert_multi() {
        let mut n = BtreeNode::new_leaf();
        for i in 0..20 {
            assert!(n.insert(
                BtreeKey::new(i as u64, 1, KeyType::Normal),
                BchVal::new(i as u64, i as u16)
            ));
        }
        for i in 0..20 {
            let found = n.search(&BtreeKey::new(i as u64, 1, KeyType::Normal));
            assert!(found.is_some(), "key {} not found", i);
        }
    }

    #[test]
    fn test_compact() {
        let mut n = BtreeNode::new_leaf();
        n.insert(BtreeKey::new(50, 1, KeyType::Normal), BchVal::new(1, 0));
        n.insert(BtreeKey::new(100, 1, KeyType::Normal), BchVal::new(2, 0));
        n.compact();
        assert_eq!(n.packed_keys, 2);
        assert!(n.search(&BtreeKey::new(50, 1, KeyType::Normal)).is_some());
        assert!(n.search(&BtreeKey::new(100, 1, KeyType::Normal)).is_some());
    }

    #[test]
    fn test_compact_dedup() {
        let mut n = BtreeNode::new_leaf();
        n.insert(BtreeKey::new(100, 1, KeyType::Normal), BchVal::new(1, 0));
        n.insert(BtreeKey::new(100, 1, KeyType::Normal), BchVal::new(2, 0));
        n.compact();
        assert_eq!(n.packed_keys, 1);
    }

    #[test]
    fn test_compact_roundtrip() {
        let mut n = BtreeNode::new_leaf();
        for i in 0..15 {
            n.insert(
                BtreeKey::new(i as u64, 1, KeyType::Normal),
                BchVal::new(i as u64, 0),
            );
        }
        n.compact();
        for i in 0..15 {
            assert!(
                n.search(&BtreeKey::new(i as u64, 1, KeyType::Normal))
                    .is_some(),
                "lost {}",
                i
            );
        }
    }

    #[test]
    fn test_btree_node_compact_fits_rounds_both_records() {
        let mut n = BtreeNode::new_leaf();

        n.node_size = BLOCK_SIZE as u32;
        assert!(!n.bch2_btree_node_compact_fits(0));

        n.node_size = (BLOCK_SIZE * 2) as u32;
        assert!(n.bch2_btree_node_compact_fits(0));

        let followon_overhead =
            std::mem::size_of::<BtreeNodeDiskEntry>() + std::mem::size_of::<BsetHeader>() + 8;
        let max_one_block_key_u64s = (BLOCK_SIZE - followon_overhead) / 8;
        assert!(n.bch2_btree_node_compact_fits(max_one_block_key_u64s as u32));
        assert!(!n.bch2_btree_node_compact_fits(max_one_block_key_u64s as u32 + 1));
    }

    #[test]
    fn test_btree_node_compact_fits_accounts_live_u64s_before_rounding() {
        let mut n = BtreeNode::new_leaf();
        n.node_size = (BLOCK_SIZE * 2) as u32;

        let mut key = 0;
        while std::mem::size_of::<BtreeNodeHeader>()
            + std::mem::size_of::<BsetHeader>()
            + n.live_data_bytes() as usize
            + 8
            <= BLOCK_SIZE
        {
            assert!(n.insert(BtreeKey::new(key, 1, KeyType::Normal), BchVal::new(key, 0),));
            key += 1;
        }

        assert!(!n.bch2_btree_node_compact_fits(1));
    }

    #[test]
    fn test_btree_node_compact_fits_uses_live_not_total_u64s() {
        let mut n = BtreeNode::new_leaf();
        n.node_size = (BLOCK_SIZE * 2) as u32;
        let mut offset = u32::from(BSET_HEADER_U64S) * 8;
        let mut key = 0;

        while (offset - u32::from(BSET_HEADER_U64S) * 8) as usize <= BLOCK_SIZE {
            let deleted = BtreeEntry::raw(Bpos::new(0, key, 0), KeyType::Deleted, Vec::new());
            offset += n.write_entry_bytes(offset, &deleted);
            key += 1;
        }
        n.sets[0].end_offset = (offset / 8) as u16;
        n.set_bset_u64s(0, n.sets[0].end_offset - BSET_HEADER_U64S);

        assert!(n.total_data_bytes() as usize > BLOCK_SIZE);
        assert_eq!(n.live_data_bytes(), 0);
        assert!(n.bch2_btree_node_compact_fits(0));
    }

    #[test]
    fn test_multi_set_search() {
        let mut n = BtreeNode::new_leaf();
        let first_key = u32::from(BSET_HEADER_U64S) * 8;
        let written = n.write_entry(
            first_key,
            &BtreeKey::new(10, 1, KeyType::Normal),
            &BchVal::new(1, 0),
            0,
        );
        n.sets[0].end_offset = ((first_key + written) / 8) as u16;
        n.sets[0].size = 0;
        n.sets[0].extra = BSET_NO_AUX_TREE_VAL;
        n.sets[0].aux_data_offset = u16::MAX;
        n.set_bset_u64s(0, (written / 8) as u16);
        n.bch2_bset_build_aux_tree(0, false);

        n.nsets = 2;
        n.sets[1] = BsetTree {
            size: 0,
            extra: BSET_NO_AUX_TREE_VAL,
            data_offset: n.sets[0].end_offset,
            aux_data_offset: u16::MAX,
            end_offset: n.sets[0].end_offset + BSET_HEADER_U64S,
        };
        n.write_bset_header(
            1,
            BsetHeader {
                seq: 0,
                journal_seq: 0,
                flags: 0,
                version: 0,
                u64s: 0,
            },
        );
        n.packed_keys = 1; n.unpacked_keys = 0;
        n.insert(BtreeKey::new(20, 1, KeyType::Normal), BchVal::new(2, 0));
        assert!(n.search(&BtreeKey::new(10, 1, KeyType::Normal)).is_some());
        assert!(n.search(&BtreeKey::new(20, 1, KeyType::Normal)).is_some());
    }

    #[test]
    fn test_node_iter_search_skips_deleted_equal_key() {
        let mut n = BtreeNode::new_leaf();
        let pos = Bpos::new(0, 42, 1);
        let first = u32::from(n.sets[0].first_key_offset()) * 8;
        let deleted = BtreeEntry::raw(pos, KeyType::Deleted, Vec::new());
        let live = BtreeEntry::new(
            pos,
            KeyType::Normal,
            KeyValue::Extent(ExtentValue {
                paddr: 7,
                size: 1,
                ver: 0,
                dev_idx: 0,
                crc32c: 0,
                crc_offset_blocks: 0,
            }),
        );
        let deleted_bytes = n.write_entry_bytes(first, &deleted);
        let live_bytes = n.write_entry_bytes(first + deleted_bytes, &live);
        n.sets[0].end_offset = ((first + deleted_bytes + live_bytes) / 8) as u16;
        n.set_bset_u64s(0, ((deleted_bytes + live_bytes) / 8) as u16);
        n.packed_keys = 1; n.unpacked_keys = 0;

        let mut iter = BtreeNodeIter::default();
        bch2_btree_node_iter_init(&mut iter, &n, &pos);
        let raw = bch2_btree_node_iter_peek_all(&mut iter, &n).unwrap();
        let packed = unsafe { &*raw.as_ptr().cast::<BkeyPacked>() };
        assert_eq!(packed.type_, KeyType::Normal as u8);
    }

    #[test]
    fn test_ro_aux_tree_search_matches_keys() {
        let mut node = BtreeNode::new_leaf();
        for i in 0..128u64 {
            assert!(node.insert(BtreeKey::new(i * 2, 1, KeyType::Normal), BchVal::new(i, 0),));
        }
        node.compact();
        node.min_key = Bpos::new(0, 0, 1);
        node.max_key = Bpos::new(0, 254, 1);
        node.bch2_bset_build_aux_tree(0, false);
        assert!(node.sets[0].has_ro_aux_tree());

        for i in 0..128u64 {
            let found = node
                .search(&BtreeKey::new(i * 2, 1, KeyType::Normal))
                .expect("RO aux lookup must find every committed key");
            assert_eq!(found.1.paddr, i);
        }
        for i in 0..127u64 {
            assert!(node
                .search(&BtreeKey::new(i * 2 + 1, 1, KeyType::Normal))
                .is_none());
        }
    }

    #[test]
    fn test_state() {
        let n = BtreeNode::new_leaf();
        assert!(n.is_alive());
        n.state.store(NodeState::Deleting as u8, Ordering::Release);
        assert!(!n.is_alive());
    }

    #[test]
    fn test_refcount() {
        let n = BtreeNode::new_leaf();
        assert_eq!(n.refcount(), 1);
        n.refcount.fetch_add(1, Ordering::Relaxed);
        assert_eq!(n.refcount(), 2);
    }

    #[test]
    fn test_large_insert() {
        let mut n = BtreeNode::new_leaf();
        for i in 0..100 {
            assert!(
                n.insert(
                    BtreeKey::new(i as u64, 1, KeyType::Normal),
                    BchVal::new(i as u64, 0)
                ),
                "insert {}",
                i
            );
        }
        n.compact();
        for i in 0..100 {
            assert!(
                n.search(&BtreeKey::new(i as u64, 1, KeyType::Normal))
                    .is_some(),
                "lost {}",
                i
            );
        }
    }

    // ─── bcachefs delete_key tests ──────────────────────────────────

    #[test]
    fn test_delete_bcachefs_insert_deleted_entry() {
        let mut n = BtreeNode::new_leaf();
        for i in 0..50 {
            n.insert(
                BtreeKey::new(i as u64, 1, KeyType::Normal),
                BchVal::new(i, 0),
            );
        }
        assert!(
            n.delete_key(&BtreeKey::new(42, 1, KeyType::Normal)),
            "delete should succeed"
        );

        // bcachefs 对齐: delete_key 物理删除 inc set 中的 entry
        // 所以 search 应立刻找不到（无 tombstone、无残留 Normal）
        assert!(
            n.search(&BtreeKey::new(42, 1, KeyType::Normal)).is_none(),
            "bcachefs delete physically removes entry from inc set"
        );

        // 其他 key 不受影响
        assert!(
            n.search(&BtreeKey::new(0, 1, KeyType::Normal)).is_some(),
            "other keys survive"
        );
    }

    #[test]
    fn test_delete_from_full_writable_bset() {
        let mut n = BtreeNode::new_leaf();
        let key = BtreeKey::new(42, 1, KeyType::Normal);
        assert!(n.insert(key, BchVal::new(1, 0)));

        let last = n.nsets as usize - 1;
        // bcachefs: __bch2_btree_u64s_remaining 保留 +1 字节给 varint，
        // 加上 delete 需要插入 3 u64s 的 Deleted 条目
        n.node_size = u32::from(n.sets[last].end_offset + 1 + 3) * 8;

        assert!(n.delete_key(&key));
        assert_eq!(n.sets[last].end_offset, n.sets[last].first_key_offset());
        assert_eq!(n.packed_keys, 0);
        assert!(n.search(&key).is_none());
    }

    #[test]
    fn test_delete_nonexistent_key() {
        let mut n = BtreeNode::new_leaf();
        n.insert(BtreeKey::new(100, 1, KeyType::Normal), BchVal::new(1, 0));
        assert!(
            !n.delete_key(&BtreeKey::new(999, 1, KeyType::Normal)),
            "nonexistent key"
        );
    }

    #[test]
    fn test_delete_then_insert_overwrite() {
        let mut n = BtreeNode::new_leaf();
        n.insert(BtreeKey::new(100, 1, KeyType::Normal), BchVal::new(1, 0));
        n.delete_key(&BtreeKey::new(100, 1, KeyType::Normal));
        n.insert(BtreeKey::new(100, 1, KeyType::Normal), BchVal::new(2, 0));

        n.compact();
        let found = n.search(&BtreeKey::new(100, 1, KeyType::Normal));
        assert!(found.is_some(), "insert-after-delete should restore key");
        assert_eq!(
            found.unwrap().1,
            ExtentValue {
                paddr: 2,
                size: 1,
                ver: 0,
                dev_idx: 0,
                crc32c: 0,
                crc_offset_blocks: 0,
            },
            "last value wins"
        );
        assert_eq!(n.packed_keys, 1);
    }

    #[test]
    fn test_delete_twice_idempotent() {
        let mut n = BtreeNode::new_leaf();
        n.insert(BtreeKey::new(100, 1, KeyType::Normal), BchVal::new(1, 0));

        assert!(
            n.delete_key(&BtreeKey::new(100, 1, KeyType::Normal)),
            "first delete"
        );
        // has_live_entry sees Deleted entry → returns false for second call
        assert!(
            !n.delete_key(&BtreeKey::new(100, 1, KeyType::Normal)),
            "second delete returns false"
        );

        n.compact();
        assert_eq!(n.packed_keys, 0, "after compact, key is gone");
    }

    #[test]
    fn test_delete_multiple_keys() {
        let mut n = BtreeNode::new_leaf();
        n.insert(BtreeKey::new(10, 1, KeyType::Normal), BchVal::new(10, 0));
        n.insert(BtreeKey::new(20, 1, KeyType::Normal), BchVal::new(20, 0));
        n.insert(BtreeKey::new(30, 1, KeyType::Normal), BchVal::new(30, 0));

        n.delete_key(&BtreeKey::new(20, 1, KeyType::Normal));
        n.compact();

        assert_eq!(n.packed_keys, 2);
        assert!(
            n.search(&BtreeKey::new(10, 1, KeyType::Normal)).is_some(),
            "key 10 survives"
        );
        assert!(
            n.search(&BtreeKey::new(20, 1, KeyType::Normal)).is_none(),
            "key 20 deleted"
        );
        assert!(
            n.search(&BtreeKey::new(30, 1, KeyType::Normal)).is_some(),
            "key 30 survives"
        );
    }

    #[test]
    fn test_delete_and_compact_removes_entries() {
        let mut n = BtreeNode::new_leaf();
        // Insert 9 keys, delete 2 → compact → 7 keys remain
        for i in 0..9 {
            n.insert(
                BtreeKey::new(i as u64, 1, KeyType::Normal),
                BchVal::new(i, 0),
            );
        }
        n.delete_key(&BtreeKey::new(1, 1, KeyType::Normal));
        n.delete_key(&BtreeKey::new(3, 1, KeyType::Normal));
        // bcachefs 对齐: delete_key 只做 whiteout（不触发 auto-compact）
        // 需手动 compact 回收空间
        n.compact();
        // After compact: 7 keys remain
        assert_eq!(n.packed_keys, 7);
        assert!(
            n.search(&BtreeKey::new(1, 1, KeyType::Normal)).is_none(),
            "key 1 gone"
        );
        assert!(
            n.search(&BtreeKey::new(3, 1, KeyType::Normal)).is_none(),
            "key 3 gone"
        );
        assert!(
            n.search(&BtreeKey::new(5, 1, KeyType::Normal)).is_some(),
            "key 5 survives"
        );
    }

    #[test]
    fn test_delete_empty_node() {
        let mut n = BtreeNode::new_leaf();
        assert!(
            !n.delete_key(&BtreeKey::new(100, 1, KeyType::Normal)),
            "empty node"
        );
    }

    #[test]
    fn test_delete_compact_dedup_insert_delete_mixed() {
        let mut n = BtreeNode::new_leaf();
        // Use baseline keys (skip 1, 2 so test keys are unique)
        for i in 0..30 {
            if i == 1 || i == 2 {
                continue;
            }
            n.insert(
                BtreeKey::new(i as u64, 1, KeyType::Normal),
                BchVal::new(i * 10, 0),
            );
        }
        // Insert A, B, overwrite A, delete A, delete B, re-insert A
        n.insert(BtreeKey::new(1, 1, KeyType::Normal), BchVal::new(11, 0));
        n.insert(BtreeKey::new(2, 1, KeyType::Normal), BchVal::new(21, 0));
        n.insert(BtreeKey::new(1, 1, KeyType::Normal), BchVal::new(12, 0)); // overwrite
        n.delete_key(&BtreeKey::new(1, 1, KeyType::Normal));
        n.delete_key(&BtreeKey::new(2, 1, KeyType::Normal));
        n.insert(BtreeKey::new(1, 1, KeyType::Normal), BchVal::new(13, 0)); // re-insert

        n.compact();
        let a = n.search(&BtreeKey::new(1, 1, KeyType::Normal)).unwrap();
        assert_eq!(
            a.1,
            ExtentValue {
                paddr: 13,
                size: 1,
                ver: 0,
                dev_idx: 0,
                crc32c: 0,
                crc_offset_blocks: 0,
            },
            "key 1 has latest value"
        );
        let b = n.search(&BtreeKey::new(2, 1, KeyType::Normal));
        assert!(b.is_none(), "key 2 was deleted and not re-inserted");
        assert!(
            n.search(&BtreeKey::new(0, 1, KeyType::Normal)).is_some(),
            "baseline key 0 survives"
        );
    }

    // ─── 磁盘格式测试 ─────────────────────────────────────

    fn build_filled_node(count: u32) -> BtreeNode {
        let mut node = BtreeNode::new_leaf();
        for i in 0..count {
            node.insert(
                BtreeKey::new(i as u64, 1, KeyType::Normal),
                BchVal::new(i as u64, i as u16),
            );
        }
        node
    }

    #[test]
    fn test_header_roundtrip() {
        let h = BtreeNodeHeader {
            magic: BTREE_NODE_MAGIC,
            version: BTREE_NODE_VERSION,
            level: 0,
            node_type: 0,
            packed_keys: 42,
            unpacked_keys: 0,
            bset_count: 1,
            crc32: 0xDEADBEEF,
            seq: 0,
            bucket_addr: 100,
            generation: 7,
            record_bytes: BLOCK_SIZE as u32,
            min_key_inode: Bpos::MIN.inode,
            min_key_offset: Bpos::MIN.offset,
            min_key_snapshot: Bpos::MIN.snapshot,
            max_key_inode: Bpos::MAX.inode,
            max_key_offset: Bpos::MAX.offset,
            max_key_snapshot: Bpos::MAX.snapshot,
            _pad: [0; 6],
        };
        let bytes = bincode::serialize(&h).unwrap();
        assert_eq!(
            bytes.len(),
            88,
            "BtreeNodeHeader must be exactly 88 bytes packed"
        );
        let h2: BtreeNodeHeader = bincode::deserialize(&bytes).unwrap();
        // 复制到本地避免 #[repr(C, packed)] 下的引用 UB
        let (m1, m2) = (h.magic, h2.magic);
        assert_eq!(m1, m2);
        let (k1, k2) = (h.packed_keys, h2.packed_keys);
        assert_eq!(k1, k2);
        let (c1, c2) = (h.crc32, h2.crc32);
        assert_eq!(c1, c2);
        assert_eq!(h.level, h2.level);
        let (b1, b2) = (h.bucket_addr, h2.bucket_addr);
        assert_eq!(b1, b2);
        assert_eq!(h.min_key(), h2.min_key());
        assert_eq!(h.max_key(), h2.max_key());
    }

    #[test]
    fn test_disk_entry_roundtrip() {
        let e = BtreeNodeDiskEntry {
            magic: BTREE_NODE_ENTRY_MAGIC,
            version: BTREE_NODE_VERSION,
            _flags: 0,
            seq: 42,
            generation: 3,
            record_bytes: BLOCK_SIZE as u32,
            crc32: 0xCAFEBABE,
            _pad: 0,
        };
        let mut bytes = [0u8; std::mem::size_of::<BtreeNodeDiskEntry>()];
        unsafe {
            std::ptr::write_unaligned(bytes.as_mut_ptr().cast::<BtreeNodeDiskEntry>(), e);
        }
        let e2: BtreeNodeDiskEntry =
            unsafe { std::ptr::read_unaligned(bytes.as_ptr().cast::<BtreeNodeDiskEntry>()) };
        let (magic, magic2) = (e.magic, e2.magic);
        let (seq, seq2) = (e.seq, e2.seq);
        let (generation, generation2) = (e.generation, e2.generation);
        let (record_bytes, record_bytes2) = (e.record_bytes, e2.record_bytes);
        let (crc, crc2) = (e.crc32, e2.crc32);
        assert_eq!(magic, magic2);
        assert_eq!(seq, seq2);
        assert_eq!(generation, generation2);
        assert_eq!(record_bytes, record_bytes2);
        assert_eq!(crc, crc2);
    }

    #[test]
    fn test_serialize_deserialize_roundtrip() {
        let mut node = build_filled_node(20);
        node.compact();
        let bucket_addr = 100;
        let data = node.serialize_to_bucket(bucket_addr).unwrap();
        assert_eq!(data.len(), BLOCK_SIZE);

        let restored = BtreeNode::deserialize_from_bucket(&data).unwrap();
        assert_eq!(restored.packed_keys, node.packed_keys);
        assert_eq!(restored.unpacked_keys, node.unpacked_keys);
        assert_eq!(restored.level, node.level);
        // 验证所有 entries 可搜索
        for i in 0..20 {
            let key = BtreeKey::new(i, 1, KeyType::Normal);
            assert!(
                restored.search(&key).is_some(),
                "key {} lost in roundtrip",
                i
            );
        }
    }

    #[test]
    fn test_empty_node_roundtrip() {
        let node = BtreeNode::new_leaf();
        let bucket_addr = 200;
        let data = node.serialize_to_bucket(bucket_addr).unwrap();
        assert_eq!(data.len(), BLOCK_SIZE);

        let restored = BtreeNode::deserialize_from_bucket(&data).unwrap();
        assert_eq!(restored.packed_keys, 0);
        assert_eq!(restored.unpacked_keys, 0);
        assert_eq!(restored.level, 0);
    }

    #[test]
    fn test_pack_bset() {
        let mut node = build_filled_node(20);
        node.compact();
        let packed = node.pack_bset().unwrap();
        assert!(
            !packed.is_empty(),
            "pack_bset should return non-empty for 20 entries"
        );
        assert!(
            packed.len() < BLOCK_SIZE,
            "packed bset should fit in one block"
        );
    }

    #[test]
    fn test_serialize_size() {
        let mut node = build_filled_node(20);
        node.compact();
        let data = node.serialize_to_bucket(100).unwrap();
        assert_eq!(data.len(), BLOCK_SIZE);
    }

    #[test]
    fn test_serialize_existing_btree() {
        let mut node = build_filled_node(20);
        node.compact();
        let key_count_before = node.packed_keys;
        let data_before = node.data.clone();

        let _serialized = node.serialize_to_bucket(100).unwrap();

        // 验证 serialize 后原节点数据不变（不消耗节点）
        assert_eq!(node.packed_keys, key_count_before);
        assert_eq!(node.data, data_before);
    }

    #[test]
    fn test_extent_rejects_corrupt_committed_crc() {
        let mut node = build_filled_node(20);
        node.compact();
        let mut record = node.serialize_initial_record(100, 7).unwrap();
        record[200] ^= 0x80;
        let ptr = crate::btree::types::BtreePtrV2 {
            block_addr: 100,
            sectors_written: (record.len() / SECTOR_SIZE) as u16,
            level: 0,
            dev_idx: 0,
            generation: 7,
        };
        assert!(matches!(
            BtreeNode::deserialize_from_extent(&record, ptr),
            Err(StorageError::ChecksumMismatch { .. })
        ));
    }

    #[test]
    fn test_extent_rejects_wrong_generation() {
        let node = build_filled_node(5);
        let record = node.serialize_initial_record(100, 7).unwrap();
        let ptr = crate::btree::types::BtreePtrV2 {
            block_addr: 100,
            sectors_written: (record.len() / SECTOR_SIZE) as u16,
            level: 0,
            dev_idx: 0,
            generation: 8,
        };
        assert!(matches!(
            BtreeNode::deserialize_from_extent(&record, ptr),
            Err(StorageError::InvalidData(_))
        ));
    }

    #[test]
    fn test_extent_rejects_append_with_wrong_bset_offset() {
        let mut node = build_filled_node(5);
        let initial = node.serialize_initial_record(100, 3).unwrap();
        let initial_sectors = (initial.len() / SECTOR_SIZE) as u16;
        // bcachefs: write 完成后 written 推进且标记 just_written，
        // prep_for_write 执行 cleanup + init_next 为后续 insert 准备新 bset
        node.node_size = 32768;
        node.written = initial_sectors;
        node.set_just_written();
        crate::btree::io::bch2_btree_node_prep_for_write(&mut node);
        node.insert(BtreeKey::new(99, 1, KeyType::Normal), BchVal::new(99, 1));
        let mut append = node.serialize_append_record(3, initial_sectors).unwrap();
        let bset_offset = std::mem::size_of::<BtreeNodeDiskEntry>();
        let mut bset: BsetHeader = unsafe {
            std::ptr::read_unaligned(append.as_ptr().add(bset_offset).cast::<BsetHeader>())
        };
        bset.flags = ((initial_sectors + SECTORS_PER_BLOCK) as u32) << 16;
        unsafe {
            std::ptr::write_unaligned(
                append.as_mut_ptr().add(bset_offset).cast::<BsetHeader>(),
                bset,
            );
        }
        BtreeNode::write_record_crc(&mut append, 24);

        let mut extent = initial;
        extent.extend_from_slice(&append);
        let ptr = crate::btree::types::BtreePtrV2 {
            block_addr: 100,
            sectors_written: (extent.len() / SECTOR_SIZE) as u16,
            level: 0,
            dev_idx: 0,
            generation: 3,
        };
        assert!(matches!(
            BtreeNode::deserialize_from_extent(&extent, ptr),
            Err(StorageError::InvalidData(_))
        ));
    }

    #[test]
    fn test_internal_pointer_value_roundtrip() {
        let mut node = BtreeNode::new_internal();
        let child = crate::btree::types::BtreePtrV2 {
            block_addr: 0x400,
            sectors_written: 16,
            level: 0,
            dev_idx: 0,
            generation: 4,
        };
        assert!(node.insert_entry(&BtreeEntry::new(
            Bpos::MIN,
            KeyType::Normal,
            crate::btree::key::KeyValue::BtreePtr(child),
        )));
        let record = node.serialize_initial_record(0x800, 2).unwrap();
        let root_ptr = crate::btree::types::BtreePtrV2 {
            block_addr: 0x800,
            sectors_written: (record.len() / SECTOR_SIZE) as u16,
            level: 1,
            dev_idx: 0,
            generation: 2,
        };
        let restored = BtreeNode::deserialize_from_extent(&record, root_ptr).unwrap();
        let entry = restored.read_entry_raw(&restored.sets[0], 1);
        assert_eq!(entry.value.as_btree_ptr(), Some(&child));
    }
}
