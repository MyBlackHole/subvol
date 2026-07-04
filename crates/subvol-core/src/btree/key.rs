//! B-tree key 和 value 类型 — bcachefs 对齐
//!
//! 核心设计：
//! - `Bpos`: 搜索位置（inode + offset + snapshot），与 bcachefs `struct bpos` 兼容
//! - `BkeyFormat`: 每节点 key 打包格式定义，对应 bcachefs `struct bkey_format`
//! - `BkeyPacked`: 变长打包 key，存储在 btree node 内
//! - `BtreeKey`/`BchVal`: 对外 API 类型（保持向后兼容）
//!
//! ## 打包格式
//!
//! 打包 key 在 btree node 内的布局：
//! ```text
//! [u64s:1][format+needs_whiteout:1][type:1][packed_fields:variable]
//! ```
//!
//! 位流编解码（LE）：
//! - 从 high word（packed key 的最后一个 u64）的 MSB 开始，向 LSB 方向填充
//! - 一个 word 填满后（bits=64 用完），移动到前一个 word
//! - 每个字段：实际值 = 存储值 + field_offset，存储在 bits_per_field 位中
//!
//! 内部 node 使用 `KEY_FORMAT_LOCAL_BTREE=0` 表示 packed 格式，
//! 外部使用 `KEY_FORMAT_CURRENT=1` 表示 unpacked 格式。

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

use crate::btree::types::BtreePtrV2;

// ─── 常量定义 ───────────────────────────────────────────────

/// 打包 key 位流的起始偏移（LE 为 0）
pub const KEY_PACKED_BITS_START: u32 = 0;

/// Packed key 格式（btree node 内部使用）
pub const KEY_FORMAT_LOCAL_BTREE: u8 = 0;
/// Unpacked/current key 格式（内存中操作时使用）
pub const KEY_FORMAT_CURRENT: u8 = 1;
/// bcachefs KEY_FORMAT_TEXT（2）— 保留/未在 subvol 中使用
///
/// bcachefs 在较新版本中定义了 KEY_FORMAT_TEXT = 2 和 KEY_FORMAT_JSON = 3，
/// 用于二进制外的文本/json 序列化。subvol 作为存储引擎不需要这些格式，
/// 标记为保留值以确保磁盘兼容性检查不会误判。
/// 7 位 format 字段支持 0..127，当前仅使用 0 和 1。
pub(crate) const _KEY_FORMAT_TEXT_RESERVED: u8 = 2;
pub(crate) const _KEY_FORMAT_JSON_RESERVED: u8 = 3;

/// 最大 field 数量
pub const BKEY_NR_FIELDS: usize = 5;

/// Field 枚举 — 对应 bcachefs `enum bch_bkey_fields`
pub const BKEY_FIELD_INODE: usize = 0;
pub const BKEY_FIELD_OFFSET: usize = 1;
pub const BKEY_FIELD_SNAPSHOT: usize = 2;
pub const BKEY_FIELD_PADDR: usize = 3;
pub const BKEY_FIELD_VER: usize = 4;

/// 每个 field 在 unpacked 格式（BKEY_FORMAT_CURRENT）中的位数
pub const BKEY_FIELD_BITS: [u8; BKEY_NR_FIELDS] = [64, 64, 32, 48, 16];

/// Unpacked bkey 的 u64s 数（3 u64s = 24 bytes：3 header + 20 bpos + 1 pad）
/// 注意：这不包括 value。完整的 bkey_i 还需要 value 的 u64s。
pub const BKEY_U64S: u8 = 3;

/// btree 指针的 key type（对应 bcachefs KEY_TYPE_btree_ptr_v2 = 18）
///
/// 用于标识 internal btree node 中的 child 指针条目。
/// bcachefs 中 btree_ptr_v2 是内部节点的标准 key type。subvol 中
/// 保持常量定义以对齐 key type 分派逻辑（commit.c / update.c 中的
/// __btree_node_type 检查）。
pub const KEY_TYPE_BTREE_PTR_V3: u8 = 19;

/// 打包 header 的字节数（u64s + format/whiteout + type）
pub const BKEY_HEADER_BYTES: u32 = 3;

// ─── 位操作辅助函数 ────────────────────────────────────────

/// 计算 v 的最高有效位位置（1-indexed），0 表示 v == 0
fn fls64(v: u64) -> u32 {
    if v == 0 {
        0
    } else {
        64 - v.leading_zeros()
    }
}

/// 生成 bits 位的全 1 掩码
fn u64_bitmask(bits: u32) -> u64 {
    if bits >= 64 {
        u64::MAX
    } else {
        (1u64 << bits) - 1
    }
}

// ─── u48 类型 ───────────────────────────────────────────────

/// 48 位物理地址（最大支持 256TB 地址空间，4K 块对齐）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(C)]
pub struct Addr48(u64);

impl Addr48 {
    pub const MAX: u64 = (1u64 << 48) - 1;

    pub fn new(v: u64) -> Self {
        assert!(v <= Self::MAX, "Addr48 overflow: {:#x}", v);
        Self(v)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

impl From<Addr48> for u64 {
    fn from(a: Addr48) -> u64 {
        a.0
    }
}

// ─── Key 类型常量 ────────────────────────────────────────────

/// Key 类型 — 对应 bcachefs KEY_TYPE_*
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum KeyType {
    Normal = 0,
    Deleted = 1,
    Whiteout = 2,
    /// Empty-value presence key, corresponding to bcachefs KEY_TYPE_set=25.
    Set = 25,
}

impl KeyType {
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => KeyType::Normal,
            1 => KeyType::Deleted,
            2 => KeyType::Whiteout,
            25 => KeyType::Set,
            _ => panic!("invalid KeyType: {}", v),
        }
    }
}

// ═══════════════════════════════════════════════════════════════
// Bpos — 搜索位置（bcachefs `struct bpos` 兼容）
// ═══════════════════════════════════════════════════════════════

/// B-tree 搜索位置 — 对应 bcachefs `struct bpos`
///
/// 排序：(inode ASC, offset ASC, snapshot ASC)
/// 注意：bcachefs 磁盘上（LE）的字段顺序是 (snapshot, offset, inode)，
///      这样 memcmp 就相当于大整数比较。但内存中我们使用自然顺序。
///      由于选项 B（独立 magic），我们保持 subvol 的字段顺序。
///
/// `#[repr(C)]` 确保固定的内存布局（24 字节：8 + 8 + 4 + 4 padding）。
/// bcachefs C: `struct bpos { __le64 inode; __le64 offset; __le32 snapshot; } __packed __aligned(4)` — 20 字节
/// Rust repr(C) 不可 packed，但选项 B 允许我们保持 24 字节。
///
/// 字段：
///   inode:  卷/设备标识（对应 bcachefs 的 inode）
///   offset:  逻辑偏移位置（原 vaddr）
///   snapshot: 快照 ID（U32_MAX 向下分配，越大越新）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(C)]
pub struct Bpos {
    /// 卷/设备标识（对应 bcachefs 的 inode，subvol 中为逻辑卷 ID）
    pub inode: u64,
    pub offset: u64,
    pub snapshot: u32,
}

impl Bpos {
    pub const MIN: Bpos = Bpos {
        inode: 0,
        offset: 0,
        snapshot: 0,
    };
    pub const MAX: Bpos = Bpos {
        inode: u64::MAX,
        offset: u64::MAX,
        snapshot: u32::MAX,
    };
    pub const ZERO: Bpos = Bpos {
        inode: 0,
        offset: 0,
        snapshot: 0,
    };

    pub fn new(inode: u64, offset: u64, snapshot: u32) -> Self {
        Self {
            inode,
            offset,
            snapshot,
        }
    }

    /// 从 BtreeKey 转换为 Bpos
    ///
    /// 映射关系：
    /// - `bpos.inode` = `key.inode`
    /// - `bpos.offset` = `key.vaddr`
    /// - `bpos.snapshot` = `key.snapshot_id`
    pub fn from_key(key: &BtreeKey) -> Self {
        Self {
            inode: unsafe { std::ptr::addr_of!(key.inode).read_unaligned() },
            offset: key.get_vaddr(),
            snapshot: key.get_snapshot_id(),
        }
    }

    pub fn is_min(&self) -> bool {
        self.inode == 0 && self.offset == 0 && self.snapshot == 0
    }

    pub fn is_max(&self) -> bool {
        self.inode == u64::MAX && self.offset == u64::MAX && self.snapshot == u32::MAX
    }

    /// successor: snapshot → offset → inode
    pub fn successor(&self) -> Self {
        let mut p = *self;
        p.snapshot = p.snapshot.wrapping_add(1);
        if p.snapshot == 0 {
            p.offset = p.offset.wrapping_add(1);
            if p.offset == 0 {
                p.inode = p.inode.wrapping_add(1);
            }
        }
        p
    }

    /// predecessor: snapshot → offset → inode
    pub fn predecessor(&self) -> Self {
        let mut p = *self;
        p.snapshot = p.snapshot.wrapping_sub(1);
        if p.snapshot == u32::MAX {
            p.offset = p.offset.wrapping_sub(1);
            if p.offset == u64::MAX {
                p.inode = p.inode.wrapping_sub(1);
            }
        }
        p
    }
}

impl std::fmt::Display for Bpos {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}@{}", self.inode, self.offset, self.snapshot)
    }
}

/// 排序: (inode ASC, offset ASC, snapshot ASC)
impl PartialOrd for Bpos {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Bpos {
    fn cmp(&self, other: &Self) -> Ordering {
        self.inode
            .cmp(&other.inode)
            .then_with(|| self.offset.cmp(&other.offset))
            .then_with(|| self.snapshot.cmp(&other.snapshot))
    }
}

// ═══════════════════════════════════════════════════════════════
// bcachefs 对齐的 bpos 比较/操作函数
// ═══════════════════════════════════════════════════════════════

/// `bpos_eq` — 对应 bcachefs `bpos_eq()`
pub fn bpos_eq(l: Bpos, r: Bpos) -> bool {
    l == r
}

/// `bpos_lt` — 对应 bcachefs `bpos_lt()`
pub fn bpos_lt(l: Bpos, r: Bpos) -> bool {
    l < r
}

/// `bpos_le` — 对应 bcachefs `bpos_le()`
pub fn bpos_le(l: Bpos, r: Bpos) -> bool {
    l <= r
}

/// `bpos_gt` — 对应 bcachefs `bpos_gt()`
pub fn bpos_gt(l: Bpos, r: Bpos) -> bool {
    l > r
}

/// `bpos_ge` — 对应 bcachefs `bpos_ge()`
pub fn bpos_ge(l: Bpos, r: Bpos) -> bool {
    l >= r
}

/// `bpos_cmp` — 对应 bcachefs `bpos_cmp()`，返回 -1/0/1
pub fn bpos_cmp(l: Bpos, r: Bpos) -> Ordering {
    l.cmp(&r)
}

/// `bpos_successor` — 对应 bcachefs `bpos_successor()`
pub fn bpos_successor(p: Bpos) -> Bpos {
    p.successor()
}

/// `bpos_predecessor` — 对应 bcachefs `bpos_predecessor()`
pub fn bpos_predecessor(p: Bpos) -> Bpos {
    p.predecessor()
}

// ═══════════════════════════════════════════════════════════════
// BkeyFormat — btree node 的 key 打包格式
// ═══════════════════════════════════════════════════════════════

/// Key 打包格式 — 对应 bcachefs `struct bkey_format`
///
/// 定义 node 内每个 key 的 packed 布局：
/// - `key_u64s`: packed key 占用的 u64 数（含 3 字节 header）
/// - `nr_fields`: field 数量（BKEY_NR_FIELDS=5）
/// - `bits_per_field[i]`: 第 i 个 field 的位数
/// - `field_offset[i]`: 第 i 个 field 的偏移量（le 编码，实际值 = 存储值 + 偏移量）
///
/// `#[repr(C, packed)]` 移除了 C repr 中的 1 字节 padding → 47 字节（C bcachefs
/// 用 6 fields 56 字节；subvol 用 5 fields 47 字节，这是选项 B 的合理差异）。
///
/// 打包过程：对每个 field，实际值减去 field_offset，用 bits_per_field 位存储。
/// 解包过程：读取 bits_per_field 位，加上 field_offset，得到实际值。
#[derive(Debug, Clone, Copy)]
#[repr(C, packed)]
pub struct BkeyFormat {
    pub key_u64s: u8,
    pub nr_fields: u8,
    pub bits_per_field: [u8; BKEY_NR_FIELDS],
    pub field_offset: [u64; BKEY_NR_FIELDS],
}

impl BkeyFormat {
    /// 创建一个最简单的格式（所有字段满位，允许打包任意值）
    pub fn new_current() -> Self {
        Self {
            key_u64s: BKEY_U64S,
            nr_fields: BKEY_NR_FIELDS as u8,
            bits_per_field: BKEY_FIELD_BITS,
            field_offset: [0; BKEY_NR_FIELDS],
        }
    }

    /// 打包 key 部分的字节数
    pub fn key_bytes(&self) -> u32 {
        self.key_u64s as u32 * 8
    }

    /// 打包 key 部分的位数（不含 header）
    pub fn key_bits(&self) -> u32 {
        // key_u64s * 8 * 8 bits = key_u64s * 64 bits
        // 减去 header 的 24 bits = 3 bytes
        self.key_u64s as u32 * 64
    }

    /// 该格式下 packed bpos 的位数
    pub fn bpos_key_bits(&self) -> u32 {
        self.bits_per_field[BKEY_FIELD_INODE] as u32
            + self.bits_per_field[BKEY_FIELD_OFFSET] as u32
            + self.bits_per_field[BKEY_FIELD_SNAPSHOT] as u32
    }

    /// high word offset（LE: key_u64s - 1）
    fn high_word_offset(&self) -> u32 {
        if cfg!(target_endian = "little") {
            self.key_u64s as u32 - 1
        } else {
            0
        }
    }

    /// 获取某个 field 的 field_offset（已经在 LE 编码）
    pub fn field_offset_value(&self, field: usize) -> u64 {
        self.field_offset[field]
    }

    /// 获取某个 field 的最大可表示值（field_offset + 2^bits - 1）
    pub fn field_max(&self, field: usize) -> u64 {
        let bits = self.bits_per_field[field] as u32;
        if bits >= 64 {
            u64::MAX
        } else {
            self.field_offset[field] + ((1u64 << bits) - 1)
        }
    }
}

impl Default for BkeyFormat {
    fn default() -> Self {
        Self::new_current()
    }
}

impl std::fmt::Display for BkeyFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "u64s={} fields=[", self.key_u64s)?;
        for i in 0..self.nr_fields as usize {
            if i > 0 {
                write!(f, ", ")?;
            }
            // copy to local before display — #[repr(C, packed)] 禁止直接引用
            let b = self.bits_per_field[i];
            let off = self.field_offset[i];
            write!(f, "{}:{}", b, off)?;
        }
        write!(f, "]")
    }
}

// ═══════════════════════════════════════════════════════════════
// BkeyPacked — 变长打包 key（btree node 内部存储格式）
// ═══════════════════════════════════════════════════════════════

/// 打包 bkey — 对应 bcachefs `struct bkey_packed`
///
/// 内存布局（3 字节 header + 变长 packed 字段）：
/// ```text
/// byte 0: u64s       — key + value 总大小（u64s）
/// byte 1: [format:7][needs_whiteout:1]
/// byte 2: type       — value 类型
/// byte 3+: key_start — 按 bkey_format 打包的字段位流
/// ```
#[derive(Debug, Clone, Copy)]
#[repr(C, packed)]
pub struct BkeyPacked {
    /// key + value 总大小（u64 为单位）
    pub u64s: u8,
    /// format:7 + needs_whiteout:1
    pub format_whiteout: u8,
    /// value 类型
    pub type_: u8,
    /// packed 字段从这里开始
    pub key_start: [u8; 0],
}

impl BkeyPacked {
    pub fn format(&self) -> u8 {
        self.format_whiteout & 0x7F
    }

    pub fn needs_whiteout(&self) -> bool {
        self.format_whiteout & 0x80 != 0
    }

    pub fn set_format(&mut self, f: u8) {
        debug_assert!(f <= 0x7F);
        self.format_whiteout = (self.format_whiteout & 0x80) | f;
    }

    pub fn set_needs_whiteout(&mut self, v: bool) {
        if v {
            self.format_whiteout |= 0x80;
        } else {
            self.format_whiteout &= 0x7F;
        }
    }

    /// 判断是否是 packed key（format != KEY_FORMAT_CURRENT）
    pub fn is_packed(&self) -> bool {
        self.format() != KEY_FORMAT_CURRENT
    }

    /// 总字节数（u64s * 8）
    pub fn total_bytes(&self) -> u32 {
        self.u64s as u32 * 8
    }
}

// ═══════════════════════════════════════════════════════════════
// 位流 Pack State / Unpack State（bcachefs bkey.c 对齐）
// ═══════════════════════════════════════════════════════════════

/// 打包状态机 — 对应 bcachefs `struct pack_state`
struct PackState {
    format: *const BkeyFormat,
    bits: u32,   // 当前 word 中剩余的位数
    w: u64,      // 当前正在构建的 word
    p: *mut u64, // 当前指向的 word 指针
}

impl PackState {
    fn new(format: &BkeyFormat, start: *mut u64, key_u64s: u32) -> Self {
        let high_off = (key_u64s - 1) as isize;
        let p = unsafe { start.offset(high_off) };
        Self {
            format: format as *const BkeyFormat,
            bits: 64, // LE: 从 MSB 开始
            w: 0,
            p,
        }
    }

    fn format(&self) -> &BkeyFormat {
        unsafe { &*self.format }
    }

    /// 前一个 word（从高向低移动）
    fn prev_word(&mut self) {
        unsafe {
            self.p = self.p.offset(-1);
        }
    }

    /// 结束打包，将当前 word 写回
    fn finish(&mut self) {
        unsafe {
            *self.p = self.w;
        }
    }
}

/// 解包状态机 — 对应 bcachefs `struct unpack_state`
struct UnpackState<'a> {
    format: &'a BkeyFormat,
    bits: u32,     // 当前 word 中剩余的位数
    w: u64,        // 当前 word
    p: *const u64, // 指向当前 word 的指针
}

impl<'a> UnpackState<'a> {
    fn new(format: &'a BkeyFormat, start: *const u64, key_u64s: u32) -> Self {
        let high_off = (key_u64s - 1) as isize;
        let p = unsafe { start.offset(high_off) };
        // LE: 从高 word 读取，高位对齐
        let w = unsafe { *p };
        Self {
            format,
            bits: 64,
            w,
            p,
        }
    }

    /// 下一个 word（从高向低移动）
    fn prev_word(&mut self) {
        unsafe {
            self.p = self.p.offset(-1);
            self.w = *self.p;
            self.bits = 64;
        }
    }
}

// ─── 打包 Field 编解码 ─────────────────────────────────────

/// 从 unpack_state 中提取一个 field 的值（LE 版本）
fn get_inc_field(state: &mut UnpackState, field: usize) -> u64 {
    let bits = state.format.bits_per_field[field] as u32;
    let offset = state.format.field_offset[field];

    let mut v: u64;
    if bits >= state.bits {
        // 从当前 word 取剩余部分
        v = state.w >> (64 - bits);
        let bits_consumed = state.bits;
        let bits_remaining = bits - bits_consumed;

        // 移动到前一个 word
        state.prev_word();

        // 从前一个 word 取剩余部分
        v |= (state.w >> 1) >> (63 - bits_remaining);
        state.w <<= bits_remaining;
        state.bits -= bits_remaining;
    } else {
        v = state.w >> (64 - bits);
        state.w <<= bits;
        state.bits -= bits;
    }

    v + offset
}

/// 将一个 field 的值写入 pack_state（LE 版本）
fn set_inc_field(state: &mut PackState, field: usize, raw_value: u64) -> bool {
    let fmt = state.format();
    let bits = fmt.bits_per_field[field] as u32;
    let offset = fmt.field_offset[field];

    if raw_value < offset {
        return false;
    }

    let v = raw_value - offset;

    if fls64(v) > bits {
        return false;
    }

    if bits > 0 {
        if bits > state.bits {
            // 当前 word 放不下：先填满当前 word（取 v 的高 fill_bits 位）
            let fill_bits = state.bits; // 当前 word 剩余位数
            let remaining = bits - fill_bits; // 要放入下个 word 的位数

            if fill_bits > 0 && remaining > 0 {
                // 一般情况：v 的高 fill_bits 位填满当前 word
                state.w |= v >> remaining;
            }
            // 若 fill_bits=0：当前 word 已满，状态 w 无需变更
            // 若 remaining=0：刚好填满，v 无需分到下个 word

            state.bits = 0;

            // 写回当前 word 并移到前一个 word
            state.finish();
            state.prev_word();
            state.w = 0;
            state.bits = 64;

            if remaining > 0 {
                // 将 v 的低 remaining 位放入新 word
                state.bits -= remaining;
                state.w |= v << state.bits;
            }
        } else {
            // 当前 word 足够放下整个 field
            state.bits -= bits;
            state.w |= v << state.bits;
        }
    }

    true
}

// ═══════════════════════════════════════════════════════════════
// 高级打包/解包 API
// ═══════════════════════════════════════════════════════════════

/// BkeyFormat 的 CURRENT 常量（所有字段满位）
pub const BKEY_FORMAT_CURRENT: BkeyFormat = BkeyFormat {
    key_u64s: BKEY_U64S,
    nr_fields: BKEY_NR_FIELDS as u8,
    bits_per_field: BKEY_FIELD_BITS,
    field_offset: [0; BKEY_NR_FIELDS],
};

/// 解包 packed bkey 中的 bpos
///
/// 对应 bcachefs `__bkey_unpack_pos()`
pub fn bkey_unpack_pos(format: &BkeyFormat, k: &BkeyPacked) -> Bpos {
    debug_assert!(k.format() == KEY_FORMAT_LOCAL_BTREE);
    debug_assert!((k.u64s as u32) >= format.key_bytes() / 8);

    let start = std::ptr::addr_of!(*k).cast::<u64>();
    let mut state = UnpackState::new(format, start, format.key_u64s as u32);

    Bpos {
        inode: get_inc_field(&mut state, BKEY_FIELD_INODE),
        offset: get_inc_field(&mut state, BKEY_FIELD_OFFSET),
        snapshot: get_inc_field(&mut state, BKEY_FIELD_SNAPSHOT) as u32,
    }
}

/// 比较两个 packed bkey 的 bpos 字段，逐 field 比较，在第一个差异处立即返回。
/// 避免在早期字段已不同时解包所有 3 个字段。
///
/// 等价于：bkey_unpack_pos(format, a).cmp(&bkey_unpack_pos(format, b))
pub fn bkey_cmp_packed(format: &BkeyFormat, a: &BkeyPacked, b: &BkeyPacked) -> std::cmp::Ordering {
    debug_assert!(a.format() == KEY_FORMAT_LOCAL_BTREE);
    debug_assert!(b.format() == KEY_FORMAT_LOCAL_BTREE);

    let start_a = std::ptr::addr_of!(*a).cast::<u64>();
    let start_b = std::ptr::addr_of!(*b).cast::<u64>();
    let mut state_a = UnpackState::new(format, start_a, format.key_u64s as u32);
    let mut state_b = UnpackState::new(format, start_b, format.key_u64s as u32);

    // 比较 inode
    match get_inc_field(&mut state_a, BKEY_FIELD_INODE)
        .cmp(&get_inc_field(&mut state_b, BKEY_FIELD_INODE))
    {
        std::cmp::Ordering::Equal => {}
        other => return other,
    }

    // 比较 offset
    match get_inc_field(&mut state_a, BKEY_FIELD_OFFSET)
        .cmp(&get_inc_field(&mut state_b, BKEY_FIELD_OFFSET))
    {
        std::cmp::Ordering::Equal => {}
        other => return other,
    }

    // 比较 snapshot
    get_inc_field(&mut state_a, BKEY_FIELD_SNAPSHOT)
        .cmp(&get_inc_field(&mut state_b, BKEY_FIELD_SNAPSHOT))
}

/// 比较 packed bkey 的 bpos 与未打包的 Bpos，逐 field 比较。
/// 在第一个差异处停止，避免解包剩余字段。
pub fn bkey_cmp_packed_vs_bpos(
    format: &BkeyFormat,
    packed: &BkeyPacked,
    bpos: &Bpos,
) -> std::cmp::Ordering {
    debug_assert!(packed.format() == KEY_FORMAT_LOCAL_BTREE);

    let start = std::ptr::addr_of!(*packed).cast::<u64>();
    let mut state = UnpackState::new(format, start, format.key_u64s as u32);

    // 比较 inode
    match get_inc_field(&mut state, BKEY_FIELD_INODE).cmp(&bpos.inode) {
        std::cmp::Ordering::Equal => {}
        other => return other,
    }

    // 比较 offset
    match get_inc_field(&mut state, BKEY_FIELD_OFFSET).cmp(&bpos.offset) {
        std::cmp::Ordering::Equal => {}
        other => return other,
    }

    // 比较 snapshot
    get_inc_field(&mut state, BKEY_FIELD_SNAPSHOT).cmp(&(bpos.snapshot as u64))
}

/// 将 bpos 打包到 packed bkey 中
///
/// 对应 bcachefs `bch2_bkey_pack_pos()`
/// 返回 true 表示成功
pub fn bkey_pack_pos(out: &mut BkeyPacked, pos: Bpos, format: &BkeyFormat) -> bool {
    let start = std::ptr::addr_of_mut!(*out).cast::<u64>();

    // 清零所有 u64
    unsafe {
        for i in 0..format.key_u64s as isize {
            *start.offset(i) = 0;
        }
    }

    let mut state = PackState::new(format, start, format.key_u64s as u32);

    if !set_inc_field(&mut state, BKEY_FIELD_INODE, pos.inode) {
        return false;
    }
    if !set_inc_field(&mut state, BKEY_FIELD_OFFSET, pos.offset) {
        return false;
    }
    if !set_inc_field(&mut state, BKEY_FIELD_SNAPSHOT, pos.snapshot as u64) {
        return false;
    }

    state.finish();

    out.u64s = format.key_u64s;
    out.set_format(KEY_FORMAT_LOCAL_BTREE);
    out.type_ = 0; // KEY_TYPE_deleted — 调用者应覆盖

    true
}

/// bcachefs `enum bkey_pack_pos_ret` (`bkey.h:454`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BkeyPackPosRet {
    Exact,
    Smaller,
    Fail,
}

/// bcachefs `bch2_bkey_pack_pos_lossy()` (`bkey.c:858`).
pub fn bch2_bkey_pack_pos_lossy(
    out: &mut BkeyPacked,
    pos: &Bpos,
    format: &BkeyFormat,
) -> BkeyPackPosRet {
    let mut input = *pos;
    let mut exact = true;

    let inode_offset = format.field_offset[BKEY_FIELD_INODE];
    let offset_offset = format.field_offset[BKEY_FIELD_OFFSET];
    let snapshot_offset = format.field_offset[BKEY_FIELD_SNAPSHOT];

    if u64::from(input.snapshot) < snapshot_offset {
        if input.offset == 0 {
            if input.inode == 0 {
                return BkeyPackPosRet::Fail;
            }
            input.inode -= 1;
            input.offset = u64::MAX;
        } else {
            input.offset -= 1;
        }
        input.snapshot = u32::MAX;
        exact = false;
    }

    if input.offset < offset_offset {
        if input.inode == 0 {
            return BkeyPackPosRet::Fail;
        }
        input.inode -= 1;
        input.offset = u64::MAX;
        input.snapshot = u32::MAX;
        exact = false;
    }

    if input.inode < inode_offset {
        return BkeyPackPosRet::Fail;
    }

    let mut inode = input.inode - inode_offset;
    let mut offset = input.offset - offset_offset;
    let mut snapshot = u64::from(input.snapshot) - snapshot_offset;
    let inode_bits = format.bits_per_field[BKEY_FIELD_INODE] as u32;
    let offset_bits = format.bits_per_field[BKEY_FIELD_OFFSET] as u32;
    let snapshot_bits = format.bits_per_field[BKEY_FIELD_SNAPSHOT] as u32;

    if fls64(inode) > inode_bits {
        inode = if inode_bits == 64 {
            u64::MAX
        } else {
            (1u64 << inode_bits) - 1
        };
        offset = if offset_bits == 64 {
            u64::MAX
        } else {
            (1u64 << offset_bits) - 1
        };
        snapshot = if snapshot_bits == 64 {
            u64::MAX
        } else {
            (1u64 << snapshot_bits) - 1
        };
        exact = false;
    } else if fls64(offset) > offset_bits {
        offset = if offset_bits == 64 {
            u64::MAX
        } else {
            (1u64 << offset_bits) - 1
        };
        snapshot = if snapshot_bits == 64 {
            u64::MAX
        } else {
            (1u64 << snapshot_bits) - 1
        };
        exact = false;
    } else if fls64(snapshot) > snapshot_bits {
        snapshot = if snapshot_bits == 64 {
            u64::MAX
        } else {
            (1u64 << snapshot_bits) - 1
        };
        exact = false;
    }

    let start = std::ptr::addr_of_mut!(*out).cast::<u64>();
    unsafe {
        for i in 0..format.key_u64s as isize {
            *start.offset(i) = 0;
        }
    }
    let mut state = PackState::new(format, start, format.key_u64s as u32);
    debug_assert!(set_inc_field(
        &mut state,
        BKEY_FIELD_INODE,
        inode + inode_offset
    ));
    debug_assert!(set_inc_field(
        &mut state,
        BKEY_FIELD_OFFSET,
        offset + offset_offset
    ));
    debug_assert!(set_inc_field(
        &mut state,
        BKEY_FIELD_SNAPSHOT,
        snapshot + snapshot_offset
    ));
    state.finish();
    out.u64s = format.key_u64s;
    out.set_format(KEY_FORMAT_LOCAL_BTREE);
    out.type_ = KeyType::Deleted as u8;

    if exact {
        BkeyPackPosRet::Exact
    } else {
        BkeyPackPosRet::Smaller
    }
}

/// unpack bkey 的 key 字段（inode, offset, snapshot）+ raw value（paddr, ver）
///
/// 对应 bcachefs `__bch2_bkey_unpack_key()` + value read
/// key 字段通过 PackState 解包，value 字段作为 raw bytes 读取
pub fn bkey_unpack(format: &BkeyFormat, k: &BkeyPacked) -> (Bpos, u8, u64, u16) {
    let start = std::ptr::addr_of!(*k).cast::<u64>();
    let mut state = UnpackState::new(format, start, format.key_u64s as u32);

    // 只解包 key 字段（inode, offset, snapshot）
    let inode = get_inc_field(&mut state, BKEY_FIELD_INODE);
    let offset = get_inc_field(&mut state, BKEY_FIELD_OFFSET);
    let snapshot = get_inc_field(&mut state, BKEY_FIELD_SNAPSHOT) as u32;

    // value 字段从 key 之后的 raw bytes 读取
    let value_off = (format.key_u64s as usize) * 8;
    let paddr: u64;
    let ver: u16;
    unsafe {
        let base = k as *const BkeyPacked as *const u8;
        let mut paddr_buf = [0u8; 8];
        std::ptr::copy_nonoverlapping(base.add(value_off), paddr_buf.as_mut_ptr(), 6);
        paddr = u64::from_le_bytes(paddr_buf);

        let mut ver_buf = [0u8; 2];
        std::ptr::copy_nonoverlapping(base.add(value_off + 6), ver_buf.as_mut_ptr(), 2);
        ver = u16::from_le_bytes(ver_buf);
    }

    let key_type = k.type_;

    (
        Bpos {
            inode: inode,
            offset,
            snapshot,
        },
        key_type,
        paddr,
        ver,
    )
}

/// unpack bkey 的 key 字段 + 返回 raw value bytes
///
/// 与 [`bkey_unpack`] 相同，但返回任意长度的 value bytes 切片，
/// 而非固定 (paddr, ver)。适用于变长 value 场景。
pub fn bkey_unpack_bytes<'a>(format: &BkeyFormat, k: &'a BkeyPacked) -> (Bpos, u8, &'a [u8]) {
    let start = std::ptr::addr_of!(*k).cast::<u64>();
    let mut state = UnpackState::new(format, start, format.key_u64s as u32);

    let inode = get_inc_field(&mut state, BKEY_FIELD_INODE);
    let offset = get_inc_field(&mut state, BKEY_FIELD_OFFSET);
    let snapshot = get_inc_field(&mut state, BKEY_FIELD_SNAPSHOT) as u32;

    let total_bytes = (k.u64s as usize) * 8;
    let value_off = (format.key_u64s as usize) * 8;
    let value_len = total_bytes - value_off;
    let value_bytes = unsafe {
        let base = k as *const BkeyPacked as *const u8;
        std::slice::from_raw_parts(base.add(value_off), value_len)
    };

    (
        Bpos {
            inode: inode,
            offset,
            snapshot,
        },
        k.type_,
        value_bytes,
    )
}

/// pack bkey 的 key 字段 + raw value bytes（变长 value 用）
///
/// 与 [`bkey_pack`] 相同，但接受任意长度 value bytes，
/// 而非固定 (paddr, ver)。设置 `u64s = key_u64s + ceil(value_bytes.len() / 8)`。
pub fn bkey_pack_raw(
    out: &mut BkeyPacked,
    pos: Bpos,
    key_type: u8,
    value_bytes: &[u8],
    format: &BkeyFormat,
) -> bool {
    let start = std::ptr::addr_of_mut!(*out).cast::<u64>();

    // 清零 key 部分的所有 u64
    unsafe {
        for i in 0..format.key_u64s as isize {
            *start.offset(i) = 0;
        }
    }

    // 打包 key 字段（inode, offset, snapshot）
    let mut state = PackState::new(format, start, format.key_u64s as u32);
    if !set_inc_field(&mut state, BKEY_FIELD_INODE, pos.inode) {
        return false;
    }
    if !set_inc_field(&mut state, BKEY_FIELD_OFFSET, pos.offset) {
        return false;
    }
    if !set_inc_field(&mut state, BKEY_FIELD_SNAPSHOT, pos.snapshot as u64) {
        return false;
    }
    state.finish();

    // value bytes 写入 key 之后
    let value_off = (format.key_u64s as usize) * 8;
    let value_u64s = value_bytes.len().div_ceil(8);
    let total_u64s = format.key_u64s + value_u64s as u8;
    unsafe {
        let base = out as *mut BkeyPacked as *mut u8;
        std::ptr::copy_nonoverlapping(value_bytes.as_ptr(), base.add(value_off), value_bytes.len());
        // 剩余部分零填充（对齐到 u64）
        for i in value_off + value_bytes.len()..total_u64s as usize * 8 {
            *base.add(i) = 0;
        }
    }

    out.u64s = total_u64s;
    out.set_format(KEY_FORMAT_LOCAL_BTREE);
    out.type_ = key_type;
    true
}

/// pack bkey 的 key 字段（inode, offset, snapshot）+ raw value（paddr, ver）
///
/// 对应 bcachefs `bch2_bkey_pack_key()` + value write
/// key 字段通过 PackState 打包，value 字段作为 raw bytes 写入 key 之后
pub fn bkey_pack(
    out: &mut BkeyPacked,
    pos: Bpos,
    key_type: u8,
    paddr: u64,
    ver: u16,
    format: &BkeyFormat,
) -> bool {
    let start = std::ptr::addr_of_mut!(*out).cast::<u64>();

    // 清零 key 部分的所有 u64
    unsafe {
        for i in 0..format.key_u64s as isize {
            *start.offset(i) = 0;
        }
    }

    // 只打包 key 字段（inode, offset, snapshot）
    let mut state = PackState::new(format, start, format.key_u64s as u32);

    if !set_inc_field(&mut state, BKEY_FIELD_INODE, pos.inode) {
        return false;
    }
    if !set_inc_field(&mut state, BKEY_FIELD_OFFSET, pos.offset) {
        return false;
    }
    if !set_inc_field(&mut state, BKEY_FIELD_SNAPSHOT, pos.snapshot as u64) {
        return false;
    }

    state.finish();

    // value 字段作为 raw bytes 写入 key 之后
    let value_off = (format.key_u64s as usize) * 8;
    unsafe {
        let base = out as *mut BkeyPacked as *mut u8;
        let paddr_bytes = paddr.to_le_bytes();
        std::ptr::copy_nonoverlapping(paddr_bytes.as_ptr(), base.add(value_off), 6);
        let ver_bytes = ver.to_le_bytes();
        std::ptr::copy_nonoverlapping(ver_bytes.as_ptr(), base.add(value_off + 6), 2);
    }

    // u64s = key_u64s + 1 value u64（paddr 48bit + ver 16bit = 64bit = 1 u64）
    out.u64s = format.key_u64s + 1;
    out.set_format(KEY_FORMAT_LOCAL_BTREE);
    out.type_ = key_type;

    true
}

/// unpack bkey 的 key 字段 + checksum ExtentValue（28B 格式）
///
/// 格式：paddr(6) + ver(2) + size(4) + dev_idx(1)
/// size=0 时转为 1（向后兼容旧 Single 编码）。
pub fn bkey_unpack_extent(format: &BkeyFormat, k: &BkeyPacked) -> (Bpos, u8, ExtentValue) {
    let start = std::ptr::addr_of!(*k).cast::<u64>();
    let mut state = UnpackState::new(format, start, format.key_u64s as u32);

    let inode = get_inc_field(&mut state, BKEY_FIELD_INODE);
    let offset = get_inc_field(&mut state, BKEY_FIELD_OFFSET);
    let snapshot = get_inc_field(&mut state, BKEY_FIELD_SNAPSHOT) as u32;

    let value_off = (format.key_u64s as usize) * 8;

    // 28B format: paddr(6) + ver(2) + size(4) + dev_idx(1) + crc32c(4)
    let paddr: u64;
    let ver: u16;
    let size: u32;
    let dev_idx: u8;
    let crc32c: u32;
    let base = k as *const BkeyPacked as *const u8;
    unsafe {
        let mut paddr_buf = [0u8; 8];
        std::ptr::copy_nonoverlapping(base.add(value_off), paddr_buf.as_mut_ptr(), 6);
        paddr = u64::from_le_bytes(paddr_buf);
        let mut ver_buf = [0u8; 2];
        std::ptr::copy_nonoverlapping(base.add(value_off + 6), ver_buf.as_mut_ptr(), 2);
        ver = u16::from_le_bytes(ver_buf);
        let mut size_buf = [0u8; 4];
        std::ptr::copy_nonoverlapping(base.add(value_off + 8), size_buf.as_mut_ptr(), 4);
        size = u32::from_le_bytes(size_buf);
        dev_idx = *base.add(value_off + 12);
        let mut crc_buf = [0u8; 4];
        std::ptr::copy_nonoverlapping(base.add(value_off + 14), crc_buf.as_mut_ptr(), 4);
        crc32c = u32::from_le_bytes(crc_buf);
    }
    let size = if size == 0 { 1 } else { size };
    let extent = ExtentValue {
        paddr,
        size,
        ver,
        dev_idx,
        crc32c,
        crc_offset_blocks: unsafe {
            let mut offset_buf = [0u8; 8];
            std::ptr::copy_nonoverlapping(base.add(value_off + 18), offset_buf.as_mut_ptr(), 8);
            u64::from_le_bytes(offset_buf)
        },
    };

    (
        Bpos {
            inode,
            offset,
            snapshot,
        },
        k.type_,
        extent,
    )
}

/// pack bkey 的 key 字段 + ExtentValue（统一 28 字节格式）
///
/// `ExtentValue::to_bytes()` 统一编码为 28 字节。
pub fn bkey_pack_extent(
    out: &mut BkeyPacked,
    pos: Bpos,
    key_type: u8,
    extent: &ExtentValue,
    format: &BkeyFormat,
) -> bool {
    let value_bytes = extent.to_bytes();
    bkey_pack_raw(out, pos, key_type, &value_bytes, format)
}

/// 计算 packed bkey 在 bset 中占用的字节数
/// 对于 packed key: format.key_u64s * 8
/// 对于 unpacked key: k.u64s * 8
pub fn bkey_packed_bytes(format: &BkeyFormat, k: &BkeyPacked) -> u32 {
    if k.is_packed() {
        format.key_bytes()
    } else {
        k.u64s as u32 * 8
    }
}

// ═══════════════════════════════════════════════════════════════
// BtreeKey — 对外 API 类型（向后兼容）
// ═══════════════════════════════════════════════════════════════

/// B-tree key — 对外 API 类型
///
/// 向后兼容的包装类型。内部使用 Bpos 作为搜索位置。
///
/// 字段:
///   vaddr:      虚拟块地址（逻辑块号），对应 bpos.offset
///   snapshot_id: 快照 ID（U32_MAX 向下分配，越大越新），对应 bpos.snapshot
///   key_type:   Normal / Deleted / Whiteout
///   version:    MVCC 版本号（bcachefs 对齐: struct bkey.bversion 的简化 u64）
///
/// 排序: (vaddr ASC, snapshot_id DESC, key_type ASC)
/// 注意: version 不参与排序，仅用于 MVCC 版本追踪。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[repr(C, packed)]
pub struct BtreeKey {
    pub inode: u64,
    pub vaddr: u64,
    /// extent 覆盖的 block 数（0=1，向后兼容标记）
    #[serde(default)]
    pub size: u32,
    pub snapshot_id: u32,
    pub key_type: KeyType,
    /// MVCC 版本号 — bcachefs 对齐: struct bkey.bversion (简化 u64)
    ///
    /// `#[serde(default)]` 保证旧版本序列化数据（不含 version 字段）可被反序列化。
    ///
    /// 注意: version **不参与** `PartialEq`/`Ord` 比较（仅用于 MVCC 追踪）。
    /// `PartialEq` 自定义实现与 `Ord` 一致，排除 version 字段。
    #[serde(default)]
    pub version: u64,
}

impl BtreeKey {
    pub const MAX_KEY: BtreeKey = BtreeKey {
        inode: u64::MAX,
        vaddr: u64::MAX,
        size: 0,
        snapshot_id: 0,
        key_type: KeyType::Normal,
        version: 0,
    };
    pub const MIN_KEY: BtreeKey = BtreeKey {
        inode: 0,
        vaddr: 0,
        size: 0,
        snapshot_id: u32::MAX,
        key_type: KeyType::Normal,
        version: 0,
    };

    pub fn new(vaddr: u64, snapshot_id: u32, key_type: KeyType) -> Self {
        Self {
            inode: 0,
            vaddr,
            size: 1,
            snapshot_id,
            key_type,
            version: 0,
        }
    }

    /// 创建带版本号的 BtreeKey
    pub fn with_version(vaddr: u64, snapshot_id: u32, key_type: KeyType, version: u64) -> Self {
        Self {
            inode: 0,
            vaddr,
            size: 1,
            snapshot_id,
            key_type,
            version,
        }
    }

    /// extent 覆盖的起始逻辑块地址
    pub fn start(&self) -> u64 {
        unsafe { std::ptr::addr_of!(self.vaddr).read_unaligned() }
    }

    /// extent 覆盖的结束（不含最后一个 block）
    pub fn end(&self) -> u64 {
        let vaddr = unsafe { std::ptr::addr_of!(self.vaddr).read_unaligned() };
        let size = unsafe { std::ptr::addr_of!(self.size).read_unaligned() };
        vaddr + (if size == 0 { 1 } else { size }) as u64
    }

    pub fn is_max(&self) -> bool {
        *self == Self::MAX_KEY
    }

    pub fn is_min(&self) -> bool {
        *self == Self::MIN_KEY
    }

    /// 安全读取 vaddr（packed struct 避免 UB）
    pub fn get_vaddr(&self) -> u64 {
        unsafe { std::ptr::addr_of!(self.vaddr).read_unaligned() }
    }

    /// 安全读取 snapshot_id（packed struct 避免 UB）
    pub fn get_snapshot_id(&self) -> u32 {
        unsafe { std::ptr::addr_of!(self.snapshot_id).read_unaligned() }
    }

    /// 安全读取 size（packed struct 避免 UB）
    pub fn get_size(&self) -> u32 {
        unsafe { std::ptr::addr_of!(self.size).read_unaligned() }
    }

    /// 转换为 Bpos
    pub fn to_bpos(&self) -> Bpos {
        Bpos {
            inode: unsafe { std::ptr::addr_of!(self.inode).read_unaligned() },
            offset: unsafe { std::ptr::addr_of!(self.vaddr).read_unaligned() },
            snapshot: unsafe { std::ptr::addr_of!(self.snapshot_id).read_unaligned() },
        }
    }

    /// 从 Bpos + KeyType 创建
    pub fn from_bpos(pos: Bpos, key_type: KeyType) -> Self {
        Self {
            inode: pos.inode,
            vaddr: pos.offset,
            size: 1,
            snapshot_id: pos.snapshot,
            key_type,
            version: 0,
        }
    }

    /// 从 Bpos + KeyType + size 创建
    pub fn from_bpos_size(pos: Bpos, key_type: KeyType, size: u32) -> Self {
        Self {
            inode: pos.inode,
            vaddr: pos.offset,
            size,
            snapshot_id: pos.snapshot,
            key_type,
            version: 0,
        }
    }

    /// 裁剪 extent 前部 — 对齐 bcachefs `bch2_cut_front`
    ///
    /// 新范围: `[new_start, self.end())`，self.vaddr 不变（结束位置不变），
    /// self.size 缩小。
    pub fn cut_front(&mut self, new_start: u64) {
        let cur_vaddr = unsafe { std::ptr::addr_of!(self.vaddr).read_unaligned() };
        let cur_size = unsafe { std::ptr::addr_of!(self.size).read_unaligned() };
        let cur_end = cur_vaddr + (if cur_size == 0 { 1 } else { cur_size }) as u64;
        assert!(new_start >= cur_vaddr && new_start < cur_end);
        let shrunk = new_start - cur_vaddr;
        self.vaddr = new_start;
        self.size = cur_size.saturating_sub(shrunk as u32);
    }

    /// 裁剪 extent 后部 — 对齐 bcachefs `bch2_cut_back`
    ///
    /// 新范围: `[self.start(), new_end)`，self.vaddr = start 不变，
    /// self.size 缩小。
    pub fn cut_back(&mut self, new_end: u64) {
        let cur_vaddr = unsafe { std::ptr::addr_of!(self.vaddr).read_unaligned() };
        let cur_size = unsafe { std::ptr::addr_of!(self.size).read_unaligned() };
        let cur_end = cur_vaddr + (if cur_size == 0 { 1 } else { cur_size }) as u64;
        assert!(new_end > cur_vaddr && new_end <= cur_end);
        self.size = (new_end - cur_vaddr) as u32;
    }

    /// 修改 extent 大小 — 对齐 bcachefs `bch2_key_resize`
    pub fn key_resize(&mut self, new_size: u32) {
        self.size = new_size;
    }
}

/// 自定义 PartialEq：仅比较位置字段（vaddr, snapshot_id, key_type），排除 version
///
/// 与 `Ord` 行为一致（排序也排除 version），满足 Rust 约定：
/// `a == b` 应隐含 `a.cmp(&b) == Equal`。
/// 派生 PartialEq 会包含 version 字段，导致 `==` 与 `cmp()` 可能不一致。
impl PartialEq for BtreeKey {
    fn eq(&self, other: &Self) -> bool {
        let a_inode = unsafe { std::ptr::addr_of!(self.inode).read_unaligned() };
        let b_inode = unsafe { std::ptr::addr_of!(other.inode).read_unaligned() };
        let a_vaddr = unsafe { std::ptr::addr_of!(self.vaddr).read_unaligned() };
        let b_vaddr = unsafe { std::ptr::addr_of!(other.vaddr).read_unaligned() };
        let a_sid = unsafe { std::ptr::addr_of!(self.snapshot_id).read_unaligned() };
        let b_sid = unsafe { std::ptr::addr_of!(other.snapshot_id).read_unaligned() };

        a_inode == b_inode
            && a_vaddr == b_vaddr
            && a_sid == b_sid
            && self.key_type == other.key_type
    }
}

impl Eq for BtreeKey {}

/// 排序: vaddr ASC, snapshot_id DESC, key_type ASC
impl PartialOrd for BtreeKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for BtreeKey {
    fn cmp(&self, other: &Self) -> Ordering {
        let a_inode = unsafe { std::ptr::addr_of!(self.inode).read_unaligned() };
        let b_inode = unsafe { std::ptr::addr_of!(other.inode).read_unaligned() };
        let a_vaddr = unsafe { std::ptr::addr_of!(self.vaddr).read_unaligned() };
        let b_vaddr = unsafe { std::ptr::addr_of!(other.vaddr).read_unaligned() };
        let a_sid = unsafe { std::ptr::addr_of!(self.snapshot_id).read_unaligned() };
        let b_sid = unsafe { std::ptr::addr_of!(other.snapshot_id).read_unaligned() };

        a_inode.cmp(&b_inode).then_with(|| {
            a_vaddr
                .cmp(&b_vaddr)
                .then_with(|| b_sid.cmp(&a_sid))
                .then_with(|| (self.key_type as u8).cmp(&(other.key_type as u8)))
        })
    }
}

impl BtreeKey {
    // 仅按 vaddr 比较（忽略 snapshot_id 和 key_type），用于 init 搜索
    pub(crate) fn vaddr_cmp(&self, other: &Self) -> Ordering {
        let a_vaddr = unsafe { std::ptr::addr_of!(self.vaddr).read_unaligned() };
        let b_vaddr = unsafe { std::ptr::addr_of!(other.vaddr).read_unaligned() };
        a_vaddr.cmp(&b_vaddr)
    }
}

impl std::fmt::Display for BtreeKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let vaddr = unsafe { std::ptr::addr_of!(self.vaddr).read_unaligned() };
        let sid = unsafe { std::ptr::addr_of!(self.snapshot_id).read_unaligned() };
        write!(f, "{}@{}:{}", vaddr, sid, self.key_type as u8)
    }
}

// ═══════════════════════════════════════════════════════════════
// BchVal — 对外 API 类型（保持向后兼容）
// ═══════════════════════════════════════════════════════════════

/// B-tree value — 6 bytes 有效 + 2 padding
///
///   paddr: 物理块地址 (48-bit, 最大 256TB)
///   ver:   版本号 (16-bit, 防 ABA)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(C)]
pub struct BchVal {
    pub paddr: Addr48,
    pub ver: u16,
}

impl BchVal {
    pub fn new(paddr: u64, ver: u16) -> Self {
        Self {
            paddr: Addr48::new(paddr),
            ver,
        }
    }
}

impl std::fmt::Display for BchVal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#x}:v{}", self.paddr.get(), self.ver)
    }
}

// ═══════════════════════════════════════════════════════════════
// ExtentValue — 单指针 extent value（bcachefs 对齐）
// ═══════════════════════════════════════════════════════════════

/// Extent value — 存储物理映射信息（统一格式）
///
/// 磁盘序列化格式（28 字节，包含 bcachefs `bch_extent_crc32.csum`）：
/// ```text
/// paddr(6) + ver(2) + size(4) + dev_idx(1) + flags(1) + crc32c(4)
/// + crc_offset_blocks(8) + reserved(2)
/// ```
/// `crc32c` 是整个未压缩 extent 的 CRC32C，存放在 btree value 中，
/// 不存放在数据块本身；这保持 bcachefs checksum 的信任链。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtentValue {
    /// 物理块地址（最多 48 位有效）
    pub paddr: u64,
    /// 连续 block 数（1 = 单 block）
    pub size: u32,
    /// 版本号（防 ABA）
    pub ver: u16,
    /// 设备索引
    pub dev_idx: u8,
    /// bcachefs `bch_extent_crc32.csum`（BCH_CSUM_crc32c = 5）
    pub crc32c: u32,
    /// Offset of this key range within the original checksummed extent.
    pub crc_offset_blocks: u64,
}

/// bcachefs 对齐: bch_extent_ptr — 设备指针（8B packed）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtentPtr {
    /// 设备索引
    pub dev: u8,
    /// bucket generation（防 ABA）
    pub gen: u8,
    /// 设备内偏移（block 单位）
    pub offset: u64,
    /// 缓存标志
    pub cached: bool,
    /// 未写入标志
    pub unwritten: bool,
}

// ─── bcachefs bch_extent_ptr 位宽常量 ───────────────────────
/// bcachefs bch_extent_ptr.offset 位宽（44 bit）
const ADDR_BITS: u64 = 44;
const ADDR_MASK: u64 = (1 << ADDR_BITS) - 1;

impl ExtentPtr {
    /// 从 ExtentValue 构造单指针（CRC 存在 extent value，不重复存入指针）
    pub fn from_extent(v: &ExtentValue) -> Self {
        Self {
            dev: v.dev_idx,
            gen: v.ver as u8,
            offset: v.paddr & ADDR_MASK,
            cached: false,
            unwritten: false,
        }
    }

    /// bcachefs `struct bch_extent_ptr` 对齐的 64-bit packed 格式:
    ///   little-endian: type:1(63) + cached:1(62) + unused:1(61) + unwritten:1(60)
    ///                  + offset:44(59-16) + dev:8(15-8) + gen:8(7-0)
    pub fn to_packed(&self) -> [u8; 8] {
        let val: u64 = (0u64 << 63) // type = 0 (regular extent ptr)
            | ((self.cached as u64) << 62)
            | (0u64 << 61) // unused = 0
            | ((self.unwritten as u64) << 60)
            | ((self.offset & ADDR_MASK) << 16)
            | ((self.dev as u64) << 8)
            | (self.gen as u64);
        val.to_le_bytes()
    }

    /// 从 bcachefs 对齐的 64-bit packed 格式反序列化
    pub fn from_packed(bytes: &[u8]) -> Self {
        let val = u64::from_le_bytes(bytes[..8].try_into().unwrap());
        Self {
            dev: ((val >> 8) & 0xFF) as u8,
            gen: (val & 0xFF) as u8,
            offset: (val >> 16) & ADDR_MASK,
            cached: (val >> 62) & 1 != 0,
            unwritten: (val >> 60) & 1 != 0,
        }
    }
}

impl ExtentValue {
    pub fn paddr(&self) -> u64 {
        self.paddr
    }

    pub fn ver(&self) -> u16 {
        self.ver
    }

    pub fn dev_idx(&self) -> u8 {
        self.dev_idx
    }

    /// 转换为 BchVal（丢弃 dev_idx，兼容非范围感知的旧调用者）
    pub fn to_bchval(&self) -> BchVal {
        BchVal::new(self.paddr, self.ver)
    }

    /// 序列化为字节（28 字节）
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = vec![0u8; 28];
        buf[..6].copy_from_slice(&self.paddr.to_le_bytes()[..6]);
        buf[6..8].copy_from_slice(&self.ver.to_le_bytes());
        buf[8..12].copy_from_slice(&self.size.to_le_bytes());
        buf[12] = self.dev_idx;
        buf[14..18].copy_from_slice(&self.crc32c.to_le_bytes());
        buf[18..26].copy_from_slice(&self.crc_offset_blocks.to_le_bytes());
        buf
    }

    /// 从 checksum extent value 反序列化。
    pub fn from_bytes(bytes: &[u8]) -> Self {
        assert!(bytes.len() >= 28, "checksum extent values must be at least 28 bytes");
        let mut paddr_buf = [0u8; 8];
        paddr_buf[..6].copy_from_slice(&bytes[..6]);
        let paddr = u64::from_le_bytes(paddr_buf);
        let ver = u16::from_le_bytes([bytes[6], bytes[7]]);
        let size = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        let dev_idx = bytes[12];
        let crc32c = u32::from_le_bytes(bytes[14..18].try_into().unwrap());
        let crc_offset_blocks = u64::from_le_bytes(bytes[18..26].try_into().unwrap());
        ExtentValue { paddr, size, ver, dev_idx, crc32c, crc_offset_blocks }
    }

    /// BtreeKey.size 对应的 value 序列化大小（u64 数）
    /// 新格式 28 字节，占 4 u64s。
    pub fn value_u64s(&self) -> u8 {
        4
    }

    /// 裁剪 extent value 前部 — 调整原始 extent 内偏移
    ///
    /// 当 BtreeKey.cut_front 时，paddr 需要增加相同的偏移量。
    pub fn cut_front(&mut self, old_start: u64, new_start: u64) {
        let shift = new_start - old_start;
        let has_crc32c = self.crc_offset_blocks >> 32 != 0 || self.crc32c != 0;
        if !has_crc32c {
            self.paddr += shift;
            self.size = self.size.saturating_sub(shift as u32);
        } else {
            self.crc_offset_blocks = self.crc_offset_blocks.saturating_add(shift);
        }
    }

    /// 裁剪 extent value 后部 — 调整 size
    ///
    /// 当 BtreeKey.cut_back 时，value 的 size 也需要缩小。
    pub fn cut_back(&mut self, old_end: u64, new_end: u64) {
        let shift = old_end - new_end;
        self.size = self.size.saturating_sub(shift as u32);
    }
}

// ═══════════════════════════════════════════════════════════════
// KeyValue — btree entry 值（bcachefs bch_val 兼容）
// ═══════════════════════════════════════════════════════════════

/// Btree entry 值 — 对应 bcachefs 的 `bch_val` 变体
///
/// - `Extent` — 新格式单指针（含 CRC32C 与原始 extent 边界）
/// - `ExtentPtrs` — 新格式多指针（可变长 entry 列表）
/// - `BtreePtr` — 内部节点 child pointer
/// - `Raw` — 任意类型 raw bytes
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyValue {
    /// 单指针 extent（paddr + ver + dev_idx + CRC32C）
    Extent(ExtentValue),
    /// 新格式多指针 extent（可变长 entry 列表 + extent block 数）
    ExtentPtrs {
        blocks: u32,
        ptrs: Vec<ExtentPtr>,
        crc32c: u32,
        crc_offset_blocks: u64,
    },
    /// Internal-node child pointer
    BtreePtr(BtreePtrV2),
    /// 任意类型序列化后的 raw bytes
    Raw(Vec<u8>),
}

impl KeyValue {
    /// 创建一个新的 Extent value（带 dev_idx）
    pub fn extent(paddr: u64, ver: u16, dev_idx: u8) -> Self {
        KeyValue::Extent(ExtentValue {
            paddr,
            size: 1,
            ver,
            dev_idx,
            crc32c: 0,
            crc_offset_blocks: 0,
        })
    }

    /// 创建 raw bytes value
    pub fn raw(bytes: Vec<u8>) -> Self {
        KeyValue::Raw(bytes)
    }

    pub fn btree_ptr(ptr: BtreePtrV2) -> Self {
        KeyValue::BtreePtr(ptr)
    }

    /// 如果是单指针 Extent 变体，返回内部的 ExtentValue 引用
    pub fn as_extent(&self) -> Option<&ExtentValue> {
        match self {
            KeyValue::Extent(v) => Some(v),
            KeyValue::ExtentPtrs { .. } | KeyValue::BtreePtr(_) | KeyValue::Raw(_) => None,
        }
    }

    /// 统一遍历所有指针（旧格式返回单元素，新格式返回全部）
    pub fn for_each_ptr(&self, mut f: impl FnMut(&ExtentPtr)) {
        match self {
            KeyValue::Extent(v) => {
                let ptr = ExtentPtr::from_extent(v);
                f(&ptr);
            }
            KeyValue::ExtentPtrs { ptrs, .. } => {
                for p in ptrs {
                    f(p);
                }
            }
            KeyValue::Raw(bytes) => {
                // btree 叶子节点读回的 extent 以 Raw 形式存储
                if bytes.len() >= 8 {
                    let count =
                        u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
                    let encoded_len = 20 + count * 8;
                    if count > 0
                        && count <= 128
                        && bytes.len() >= encoded_len
                        && bytes.len() < encoded_len + 8
                    {
                        for i in 0..count {
                            let off = 20 + i * 8;
                            let ptr = ExtentPtr::from_packed(&bytes[off..off + 8]);
                            f(&ptr);
                        }
                        return;
                    }
                }
                // 回退: 单指针格式
                    if (28..36).contains(&bytes.len()) {
                    let v = ExtentValue::from_bytes(bytes);
                    let ptr = ExtentPtr::from_extent(&v);
                    f(&ptr);
                }
            }
            KeyValue::BtreePtr(_) => {}
        }
    }

    /// 指针数量
    pub fn nr_ptrs(&self) -> usize {
        match self {
            KeyValue::Extent(_) => 1,
            KeyValue::ExtentPtrs { ptrs, .. } => ptrs.len(),
            KeyValue::Raw(bytes) => {
                if bytes.len() >= 8 {
                    let count =
                        u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
                    let encoded_len = 20 + count * 8;
                    if count > 0
                        && count <= 128
                        && bytes.len() >= encoded_len
                        && bytes.len() < encoded_len + 8
                    {
                        return count;
                    }
                }
                if (28..36).contains(&bytes.len()) {
                    1
                } else {
                    0
                }
            }
            KeyValue::BtreePtr(_) => 0,
        }
    }

    pub fn as_btree_ptr(&self) -> Option<&BtreePtrV2> {
        match self {
            KeyValue::BtreePtr(ptr) => Some(ptr),
            KeyValue::Extent(_) | KeyValue::ExtentPtrs { .. } | KeyValue::Raw(_) => None,
        }
    }

    /// 返回 value 的序列化字节
    pub fn to_bytes(&self) -> Vec<u8> {
        match self {
            KeyValue::Extent(v) => v.to_bytes(),
            KeyValue::ExtentPtrs { blocks, ptrs, crc32c, crc_offset_blocks } => {
                // 格式: blocks(4) + count(4) + crc32c(4) + crc_offset(4)
                //     + entries(8B each)
                let count = ptrs.len() as u32;
                let mut buf = Vec::with_capacity(20 + count as usize * 8);
                buf.extend_from_slice(&blocks.to_le_bytes());
                buf.extend_from_slice(&count.to_le_bytes());
                buf.extend_from_slice(&crc32c.to_le_bytes());
                buf.extend_from_slice(&crc_offset_blocks.to_le_bytes());
                for p in ptrs {
                    buf.extend_from_slice(&p.to_packed());
                }
                buf
            }
            KeyValue::BtreePtr(ptr) => ptr.to_bytes().to_vec(),
            KeyValue::Raw(bytes) => bytes.clone(),
        }
    }

    /// 从字节反序列化 KeyValue（自动检测单指针 vs 多指针格式）
    pub fn from_bytes(bytes: &[u8]) -> Self {
        if bytes.len() < 28 {
            return KeyValue::Raw(bytes.to_vec());
        }
        if bytes.len() >= 8 {
            // 检测多指针格式: blocks(4) + count(4) + entries(8B each)
            let count = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
            let encoded_len = 20 + count * 8;
            if count > 0 && count <= 128 && bytes.len() >= encoded_len && bytes.len() < encoded_len + 8 {
                let blocks = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
                let crc32c = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
                let crc_offset_blocks = u64::from_le_bytes(bytes[12..20].try_into().unwrap());
                let mut ptrs = Vec::with_capacity(count);
                for i in 0..count {
                    let off = 20 + i * 8;
                    ptrs.push(ExtentPtr::from_packed(&bytes[off..off + 8]));
                }
                return KeyValue::ExtentPtrs { blocks, ptrs, crc32c, crc_offset_blocks };
            }
        }
        if (28..36).contains(&bytes.len()) {
            KeyValue::Extent(ExtentValue::from_bytes(bytes))
        } else {
            KeyValue::Raw(bytes.to_vec())
        }
    }
}

impl From<ExtentValue> for KeyValue {
    fn from(v: ExtentValue) -> Self {
        KeyValue::Extent(v)
    }
}

impl From<Vec<ExtentPtr>> for KeyValue {
    fn from(ptrs: Vec<ExtentPtr>) -> Self {
        // 默认 blocks=1（调用者应在构造后设置正确的 blocks）
        KeyValue::ExtentPtrs { blocks: 1, ptrs, crc32c: 0, crc_offset_blocks: 0 }
    }
}

impl KeyValue {
    /// 返回 extent 覆盖的 block 数
    pub fn extent_blocks(&self) -> u32 {
        match self {
            KeyValue::Extent(v) => v.size,
            KeyValue::ExtentPtrs { blocks, .. } => *blocks,
            KeyValue::Raw(bytes) => {
                if bytes.len() >= 8 {
                    let count =
                        u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
                    let encoded_len = 20 + count * 8;
                    if count > 0
                        && count <= 128
                        && bytes.len() >= encoded_len
                        && bytes.len() < encoded_len + 8
                    {
                        return u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
                    }
                }
                if (28..36).contains(&bytes.len()) {
                    let v = ExtentValue::from_bytes(bytes);
                    v.size
                } else {
                    0
                }
            }
            _ => 0,
        }
    }
}

impl std::fmt::Display for KeyValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeyValue::Extent(v) => {
                if v.size == 1 {
                    write!(f, "extent({:#x}:v{})", v.paddr, v.ver)
                } else {
                    write!(f, "extent({:#x}+{}:v{})", v.paddr, v.size, v.ver)
                }
            }
            KeyValue::ExtentPtrs { blocks, ptrs, .. } => {
                write!(f, "extent_ptrs({}blocks, [", blocks)?;
                for (i, p) in ptrs.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "dev={} gen={} off={:#x}", p.dev, p.gen, p.offset)?;
                }
                write!(f, "])")
            }
            KeyValue::BtreePtr(ptr) => write!(
                f,
                "btree_ptr({:#x}, sectors={}, level={}, gen={})",
                ptr.block_addr, ptr.sectors_written, ptr.level, ptr.generation
            ),
            KeyValue::Raw(bytes) => write!(f, "raw({} bytes)", bytes.len()),
        }
    }
}

// ═══════════════════════════════════════════════════════════════
// BtreeEntry — btree entry（Bpos + KeyType + KeyValue）
// ═══════════════════════════════════════════════════════════════

/// Btree entry — 一个位置上的完整条目
///
/// 对应 bcachefs 的 `btree_entry_variant` 概念：
/// - `pos`: 搜索位置（inode, offset, snapshot）
/// - `key_type`: 操作类型（Normal/Deleted/Whiteout）
/// - `value`: 值（Extent 或其他变体）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BtreeEntry {
    pub pos: Bpos,
    pub key_type: KeyType,
    #[serde(default)]
    pub needs_whiteout: bool,
    pub value: KeyValue,
}

impl BtreeEntry {
    pub fn new(pos: Bpos, key_type: KeyType, value: KeyValue) -> Self {
        Self {
            pos,
            key_type,
            needs_whiteout: false,
            value,
        }
    }

    /// 转换为 (BtreeKey, BchVal) 对
    ///
    /// 注意：BtreeKey 的 vaddr = pos.offset, snapshot_id = pos.snapshot。
    /// inode 在旧格式中为 0。
    pub fn to_key_value(&self) -> (BtreeKey, BchVal) {
        let size = match &self.value {
            KeyValue::Extent(v) => v.size,
            KeyValue::ExtentPtrs { blocks, .. } => *blocks,
            KeyValue::BtreePtr(_) => 1,
            KeyValue::Raw(bytes) => match KeyValue::from_bytes(bytes) {
                KeyValue::ExtentPtrs { blocks, .. } => blocks,
                KeyValue::Extent(_) | KeyValue::BtreePtr(_) | KeyValue::Raw(_) => 1,
            },
        };
        let key = BtreeKey {
            inode: self.pos.inode,
            vaddr: self.pos.offset,
            size,
            snapshot_id: self.pos.snapshot,
            key_type: self.key_type,
            version: 0,
        };
        let value = match &self.value {
            KeyValue::Extent(v) => v.to_bchval(),
            KeyValue::ExtentPtrs { ptrs, .. } => {
                if let Some(first) = ptrs.first() {
                    BchVal::new(first.offset, first.gen as u16)
                } else {
                    BchVal::new(0, 0)
                }
            }
            // Transitional legacy projection for callers not yet migrated to
            // `KeyValue::BtreePtr`; persistence always uses the full 16-byte value.
            KeyValue::BtreePtr(ptr) => BchVal::new(ptr.block_addr, ptr.generation as u16),
            KeyValue::Raw(bytes) => match KeyValue::from_bytes(bytes) {
                KeyValue::ExtentPtrs { ptrs, .. } => {
                    ptrs.first().map_or(BchVal::new(0, 0), |ptr| {
                        BchVal::new(ptr.offset, ptr.gen as u16)
                    })
                }
                KeyValue::Extent(value) => value.to_bchval(),
                KeyValue::BtreePtr(_) | KeyValue::Raw(_) => {
                    BchVal::new(0, 0)
                }
            },
        };
        (key, value)
    }

    /// 创建 raw bytes 类型的 entry
    pub fn raw(pos: Bpos, key_type: KeyType, bytes: Vec<u8>) -> Self {
        Self {
            pos,
            key_type,
            needs_whiteout: false,
            value: KeyValue::Raw(bytes),
        }
    }

    /// 从 (BtreeKey, BchVal) 对创建
    pub fn from_key_value(key: &BtreeKey, value: &BchVal) -> Self {
        Self {
            pos: Bpos {
                inode: unsafe { std::ptr::addr_of!(key.inode).read_unaligned() },
                offset: unsafe { std::ptr::addr_of!(key.vaddr).read_unaligned() },
                snapshot: unsafe { std::ptr::addr_of!(key.snapshot_id).read_unaligned() },
            },
            key_type: key.key_type,
            needs_whiteout: false,
            value: KeyValue::Extent(ExtentValue {
                paddr: value.paddr.get(),
                size: key.size,
                ver: value.ver,
                dev_idx: 0,
                crc32c: 0,
                crc_offset_blocks: 0,
            }),
        }
    }
}

impl From<(BtreeKey, BchVal)> for BtreeEntry {
    fn from(pair: (BtreeKey, BchVal)) -> Self {
        Self::from_key_value(&pair.0, &pair.1)
    }
}

impl From<(Bpos, KeyType, BchVal)> for BtreeEntry {
    fn from((pos, key_type, value): (Bpos, KeyType, BchVal)) -> Self {
        Self::new(
            pos,
            key_type,
            KeyValue::Extent(ExtentValue {
                paddr: value.paddr.get(),
                size: 1,
                ver: value.ver,
                dev_idx: 0,
                crc32c: 0,
                crc_offset_blocks: 0,
            }),
        )
    }
}

impl std::fmt::Display for BtreeEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}:{}", self.pos, self.key_type as u8, self.value)
    }
}

// ═══════════════════════════════════════════════════════════════
// 辅助函数 — entry 大小管理
// ═══════════════════════════════════════════════════════════════

/// 旧格式的 entry 大小（BtreeKey + BchVal），用于向后兼容
pub const LEGACY_ENTRY_SIZE: u32 =
    (std::mem::size_of::<BtreeKey>() + std::mem::size_of::<BchVal>()) as u32;

/// 计算 bset 中一个 entry 的字节数
///
/// 对 packed 和 unpacked 都通过 `k.u64s * 8` 动态计算。
/// 对于固定 8 字节 value 的旧格式，`u64s = key_u64s + 1`，结果不变。
/// 对于变长 value，`u64s = key_u64s + value_u64s`，得到正确大小。
pub fn entry_size_bytes(k: &BkeyPacked) -> u32 {
    k.u64s as u32 * 8
}

/// 计算 BtreeEntry 的 packed 字节大小
///
/// 对变长 `KeyValue::Raw`，根据 value 的实际字节数计算 u64s：
/// `u64s = BKEY_U64S + ceil(value_bytes / 8)`。
/// 对固定大小 `KeyValue::Extent`，value 固定 8 字节，结果 = 32B。
pub fn entry_packed_size(entry: &BtreeEntry) -> u32 {
    let value_bytes = entry.value.to_bytes();
    let value_u64s = value_bytes.len().div_ceil(8); // ceil division
    (BKEY_U64S as u32 + value_u64s as u32) * 8
}

/// 从 bset 数据中读取下一个 entry 的打包 key 指针
pub fn next_packed_key(data: &[u8], offset: u32) -> (&BkeyPacked, u32) {
    let k = unsafe { &*(data.as_ptr().add(offset as usize) as *const BkeyPacked) };
    let size = entry_size_bytes(k);
    (k, offset + size)
}

// ═══════════════════════════════════════════════════════════════
// 测试
// ═══════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    // ─── Bpos 测试 ──────────────────────────────────────

    #[test]
    fn test_bpos_ordering() {
        let a = Bpos::new(0, 100, 5);
        let b = Bpos::new(0, 100, 10);
        assert!(a < b, "snapshot ASC: 5 < 10");

        let c = Bpos::new(0, 50, 1);
        let d = Bpos::new(0, 100, 1);
        assert!(c < d, "offset ASC: 50 < 100");

        let e = Bpos::new(1, 0, 0);
        let f = Bpos::new(2, 0, 0);
        assert!(e < f, "inode ASC: 1 < 2");
    }

    #[test]
    fn test_bpos_min_max() {
        assert!(Bpos::MIN < Bpos::MAX);
        let mid = Bpos::new(500, 100, 1);
        assert!(Bpos::MIN < mid);
        assert!(mid < Bpos::MAX);
    }

    #[test]
    fn test_bpos_successor_predecessor() {
        let p = Bpos::new(1, 100, 5);
        let succ = p.successor();
        assert!(p < succ);
        let pred = succ.predecessor();
        assert_eq!(p, pred);

        // wrap-around: snapshot overflow
        let p2 = Bpos::new(1, 100, u32::MAX);
        let succ2 = p2.successor();
        assert_eq!(succ2.snapshot, 0);
        assert_eq!(succ2.offset, 101);

        // wrap-around: offset overflow
        let p3 = Bpos::new(1, u64::MAX, u32::MAX);
        let succ3 = p3.successor();
        assert_eq!(succ3.snapshot, 0);
        assert_eq!(succ3.offset, 0);
        assert_eq!(succ3.inode, 2);
    }

    #[test]
    fn test_bpos_display() {
        let p = Bpos::new(1, 100, 5);
        let s = format!("{}", p);
        assert!(s.contains("1"));
        assert!(s.contains("100"));
        assert!(s.contains("5"));
    }

    // ─── BkeyFormat 测试 ────────────────────────────────

    #[test]
    fn test_bkey_format_current() {
        let fmt = BkeyFormat::new_current();
        assert_eq!(fmt.key_u64s, BKEY_U64S);
        assert_eq!(fmt.nr_fields as usize, BKEY_NR_FIELDS);
        assert_eq!(fmt.bits_per_field, BKEY_FIELD_BITS);
    }

    #[test]
    fn test_bkey_format_field_max() {
        let fmt = BkeyFormat::new_current();
        assert_eq!(fmt.field_max(BKEY_FIELD_INODE), u64::MAX);
        assert_eq!(fmt.field_max(BKEY_FIELD_OFFSET), u64::MAX);
        assert_eq!(fmt.field_max(BKEY_FIELD_SNAPSHOT), u32::MAX as u64);
        assert_eq!(fmt.field_max(BKEY_FIELD_PADDR), Addr48::MAX);
        assert_eq!(fmt.field_max(BKEY_FIELD_VER), u16::MAX as u64);
    }

    // ─── Pack/Unpack 测试 ───────────────────────────────

    /// 创建一个包含足够空间的 BkeyPacked 缓冲区
    fn make_packed_buf() -> (Vec<u8>, &'static mut BkeyPacked) {
        let mut buf = vec![0u8; 64]; // 8 u64s = 足够空间
        let ptr = buf.as_mut_ptr() as *mut BkeyPacked;
        let k = unsafe { &mut *ptr };
        (buf, k)
    }

    #[test]
    fn test_pack_unpack_bpos_roundtrip() {
        let fmt = BkeyFormat::new_current();
        let pos = Bpos::new(1, 0xABCD_1234_5678, 42);

        let (_buf, pk) = make_packed_buf();
        assert!(bkey_pack_pos(pk, pos, &fmt), "pack_pos should succeed");

        let unpacked = bkey_unpack_pos(&fmt, pk);
        assert_eq!(unpacked, pos, "roundtrip: packed pos should equal original");
    }

    #[test]
    fn test_pack_unpack_min_max() {
        let fmt = BkeyFormat::new_current();

        let (_buf, pk) = make_packed_buf();
        assert!(bkey_pack_pos(pk, Bpos::MIN, &fmt));
        assert_eq!(bkey_unpack_pos(&fmt, pk), Bpos::MIN);

        assert!(bkey_pack_pos(pk, Bpos::MAX, &fmt));
        assert_eq!(bkey_unpack_pos(&fmt, pk), Bpos::MAX);
    }

    #[test]
    fn test_pack_unpack_bkey_roundtrip() {
        let fmt = BkeyFormat::new_current();
        let pos = Bpos::new(42, 0xDEAD_BEEF, 7);
        let paddr = 0xABCD_FFFF_FFFFu64;
        let ver = 0x1234u16;

        let (_buf, pk) = make_packed_buf();
        assert!(
            bkey_pack(pk, pos, 0, paddr, ver, &fmt),
            "pack should succeed"
        );

        let (unpacked_pos, unpacked_type, unpacked_paddr, unpacked_ver) = bkey_unpack(&fmt, pk);
        assert_eq!(unpacked_pos, pos);
        assert_eq!(unpacked_type, 0);
        assert_eq!(unpacked_paddr, paddr);
        assert_eq!(unpacked_ver, ver);
    }

    #[test]
    fn test_pack_overflow_returns_false() {
        // 创建一个只能容纳小值的格式
        let fmt = BkeyFormat {
            key_u64s: 2,
            nr_fields: BKEY_NR_FIELDS as u8,
            bits_per_field: [4, 4, 4, 4, 4], // 每个 field 4 位，最大 15
            field_offset: [0; BKEY_NR_FIELDS],
        };

        let (_buf, pk) = make_packed_buf();
        // 16 太大，4 位放不下
        assert!(
            !bkey_pack_pos(pk, Bpos::new(16, 0, 0), &fmt),
            "pack should fail on overflow"
        );
        // 15 是 4 位的最大值，应成功
        assert!(
            bkey_pack_pos(pk, Bpos::new(15, 0, 0), &fmt),
            "pack should succeed within range"
        );
    }

    #[test]
    fn test_pack_underflow_returns_false() {
        let fmt = BkeyFormat {
            key_u64s: 2,
            nr_fields: BKEY_NR_FIELDS as u8,
            bits_per_field: [4, 4, 4, 4, 4],
            // 只有 inode 字段有 field_offset, offset/snapshot 无偏移
            field_offset: [10, 0, 0, 0, 0],
        };

        let (_buf, pk) = make_packed_buf();
        // 9 < 10 → underflow
        assert!(
            !bkey_pack_pos(pk, Bpos::new(9, 0, 0), &fmt),
            "pack should fail on underflow"
        );
        // 10 == field_offset → OK, value=0
        assert!(
            bkey_pack_pos(pk, Bpos::new(10, 0, 0), &fmt),
            "pack should succeed at field_offset boundary"
        );
    }

    #[test]
    fn test_bkey_pack_pos_lossy_exact_smaller_fail() {
        let fmt = BkeyFormat {
            key_u64s: 2,
            nr_fields: BKEY_NR_FIELDS as u8,
            bits_per_field: [4, 4, 4, 4, 4],
            field_offset: [10, 10, 10, 0, 0],
        };
        let (_buf, packed) = make_packed_buf();

        assert_eq!(
            bch2_bkey_pack_pos_lossy(packed, &Bpos::new(11, 12, 13), &fmt),
            BkeyPackPosRet::Exact
        );
        assert_eq!(bkey_unpack_pos(&fmt, packed), Bpos::new(11, 12, 13));

        assert_eq!(
            bch2_bkey_pack_pos_lossy(packed, &Bpos::new(11, 12, 9), &fmt),
            BkeyPackPosRet::Smaller
        );
        assert_eq!(bkey_unpack_pos(&fmt, packed), Bpos::new(11, 11, 25));

        assert_eq!(
            bch2_bkey_pack_pos_lossy(packed, &Bpos::new(9, 12, 13), &fmt),
            BkeyPackPosRet::Fail
        );
    }

    #[test]
    fn test_entry_size_bytes() {
        let fmt = BkeyFormat::new_current();
        let (_buf, pk) = make_packed_buf();
        assert!(bkey_pack_pos(pk, Bpos::new(1, 2, 3), &fmt));

        // packed entry: manually set u64s to key+value (bkey_pack_pos only packs key fields)
        pk.u64s = fmt.key_u64s + 1; // 3 key + 1 value = 4 u64s = 32 bytes
        assert!(pk.is_packed());
        assert_eq!(entry_size_bytes(pk), 32);

        // unpacked entry (format=KEY_FORMAT_CURRENT)
        let (_buf2, pk2) = make_packed_buf();
        pk2.u64s = 4; // 3 u64s key + 1 u64 value
        pk2.set_format(KEY_FORMAT_CURRENT);
        assert!(!pk2.is_packed());
        assert_eq!(entry_size_bytes(pk2), 32);

        // 变长 value 的 entry
        let (_buf3, pk3) = make_packed_buf();
        pk3.u64s = 5; // 3 u64s key + 2 u64s value (16 bytes)
        pk3.set_format(KEY_FORMAT_LOCAL_BTREE);
        assert!(pk3.is_packed());
        assert_eq!(entry_size_bytes(pk3), 40);
    }

    #[test]
    fn test_bkey_pack_raw_and_unpack_bytes() {
        let fmt = BkeyFormat::new_current();
        let (_buf, pk) = make_packed_buf();

        // 变长 value: 16 bytes
        let value_bytes: [u8; 16] = [
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16,
            0x17, 0x18,
        ];
        assert!(bkey_pack_raw(
            pk,
            Bpos::new(42, 100, 5),
            0,
            &value_bytes,
            &fmt
        ));

        assert_eq!(pk.u64s, fmt.key_u64s + 2); // key(3) + value(2 u64s)
        assert!(pk.is_packed());
        assert_eq!(pk.type_, 0);

        // unpack_bytes 读出正确的内容
        let (pos, key_type, unpacked) = bkey_unpack_bytes(&fmt, pk);
        assert_eq!(pos, Bpos::new(42, 100, 5));
        assert_eq!(key_type, 0);
        assert_eq!(unpacked, &value_bytes);

        // entry_size_bytes 动态计算正确
        assert_eq!(entry_size_bytes(pk), 40); // (3+2) * 8

        // 1 u64 (8 bytes) value
        let value8 = [0xAAu8; 8];
        assert!(bkey_pack_raw(pk, Bpos::new(1, 2, 3), 1, &value8, &fmt));
        assert_eq!(pk.u64s, fmt.key_u64s + 1);
        let (_pos2, _kt2, unpacked2) = bkey_unpack_bytes(&fmt, pk);
        assert_eq!(unpacked2, &value8[..]);
        assert_eq!(entry_size_bytes(pk), 32); // (3+1) * 8

        // 非对齐 value (7 bytes): u64s = key_u64s + ceil(7/8) = key_u64s + 1
        let value7 = [0xBBu8; 7];
        assert!(bkey_pack_raw(pk, Bpos::new(10, 20, 7), 0, &value7, &fmt));
        assert_eq!(pk.u64s, fmt.key_u64s + 1);
        let (_pos3, _kt3, unpacked3) = bkey_unpack_bytes(&fmt, pk);
        // unpack 出的 value_bytes 包含零填充（7 bytes data + 1 byte zero）
        assert_eq!(&unpacked3[..7], &value7[..]);
        assert_eq!(unpacked3[7], 0); // 零填充
    }

    #[test]
    fn test_bkey_pack_raw_max_value() {
        let fmt = BkeyFormat::new_current();
        let mut buf = vec![0u8; 256];
        let pk = unsafe { &mut *(buf.as_mut_ptr() as *mut BkeyPacked) };

        // 大 value (32 bytes, 4 u64s): key(3) + value(4) = 7 u64s
        let value32 = [0xDEu8; 32];
        assert!(bkey_pack_raw(pk, Bpos::new(100, 200, 1), 2, &value32, &fmt));
        assert_eq!(pk.u64s, fmt.key_u64s + 4);
        assert_eq!(entry_size_bytes(pk), 56); // (3+4) * 8

        let (pos, key_type, unpacked) = bkey_unpack_bytes(&fmt, pk);
        assert_eq!(pos.offset, 200);
        assert_eq!(key_type, 2);
        assert_eq!(unpacked, &value32[..]);
    }

    // ─── 现有类型兼容测试 ──────────────────────────────

    #[test]
    fn test_key_ordering_snapshot_desc() {
        let older = BtreeKey::new(100, 5, KeyType::Normal);
        let newer = BtreeKey::new(100, 10, KeyType::Normal);
        assert!(
            newer < older,
            "higher snapshot_id should come first (be lesser)"
        );
    }

    #[test]
    fn test_key_ordering_vaddr_asc() {
        let low = BtreeKey::new(50, 1, KeyType::Normal);
        let high = BtreeKey::new(100, 1, KeyType::Normal);
        assert!(low < high);
    }

    #[test]
    fn test_key_ordering_key_type() {
        let normal = BtreeKey::new(100, 1, KeyType::Normal);
        let deleted = BtreeKey::new(100, 1, KeyType::Deleted);
        assert!(normal < deleted);
    }

    #[test]
    fn test_key_equal_full() {
        let a = BtreeKey::new(42, 7, KeyType::Normal);
        let b = BtreeKey::new(42, 7, KeyType::Normal);
        assert_eq!(a, b);
    }

    #[test]
    fn test_key_min_max() {
        assert!(BtreeKey::MIN_KEY < BtreeKey::MAX_KEY);
        let normal = BtreeKey::new(500, 100, KeyType::Normal);
        assert!(BtreeKey::MIN_KEY < normal);
        assert!(normal < BtreeKey::MAX_KEY);
    }

    #[test]
    fn test_u48_bounds() {
        let zero = Addr48::new(0);
        assert_eq!(zero.get(), 0);
        let max = Addr48::new(Addr48::MAX);
        assert_eq!(max.get(), Addr48::MAX);
        let mid = Addr48::new(0xABCD_FFFF_FFFF);
        assert_eq!(mid.get(), 0xABCD_FFFF_FFFF);
    }

    #[test]
    #[should_panic(expected = "Addr48 overflow")]
    fn test_u48_overflow() {
        let _ = Addr48::new(1u64 << 48);
    }

    #[test]
    fn test_value_roundtrip() {
        let v = BchVal::new(0x1234_5678_9ABC, 42);
        assert_eq!(v.paddr.get(), 0x1234_5678_9ABC);
        assert_eq!(v.ver, 42);
    }

    #[test]
    fn test_key_display() {
        let k = BtreeKey::new(100, 5, KeyType::Normal);
        let s = format!("{}", k);
        assert!(s.contains("100"));
        assert!(s.contains("5"));
    }

    #[test]
    fn test_key_serialization_size() {
        assert_eq!(std::mem::size_of::<BtreeKey>(), 33);
        assert_eq!(std::mem::size_of::<BchVal>(), 16);
    }

    #[test]
    fn test_btree_key_to_bpos_roundtrip() {
        let k = BtreeKey::new(100, 5, KeyType::Normal);
        let pos = k.to_bpos();
        assert_eq!(pos.inode, 0);
        assert_eq!(pos.offset, 100);
        assert_eq!(pos.snapshot, 5);

        let k2 = BtreeKey::from_bpos(pos, KeyType::Normal);
        assert_eq!(k, k2);
    }

    #[test]
    fn test_bkey_packed_header_manipulation() {
        let (_buf, pk) = make_packed_buf();
        pk.u64s = 4;
        pk.set_format(KEY_FORMAT_LOCAL_BTREE);
        pk.set_needs_whiteout(true);
        pk.type_ = 1;

        assert_eq!(pk.u64s, 4);
        assert_eq!(pk.format(), KEY_FORMAT_LOCAL_BTREE);
        assert!(pk.needs_whiteout());
        assert_eq!(pk.type_, 1);
        assert!(pk.is_packed());

        pk.set_format(KEY_FORMAT_CURRENT);
        assert!(!pk.is_packed());
    }

    #[test]
    fn test_fls64_zero() {
        assert_eq!(fls64(0), 0);
        assert_eq!(fls64(1), 1);
        assert_eq!(fls64(0xFF), 8);
        assert_eq!(fls64(0x8000_0000_0000_0001), 64);
    }

    #[test]
    fn test_u64_bitmask_edge() {
        assert_eq!(u64_bitmask(0), 0);
        assert_eq!(u64_bitmask(1), 1);
        assert_eq!(u64_bitmask(64), u64::MAX);
    }

    #[test]
    fn test_bpos_various_roundtrip() {
        let fmt = BkeyFormat::new_current();
        let cases = [
            Bpos::new(0, 0, 0),
            Bpos::new(1, 1, 1),
            Bpos::new(u64::MAX, u64::MAX, u32::MAX),
            Bpos::new(0x1234_5678_9ABC_DEF0, 0xFEDC_BA09_8765_4321, 0xABCD_EF01),
            Bpos::new(42, 0, 0),
            Bpos::new(0, 999_999_999, 999_999),
        ];

        let (_buf, pk) = make_packed_buf();
        for &pos in &cases {
            assert!(
                bkey_pack_pos(pk, pos, &fmt),
                "pack_pos should succeed for {:?}",
                pos
            );
            let unpacked = bkey_unpack_pos(&fmt, pk);
            assert_eq!(unpacked, pos, "roundtrip mismatch for {:?}", pos);
        }
    }

    #[test]
    fn test_bkey_packed_display() {
        let fmt = BkeyFormat::new_current();
        assert_eq!(
            format!("{}", fmt),
            "u64s=3 fields=[64:0, 64:0, 32:0, 48:0, 16:0]"
        );
    }

    #[test]
    fn test_bkey_format_default() {
        let fmt: BkeyFormat = Default::default();
        assert_eq!(fmt.key_u64s, BKEY_U64S);
    }

    #[test]
    fn test_bpos_is_min_is_max() {
        assert!(Bpos::MIN.is_min());
        assert!(Bpos::MAX.is_max());
        assert!(!Bpos::new(1, 0, 0).is_min());
        assert!(!Bpos::new(0, 1, 0).is_min());
    }

    // ─── KeyValue 测试 ─────────────────────────────────

    #[test]
    fn test_key_value_extent_create() {
        let v = KeyValue::extent(0xABCD, 42, 0);
        if let KeyValue::Extent(bv) = &v {
            assert_eq!(bv.paddr(), 0xABCD);
            assert_eq!(bv.ver(), 42);
        } else {
            panic!("expected Extent variant");
        }
    }

    #[test]
    fn test_key_value_btree_ptr_fixed_encoding() {
        let ptr = BtreePtrV2 {
            block_addr: 0x1234,
            sectors_written: 16,
            level: 1,
            dev_idx: 0,
            generation: 9,
        };
        let value = KeyValue::btree_ptr(ptr);
        assert_eq!(value.to_bytes().len(), BtreePtrV2::DISK_BYTES);
        assert_eq!(value.as_btree_ptr(), Some(&ptr));
        assert_eq!(BtreePtrV2::from_bytes(&value.to_bytes()).unwrap(), ptr);
    }

    #[test]
    fn test_key_value_from_btreevalue() {
        let bv = BchVal::new(100, 7);
        let kv = KeyValue::extent(bv.paddr.get(), bv.ver, 0);
        assert!(kv.as_extent().is_some());
        assert_eq!(kv.as_extent().unwrap().paddr(), 100);
    }

    #[test]
    fn test_key_value_from_tuple() {
        let kv = KeyValue::extent(0xFF_FFFF, 1, 0);
        let ext = kv.as_extent().unwrap();
        assert_eq!(ext.paddr(), 0xFF_FFFF);
        assert_eq!(ext.ver(), 1);
    }

    #[test]
    fn test_key_value_display() {
        let v = KeyValue::extent(0xABCD, 42, 0);
        let s = format!("{}", v);
        assert!(s.contains("extent"));
        assert!(
            s.contains("abcd"),
            "expected lowercase hex 'abcd' in '{}'",
            s
        );
    }

    // ─── BtreeEntry 测试 ────────────────────────────────

    #[test]
    fn test_btree_entry_new() {
        let pos = Bpos::new(1, 100, 5);
        let entry = BtreeEntry::new(pos, KeyType::Normal, KeyValue::extent(0xFF, 1, 0));
        assert_eq!(entry.pos, pos);
        assert_eq!(entry.key_type, KeyType::Normal);
        assert!(matches!(entry.value, KeyValue::Extent(_)));
    }

    #[test]
    fn test_btree_entry_to_key_value() {
        let entry = BtreeEntry {
            pos: Bpos::new(1, 100, 5),
            key_type: KeyType::Normal,
            needs_whiteout: false,
            value: KeyValue::extent(0xFF, 1, 0),
        };
        let (key, value) = entry.to_key_value();
        assert_eq!(key.get_vaddr(), 100);
        assert_eq!(key.get_snapshot_id(), 5);
        assert_eq!(key.key_type, KeyType::Normal);
        assert_eq!(value.paddr.get(), 0xFF);
    }

    #[test]
    fn test_btree_entry_from_key_value() {
        let k = BtreeKey::new(100, 5, KeyType::Normal);
        let v = BchVal::new(0xFF, 1);
        let entry = BtreeEntry::from_key_value(&k, &v);
        assert_eq!(entry.pos.inode, 0);
        assert_eq!(entry.pos.offset, 100);
        assert_eq!(entry.pos.snapshot, 5);
        assert!(matches!(entry.value, KeyValue::Extent(ev) if ev.paddr() == 0xFF));
    }

    #[test]
    fn test_btree_entry_from_pair() {
        let pair = (
            BtreeKey::new(200, 3, KeyType::Normal),
            BchVal::new(0xABC, 2),
        );
        let entry: BtreeEntry = pair.into();
        assert_eq!(entry.pos.offset, 200);
        assert_eq!(entry.pos.snapshot, 3);
    }

    #[test]
    fn test_btree_entry_from_bpos() {
        let pos = Bpos::new(0, 300, 7);
        let entry: BtreeEntry = (pos, KeyType::Normal, BchVal::new(42, 0)).into();
        assert_eq!(entry.pos.offset, 300);
        assert_eq!(entry.pos.snapshot, 7);
        assert_eq!(entry.key_type, KeyType::Normal);
    }

    #[test]
    fn test_btree_entry_roundtrip() {
        let original = BtreeEntry {
            pos: Bpos::new(0, 500, 10),
            key_type: KeyType::Normal,
            needs_whiteout: false,
            value: KeyValue::extent(0x1234, 99, 0),
        };
        let (key, value) = original.to_key_value();
        let restored = BtreeEntry::from_key_value(&key, &value);
        assert_eq!(original.pos.offset, restored.pos.offset);
        assert_eq!(original.pos.snapshot, restored.pos.snapshot);
        assert_eq!(original.key_type, restored.key_type);
        assert_eq!(
            original.value.as_extent().unwrap().paddr(),
            restored.value.as_extent().unwrap().paddr(),
        );
    }

    #[test]
    fn test_btree_entry_display() {
        let entry = BtreeEntry {
            pos: Bpos::new(0, 100, 5),
            key_type: KeyType::Normal,
            needs_whiteout: false,
            value: KeyValue::extent(0xFF, 1, 0),
        };
        let s = format!("{}", entry);
        assert!(s.contains("100"));
        assert!(s.contains("5"));
        assert!(s.contains("0"));
    }

    // ─── ExtentPtr 测试 ─────────────────────────────────

    #[test]
    fn test_extent_ptr_pack_roundtrip() {
        let ptr = ExtentPtr {
            dev: 1,
            gen: 5,
            offset: 0xABCD,
            cached: false,
            unwritten: false,
        };
        let packed = ptr.to_packed();
        let unpacked = ExtentPtr::from_packed(&packed);
        assert_eq!(ptr, unpacked);
    }

    #[test]
    fn test_extent_ptr_cached_flag() {
        let ptr = ExtentPtr {
            dev: 0,
            gen: 10,
            offset: 100,
            cached: true,
            unwritten: false,
        };
        let packed = ptr.to_packed();
        let unpacked = ExtentPtr::from_packed(&packed);
        assert!(unpacked.cached);
        assert!(!unpacked.unwritten);
    }

    #[test]
    fn test_extent_ptr_from_extent() {
        let ext = ExtentValue {
            paddr: 0x5000,
            size: 1,
            ver: 42,
            dev_idx: 2,
            crc32c: 0,
            crc_offset_blocks: 0,
        };
        let ptr = ExtentPtr::from_extent(&ext);
        assert_eq!(ptr.dev, 2);
        assert_eq!(ptr.gen, 42);
        assert_eq!(ptr.offset, 0x5000);
        assert!(!ptr.cached);
        assert!(!ptr.unwritten);
    }

    // ─── KeyValue::ExtentPtrs 测试 ──────────────────────

    #[test]
    fn test_extent_ptrs_for_each_ptr() {
        let ptrs = vec![
            ExtentPtr {
                dev: 0,
                gen: 1,
                offset: 100,
                cached: false,
                unwritten: false,
            },
            ExtentPtr {
                dev: 1,
                gen: 3,
                offset: 500,
                cached: true,
                unwritten: false,
            },
        ];
        let kv = KeyValue::ExtentPtrs { blocks: 4, ptrs, crc32c: 0, crc_offset_blocks: 0 };
        let mut count = 0;
        kv.for_each_ptr(|_| count += 1);
        assert_eq!(count, 2);
        assert_eq!(kv.nr_ptrs(), 2);
        assert_eq!(kv.extent_blocks(), 4);
    }

    #[test]
    fn test_extent_ptrs_to_bytes_roundtrip() {
        let ptrs = vec![
            ExtentPtr {
                dev: 0,
                gen: 1,
                offset: 100,
                cached: false,
                unwritten: false,
            },
            ExtentPtr {
                dev: 2,
                gen: 7,
                offset: 0xFFFF,
                cached: true,
                unwritten: false,
            },
        ];
        let original = KeyValue::ExtentPtrs { blocks: 8, ptrs, crc32c: 0, crc_offset_blocks: 0 };
        let bytes = original.to_bytes();
        let restored = KeyValue::from_bytes(&bytes);
        assert_eq!(original, restored);
    }

    #[test]
    fn test_short_raw_value_does_not_enter_extent_decoder() {
        assert_eq!(KeyValue::from_bytes(&[]), KeyValue::Raw(Vec::new()));
        assert_eq!(
            KeyValue::from_bytes(&[1, 2, 3]),
            KeyValue::Raw(vec![1, 2, 3])
        );
        assert_eq!(KeyValue::from_bytes(&[0u8; 9]), KeyValue::Raw(vec![0u8; 9]));
        assert_eq!(
            KeyValue::from_bytes(&[0u8; 17]),
            KeyValue::Raw(vec![0u8; 17])
        );
    }

    #[test]
    fn test_extent_ptr_display() {
        let ptrs = vec![ExtentPtr {
            dev: 0,
            gen: 1,
            offset: 100,
            cached: false,
            unwritten: false,
        }];
        let kv = KeyValue::ExtentPtrs { blocks: 3, ptrs, crc32c: 0, crc_offset_blocks: 0 };
        let s = format!("{}", kv);
        assert!(s.contains("blocks"));
        assert!(s.contains("dev=0"));
        assert!(s.contains("gen=1"));
    }

    // ─── bkey_cmp_packed 测试 ───────────────────────────

    #[test]
    fn test_bkey_cmp_packed_vs_unpacked() {
        let fmt = BkeyFormat::new_current();
        let test_cases = vec![
            (Bpos::new(0, 0, 0), Bpos::new(0, 0, 0)),      // 相等
            (Bpos::new(0, 1, 0), Bpos::new(0, 0, 0)),      // offset 不同 — 相同 inode
            (Bpos::new(1, 0, 0), Bpos::new(0, 0, 0)),      // inode 不同
            (Bpos::new(0, 0, 1), Bpos::new(0, 0, 0)),      // snapshot 不同
            (Bpos::new(0, 100, 5), Bpos::new(0, 50, 5)),   // offset 不同
            (Bpos::new(0, 100, 10), Bpos::new(0, 100, 5)), // snapshot 不同
            (Bpos::MIN, Bpos::MAX),
            (Bpos::new(u64::MAX, u64::MAX, u32::MAX), Bpos::new(0, 0, 0)),
        ];
        for (a, b) in test_cases {
            let (_buf_a, pk_a) = make_packed_buf();
            bkey_pack_pos(pk_a, a, &fmt);
            let (_buf_b, pk_b) = make_packed_buf();
            bkey_pack_pos(pk_b, b, &fmt);

            assert_eq!(
                bkey_cmp_packed(&fmt, pk_a, pk_b),
                a.cmp(&b),
                "bkey_cmp_packed({:?}, {:?}) should equal cmp",
                a,
                b,
            );
            assert_eq!(
                bkey_cmp_packed_vs_bpos(&fmt, pk_a, &b),
                a.cmp(&b),
                "bkey_cmp_packed_vs_bpos({:?}, {:?}) should equal cmp",
                a,
                b,
            );
        }
    }
}
