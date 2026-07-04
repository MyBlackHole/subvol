//! Bucket 状态管理 — bcachefs 对齐

use serde::{Deserialize, Serialize};

use crate::types::StorageError;

/// Bucket 数据类型 — 对齐 bcachefs `enum bch_data_type`（BCH_DATA_TYPES）
///
/// 数值与 bcachefs C 源码完全一致：
/// - BCH_DATA_free=0, BCH_DATA_sb=1, BCH_DATA_journal=2, BCH_DATA_btree=3,
///   BCH_DATA_user=4, BCH_DATA_cached=5, BCH_DATA_parity=6, BCH_DATA_stripe=7,
///   BCH_DATA_need_gc_gens=8, BCH_DATA_need_discard=9, BCH_DATA_unstriped=10
///
/// 对应本地 bcachefs `fs/alloc/accounting_format.h:55-75`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum BchDataType {
    /// 空闲（BCH_DATA_free = 0）
    Free = 0,
    /// 超块（BCH_DATA_sb = 1）
    Sb = 1,
    /// Journal/WAL（BCH_DATA_journal = 2）
    Journal = 2,
    /// Btree 节点（BCH_DATA_btree = 3）
    Btree = 3,
    /// 用户数据（BCH_DATA_user = 4）
    User = 4,
    /// 缓存（BCH_DATA_cached = 5）
    Cached = 5,
    /// RAID 奇偶校验（BCH_DATA_parity = 6）
    Parity = 6,
    /// RAID 条带（BCH_DATA_stripe = 7）
    Stripe = 7,
    /// 需要 GC 代际更新（BCH_DATA_need_gc_gens = 8）
    NeedGcGens = 8,
    /// 需要丢弃（BCH_DATA_need_discard = 9）
    NeedDiscard = 9,
    /// 非条带数据（BCH_DATA_unstriped = 10）
    Unstriped = 10,
}

impl BchDataType {
    /// 从原始 u8 值构造 BchDataType（安全，返回 None 表示无效值）
    pub fn from_raw(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Free),
            1 => Some(Self::Sb),
            2 => Some(Self::Journal),
            3 => Some(Self::Btree),
            4 => Some(Self::User),
            5 => Some(Self::Cached),
            6 => Some(Self::Parity),
            7 => Some(Self::Stripe),
            8 => Some(Self::NeedGcGens),
            9 => Some(Self::NeedDiscard),
            10 => Some(Self::Unstriped),
            _ => None,
        }
    }
}

/// BUCKET_GC_GEN_MAX — gc_gen = gen - oldest_gen 的最大允许值
///
/// bcachefs 对应 (background.h:31): `#define BUCKET_GC_GEN_MAX 96U`
/// 当 gc_gen >= 96 时，bucket 被标记为 NeedGcGens，强制 GC 扫描并刷新 gen。
pub const BUCKET_GC_GEN_MAX: u8 = 96;

/// bcachefs gen_cmp (buckets.h:104): (s8)(a - b)
/// 有符号 i8 差值，用于 gen 的代际包装比较
pub fn gen_cmp(a: u8, b: u8) -> i32 {
    (a.wrapping_sub(b) as i8) as i32
}

/// bcachefs gen_after (buckets.h:109): max(0, gen_cmp(a, b))
/// 当 a > b（有符号）时返回正值，否则为 0
pub fn gen_after(a: u8, b: u8) -> i32 {
    i32::max(0, gen_cmp(a, b))
}

/// bucket_ref_update 的 gen 校验 + type mismatch 检查
///
/// 对应 bcachefs bch2_bucket_ref_update (buckets.c:483-541) 的检查序列 ①-⑤。
/// 返回：
/// - `Ok(None)` → 跳过此指针（stale cached）
/// - `Ok(Some(ptr_data_type))` → 检查通过，可以继续
/// - `Err` → 检查不通过，需终止
pub fn bucket_ref_update_checks(
    bucket_gen: u8,
    ptr_gen: u8,
    ptr_cached: bool,
    ptr_data_type: BchDataType,
    existing_type: BchDataType,
    bucket_idx: u64,
) -> Result<Option<BchDataType>, StorageError> {
    // ① gen_after: ptr gen > bucket gen（有符号）→ ptr gen 更新于 bucket gen
    if gen_after(ptr_gen, bucket_gen) > 0 {
        return Err(StorageError::InvalidData(format!(
            "ptr_gen_newer_than_bucket_gen: bucket {} gen {} ptr gen {}",
            bucket_idx, bucket_gen, ptr_gen,
        )));
    }

    // ② BUCKET_GC_GEN_MAX: bucket gen 比 ptr gen 领先 > 96 → ptr 过旧
    if gen_cmp(bucket_gen, ptr_gen) > BUCKET_GC_GEN_MAX as i32 {
        return Err(StorageError::InvalidData(format!(
            "ptr_too_stale: bucket {} gen {} ptr gen {} diff > {}",
            bucket_idx, bucket_gen, ptr_gen, BUCKET_GC_GEN_MAX,
        )));
    }

    // ③ stale cached ptr: gen 不匹配 + cached → 跳过
    if bucket_gen != ptr_gen && ptr_cached {
        return Ok(None);
    }

    // ④ stale dirty ptr: gen 不匹配 + 非 cached → 错误
    if bucket_gen != ptr_gen {
        return Err(StorageError::InvalidData(format!(
            "stale dirty ptr: bucket {} gen {} ptr gen {}",
            bucket_idx, bucket_gen, ptr_gen,
        )));
    }

    // ⑤ bucket_data_type_mismatch: bucket 已有类型与 ptr 类型冲突
    if bucket_data_type_mismatch(existing_type, ptr_data_type) {
        return Err(StorageError::InvalidData(format!(
            "ptr_bucket_data_type_mismatch: bucket {} type {:?} ptr type {:?}",
            bucket_idx, existing_type, ptr_data_type,
        )));
    }

    Ok(Some(ptr_data_type))
}

/// BCH_DATA_NR — bcachefs 数据类型总数
///
/// bcachefs C 源码（fs/alloc/accounting_format.h:68-76）:
/// ```c
/// enum bch_data_type {
///     BCH_DATA_free=0,  ..., BCH_DATA_unstriped=10,
///     BCH_DATA_NR       // = 11，作为 enum 最后一个条目用作数组尺寸
/// };
/// ```
/// BCH_DATA_NR 在 C 中并非有效的数据类型变体，而是 `BCH_DATA_unstriped + 1 = 11`，
/// 用作 `bch_devs_mask rw_devs[BCH_DATA_NR]` 等数组的静态尺寸。
///
pub const BCH_DATA_NR: usize = 11;

/// Bucket 元数据（运行时精简层）
///
/// 三层架构：gen 在 gens[]，GC 状态在 GcBucket，运行时 Bucket 为精简层。
/// state 字段作为缓存保留，以 sector 计数为真实来源。
///
/// 布局（__aligned(sizeof(long)) = 8 字节对齐）:
///   offset 0: state (BchDataType = u8 enmu)
///   offset 1: pad (u8)
///   offset 2: pad (u8)
///   offset 3: pad (u8)
///   offset 4: dirty_sectors (u32)
///   offset 8: cached_sectors (u32)
///   offset 12: stripe_sectors (u32)
///   offset 16: journal_seq_nonempty (u64)
///   offset 24: journal_seq_empty (u64)
///   offset 32: group (u32)
///   offset 36: oldest_gen (u8)
///   offset 37: flags (u8)
///   offset 38: nocow_locked (bool = u8, 1 byte)
///   offset 39-47: padding
///   total: 40 bytes
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[repr(C, align(8))]
pub struct Bucket {
    /// Bucket 数据类型（枚举映射）
    pub state: BchDataType,
    /// 脏扇区计数 — bcachefs 从 dirty_sectors > 0 推导 data_type
    pub dirty_sectors: u32,
    /// 缓存扇区计数
    pub cached_sectors: u32,
    /// 条带扇区计数（对齐 bcachefs struct bch_alloc_v4.stripe_sectors）
    pub stripe_sectors: u32,
    /// Journal seq（记录最后使此 bucket 从空→非空的 journal entry seq）
    pub journal_seq_nonempty: u64,
    /// Bucket 变空时的 journal seq
    pub journal_seq_empty: u64,
    /// 所属 allocation group
    pub group: u32,
    /// 最老仍需保留的 generation
    #[serde(default)]
    pub oldest_gen: u8,
    /// 预留标志位（NEED_DISCARD, NEED_INC_GEN 等）
    #[serde(default)]
    pub flags: u8,
    /// nocow 锁定标记 — 内存运行时标记，不持久化
    #[serde(default)]
    pub nocow_locked: bool,
}

impl Bucket {
    /// 创建空闲 bucket
    pub const fn free(group: u32) -> Self {
        Self {
            state: BchDataType::Free,
            dirty_sectors: 0,
            cached_sectors: 0,
            stripe_sectors: 0,
            journal_seq_nonempty: 0,
            journal_seq_empty: 0,
            group,
            oldest_gen: 0,
            flags: 0,
            nocow_locked: false,
        }
    }

    /// 是否空闲
    pub fn is_free(&self) -> bool {
        self.state == BchDataType::Free
    }

    /// 标记为已分配
    pub fn mark_allocated(&mut self) {
        self.state = BchDataType::User;
    }

    /// 标记为空闲 — 对应 bcachefs `__discard_mark_free()` (discard.c:163)
    ///
    /// 仅设置 data_type=Free 并清零 journal seq。不操作 oldest_gen 或 gen，
    /// 由调用者（触发器中）负责 gen/oldest_gen 的递增。
    pub fn mark_free(&mut self) {
        self.state = BchDataType::Free;
        self.journal_seq_nonempty = 0;
        self.journal_seq_empty = 0;
    }
}

/// GC 阶段使用的 bucket 视图 — 精确对齐 bcachefs `struct bucket` (16 字节)
///
/// bcachefs 对应: `buckets_types.h:37 struct bucket`
///
/// 布局（__aligned(sizeof(long)) = 8 字节对齐）:
///   offset 0: lock (u8)
///   offset 1: gen_valid_data_type (u8) — bit 0 = gen_valid, bits 1-7 = data_type
///   offset 2: gen (u8)
///   offset 3: pad (u8)
///   offset 4: dirty_sectors (u32)
///   offset 8: cached_sectors (u32)
///   offset 12: stripe_sectors (u32)
///   total: 16 bytes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C, align(8))]
pub struct GcBucket {
    pub lock: u8,
    /// bit 0 = gen_valid, bits 1-7 = data_type (BCH_DATA_*)
    pub gen_valid_data_type: u8,
    pub gen: u8,
    _pad: u8,
    pub dirty_sectors: u32,
    pub cached_sectors: u32,
    pub stripe_sectors: u32,
}

impl GcBucket {
    pub const fn zero() -> Self {
        Self {
            lock: 0,
            gen_valid_data_type: 0,
            gen: 0,
            _pad: 0,
            dirty_sectors: 0,
            cached_sectors: 0,
            stripe_sectors: 0,
        }
    }

    pub fn gen_valid(&self) -> bool {
        self.gen_valid_data_type & 1 != 0
    }

    pub fn set_gen_valid(&mut self, v: bool) {
        if v {
            self.gen_valid_data_type |= 1;
        } else {
            self.gen_valid_data_type &= !1;
        }
    }

    pub fn data_type(&self) -> u8 {
        self.gen_valid_data_type >> 1
    }

    pub fn set_data_type(&mut self, dt: u8) {
        self.gen_valid_data_type = (self.gen_valid_data_type & 1) | (dt << 1);
    }
}

/// 将 BchAllocEntry 转换为 GcBucket — 对应 bcachefs `alloc_to_bucket()` (buckets.h:143)
///
/// 从 Alloc btree 的持久化状态复制到 GC 用的 struct bucket 视图。
/// 不复制 stripe_refcount（struct bucket 不含此字段）。
pub fn alloc_to_bucket(dst: &mut GcBucket, src: &crate::alloc::BchAllocEntry) {
    dst.gen = src.gen;
    dst.set_data_type(src.data_type);
    dst.stripe_sectors = src.stripe_sectors;
    dst.dirty_sectors = src.dirty_sectors;
    dst.cached_sectors = src.cached_sectors;
}

/// 将 GcBucket 转换为 BchAllocEntry — 对应 bcachefs `__bucket_m_to_alloc()` (buckets.h:152)
///
/// 恢复最后写入的 alloc_entry 字段（gen、data_type、sector counts）。
pub fn __bucket_m_to_alloc(src: &GcBucket) -> crate::alloc::BchAllocEntry {
    crate::alloc::BchAllocEntry {
        journal_seq_nonempty: 0,
        journal_seq_empty: 0,
        stripe_refcount: 0,
        stripe_sectors: src.stripe_sectors,
        dirty_sectors: src.dirty_sectors,
        cached_sectors: src.cached_sectors,
        data_type: src.data_type(),
        flags: 0,
        gen: src.gen,
        oldest_gen: 0,
        stripe_redundancy_obsolete: 0,
        io_time: [0; 2],
        nr_external_backpointers: 0,
        pad: 0,
    }
}

/// 归一化 data_type 为 bucket 存储的数据类型 — 对应 bcachefs `bucket_data_type()` (background.h:44)
///
/// 在 dirty 分支中，Cached 和 Stripe 都视为 User 数据。
/// bcachefs 注释: "cached and stripe data are both user data from the bucket's perspective."
pub(crate) fn bucket_data_type(data_type: BchDataType) -> BchDataType {
    match data_type {
        BchDataType::Cached | BchDataType::Stripe => BchDataType::User,
        _ => data_type,
    }
}

/// 检查 data_type 是否属于「空 bucket」— 对应 bcachefs `data_type_is_empty()` (accounting_format.h:78)
pub(crate) fn data_type_is_empty(data_type: BchDataType) -> bool {
    matches!(
        data_type,
        BchDataType::Free | BchDataType::NeedGcGens | BchDataType::NeedDiscard
    )
}

/// 检查 bucket 已有类型与 ptr 类型的归一化后是否冲突 — 对应 bcachefs `bucket_data_type_mismatch()` (background.h:55)
pub(crate) fn bucket_data_type_mismatch(bucket: BchDataType, ptr: BchDataType) -> bool {
    !data_type_is_empty(bucket) && bucket_data_type(bucket) != bucket_data_type(ptr)
}

/// 推导 bucket 的数据类型 — 对应 bcachefs `alloc_data_type()` (background.h:124)
///
/// bcachefs 使用扇区计数 + stored data_type + generation 推导最终 data_type：
/// 1. stripe_refcount > 0 → Stripe（parity 除外）
/// 2. stripe_sectors + dirty_sectors > 0 → bucket_data_type(data_type)
/// 3. cached_sectors > 0 → Cached
/// 4. stored data_type == NeedDiscard → NeedDiscard（透传待 TRIM）
/// 5. gen - oldest_gen >= BUCKET_GC_GEN_MAX → NeedGcGens（强制 GC 扫描）
/// 6. 否则 → Free
pub fn derive_data_type(
    dirty_sectors: u32,
    cached_sectors: u32,
    stripe_sectors: u32,
    stripe_refcount: u32,
    gen: u8,
    oldest_gen: u8,
    data_type: BchDataType,
) -> BchDataType {
    if stripe_refcount > 0 {
        return if data_type == BchDataType::Parity {
            BchDataType::Parity
        } else {
            BchDataType::Stripe
        };
    }
    if stripe_sectors + dirty_sectors > 0 {
        return bucket_data_type(data_type);
    }
    if cached_sectors > 0 {
        return BchDataType::Cached;
    }
    if data_type == BchDataType::NeedDiscard {
        return BchDataType::NeedDiscard;
    }
    if gen.wrapping_sub(oldest_gen) >= BUCKET_GC_GEN_MAX {
        return BchDataType::NeedGcGens;
    }
    BchDataType::Free
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alloc::BchAllocEntry;

    #[test]
    fn test_bucket_new_free() {
        let b = Bucket::free(1);
        assert!(b.is_free());
        assert_eq!(b.group, 1);
    }

    #[test]
    fn test_bucket_mark_allocated() {
        let mut b = Bucket::free(0);
        b.mark_allocated();
        assert_eq!(b.state, BchDataType::User);
    }

    #[test]
    fn test_bucket_mark_free() {
        let mut b = Bucket::free(2);
        b.mark_allocated();
        b.journal_seq_nonempty = 99;
        b.journal_seq_empty = 42;
        b.mark_free();
        assert_eq!(b.state, BchDataType::Free);
        assert_eq!(b.journal_seq_nonempty, 0);
        assert_eq!(b.journal_seq_empty, 0);
    }

    #[test]
    fn test_bch_data_type_from_raw_matches_bcachefs() {
        let expected = [
            BchDataType::Free,
            BchDataType::Sb,
            BchDataType::Journal,
            BchDataType::Btree,
            BchDataType::User,
            BchDataType::Cached,
            BchDataType::Parity,
            BchDataType::Stripe,
            BchDataType::NeedGcGens,
            BchDataType::NeedDiscard,
            BchDataType::Unstriped,
        ];

        assert_eq!(expected.len(), BCH_DATA_NR);
        for (raw, expected) in expected.into_iter().enumerate() {
            assert_eq!(BchDataType::from_raw(raw as u8), Some(expected));
        }
        for raw in [11, 12, 13, 14, u8::MAX] {
            assert_eq!(BchDataType::from_raw(raw), None);
        }
    }

    #[test]
    fn test_bucket_serde_roundtrip() {
        let b = Bucket::free(3);
        let data = bincode::serialize(&b).unwrap();
        let restored: Bucket = bincode::deserialize(&data).unwrap();
        assert_eq!(restored.state, b.state);
        assert_eq!(restored.group, b.group);
    }

    #[test]
    fn test_derive_data_type_stripe() {
        assert_eq!(
            derive_data_type(0, 0, 0, 1, 0, 0, BchDataType::Free),
            BchDataType::Stripe
        );
        assert_eq!(
            derive_data_type(100, 0, 0, 1, 0, 0, BchDataType::User),
            BchDataType::Stripe
        );
    }

    #[test]
    fn test_derive_data_type_parity() {
        // stripe_refcount > 0 + data_type == Parity → Parity (not Stripe)
        assert_eq!(
            derive_data_type(0, 0, 0, 1, 0, 0, BchDataType::Parity),
            BchDataType::Parity
        );
    }

    #[test]
    fn test_derive_data_type_dirty_sectors() {
        assert_eq!(
            derive_data_type(1, 0, 0, 0, 0, 0, BchDataType::User),
            BchDataType::User
        );
        assert_eq!(
            derive_data_type(50, 0, 0, 0, 0, 0, BchDataType::Btree),
            BchDataType::Btree
        );
        // Cached + dirty → User (bucket_data_type)
        assert_eq!(
            derive_data_type(10, 5, 0, 0, 0, 0, BchDataType::Cached),
            BchDataType::User
        );
    }

    #[test]
    fn test_derive_data_type_stripe_sectors_dirty_branch() {
        // stripe_sectors > 0 + dirty=0 → enters dirty branch (bcachefs bch2_bucket_sectors_dirty)
        assert_eq!(
            derive_data_type(0, 0, 50, 0, 0, 0, BchDataType::User),
            BchDataType::User
        );
        assert_eq!(
            derive_data_type(0, 0, 10, 0, 0, 0, BchDataType::Btree),
            BchDataType::Btree
        );
    }

    #[test]
    fn test_derive_data_type_cached() {
        assert_eq!(
            derive_data_type(0, 1, 0, 0, 0, 0, BchDataType::Free),
            BchDataType::Cached
        );
    }

    #[test]
    fn test_derive_data_type_need_discard() {
        // NeedDiscard passthrough (all sector counts = 0)
        assert_eq!(
            derive_data_type(0, 0, 0, 0, 0, 0, BchDataType::NeedDiscard),
            BchDataType::NeedDiscard
        );
    }

    #[test]
    fn test_derive_data_type_need_gc_gens() {
        // gen - oldest_gen >= 96 → NeedGcGens
        assert_eq!(
            derive_data_type(0, 0, 0, 0, 200, 100, BchDataType::Free),
            BchDataType::NeedGcGens
        );
        // gen - oldest_gen < 96 → Free
        assert_eq!(
            derive_data_type(0, 0, 0, 0, 100, 100, BchDataType::Free),
            BchDataType::Free
        );
        // wrapping: gen(5) - oldest_gen(200) = 61 (wrapping) < 96 → Free
        assert_eq!(
            derive_data_type(0, 0, 0, 0, 5, 200, BchDataType::Free),
            BchDataType::Free
        );
    }

    #[test]
    fn test_derive_data_type_free() {
        assert_eq!(
            derive_data_type(0, 0, 0, 0, 0, 0, BchDataType::Free),
            BchDataType::Free
        );
    }

    #[test]
    fn test_derive_data_type_free_with_gen_zero() {
        assert_eq!(
            derive_data_type(0, 0, 0, 0, 0, 0, BchDataType::Free),
            BchDataType::Free
        );
    }

    #[test]
    fn test_derive_data_type_dirty_with_gen() {
        assert_eq!(
            derive_data_type(100, 0, 0, 0, 5, 2, BchDataType::User),
            BchDataType::User
        );
    }

    #[test]
    fn test_derive_data_type_cached_with_gen() {
        assert_eq!(
            derive_data_type(0, 50, 0, 0, 10, 10, BchDataType::Free),
            BchDataType::Cached
        );
    }

    #[test]
    fn test_derive_data_type_stripe_sectors_dirty_branch_with_gen() {
        assert_eq!(
            derive_data_type(0, 0, 30, 0, 3, 1, BchDataType::User),
            BchDataType::User
        );
    }

    #[test]
    fn test_derive_data_type_cached_with_dirty_collision() {
        // cached + dirty 并存时 dirty 优先
        assert_eq!(
            derive_data_type(10, 50, 0, 0, 0, 0, BchDataType::User),
            BchDataType::User
        );
    }

    #[test]
    fn test_derive_data_type_need_gc_gens_when_gen_wraps_below_oldest_gen() {
        // gen=0, oldest_gen=100 → wrapping_sub = 156 >= 96 → NeedGcGens
        assert_eq!(
            derive_data_type(0, 0, 0, 0, 0, 100, BchDataType::Free),
            BchDataType::NeedGcGens
        );
    }

    #[test]
    fn test_derive_data_type_free_when_gen_close_to_oldest_gen() {
        // gen=101, oldest_gen=100 → wrapping_sub = 1 < 96 → Free
        assert_eq!(
            derive_data_type(0, 0, 0, 0, 101, 100, BchDataType::Free),
            BchDataType::Free
        );
    }

    #[test]
    fn test_gc_bucket_zero() {
        let g = GcBucket::zero();
        assert_eq!(g.lock, 0);
        assert_eq!(g.gen_valid_data_type, 0);
        assert_eq!(g.gen, 0);
        assert!(!g.gen_valid());
        assert_eq!(g.data_type(), 0);
    }

    #[test]
    fn test_gc_bucket_gen_valid() {
        let mut g = GcBucket::zero();
        assert!(!g.gen_valid());
        g.set_gen_valid(true);
        assert!(g.gen_valid());
        g.set_gen_valid(false);
        assert!(!g.gen_valid());
    }

    #[test]
    fn test_gc_bucket_data_type() {
        let mut g = GcBucket::zero();
        assert_eq!(g.data_type(), 0);
        g.set_data_type(3);
        assert_eq!(g.data_type(), 3);
        assert!(!g.gen_valid()); // gen_valid bit should be preserved as 0
                                 // setting data_type should not clobber gen_valid
        g.set_gen_valid(true);
        g.set_data_type(5);
        assert!(g.gen_valid());
        assert_eq!(g.data_type(), 5);
    }

    #[test]
    fn test_gc_bucket_repr() {
        // 验证 GcBucket 确实是 16 字节
        use std::mem;
        assert_eq!(mem::size_of::<GcBucket>(), 16);
        // 验证对齐为 8
        assert_eq!(mem::align_of::<GcBucket>(), 8);
    }

    #[test]
    fn test_alloc_to_bucket_copies_fields() {
        let entry = BchAllocEntry {
            journal_seq_nonempty: 42,
            journal_seq_empty: 7,
            stripe_refcount: 3,
            stripe_sectors: 100,
            dirty_sectors: (200 * crate::alloc::SECTORS_PER_BLOCK) as u32,
            cached_sectors: (50 * crate::alloc::SECTORS_PER_BLOCK) as u32,
            data_type: BchDataType::User as u8,
            flags: 0,
            gen: 5,
            oldest_gen: 2,
            stripe_redundancy_obsolete: 0,
            io_time: [0; 2],
            nr_external_backpointers: 0,
            pad: 0,
        };
        let mut gc = GcBucket::zero();
        alloc_to_bucket(&mut gc, &entry);
        assert_eq!(gc.gen, 5);
        assert_eq!(gc.data_type(), BchDataType::User as u8);
        // alloc_to_bucket 不设置 gen_valid（与 bcachefs C 一致）
        assert_eq!(
            gc.dirty_sectors,
            200 * crate::alloc::SECTORS_PER_BLOCK as u32
        );
        assert_eq!(
            gc.cached_sectors,
            50 * crate::alloc::SECTORS_PER_BLOCK as u32
        );
        assert_eq!(gc.stripe_sectors, 100);
    }

    #[test]
    fn test_bucket_m_to_alloc_roundtrip() {
        let mut gc = GcBucket::zero();
        gc.gen = 7;
        gc.set_data_type(BchDataType::Btree as u8);
        gc.set_gen_valid(true);
        gc.dirty_sectors = 300 * crate::alloc::SECTORS_PER_BLOCK as u32;
        gc.cached_sectors = 0;
        gc.stripe_sectors = 0;
        let entry = __bucket_m_to_alloc(&gc);
        assert_eq!(entry.gen, 7);
        assert_eq!(entry.data_type, BchDataType::Btree as u8);
        assert_eq!(
            entry.dirty_sectors,
            300 * crate::alloc::SECTORS_PER_BLOCK as u32
        );
        assert_eq!(entry.cached_sectors, 0);
        assert_eq!(entry.stripe_sectors, 0);
    }

    #[test]
    fn test_alloc_gc_gen_basic() {
        assert_eq!(crate::alloc::alloc_gc_gen(10, 5), 5);
        assert_eq!(crate::alloc::alloc_gc_gen(5, 10), 251); // wrapping: 5 - 10 = 251u8
        assert_eq!(crate::alloc::alloc_gc_gen(0, 0), 0);
    }

    #[test]
    fn test_alloc_gc_gen_threshold() {
        assert_eq!(crate::alloc::alloc_gc_gen(96, 0), 96);
        assert_eq!(crate::alloc::alloc_gc_gen(95, 0), 95);
        // gen=5, oldest_gen=200 → wrapping_sub = 61 (not >= 96)
        assert_eq!(crate::alloc::alloc_gc_gen(5, 200), 61);
    }
}
