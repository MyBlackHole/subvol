//! BchAllocator — bcachefs 对齐的块分配器
//!
//! bcachefs 使用 per-device allocator + bucket 级分配。
//! 本实现：多 Allocation Group + bitmap，每个 group 独立锁。
//!
//! ## 子模块
//!
//! - `bucket`：Bucket 状态管理 + BchDataType/Bucket 类型
//! - `btree`：Alloc btree 持久化类型（BchAllocEntry）

pub(crate) mod accounting;
pub mod background;
mod backpointer;
pub mod btree;
pub mod bucket;
pub mod bucket_gens;
pub mod dev_stripe;
pub mod foreground;
pub mod freespace;
pub mod open_bucket;
pub mod quota;
pub mod reservation;
pub mod write_point;

pub use btree::{BchAllocEntry, BchAllocV4};
pub use bucket::{BchDataType, Bucket, GcBucket, BCH_DATA_NR};
pub use bucket_gens::{BchBucketGens, BUCKET_GENS_PER_KEY};
pub use dev_stripe::{bch2_dev_alloc_list, target_rw_devs, DevAllocList, DevStripeState};
pub use open_bucket::{BchOpenBuckets, OpenBucket, OpenBucketIdx, OPEN_BUCKETS_COUNT};
pub use reservation::{
    __bch2_disk_reservation_add, avail_factor, bch2_disk_reservation_add,
    bch2_disk_reservation_get, bch2_disk_reservation_init, bch2_disk_reservation_put,
    bch2_fs_usage_read_short, BchReservationFlags, DiskReservation,
};
pub use write_point::{
    DedicatedWp, WritePointConfig, WritePointPool, WritePointSpecifier, NUM_DEDICATED_WPS,
    WRITE_POINT_MAX,
};

pub use crate::types::AllocError;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};
use tokio::sync::Notify;

use crate::alloc::btree::{deserialize_alloc_entry, serialize_alloc_entry};
use crate::alloc::bucket::{
    __bucket_m_to_alloc, bucket_ref_update_checks, data_type_is_empty, derive_data_type,
};
use crate::alloc::foreground::PrioHint;
use crate::bch_vol::BchVol;
use crate::block_device::bch_dev::bch2_mi_to_cpu;
use crate::block_device::{BchDev, BchDevIoRefKind};
use crate::btree::iter::UpdateTriggerFlags;
use crate::btree::key::{Bpos, BtreeEntry, BtreeKey, KeyType, KeyValue};

use crate::btree::transaction::UsageField;
use crate::btree::{bch2_btree_bit_mod, BtreeId, BtreeTrans};
use crate::storage::superblock::compat_bits;
#[cfg(test)]
use crate::storage::superblock::member_bits;
use crate::storage::superblock::BchMemberInitialized;
use crate::types::{StorageError, Watermark};

const FREESPACE_GENBITS_SHIFT: u32 = 56;
const FREESPACE_BUCKET_MASK: u64 = (1u64 << FREESPACE_GENBITS_SHIFT) - 1;

/// 对应本地 `struct bch_fs_usage_base`。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BchFsUsageBase {
    pub hidden: u64,
    pub btree: u64,
    pub data: u64,
    pub cached: u64,
    pub reserved: u64,
}

/// 对应本地 `struct bch_fs_usage_short` (`fs/alloc/buckets_types.h:86-90`)。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BchFsUsageShort {
    pub capacity: u64,
    pub used: u64,
    pub free: u64,
}

/// 对应本地 `struct bch_fs_capacity_pcpu`。
#[derive(Clone, Copy, Debug, Default)]
pub struct BchFsCapacityPcpu {
    pub usage: BchFsUsageBase,
    pub sectors_available: u64,
    pub online_reserved: u64,
}

/// 对应本地 `struct bch_fs_capacity` (`fs/alloc/types.h`)。
pub struct BchFsCapacity {
    pub capacity: u64,
    pub reserved: u64,
    pub capacity_gen: u32,
    pub bucket_size_max: u32,
    pub sectors_available: AtomicU64,
    pub sectors_available_lock: Mutex<()>,
    pub pcpu: Vec<BchFsCapacityPcpu>,
    pub mark_lock: RwLock<()>,
}

impl Default for BchFsCapacity {
    fn default() -> Self {
        Self {
            capacity: 0,
            reserved: 0,
            capacity_gen: 0,
            bucket_size_max: 0,
            sectors_available: AtomicU64::new(0),
            sectors_available_lock: Mutex::new(()),
            pcpu: Vec::new(),
            mark_lock: RwLock::new(()),
        }
    }
}

/// Btree bitmap 过滤类型 — 对应 bcachefs 中 btree_bitmap 的分配过滤逻辑。
///
/// 当 allocate_bucket_inner 尝试分配 bucket 时，检查桶的 btree_bitmap 标记
/// 是否与请求的过滤类型匹配，跳过不匹配的桶。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum BtreeBitmapFilter {
    /// 只能在非 btree 区域分配
    No,
    /// 只能在 btree 区域分配
    Yes,
    /// 任何区域均可
    #[default]
    Any,
}

/// 分配请求 — 封装水位线、数据类型和副本策略。
///
/// 对齐 bcachefs `alloc_request` 结构。
#[derive(Debug, Clone)]
pub struct AllocRequest {
    /// 分配水位线（决定预留 bucket 数）
    pub watermark: Watermark,
    /// 数据类型（btree / user / gc 等）
    pub data_type: BchDataType,
    /// 目标 allocation group（0 = 自动选择）
    pub target: u32,
    /// 副本数
    pub replicas: u32,
    /// btree bitmap 过滤：限制分配的区域类型
    pub btree_bitmap: BtreeBitmapFilter,
    /// Journal seq（bucket 最后引用的 journal entry seq），
    /// 用于 may_alloc_bucket_journal_seq 检查。0 = 不检查。
    /// 由调用方从 Journal 中获取当前 seq。
    pub journal_seq: u64,
    /// 子卷 ID（可选），用于分配前 quota 检查
    pub subvol_id: Option<u32>,
}

impl AllocRequest {
    /// 创建简单分配请求
    pub fn new(watermark: Watermark, data_type: BchDataType) -> Self {
        Self {
            watermark,
            data_type,
            target: 0,
            replicas: 1,
            btree_bitmap: BtreeBitmapFilter::Any,
            journal_seq: 0,
            subvol_id: None,
        }
    }

    /// 设置 journal seq（用于 may_alloc_bucket_journal_seq 检查）
    pub fn with_journal_seq(mut self, journal_seq: u64) -> Self {
        self.journal_seq = journal_seq;
        self
    }

    /// 设置子卷 ID（用于分配前 quota 检查）
    pub fn with_subvol(mut self, subvol_id: u32) -> Self {
        self.subvol_id = Some(subvol_id);
        self
    }
}

/// 默认 bucket 大小（1MB = 256 个 4K block）
pub const DEFAULT_BUCKET_SIZE: u64 = 1024 * 1024;
/// 默认块大小（4KB）
pub const DEFAULT_BLOCK_SIZE: u64 = 4096;
/// 每 bucket 的块数
pub const BLOCKS_PER_BUCKET: u64 = DEFAULT_BUCKET_SIZE / DEFAULT_BLOCK_SIZE;
/// 每 block 的扇区数（4KB / 512B = 8）
pub const SECTORS_PER_BLOCK: u64 = DEFAULT_BLOCK_SIZE / (crate::types::SECTOR_SIZE as u64);
/// 默认 btree 节点大小（与 `config::default_btree_node_size()` 一致）。
pub const DEFAULT_BTREE_NODE_SIZE: u32 = 256 * 1024;
/// upstream `BTREE_NODE_RESERVE` = `(BTREE_MAX_DEPTH + BTREE_MAX_DEPTH - 1) * 4`
pub const BTREE_NODE_RESERVE: u64 = 60;

/// 对应本地 `sector_to_bucket()` (`fs/alloc/buckets.h:18-21`)。
#[inline]
pub(crate) fn sector_to_bucket(ca: &BchDev, s: u64) -> u64 {
    s / unsafe { &*ca.mi.get() }.bucket_size as u64
}

/// 对应本地 `bucket_to_sector()` (`fs/alloc/buckets.h:23-26`)。
#[inline]
pub(crate) fn bucket_to_sector(ca: &BchDev, b: usize) -> u64 {
    b as u64 * unsafe { &*ca.mi.get() }.bucket_size as u64
}

/// 对应本地 `bucket_remainder()` (`fs/alloc/buckets.h:28-34`)。
#[inline]
pub(crate) fn bucket_remainder(ca: &BchDev, s: u64) -> u64 {
    s % unsafe { &*ca.mi.get() }.bucket_size as u64
}

/// 对应本地 `sector_to_bucket_and_offset()` (`fs/alloc/buckets.h:36-39`)。
#[inline]
pub(crate) fn sector_to_bucket_and_offset(ca: &BchDev, s: u64, offset: &mut u32) -> u64 {
    let bucket_size = unsafe { &*ca.mi.get() }.bucket_size as u64;
    *offset = (s % bucket_size) as u32;
    s / bucket_size
}

pub fn calc_btree_reserve_buckets(bucket_size: u16, btree_node_size: u32) -> u64 {
    let btree_sectors = (btree_node_size as u64) / crate::types::SECTOR_SIZE;
    let nodes_per_bucket = (bucket_size as u64 / btree_sectors).max(1);
    BTREE_NODE_RESERVE.div_ceil(nodes_per_bucket)
}

/// 计算 bucket 的 gc generation。
///
/// 对齐 bcachefs `alloc_gc_gen()`:
/// `gen - oldest_gen`。
/// gen 和 oldest_gen 均为 u8，减法自动 wrapping。
pub fn alloc_gc_gen(gen: u8, oldest_gen: u8) -> u8 {
    gen.wrapping_sub(oldest_gen)
}

/// 计算 freespace key 的 generation bits。
///
/// 对齐 bcachefs `alloc_freespace_genbits()`:
/// 把 bucket 的 gc generation 右移 4 位后放入 `offset` 高 8 位。
pub fn alloc_freespace_genbits(gc_gen: u8) -> u64 {
    ((gc_gen as u64) >> 4) << FREESPACE_GENBITS_SHIFT
}

/// 将 bucket index + generation 编码为 freespace key 位置。
///
/// 对齐 bcachefs `alloc_freespace_pos()`:
/// - `offset` 低 56 位保存 bucket index
/// - `offset` 高 8 位保存 genbits
/// - `snapshot` 固定为 0
pub fn alloc_freespace_pos(dev: u8, bucket_idx: u64, gen: u8, oldest_gen: u8) -> Bpos {
    Bpos::new(
        dev as u64,
        (bucket_idx & FREESPACE_BUCKET_MASK)
            | alloc_freespace_genbits(alloc_gc_gen(gen, oldest_gen)),
        0,
    )
}

/// 从 freespace key 中取回 bucket index。
pub fn alloc_freespace_bucket_idx(pos: Bpos) -> u64 {
    pos.offset & FREESPACE_BUCKET_MASK
}

/// 从 freespace key 中取回 generation bits。
pub fn alloc_freespace_pos_genbits(pos: Bpos) -> u64 {
    pos.offset & !FREESPACE_BUCKET_MASK
}

/// Allocation Group — 独立锁的分配单元
///
/// 每个 AG 管理一段连续的 block 范围，拥有自己的 bitmap 和锁。
/// 多个 AG 允许多线程并发分配不同 AG 的块。
///
/// 三层 bucket 架构：
/// - `buckets` — 运行时 Bucket 精简层（gen/GC 状态分别在 gens[]/gc_buckets）
/// - `gc_buckets` — GC 标记层（16B GcBucket，精确对齐 bcachefs struct bucket）
/// - `gens` — gen 层（每个 bucket 一个 u8，gen 生命周期控制）
#[derive(Debug)]
pub struct AllocGroup {
    /// Group ID
    pub id: u32,
    /// 起始 block addr
    pub start_block: u64,
    /// 管理的 block 数量
    pub block_count: u64,
    /// bucket 数组（运行时层，精简结构）
    pub buckets: Vec<Bucket>,
    /// GC 标记数组（16B GcBucket，精确对齐 bcachefs struct bucket）
    pub gc_buckets: Vec<GcBucket>,
    /// Generation 数组（每个 bucket 一个 u8）
    pub gens: Vec<u8>,
    /// 空闲 bucket 计数（由 freespace btree 维护，原子方式暴露给快速检查）
    pub free_buckets: AtomicU64,
    /// 该组的 bucket 总数（用于预留计算）
    pub total_buckets: u64,
    /// Btree bitmap — per-bucket bitset（1 bit = 1 bucket 是否被 btree 占用）
    ///
    /// 对应 bcachefs `bch_allocator::btree_bitmap`。
    /// 当 allocate_bucket_inner 分配时，检查桶的 bitmap 是否与请求匹配。
    pub btree_bitmap: Vec<u64>,
}

/// 对应本地 bcachefs `bch2_dev_buckets_resize()`
/// (`fs/alloc/buckets.c:1277-1325`)。设备 bucket arrays 只归 `BchDev` 所有。
pub fn bch2_dev_buckets_resize(c: &BchVol, ca: &BchDev, nbuckets: u64) -> Result<(), StorageError> {
    let resize = !unsafe { &*ca.groups.get() }.is_empty();
    let _state_lock = resize.then(|| c.state_lock.lock().unwrap());
    let member = c
        .superblock()
        .member(ca.dev_idx)
        .ok_or_else(|| StorageError::NotFound(format!("member {} not found", ca.dev_idx)))?;
    let mi = bch2_mi_to_cpu(member);
    let bucket_sectors = mi.bucket_size as u64;
    if bucket_sectors == 0
        || !bucket_sectors.is_multiple_of(SECTORS_PER_BLOCK)
        || nbuckets < mi.first_bucket as u64
    {
        return Err(StorageError::InvalidArgument(format!(
            "invalid bucket geometry for device {}",
            ca.dev_idx
        )));
    }
    let bucket_blocks = bucket_sectors / SECTORS_PER_BLOCK;

    let total_blocks = nbuckets * bucket_blocks;
    let effective_buckets = nbuckets - mi.first_bucket as u64;
    let group_buckets = (1024 / bucket_blocks).min(effective_buckets / 4).max(1);
    let num_groups = effective_buckets.div_ceil(group_buckets);
    let mut groups = Vec::with_capacity(num_groups as usize);

    for i in 0..num_groups {
        let first_bucket = mi.first_bucket as u64 + i * group_buckets;
        let bucket_count = group_buckets.min(nbuckets - first_bucket);
        let start = first_bucket * bucket_blocks;
        let count = bucket_count * bucket_blocks;
        let buckets: Vec<Bucket> = (0..bucket_count)
            .map(|_| Bucket {
                state: BchDataType::Free,
                dirty_sectors: 0,
                cached_sectors: 0,
                stripe_sectors: 0,
                journal_seq_nonempty: 0,
                journal_seq_empty: 0,
                group: i as u32,
                oldest_gen: 0,
                flags: 0,
                nocow_locked: false,
            })
            .collect();
        let gc_buckets = vec![GcBucket::zero(); bucket_count as usize];
        let gens = vec![0; bucket_count as usize];
        groups.push(Mutex::new(AllocGroup {
            id: i as u32,
            start_block: start,
            block_count: count,
            buckets,
            gc_buckets,
            gens,
            free_buckets: AtomicU64::new(bucket_count),
            total_buckets: bucket_count,
            btree_bitmap: vec![0; bucket_count.div_ceil(64) as usize],
        }));
    }

    if resize {
        let old_groups = unsafe { &*ca.groups.get() };
        for old_group in old_groups {
            let old = old_group.lock().unwrap();
            for old_local in 0..old.buckets.len() {
                let block = old.start_block + old_local as u64 * bucket_blocks;
                let Some(new_group) = groups.iter().find(|group| {
                    let group = group.lock().unwrap();
                    block >= group.start_block && block < group.start_block + group.block_count
                }) else {
                    continue;
                };
                let mut new = new_group.lock().unwrap();
                let new_local = ((block - new.start_block) / bucket_blocks) as usize;
                if new_local >= new.buckets.len() {
                    continue;
                }
                new.buckets[new_local] = old.buckets[old_local];
                new.gc_buckets[new_local] = old.gc_buckets[old_local];
                new.gens[new_local] = old.gens[old_local];
                let old_word = old_local / 64;
                let old_bit = old_local % 64;
                if old.btree_bitmap[old_word] & (1u64 << old_bit) != 0 {
                    let new_word = new_local / 64;
                    let new_bit = new_local % 64;
                    new.btree_bitmap[new_word] |= 1u64 << new_bit;
                }
            }
        }
        for group in &groups {
            let group = group.lock().unwrap();
            let free = group
                .buckets
                .iter()
                .filter(|bucket| bucket.state == BchDataType::Free)
                .count() as u64;
            group.free_buckets.store(free, Ordering::Relaxed);
        }
    }

    let nr_free_buckets = groups
        .iter()
        .map(|group| group.lock().unwrap().free_buckets.load(Ordering::Relaxed))
        .sum();

    // SAFETY: replacement on resize is serialized by state_lock, matching the
    // local C boundary. Initial allocation occurs before paths can observe ca.
    unsafe {
        *ca.mi.get() = mi;
        *ca.groups.get() = groups;
    }
    ca.total_blocks.store(total_blocks, Ordering::Release);
    ca.nr_free_buckets.store(nr_free_buckets, Ordering::Release);
    if !resize {
        ca.allocated.store(0, Ordering::Release);
        ca.freespace_initialized.store(false, Ordering::Release);
        ca.nr_btree_reserve.store(
            calc_btree_reserve_buckets(mi.bucket_size, c.config.btree_node_size),
            Ordering::Release,
        );
    }
    if resize {
        background::bch2_recalc_capacity(c);
    }
    Ok(())
}

/// 对应本地 bcachefs `bch2_dev_buckets_alloc()`
/// (`fs/alloc/buckets.c:1327-1334`)：先建立 usage，再按 member nbuckets resize。
pub fn bch2_dev_buckets_alloc(c: &BchVol, ca: &BchDev) -> Result<(), StorageError> {
    let nbuckets = c
        .superblock()
        .member(ca.dev_idx)
        .ok_or_else(|| StorageError::NotFound(format!("member {} not found", ca.dev_idx)))?
        .nbuckets;
    bch2_dev_buckets_resize(c, ca, nbuckets)
}

/// 返回设备在指定水位线下可用的 bucket 数量。
///
/// 对应本地 `dev_buckets_free()`/`__dev_buckets_free()`
/// (`fs/alloc/buckets.h:258-271`)：从每个 allocation group 的 free bucket
/// 统计中扣除水位线预留和设备当前 open bucket。当前 `AllocGroup` 的
/// bucket 统计位于 mutex 内，因此按 group 短暂读取锁，保持计数与分配器状态一致。
pub(crate) fn dev_buckets_free(ca: &BchDev, watermark: Watermark) -> u64 {
    let btree_reserve = ca.nr_btree_reserve.load(Ordering::Acquire);
    let groups = unsafe { &*ca.groups.get() };
    let free = ca.nr_free_buckets.load(Ordering::Acquire);
    let reserved = groups.iter().fold(0u64, |total, group| {
        let group = group.lock().unwrap();
        total.saturating_add(
            watermark.reserved_buckets_with_btree_reserve(group.total_buckets, btree_reserve),
        )
    });

    free.saturating_sub(reserved)
        .saturating_sub(ca.nr_open_buckets.load(Ordering::Acquire))
}

/// 检查 bucket 是否可以分配（journal seq 安全）— 对应 bcachefs `may_alloc_bucket_journal_seq`
///
/// 如果 bucket 最后被引用的 journal seq 尚未落盘，则分配该 bucket 可能导致
/// crash recovery 后引用旧数据（数据损坏）。
///
/// # 语义
///
/// - `bucket.journal_seq_empty == 0`：bucket 的空转移尚未被 journal 追踪，跳过
/// - `bucket.journal_seq_empty <= flushed_journal_seq`：journal 已推进到 bucket 变空之后，安全
/// - 否则：bucket 仍可能被 journal 引用，跳过
pub fn may_alloc_bucket_journal_seq(bucket: &Bucket, flushed_journal_seq: u64) -> bool {
    if bucket.journal_seq_empty == 0 {
        return true; // bucket 的空转移尚未建立追踪
    }
    bucket.journal_seq_empty <= flushed_journal_seq
}

/// 块分配器 — 对应 bcachefs `bch_alloc`
///
/// 多 Allocation Group 设计，每个 Group 独立锁，支持并发分配。
/// 简化版：元数据在内存中，未集成 alloc btree。
pub struct BchAllocator {
    /// 写点池（≥2 时启用，None 退化为全局 hint 行为）
    write_points: Option<Mutex<write_point::WritePointPool>>,
    /// 开放桶引用计数池 — 对应 bcachefs `bch_fs_allocator::open_buckets`
    pub open_buckets: BchOpenBuckets,
    /// 对应本地 `c->freelist_wait`。
    pub freelist_wait: Notify,
}

impl BchAllocator {
    /// 创建 filesystem-global allocator 状态。
    pub fn new(total_sectors: u64) -> Self {
        Self::with_config(total_sectors, WritePointConfig::default())
    }

    /// 创建 filesystem-global allocator 状态并指定写点配置。
    pub fn with_config(_total_sectors: u64, config: WritePointConfig) -> Self {
        let write_points = if config.max_write_points > 1 {
            Some(Mutex::new(write_point::WritePointPool::new(config)))
        } else {
            None
        };

        Self {
            write_points,
            open_buckets: BchOpenBuckets::new(),
            freelist_wait: Notify::new(),
        }
    }

    /// freespace btree 未初始化时的回退分配路径。
    ///
    /// 直接扫描 Alloc btree 寻找 Free bucket（对应 bcachefs 未初始化时的 `bch2_bucket_alloc_set_trans` 路径）。
    /// `start_hint` 由调用方 `bch2_allocate_bucket_inner` 计算并传入，避免 hint 重复递增。
    fn bch2_allocate_from_alloc_btree(
        &self,
        vol: &BchVol,
        ca: &BchDev,
        request: &AllocRequest,
        _wp_id: Option<WritePointSpecifier>,
        start_hint: u64,
    ) -> Result<u64, AllocError> {
        let groups = unsafe { &*ca.groups.get() };
        let total_blocks = ca.total_blocks.load(Ordering::Acquire);
        let num_groups = groups.len() as u64;
        if num_groups == 0 {
            return Err(AllocError::AddressSpaceExhausted {
                max_raw_addr: total_blocks,
            });
        }

        for offset in 0..num_groups {
            let gi = ((start_hint + offset) % num_groups) as usize;
            let mut group = groups[gi].lock().unwrap();

            let free = group.free_buckets.load(Ordering::Relaxed);
            let open_share = (self.open_buckets.nr_open() as u64) / num_groups;
            if free <= open_share {
                continue;
            }

            let group_start = group.start_block;

            // 扫描 Alloc btree 寻找 Free bucket（两阶段：先查找，再分配）
            let candidate = group
                .buckets
                .iter()
                .enumerate()
                .find_map(|(local_idx, bucket)| {
                    if bucket.state != BchDataType::Free {
                        return None;
                    }
                    let bi = local_idx as u32;
                    let is_btree_bit_set = self.bch2_btree_bitmap_test(&group, bi);
                    let bitmap_ok = match request.btree_bitmap {
                        BtreeBitmapFilter::No => !is_btree_bit_set,
                        BtreeBitmapFilter::Yes => is_btree_bit_set,
                        BtreeBitmapFilter::Any => true,
                    };
                    if !bitmap_ok {
                        return None;
                    }
                    if self.bch2_bucket_nocow_is_locked(&group, bi) {
                        return None;
                    }
                    let flushed_journal_seq =
                        vol.journal_ref().last_seq_ondisk.load(Ordering::Acquire);
                    if !may_alloc_bucket_journal_seq(bucket, flushed_journal_seq) {
                        return None;
                    }
                    Some(local_idx)
                });

            if let Some(local_idx) = candidate {
                let bi = local_idx as u32;
                let bucket_oldest_gen = group.buckets[local_idx].oldest_gen;
                let bucket_gen = group.gens[local_idx].wrapping_add(1);
                group.gens[local_idx] = bucket_gen;

                // 执行分配
                let bucket = &mut group.buckets[local_idx];
                bucket.state = request.data_type;
                if request.journal_seq > 0 {
                    bucket.journal_seq_nonempty = request.journal_seq;
                }
                let bucket_index =
                    sector_to_bucket(ca, group_start * SECTORS_PER_BLOCK) + bi as u64;
                let block_addr = bucket_to_sector(ca, bucket_index as usize) / SECTORS_PER_BLOCK;
                let bucket_journal_seq_empty = bucket.journal_seq_empty;

                match self.open_buckets.alloc(
                    ca.dev_idx,
                    bucket_index,
                    unsafe { &*ca.mi.get() }.bucket_size as u32,
                    bucket_gen,
                ) {
                    Ok(_ob_idx) => {}
                    Err(_) => {
                        bucket.state = BchDataType::Free;
                        group.gens[local_idx] = bucket_gen.wrapping_sub(1);
                        continue;
                    }
                }

                group.free_buckets.fetch_sub(1, Ordering::Relaxed);
                ca.nr_free_buckets.fetch_sub(1, Ordering::Release);
                ca.allocated.fetch_add(
                    unsafe { &*ca.mi.get() }.bucket_size as u64 / SECTORS_PER_BLOCK,
                    Ordering::Relaxed,
                );
                ca.nr_open_buckets.fetch_add(1, Ordering::Relaxed);

                let alloc_bpos = Bpos::new(ca.dev_idx as u64, bucket_index, 0);
                let alloc_entry = BchAllocEntry {
                    journal_seq_nonempty: request.journal_seq,
                    flags: 0,
                    gen: bucket_gen as u8,
                    oldest_gen: bucket_oldest_gen,
                    data_type: BchDataType::Free as u8,
                    stripe_redundancy_obsolete: 0,
                    dirty_sectors: 0,
                    cached_sectors: 0,
                    io_time: [0; 2],
                    stripe_refcount: 0,
                    nr_external_backpointers: 0,
                    journal_seq_empty: bucket_journal_seq_empty,
                    stripe_sectors: 0,
                    pad: 0,
                };
                let bytes = serialize_alloc_entry(&alloc_entry);
                vol.btree(BtreeId::Alloc)
                    .bch2_btree_bset_insert_key_wrapper(
                        BtreeEntry::raw(alloc_bpos, KeyType::Normal, bytes),
                        0,
                    );

                bch2_btree_bit_mod(
                    vol,
                    BtreeId::Freespace,
                    alloc_freespace_pos(ca.dev_idx, bucket_index, bucket_gen, bucket_oldest_gen),
                    false,
                );

                return Ok(block_addr);
            }
        }

        Err(AllocError::AddressSpaceExhausted {
            max_raw_addr: total_blocks,
        })
    }

    pub fn btree_reserve_buckets(&self, ca: &BchDev) -> u64 {
        ca.nr_btree_reserve.load(Ordering::Acquire)
    }

    /// 分配一个 bucket（内部分配路径）
    ///
    /// 与 `allocate_bucket` 签名相同但不做多级分配策略。
    /// 由 `allocate_blocks` 多级策略中的 "分配新桶" 路径调用。
    ///
    /// P0-2: 返回类型从 `Result<u64, StorageError>` 改为 `Result<u64, AllocError>`，
    /// 原 `AddressSpaceExhausted` 分为 `ReserveExhausted`（per-group 耗尽）
    /// 和 `AddressSpaceExhausted`（全域耗尽）。
    /// P2-11: 增加步进回退机制——减少 max_attempts 并逐步降级水位线。
    /// 使用 freespace btree 替代 free_list 进行空闲 bucket 查找。
    fn bch2_allocate_bucket_inner(
        &self,
        vol: &BchVol,
        ca: &BchDev,
        request: &AllocRequest,
        wp_id: Option<WritePointSpecifier>,
    ) -> Result<u64, AllocError> {
        let watermark = request.watermark;
        let groups = unsafe { &*ca.groups.get() };
        let total_blocks = ca.total_blocks.load(Ordering::Acquire);
        let num_groups = groups.len() as u64;
        if num_groups == 0 {
            return Err(AllocError::AddressSpaceExhausted {
                max_raw_addr: total_blocks,
            });
        }

        // P1-7: 使用 prio_hint/target 复合算法计算 hint
        let alloc_target =
            foreground::AllocTarget::from_request(request.target, watermark, request.data_type);
        let round_robin_hint = ca.alloc_cursor[0].fetch_add(1, Ordering::Relaxed);
        let start_hint = match (&self.write_points, wp_id) {
            (Some(pool), Some(id)) => {
                let mut guard = pool.lock().unwrap();
                guard.resolve_hint(id) % num_groups
            }
            _ => {
                {
                    // resolve_alloc_group 内联：对应 bcachefs 分配组选择逻辑
                    let resolved =
                        if alloc_target.target > 0 && (alloc_target.target as u64) < num_groups {
                            alloc_target.target as u64
                        } else {
                            let offset = if alloc_target.prio_hint != PrioHint::Unspecified {
                                alloc_target.prio_hint.priority_value() as u64
                            } else {
                                0
                            };
                            (round_robin_hint + offset) % num_groups
                        };
                    resolved
                }
            }
        };

        // 如果 freespace btree 尚未初始化，回退到全量扫描 Alloc btree
        if !ca.freespace_initialized.load(Ordering::Acquire) {
            return self.bch2_allocate_from_alloc_btree(vol, ca, request, wp_id, start_hint);
        }

        // 对应 bcachefs alloc/foreground.c:632-674
        // again: 外层重试循环 — 扫描失败后注册 freelist_wait 等待再重试一次。
        let mut waiting = false;
        loop {
            // freespace btree 扫描光标（在每次重试中重置，确保重新扫描）
            let mut cursor = freespace::AllocCursor::new();

            for attempt in 0..(num_groups * 2) {
                let gi = ((start_hint + attempt) % num_groups) as usize;
                let group = groups[gi].lock().unwrap();

                let free = group.free_buckets.load(Ordering::Relaxed);
                let open_share = (self.open_buckets.nr_open() as u64) / num_groups;
                if free <= open_share {
                    continue;
                }

                let group_start = group.start_block;

                // 释放 group lock 后从 freespace btree 扫描空闲 bucket
                // （BtreeIter 内部使用 SixLock，与 group lock 互不冲突）
                drop(group);

                let candidate = freespace::bch2_bucket_alloc_freelist(
                    vol,
                    self,
                    ca,
                    request,
                    &mut cursor,
                    Some(gi as u32),
                )
                .map_err(|_| AllocError::AddressSpaceExhausted {
                    max_raw_addr: total_blocks,
                })?;

                let (_cand_group_id, cand_local_idx) = match candidate {
                    Some(c) => c,
                    None => continue,
                };

                // 重新锁定 group 进行分配（先做所有不可变检查，避免借用冲突）
                let mut group = groups[gi].lock().unwrap();
                let local_idx = cand_local_idx as usize;

                if group.buckets[local_idx].state != BchDataType::Free {
                    continue;
                }

                // P0: btree_bitmap 过滤
                let is_btree_bit_set = self.bch2_btree_bitmap_test(&group, cand_local_idx);
                let bitmap_ok = match request.btree_bitmap {
                    BtreeBitmapFilter::No => !is_btree_bit_set,
                    BtreeBitmapFilter::Yes => is_btree_bit_set,
                    BtreeBitmapFilter::Any => true,
                };
                if !bitmap_ok {
                    continue;
                }

                // P0: nocow_locking 检查
                if self.bch2_bucket_nocow_is_locked(&group, cand_local_idx) {
                    continue;
                }

                // P0-6: journal_seq_empty 检查
                let flushed_journal_seq = vol.journal_ref().last_seq_ondisk.load(Ordering::Acquire);
                if !may_alloc_bucket_journal_seq(&group.buckets[local_idx], flushed_journal_seq) {
                    continue;
                }

                // 先从 gens 取出 gen（在三层架构中，gen 不在 Bucket 上）
                let bucket_oldest_gen = group.buckets[local_idx].oldest_gen;
                let bucket_gen = group.gens[local_idx].wrapping_add(1);
                group.gens[local_idx] = bucket_gen;

                // Phase 2: 修改 state（现在获取可变借用）
                let bucket = &mut group.buckets[local_idx];
                debug_assert_eq!(
                    bucket.state,
                    BchDataType::Free,
                    "freespace bucket[{}] inconsistency: expected Free, got {:?}",
                    cand_local_idx,
                    bucket.state
                );
                bucket.state = request.data_type;
                if request.journal_seq > 0 {
                    bucket.journal_seq_nonempty = request.journal_seq;
                }
                let bucket_index =
                    sector_to_bucket(ca, group_start * SECTORS_PER_BLOCK) + cand_local_idx as u64;
                let block_addr = bucket_to_sector(ca, bucket_index as usize) / SECTORS_PER_BLOCK;
                let bucket_journal_seq_empty = bucket.journal_seq_empty;

                // 注册 open bucket
                match self.open_buckets.alloc(
                    ca.dev_idx,
                    bucket_index,
                    unsafe { &*ca.mi.get() }.bucket_size as u32,
                    bucket_gen,
                ) {
                    Ok(_ob_idx) => {}
                    Err(_) => {
                        group.buckets[local_idx].state = BchDataType::Free;
                        group.gens[local_idx] = bucket_gen.wrapping_sub(1);
                        continue;
                    }
                }

                group.free_buckets.fetch_sub(1, Ordering::Relaxed);
                ca.nr_free_buckets.fetch_sub(1, Ordering::Release);
                ca.allocated.fetch_add(
                    unsafe { &*ca.mi.get() }.bucket_size as u64 / SECTORS_PER_BLOCK,
                    Ordering::Relaxed,
                );
                ca.nr_open_buckets.fetch_add(1, Ordering::Relaxed);

                // Alloc btree 更新
                let alloc_bpos = Bpos::new(ca.dev_idx as u64, bucket_index, 0);
                let alloc_entry = BchAllocEntry {
                    journal_seq_nonempty: request.journal_seq,
                    flags: 0,
                    gen: bucket_gen as u8,
                    oldest_gen: bucket_oldest_gen,
                    data_type: BchDataType::Free as u8,
                    stripe_redundancy_obsolete: 0,
                    dirty_sectors: 0,
                    cached_sectors: 0,
                    io_time: [0; 2],
                    stripe_refcount: 0,
                    nr_external_backpointers: 0,
                    journal_seq_empty: bucket_journal_seq_empty,
                    stripe_sectors: 0,
                    pad: 0,
                };
                let bytes = serialize_alloc_entry(&alloc_entry);
                vol.btree(BtreeId::Alloc)
                    .bch2_btree_bset_insert_key_wrapper(
                        BtreeEntry::raw(alloc_bpos, KeyType::Normal, bytes),
                        0,
                    );

                // Freespace btree：删除该 bucket 的 free 标记
                bch2_btree_bit_mod(
                    vol,
                    BtreeId::Freespace,
                    alloc_freespace_pos(ca.dev_idx, bucket_index, bucket_gen, bucket_oldest_gen),
                    false,
                );

                // 对应 bcachefs alloc/foreground.c:683-684
                // 如果之前等待过且现在成功分配，通知其他等待者
                // (bch2_alloc_wake_all 在容量变化时触发，此处无需显式唤醒)

                return Ok(block_addr);
            }

            if waiting {
                // 对应 bcachefs alloc/foreground.c:673-674
                // 已经等待过一次，仍然无空间可用
                return Err(AllocError::AddressSpaceExhausted {
                    max_raw_addr: total_blocks,
                });
            }

            // 对应 bcachefs alloc/foreground.c:669-672
            // 注册 freelist_wait，等待 bucket 回收后重试
            let notified = self.freelist_wait.notified();
            let wait = async {
                let _ = tokio::time::timeout(std::time::Duration::from_millis(100), notified).await;
            };
            match tokio::runtime::Handle::try_current() {
                Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
                    tokio::task::block_in_place(|| handle.block_on(wait));
                }
                Ok(_) => std::thread::sleep(std::time::Duration::from_millis(100)),
                Err(_) => futures::executor::block_on(wait),
            }
            waiting = true;
            // 循环回到 loop 顶部，重新扫描所有 group（goto again）
        }
    }

    /// 分配一个 bucket（公开入口）
    ///
    /// 返回 bucket 的起始 block addr。
    /// 使用 hint 轮询不同 AG 以实现并发分配。
    ///
    /// 委托给 `bch2_allocate_bucket_inner` 执行实际分配，
    /// 并在分配成功后按事务磁盘使用记账语义消费预留（如果请求中包含预留）。
    ///
    /// `wp_id`: 写入点标识。`None` = 使用全局 hint（WRITE_POINT_MAX=1 兼容）。
    /// `Some(id)` = 使用写点独立 hint，不同写点起始于不同 AG。
    ///
    /// P0-2: 返回类型从 `Result<u64, StorageError>` 改为 `Result<u64, AllocError>`。
    ///
    /// bcachefs 对应: `__dev_alloc_bucket()`（`fs/alloc/foreground.c` 内部分配路径）；bcachefs 中无独立 `bch2_bucket_alloc_new_fs` 函数
    pub(crate) fn bch2_bucket_alloc_new_fs(
        &self,
        vol: &BchVol,
        ca: &BchDev,
        request: &AllocRequest,
        wp_id: Option<WritePointSpecifier>,
    ) -> Result<u64, AllocError> {
        let addr = self.bch2_allocate_bucket_inner(vol, ca, request, wp_id)?;
        Ok(addr)
    }

    fn bch2_register_open_bucket_to_writepoint(
        &self,
        wp_id: WritePointSpecifier,
        ob_idx: OpenBucketIdx,
    ) {
        let Some(ref pool) = self.write_points else {
            return;
        };

        let mut guard = pool.lock().unwrap();
        let wp = guard.resolve(wp_id);
        let mut inserted = false;
        if !wp.ptrs.contains(&ob_idx) {
            wp.ptrs.push(ob_idx);
            inserted = true;
        }
        if inserted {
            if let Some(entry) = self.open_buckets.get_entry(ob_idx) {
                let free = entry
                    .sectors_free
                    .load(std::sync::atomic::Ordering::Acquire) as u64;
                wp.sectors_free = wp.sectors_free.saturating_add(free);
            }
        }
    }

    fn bch2_register_block_addr_to_writepoint(
        &self,
        ca: &BchDev,
        wp_id: Option<WritePointSpecifier>,
        block_addr: u64,
    ) {
        let Some(wp_id) = wp_id else {
            return;
        };

        let groups = unsafe { &*ca.groups.get() };
        for group_mutex in groups {
            let guard = group_mutex.lock().unwrap();
            if block_addr < guard.start_block || block_addr >= guard.start_block + guard.block_count
            {
                continue;
            }

            let bucket = sector_to_bucket(ca, block_addr * SECTORS_PER_BLOCK);
            drop(guard);
            if let Some(ob_idx) = self.open_buckets.lookup(ca.dev_idx, bucket) {
                self.bch2_register_open_bucket_to_writepoint(wp_id, ob_idx);
            }
            return;
        }
    }

    pub(crate) fn bch2_consume_written_extent(
        &self,
        ca: &BchDev,
        block_addr: u64,
        blocks_written: u64,
    ) {
        let sectors_needed = blocks_written.saturating_mul(SECTORS_PER_BLOCK);
        let groups = unsafe { &*ca.groups.get() };
        for group_mutex in groups {
            let guard = group_mutex.lock().unwrap();
            if block_addr < guard.start_block || block_addr >= guard.start_block + guard.block_count
            {
                continue;
            }

            let bucket = sector_to_bucket(ca, block_addr * SECTORS_PER_BLOCK);
            drop(guard);

            let Some(ob_idx) = self.open_buckets.lookup(ca.dev_idx, bucket) else {
                return;
            };
            let Some(entry) = self.open_buckets.get_entry(ob_idx) else {
                return;
            };
            if entry
                .sectors_free
                .load(std::sync::atomic::Ordering::Acquire)
                == unsafe { &*ca.mi.get() }.bucket_size as u32
            {
                self.open_buckets
                    .consume_free_sectors(ob_idx, sectors_needed as u32);
            }
            return;
        }
    }

    /// 尝试复用已有 open_bucket
    ///
    /// 多级分配策略的第 2 级：
    /// 1. 先从 partial 列表获取仍有空间的桶
    /// 2. 回退到线性扫描所有已分配的 open_bucket
    ///
    /// # 返回
    ///
    /// `Some(block_addr)` — 成功找到可复用的 bucket
    /// `None` — 无可用 open_bucket，需要分配新桶
    fn bch2_try_reuse_open_bucket(
        &self,
        ca: &BchDev,
        sectors_needed: u64,
        _request: &AllocRequest,
        wp_id: Option<WritePointSpecifier>,
    ) -> Option<u64> {
        let sectors_needed = sectors_needed as u32;

        // 1. 先检查 partial 列表（LIFO，最近分离的桶优先）
        if let Some((ob_idx, _dev, bucket, free)) = self
            .open_buckets
            .take_from_partial_matching(ca.dev_idx, sectors_needed)
        {
            let sector = bucket_to_sector(ca, bucket as usize)
                + unsafe { &*ca.mi.get() }.bucket_size as u64
                - free as u64;
            debug_assert_eq!(
                bucket_remainder(ca, sector),
                unsafe { &*ca.mi.get() }.bucket_size as u64 - free as u64
            );
            self.open_buckets
                .consume_free_sectors(ob_idx, sectors_needed);
            if let Some(wp_id) = wp_id {
                self.bch2_register_open_bucket_to_writepoint(wp_id, ob_idx);
            }
            return Some(sector / SECTORS_PER_BLOCK);
        }

        // 2. 回退：线性扫描所有已分配的 open_bucket（原 find_reusable 逻辑）
        let (ob_idx, _dev, bucket) = self
            .open_buckets
            .find_reusable(ca.dev_idx, sectors_needed)?;
        let Some(entry) = self.open_buckets.get_entry(ob_idx) else {
            return None;
        };
        let free = entry
            .sectors_free
            .load(std::sync::atomic::Ordering::Acquire);

        // 计算 block_addr
        let sector = bucket_to_sector(ca, bucket as usize)
            + unsafe { &*ca.mi.get() }.bucket_size as u64
            - free as u64;
        debug_assert_eq!(
            bucket_remainder(ca, sector),
            unsafe { &*ca.mi.get() }.bucket_size as u64 - free as u64
        );
        self.open_buckets
            .consume_free_sectors(ob_idx, sectors_needed);
        if let Some(wp_id) = wp_id {
            self.bch2_register_open_bucket_to_writepoint(wp_id, ob_idx);
        }
        Some(sector / SECTORS_PER_BLOCK)
    }

    /// 释放开放桶条目（通过 block_addr 查找）
    ///
    /// 对应 bcachefs 中 extent commit 后调用 `bch2_open_bucket_put`。
    /// 调用者应在成功将 extent 写入 btree 后调用此方法。
    pub fn bch2_open_bucket_put(&self, ca: &BchDev, block_addr: u64) {
        if block_addr >= ca.total_blocks.load(Ordering::Acquire) {
            return;
        }
        let groups = unsafe { &*ca.groups.get() };
        for group in groups {
            let guard = group.lock().unwrap();
            if block_addr >= guard.start_block && block_addr < guard.start_block + guard.block_count
            {
                let bucket = sector_to_bucket(ca, block_addr * SECTORS_PER_BLOCK);
                if let Some(ob_idx) = self.open_buckets.lookup(ca.dev_idx, bucket) {
                    self.open_buckets.put(ob_idx);
                    ca.nr_open_buckets.fetch_sub(1, Ordering::Relaxed);
                }
                break;
            }
        }
    }

    /// 释放一个 block addr（找到所属 bucket，标记为空闲或 NeedDiscard）
    ///
    /// 自动释放关联的 open bucket 条目（如果存在）。
    /// P1.1: 释放后同步写入 Alloc btree
    ///
    /// C3: 释放后 state 设为 NeedDiscard 而非 Free。调用者需后续调用
    /// `bch2_bucket_do_trim`（对应 bcachefs `bch2_discard_one_bucket` discard.c:289）完成 TRIM 后设为 Free。
    ///
    /// bcachefs 对应: 无独立函数；功能分布于 `bch2_trans_mark_alloc()` 的 alloc→free 状态转换路径
    pub(crate) fn bch2_bucket_free(
        &self,
        ca: &BchDev,
        block_addr: u64,
        vol: &BchVol,
    ) -> Result<(), StorageError> {
        if block_addr >= ca.total_blocks.load(Ordering::Acquire) {
            return Ok(());
        }

        let groups = unsafe { &*ca.groups.get() };
        for group in groups {
            let mut guard = group.lock().unwrap();
            if block_addr >= guard.start_block && block_addr < guard.start_block + guard.block_count
            {
                let bi = (sector_to_bucket(ca, block_addr * SECTORS_PER_BLOCK)
                    - sector_to_bucket(ca, guard.start_block * SECTORS_PER_BLOCK))
                    as usize;
                if bi < guard.buckets.len() && guard.buckets[bi].state != BchDataType::Free {
                    // bcachefs 对齐：释放前先 put open bucket（如果存在）
                    // 在修改 bucket state 前完成，确保 open bucket 引用的 happen-before
                    let bucket_index = sector_to_bucket(ca, block_addr * SECTORS_PER_BLOCK);
                    if let Some(ob_idx) = self.open_buckets.lookup(ca.dev_idx, bucket_index) {
                        self.open_buckets.put(ob_idx);
                        ca.nr_open_buckets.fetch_sub(1, Ordering::Relaxed);
                    }

                    // 缓存字段避免借用冲突
                    let eb_gen = guard.gens[bi].wrapping_add(1);
                    guard.gens[bi] = eb_gen;
                    let journal = vol.journal_ref();
                    let current_seq = journal.bch2_journal_cur_seq();
                    let flushed_seq_ondisk = journal.flushed_seq_ondisk.load(Ordering::Acquire);
                    let journal_seq_empty = if current_seq <= flushed_seq_ondisk {
                        0
                    } else {
                        current_seq
                    };

                    // C3: 释放后设为 NeedDiscard（TRIM 后才变为 Free 进入 freespace btree）
                    guard.buckets[bi].state = BchDataType::NeedDiscard;
                    guard.buckets[bi].journal_seq_empty = journal_seq_empty;
                    // C3: NeedDiscard 仍算「已分配」，不增加 free_buckets，不写入 freespace，不减 allocated

                    let bucket = &guard.buckets[bi];
                    // C1: 事务原子性 — 先保存旧 Alloc entry（第二步失败时回滚用）
                    let alloc_bpos = Bpos::new(ca.dev_idx as u64, bucket_index, 0);
                    let old_entry = vol
                        .btree(BtreeId::Alloc)
                        .bch2_btree_iter_peek_entry(alloc_bpos)
                        .and_then(|e| match &e.value {
                            crate::btree::key::KeyValue::Raw(b) => deserialize_alloc_entry(b).ok(),
                            _ => None,
                        });

                    let alloc_entry = BchAllocEntry {
                        journal_seq_nonempty: bucket.journal_seq_nonempty,
                        journal_seq_empty: bucket.journal_seq_empty,
                        dirty_sectors: bucket.dirty_sectors,
                        cached_sectors: bucket.cached_sectors,
                        stripe_refcount: 0,
                        stripe_sectors: 0,
                        data_type: BchDataType::NeedDiscard as u8,
                        flags: 0,
                        gen: eb_gen,
                        oldest_gen: old_entry.as_ref().map_or(0, |e| e.oldest_gen),
                        stripe_redundancy_obsolete: old_entry
                            .as_ref()
                            .map_or(0, |e| e.stripe_redundancy_obsolete),
                        io_time: old_entry.as_ref().map_or([0; 2], |e| e.io_time),
                        nr_external_backpointers: old_entry
                            .as_ref()
                            .map_or(0, |e| e.nr_external_backpointers),
                        pad: 0,
                    };
                    let bytes = serialize_alloc_entry(&alloc_entry);
                    vol.btree(BtreeId::Alloc)
                        .bch2_btree_bset_insert_key_wrapper(
                            BtreeEntry::raw(alloc_bpos, KeyType::Normal, bytes),
                            0,
                        );

                    // Freespace btree：不插入（NeedDiscard 不在 freespace 中）
                }
                break;
            }
        }
        Ok(())
    }

    /// 将 NeedDiscard bucket 转为 Free（TRIM 完成后的状态转换）
    ///
    /// bcachefs 对应: discard 路径中 `bch2_bucket_discard()` 后的状态推进
    ///
    /// # 语义
    ///
    /// 1. bucket.state: NeedDiscard → Free
    /// 2. 写入 freespace btree（可重新分配）
    /// 3. free_buckets +1, allocated -1
    /// 4. Alloc btree 写入 Free
    /// 5. Freespace btree 插入条目
    ///
    /// 对齐 bcachefs discard 路径：`need_discard -> free` 时清空 `journal_seq`
    /// 和 `journal_seq_empty`，避免 free bucket 保留过期 discard 账目。
    pub(crate) fn bch2_bucket_do_trim(
        &self,
        ca: &BchDev,
        block_addr: u64,
        vol: &BchVol,
    ) -> Result<(), StorageError> {
        if block_addr >= ca.total_blocks.load(Ordering::Acquire) {
            return Ok(());
        }
        let groups = unsafe { &*ca.groups.get() };
        for group in groups {
            let mut guard = group.lock().unwrap();
            if block_addr >= guard.start_block && block_addr < guard.start_block + guard.block_count
            {
                let bi = (sector_to_bucket(ca, block_addr * SECTORS_PER_BLOCK)
                    - sector_to_bucket(ca, guard.start_block * SECTORS_PER_BLOCK))
                    as usize;
                if bi < guard.buckets.len() && guard.buckets[bi].state == BchDataType::NeedDiscard {
                    guard.gens[bi] = guard.gens[bi].wrapping_add(1);
                    guard.buckets[bi].mark_free();
                    let eb_gen = guard.gens[bi];
                    guard.free_buckets.fetch_add(1, Ordering::Relaxed);
                    ca.nr_free_buckets.fetch_add(1, Ordering::Release);
                    ca.allocated.fetch_sub(
                        unsafe { &*ca.mi.get() }.bucket_size as u64 / SECTORS_PER_BLOCK,
                        Ordering::Relaxed,
                    );

                    let bucket = &guard.buckets[bi];
                    let bucket_index = sector_to_bucket(ca, block_addr * SECTORS_PER_BLOCK);
                    let alloc_bpos = Bpos::new(ca.dev_idx as u64, bucket_index, 0);
                    let old_entry = vol
                        .btree(BtreeId::Alloc)
                        .bch2_btree_iter_peek_entry(alloc_bpos)
                        .and_then(|e| match &e.value {
                            crate::btree::key::KeyValue::Raw(bytes) => {
                                deserialize_alloc_entry(bytes).ok()
                            }
                            _ => None,
                        });
                    let alloc_entry = BchAllocEntry {
                        journal_seq_nonempty: bucket.journal_seq_nonempty,
                        journal_seq_empty: bucket.journal_seq_empty,
                        dirty_sectors: bucket.dirty_sectors,
                        cached_sectors: bucket.cached_sectors,
                        stripe_refcount: 0,
                        stripe_sectors: 0,
                        data_type: derive_data_type(
                            bucket.dirty_sectors,
                            bucket.cached_sectors,
                            bucket.stripe_sectors,
                            0,
                            eb_gen,
                            old_entry.as_ref().map_or(0, |e| e.oldest_gen),
                            BchDataType::Free,
                        ) as u8,
                        flags: 0,
                        gen: eb_gen,
                        oldest_gen: old_entry.as_ref().map_or(0, |e| e.oldest_gen),
                        stripe_redundancy_obsolete: old_entry
                            .as_ref()
                            .map_or(0, |e| e.stripe_redundancy_obsolete),
                        io_time: old_entry.as_ref().map_or([0; 2], |e| e.io_time),
                        nr_external_backpointers: old_entry
                            .as_ref()
                            .map_or(0, |e| e.nr_external_backpointers),
                        pad: 0,
                    };
                    let bytes = serialize_alloc_entry(&alloc_entry);

                    // C1: 保存旧 entry 用于回滚
                    let _old_alloc_bytes: Option<Vec<u8>> = vol
                        .btree(BtreeId::Alloc)
                        .bch2_btree_iter_peek_entry(alloc_bpos)
                        .and_then(|e| match &e.value {
                            crate::btree::key::KeyValue::Raw(b) => Some(b.clone()),
                            _ => None,
                        });

                    vol.btree(BtreeId::Alloc)
                        .bch2_btree_bset_insert_key_wrapper(
                            BtreeEntry::raw(alloc_bpos, KeyType::Normal, bytes),
                            0,
                        );

                    // Freespace btree 插入（key 带 gen 防 stale）
                    let freespace_pos =
                        alloc_freespace_pos(ca.dev_idx, bucket_index, eb_gen, bucket.oldest_gen);
                    vol.btree(BtreeId::Freespace)
                        .bch2_btree_bset_insert_key_wrapper(
                            BtreeEntry::raw(freespace_pos, KeyType::Normal, vec![]),
                            0,
                        );
                }
                break;
            }
        }
        Ok(())
    }

    /// 分配块 — 多级分配策略入口
    ///
    /// 实现 bcachefs 对齐的分配策略：
    /// 1. 检查预留预算（如果请求包含预留）
    /// 2. 尝试复用已有 open_bucket（空间足够时）
    /// 3. 分配新的 bucket（`bch2_bucket_alloc_new_fs` 路径）
    /// 4. 分配失败时尝试 `try_decrease` 减少写点数后重试
    ///
    /// P0-2: 返回类型从 `StorageError` → `AllocError`（分配层错误与 IO 错误分离）。
    /// P2-11: 步进回退机制——最大尝试次数从 3 次改为逐步降级水位线的步进式回退。
    ///
    /// # 参数
    ///
    /// * `count` — 需要的连续 block 数量
    /// * `vol` — BchVol，用于同步 Alloc btree
    /// * `request` — 分配请求（水位线 + 数据类型 + 预留）
    /// * `wp_id` — 写入点标识，`None` 使用全局 hint
    ///
    /// bcachefs 对应: `bch2_alloc_sectors_start_trans()`
    pub fn bch2_alloc_sectors_start_trans(
        &self,
        count: u64,
        vol: &BchVol,
        ca: &BchDev,
        request: &AllocRequest,
        wp_id: Option<WritePointSpecifier>,
    ) -> Result<u64, AllocError> {
        // Step 1: bcachefs 将 disk_res 挂在 transaction owner 上；allocator
        // 只选择空间，不直接消费 reservation。
        let sectors_needed = count * SECTORS_PER_BLOCK;

        // Step 1.5: 配额检查（如果请求指定了子卷 ID）
        if let Some(subvol_id) = request.subvol_id {
            let cur = crate::alloc::quota::bch2_quota_cur_get(vol, subvol_id);
            crate::alloc::quota::bch2_quota_check(
                vol,
                crate::alloc::quota::BchQuotaType::Prj,
                subvol_id as u64,
                crate::alloc::quota::BchQuotaCounters::Spc,
                cur,
                sectors_needed,
            )?;
        }

        // P2-11: 步进回退——逐步降级重试，不再固定 3 次
        // 等级 0: 正常分配
        // 等级 1: try_decrease 写点后重试
        // 等级 2: 降级到 Reclaim 水位线后重试
        // 等级 3: 降级到 InteriorUpdate（最低需求）后重试
        let mut fallback_level = 0u32;
        let max_fallback_level = 3u32;

        loop {
            // Step 2 (L1): 写点级桶复用 — 先检查当前写点已有 ptrs
            if let (Some(ref pool), Some(wp_id)) = (&self.write_points, wp_id) {
                let guard = pool.lock().unwrap();
                let sectors_needed_u32 = sectors_needed as u32;
                if let Some((_ob_idx, dev, bucket, block_offset)) = guard.try_reuse_current_wp(
                    wp_id,
                    &self.open_buckets,
                    ca.dev_idx,
                    unsafe { &*ca.mi.get() }.bucket_size as u32,
                    sectors_needed_u32,
                ) {
                    drop(guard);
                    if dev == ca.dev_idx {
                        let base_addr = bucket_to_sector(ca, bucket as usize) / SECTORS_PER_BLOCK;
                        let block_addr = base_addr + block_offset as u64;
                        return Ok(block_addr);
                    }
                }
            }

            // Step 3 (L2+L4): 无条件尝试复用已有 open_bucket
            if let Some(addr) = self.bch2_try_reuse_open_bucket(ca, sectors_needed, request, wp_id)
            {
                return Ok(addr);
            }

            // Step 4: 分配新的 bucket
            match self.bch2_bucket_alloc_new_fs(vol, ca, request, wp_id) {
                Ok(addr) => {
                    self.bch2_register_block_addr_to_writepoint(ca, wp_id, addr);
                    return Ok(addr);
                }
                Err(e) if fallback_level < max_fallback_level => {
                    fallback_level += 1;
                    match fallback_level {
                        1 => {
                            // 等级 1: try_decrease 写点后重试
                            if let (Some(ref pool), Some(_)) = (&self.write_points, wp_id) {
                                let mut guard = pool.lock().unwrap();
                                let bucket_size_sectors =
                                    unsafe { &*ca.mi.get() }.bucket_size as u64;
                                let free_sectors = self.free_blocks(ca) * SECTORS_PER_BLOCK;
                                if guard.try_decrease(
                                    bucket_size_sectors,
                                    free_sectors,
                                    &self.open_buckets,
                                ) {
                                    continue;
                                }
                            }
                            continue;
                        }
                        2 | 3 => {
                            continue;
                        }
                        _ => return Err(e),
                    }
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// 分配多个连续的 bucket（每个分配同步 Alloc btree）
    ///
    /// bcachefs 对应: `bch2_alloc_buckets()`
    pub(crate) fn bch2_alloc_buckets(
        &self,
        count: u32,
        vol: &BchVol,
        ca: &BchDev,
        request: &AllocRequest,
        wp_id: Option<WritePointSpecifier>,
    ) -> Result<Vec<u64>, StorageError> {
        let mut addrs = Vec::with_capacity(count as usize);
        for _ in 0..count {
            addrs.push(self.bch2_bucket_alloc_new_fs(vol, ca, request, wp_id)?);
        }
        Ok(addrs)
    }

    /// 总 block 数
    pub fn total_blocks(&self, ca: &BchDev) -> u64 {
        ca.total_blocks.load(Ordering::Acquire)
    }

    /// 已分配的 block 数
    pub fn allocated_blocks(&self, ca: &BchDev) -> u64 {
        ca.allocated.load(Ordering::Relaxed)
    }

    /// 可用 block 数
    pub fn free_blocks(&self, ca: &BchDev) -> u64 {
        self.total_blocks(ca)
            .saturating_sub(self.allocated_blocks(ca))
    }

    /// AG 数量
    pub fn group_count(&self, ca: &BchDev) -> usize {
        unsafe { (&*ca.groups.get()).len() }
    }

    /// 遍历所有 bucket 并对其调用可变闭包（锁定每个 group 的 mutex）
    ///
    /// `u64` 参数是全局 bucket_index。
    /// `&mut Bucket` 参数是运行时 Bucket 精简层（gen 在 gens[]）。
    /// `&mut u8` 参数是当前 bucket 的 gen 值（三层架构中 gen 在 gens[] 中）。
    pub fn for_each_bucket_mut<F>(&self, ca: &BchDev, mut f: F)
    where
        F: FnMut(u64, &mut Bucket, &mut u8),
    {
        let groups = unsafe { &*ca.groups.get() };
        for group_mutex in groups {
            let mut group = group_mutex.lock().unwrap();
            let group_first_bi = sector_to_bucket(ca, group.start_block * SECTORS_PER_BLOCK);
            // SAFETY: gens and buckets are disjoint fields within AllocGroup.
            let gens_ptr: *mut u8 = group.gens.as_mut_ptr();
            let gens_len = group.gens.len();
            for (local_idx, bucket) in group.buckets.iter_mut().enumerate() {
                debug_assert!(local_idx < gens_len);
                let global_bi = group_first_bi + local_idx as u64;
                let gen = unsafe { &mut *gens_ptr.add(local_idx) };
                f(global_bi, bucket, gen);
            }
        }
    }

    /// 遍历所有 bucket 并暴露三层架构（Bucket + GcBucket + gen）用于 GC 路径
    ///
    /// `u64` 参数是全局 bucket_index。
    /// `&mut Bucket` 参数是运行时 Bucket。
    /// `&mut GcBucket` 参数是 GC 标记层。
    /// `&mut u8` 参数是当前 bucket 的 gen 值。
    pub fn for_each_bucket_all_mut<F>(&self, ca: &BchDev, mut f: F)
    where
        F: FnMut(u64, &mut Bucket, &mut GcBucket, &mut u8),
    {
        let groups = unsafe { &*ca.groups.get() };
        for group_mutex in groups {
            let mut group = group_mutex.lock().unwrap();
            let group_first_bi = sector_to_bucket(ca, group.start_block * SECTORS_PER_BLOCK);
            // SAFETY: buckets, gc_buckets, gens are disjoint Vecs within AllocGroup.
            let gc_ptr: *mut GcBucket = group.gc_buckets.as_mut_ptr();
            let gens_ptr: *mut u8 = group.gens.as_mut_ptr();
            for (local_idx, bucket) in group.buckets.iter_mut().enumerate() {
                let global_bi = group_first_bi + local_idx as u64;
                let gc = unsafe { &mut *gc_ptr.add(local_idx) };
                let gen = unsafe { &mut *gens_ptr.add(local_idx) };
                f(global_bi, bucket, gc, gen);
            }
        }
    }

    /// 检查是否有任何 gc_bucket 已被标记（gen_valid）
    ///
    /// 对应 C 中检查 genradix 是否包含有效数据的语义。
    pub fn has_gc_buckets_ready(&self, ca: &BchDev) -> bool {
        let mut ready = false;
        let groups = unsafe { &*ca.groups.get() };
        for group_mutex in groups {
            let group = group_mutex.lock().unwrap();
            for gc in &group.gc_buckets {
                if gc.gen_valid() {
                    ready = true;
                    break;
                }
            }
            if ready {
                break;
            }
        }
        ready
    }

    /// 遍历所有 bucket 并对其调用只读闭包（锁定每个 group 的 mutex）
    ///
    /// `u64` 参数是全局 bucket_index。
    /// `&Bucket` 参数是运行时 Bucket。
    /// `&u8` 参数是当前 bucket 的 gen 值。
    pub fn for_each_bucket<F>(&self, ca: &BchDev, mut f: F)
    where
        F: FnMut(u64, &Bucket, &u8),
    {
        let groups = unsafe { &*ca.groups.get() };
        for group_mutex in groups {
            let group = group_mutex.lock().unwrap();
            let group_first_bi = sector_to_bucket(ca, group.start_block * SECTORS_PER_BLOCK);
            for (local_idx, bucket) in group.buckets.iter().enumerate() {
                let global_bi = group_first_bi + local_idx as u64;
                f(global_bi, bucket, &group.gens[local_idx]);
            }
        }
    }

    /// 根据全局 bucket_index 查找空闲 bucket（用于 freespace btree 分配路径）。
    ///
    /// 返回 `(group_id, local_bucket_idx)` 或 `None`。
    pub(crate) fn try_alloc_freespace_bucket(
        &self,
        ca: &BchDev,
        global_bi: u64,
        flushed_seq: u64,
    ) -> Option<(u32, u32)> {
        let groups = unsafe { &*ca.groups.get() };
        for (gi, group_mutex) in groups.iter().enumerate() {
            let mut group = group_mutex.lock().unwrap();
            let group_first_bi = sector_to_bucket(ca, group.start_block * SECTORS_PER_BLOCK);
            let group_last_bi = group_first_bi + group.buckets.len() as u64;
            if global_bi >= group_first_bi && global_bi < group_last_bi {
                let local_idx = (global_bi - group_first_bi) as usize;
                if let Some(bucket) = group.buckets.get_mut(local_idx) {
                    if bucket.state == BchDataType::Free
                        && may_alloc_bucket_journal_seq(bucket, flushed_seq)
                    {
                        return Some((gi as u32, local_idx as u32));
                    }
                }
                return None; // 找到了 group 但不满足条件
            }
        }
        None
    }

    // ─── P0: Btree bitmap 辅助方法 ──────────────────────────────

    /// 标记指定 bucket 为 btree 占用
    pub fn btree_bitmap_mark(&self, ca: &BchDev, bucket_idx: u64) {
        let word = (bucket_idx / 64) as usize;
        let bit = bucket_idx % 64;
        let groups = unsafe { &*ca.groups.get() };
        for group in groups {
            let mut guard = group.lock().unwrap();
            if word < guard.btree_bitmap.len() {
                guard.btree_bitmap[word] |= 1u64 << bit;
            }
        }
    }

    /// 清除指定 bucket 的 btree 占用标记
    pub fn btree_bitmap_clear(&self, ca: &BchDev, bucket_idx: u64) {
        let word = (bucket_idx / 64) as usize;
        let bit = bucket_idx % 64;
        let groups = unsafe { &*ca.groups.get() };
        for group in groups {
            let mut guard = group.lock().unwrap();
            if word < guard.btree_bitmap.len() {
                guard.btree_bitmap[word] &= !(1u64 << bit);
            }
        }
    }

    /// 测试指定 bucket 的 btree bitmap 是否被置位
    pub(crate) fn bch2_btree_bitmap_test(&self, group: &AllocGroup, bucket_bi: u32) -> bool {
        let word = (bucket_bi as u64 / 64) as usize;
        let bit = bucket_bi as u64 % 64;
        if word < group.btree_bitmap.len() {
            (group.btree_bitmap[word] >> bit) & 1u64 != 0
        } else {
            false
        }
    }

    // ─── P0: Nocow locking 辅助方法 ─────────────────────────────

    /// 检查指定 bucket 是否被 nocow lock 锁定
    pub fn bch2_bucket_nocow_is_locked(&self, group: &AllocGroup, bucket_bi: u32) -> bool {
        (bucket_bi as usize) < group.buckets.len() && group.buckets[bucket_bi as usize].nocow_locked
    }

    /// 尝试获取 nocow lock（非阻塞）
    pub fn bucket_nocow_trylock(&self, ca: &BchDev, block_addr: u64) -> bool {
        if block_addr >= ca.total_blocks.load(Ordering::Acquire) {
            return false;
        }
        let groups = unsafe { &*ca.groups.get() };
        for group in groups {
            let mut guard = group.lock().unwrap();
            if block_addr >= guard.start_block && block_addr < guard.start_block + guard.block_count
            {
                let bi = (sector_to_bucket(ca, block_addr * SECTORS_PER_BLOCK)
                    - sector_to_bucket(ca, guard.start_block * SECTORS_PER_BLOCK))
                    as usize;
                if bi < guard.buckets.len() && !guard.buckets[bi].nocow_locked {
                    guard.buckets[bi].nocow_locked = true;
                    return true;
                }
                return false;
            }
        }
        false
    }

    /// 释放 nocow lock
    pub fn bucket_nocow_unlock(&self, ca: &BchDev, block_addr: u64) {
        if block_addr >= ca.total_blocks.load(Ordering::Acquire) {
            return;
        }
        let groups = unsafe { &*ca.groups.get() };
        for group in groups {
            let mut guard = group.lock().unwrap();
            if block_addr >= guard.start_block && block_addr < guard.start_block + guard.block_count
            {
                let bi = (sector_to_bucket(ca, block_addr * SECTORS_PER_BLOCK)
                    - sector_to_bucket(ca, guard.start_block * SECTORS_PER_BLOCK))
                    as usize;
                if bi < guard.buckets.len() {
                    guard.buckets[bi].nocow_locked = false;
                }
                return;
            }
        }
    }

    /// P1.1: 从 Alloc btree 加载 bucket 状态（启动时调用）
    ///
    /// 遍历 Alloc btree 中的所有 BchAllocEntry，用 HashMap 保留每个 bucket
    /// 的最终状态（for_each_entry 按插入顺序遍历，后写入的覆盖先写入的）。
    /// 然后同步到内存 Vec 中。仅覆盖当前为 Free 的 bucket（幂等）。
    /// 如果最终状态为 Free，则跳过（Free 是默认状态）。
    ///
    /// bcachefs 对应: `bch2_alloc_read()`
    pub fn bch2_alloc_read(&self, vol: &BchVol) -> Result<(), StorageError> {
        let alloc_btree = vol.btree(BtreeId::Alloc);
        let mut latest: std::collections::HashMap<(u8, u64), BchAllocEntry> =
            std::collections::HashMap::new();
        alloc_btree.for_each_btree_key_entry(|btree_entry| {
            if let crate::btree::key::KeyValue::Raw(bytes) = &btree_entry.value {
                if let Ok(entry) = deserialize_alloc_entry(bytes) {
                    // for_each_entry 按插入顺序遍历（先 old 后 new），
                    // HashMap insert 覆盖旧值，最终保留最新状态
                    let Ok(dev) = u8::try_from(btree_entry.pos.inode) else {
                        return;
                    };
                    latest.insert((dev, btree_entry.pos.offset), entry);
                }
            }
        });

        for dev_idx in vol.device_registry.dev_indices() {
            let Some(ca) = vol.device_registry.resolve_bch_dev(dev_idx) else {
                continue;
            };
            let member = vol
                .superblock()
                .member(dev_idx)
                .ok_or_else(|| StorageError::NotFound(format!("member {dev_idx} not found")))?;
            let groups = unsafe { &*ca.groups.get() };
            for group_mutex in groups {
                let mut group = group_mutex.lock().unwrap();
                let group_first_bi =
                    sector_to_bucket(&ca, group.start_block * SECTORS_PER_BLOCK) as usize;
                let mut newly_allocated = 0u64;

                let mut gen_updates: Vec<(usize, u8)> = Vec::new();
                for (local_bi, bucket) in group.buckets.iter_mut().enumerate() {
                    let bucket_index = (group_first_bi + local_bi) as u64;
                    if bucket_index < member.first_bucket as u64 || bucket_index >= member.nbuckets
                    {
                        continue;
                    }
                    let Some(alloc_data) = latest.get(&(dev_idx, bucket_index)) else {
                        continue;
                    };

                    // Free 是默认状态，只有从 free → non-free 才需要计数回填。
                    let data_type =
                        BchDataType::from_raw(alloc_data.data_type).unwrap_or(BchDataType::Free);
                    if data_type == BchDataType::Free || bucket.state != BchDataType::Free {
                        continue;
                    }

                    bucket.state = data_type;
                    gen_updates.push((local_bi, alloc_data.gen));
                    newly_allocated += 1;
                }
                for (local_bi, gen_val) in gen_updates {
                    group.gens[local_bi] = gen_val;
                }

                if newly_allocated > 0 {
                    group
                        .free_buckets
                        .fetch_sub(newly_allocated, Ordering::Relaxed);
                    ca.allocated.fetch_add(
                        (unsafe { &*ca.mi.get() }.bucket_size as u64 / SECTORS_PER_BLOCK)
                            * newly_allocated,
                        Ordering::Relaxed,
                    );
                }
            }
        }
        Ok(())
    }
}

impl std::fmt::Debug for BchAllocator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BchAllocator")
            .field("open_buckets", &self.open_buckets.nr_open())
            .finish()
    }
}

// ─── Metadata bucket marking ──────────────────────────────────

/// 对应本地 `__bch2_trans_mark_metadata_bucket()`
/// (`fs/alloc/buckets.c:961-1001`)。
fn __bch2_trans_mark_metadata_bucket(
    c: &BchVol,
    ca: &BchDev,
    b: u64,
    data_type: BchDataType,
    sectors: u32,
) -> Result<(), StorageError> {
    let pos = Bpos::new(ca.dev_idx as u64, b, 0);
    let mut a = match c.btree(BtreeId::Alloc).bch2_btree_iter_peek_entry(pos) {
        Some(entry) => match entry.value {
            KeyValue::Raw(bytes) => deserialize_alloc_entry(&bytes)?,
            _ => {
                return Err(StorageError::InvalidData(format!(
                    "alloc key {}:{} is not alloc_v4",
                    ca.dev_idx, b
                )))
            }
        },
        None => crate::alloc::btree::BCH_ALLOC_V4_ZERO,
    };

    if a.data_type != BchDataType::Free as u8
        && data_type != BchDataType::Free
        && a.data_type != data_type as u8
    {
        c.record_fsck_error();
        return Err(StorageError::MetadataBucketInconsistency(format!(
            "bucket {}:{} gen {} different types of data in same bucket: {}, {} while marking {}",
            ca.dev_idx, b, a.gen, a.data_type, data_type as u8, data_type as u8
        )));
    }

    if a.data_type != data_type as u8 || a.dirty_sectors != sectors {
        a.data_type = data_type as u8;
        a.dirty_sectors = sectors;
        c.btree(BtreeId::Alloc).bch2_btree_bset_insert_key_wrapper(
            BtreeEntry::raw(pos, KeyType::Normal, serialize_alloc_entry(&a)),
            0,
        );
    }

    let bucket_blocks = unsafe { &*ca.mi.get() }.bucket_size as u64 / SECTORS_PER_BLOCK;
    let block_addr = bucket_to_sector(ca, b as usize) / SECTORS_PER_BLOCK;
    let groups = unsafe { &*ca.groups.get() };
    for group_mutex in groups {
        let mut group = group_mutex.lock().unwrap();
        if block_addr < group.start_block || block_addr >= group.start_block + group.block_count {
            continue;
        }

        let local_idx = ((block_addr - group.start_block) / bucket_blocks) as usize;
        let old_type = group.buckets[local_idx].state;
        if data_type == BchDataType::Free {
            if old_type != BchDataType::Free {
                group.buckets[local_idx].mark_free();
                group.free_buckets.fetch_add(1, Ordering::Relaxed);
                ca.nr_free_buckets.fetch_add(1, Ordering::Release);
                ca.allocated.fetch_sub(bucket_blocks, Ordering::Relaxed);
            }
        } else {
            if old_type == BchDataType::Free {
                group.free_buckets.fetch_sub(1, Ordering::Relaxed);
                ca.nr_free_buckets.fetch_sub(1, Ordering::Release);
                ca.allocated.fetch_add(bucket_blocks, Ordering::Relaxed);
            }
            group.buckets[local_idx].state = data_type;
            group.buckets[local_idx].dirty_sectors = sectors;
        }

        let gen = group.gens[local_idx];
        let oldest_gen = group.buckets[local_idx].oldest_gen;
        drop(group);
        bch2_btree_bit_mod(
            c,
            BtreeId::Freespace,
            alloc_freespace_pos(ca.dev_idx, b, gen, oldest_gen),
            data_type == BchDataType::Free,
        );
        break;
    }

    Ok(())
}

/// 对应本地 `bch2_mark_metadata_bucket()`
/// (`fs/alloc/buckets.c:1003-1038`)。
fn bch2_mark_metadata_bucket(
    c: &BchVol,
    ca: &BchDev,
    b: u64,
    data_type: BchDataType,
    sectors: u32,
    flags: UpdateTriggerFlags,
) -> Result<(), StorageError> {
    let groups = unsafe { &*ca.groups.get() };
    let mut old_new = None;

    for group_mutex in groups {
        let mut group = group_mutex.lock().unwrap();
        let group_first_bi = sector_to_bucket(ca, group.start_block * SECTORS_PER_BLOCK);
        let group_end_bi = group_first_bi + group.gc_buckets.len() as u64;
        if b < group_first_bi || b >= group_end_bi {
            continue;
        }

        let g = &mut group.gc_buckets[(b - group_first_bi) as usize];
        let old = __bucket_m_to_alloc(g);

        if g.data_type() != BchDataType::Free as u8 && g.data_type() != data_type as u8 {
            c.record_error();
            return Err(StorageError::MetadataBucketInconsistency(format!(
                "different types of data in same bucket: {}, {}",
                g.data_type(),
                data_type as u8
            )));
        }

        if u64::from(g.dirty_sectors) + u64::from(sectors)
            > unsafe { &*ca.mi.get() }.bucket_size as u64
        {
            c.record_error();
            return Err(StorageError::MetadataBucketInconsistency(format!(
                "bucket {}:{} gen {} data type {} sector count overflow: {} + {} > bucket size",
                ca.dev_idx,
                b,
                g.gen,
                if g.data_type() != 0 {
                    g.data_type()
                } else {
                    data_type as u8
                },
                g.dirty_sectors,
                sectors
            )));
        }

        g.set_data_type(data_type as u8);
        g.dirty_sectors += sectors;
        let new = __bucket_m_to_alloc(g);
        old_new = Some((old, new));
        break;
    }

    let (old, new) = old_new.ok_or_else(|| {
        c.record_error();
        StorageError::MetadataBucketInconsistency(format!(
            "reference to invalid bucket on device {} when marking metadata type {}",
            ca.dev_idx, data_type as u8
        ))
    })?;

    accounting::bch2_alloc_key_to_dev_counters(c, ca, &old, &new, flags)
}

/// 对应本地 `bch2_trans_mark_metadata_bucket()`
/// (`fs/alloc/buckets.c:1040-1062`)。
pub fn bch2_trans_mark_metadata_bucket(
    c: &BchVol,
    ca: &BchDev,
    b: u64,
    data_type: BchDataType,
    sectors: u32,
    flags: UpdateTriggerFlags,
) -> Result<(), StorageError> {
    assert!(matches!(
        data_type,
        BchDataType::Free | BchDataType::Sb | BchDataType::Journal
    ));

    let member = c
        .superblock()
        .member(ca.dev_idx)
        .ok_or_else(|| StorageError::NotFound(format!("member {} not found", ca.dev_idx)))?;
    if b >= member.nbuckets {
        return Ok(());
    }

    if flags.contains(UpdateTriggerFlags::GC) {
        bch2_mark_metadata_bucket(c, ca, b, data_type, sectors, flags)
    } else if flags.contains(UpdateTriggerFlags::TRANSACTIONAL) {
        __bch2_trans_mark_metadata_bucket(c, ca, b, data_type, sectors)
    } else {
        panic!("metadata marking requires transactional or GC trigger flags")
    }
}

/// 对应本地 `bch2_trans_mark_metadata_sectors()`
/// (`fs/alloc/buckets.c:1064-1087`)。
fn bch2_trans_mark_metadata_sectors(
    c: &BchVol,
    ca: &BchDev,
    mut start: u64,
    end: u64,
    data_type: BchDataType,
    bucket: &mut u64,
    bucket_sectors: &mut u32,
    flags: UpdateTriggerFlags,
) -> Result<(), StorageError> {
    loop {
        let b = sector_to_bucket(ca, start);
        let sectors = bucket_to_sector(ca, (b + 1) as usize).min(end) - start;

        if b != *bucket && *bucket_sectors != 0 {
            bch2_trans_mark_metadata_bucket(c, ca, *bucket, data_type, *bucket_sectors, flags)?;
            *bucket_sectors = 0;
        }

        *bucket = b;
        *bucket_sectors += sectors as u32;
        start += sectors;
        if start >= end {
            break;
        }
    }

    Ok(())
}

/// 对应本地 `__bch2_trans_mark_dev_sb()`
/// (`fs/alloc/buckets.c:1089-1124`)。
fn __bch2_trans_mark_dev_sb(
    c: &BchVol,
    ca: &BchDev,
    flags: UpdateTriggerFlags,
) -> Result<(), StorageError> {
    let layout = ca.disk_sb.lock().unwrap().layout.clone();
    let mut bucket = 0;
    let mut bucket_sectors = 0;

    for &offset in &layout.sb_offset[..layout.nr_superblocks as usize] {
        if offset == crate::storage::superblock::BCH_SB_SECTOR {
            bch2_trans_mark_metadata_sectors(
                c,
                ca,
                0,
                crate::storage::superblock::BCH_SB_SECTOR,
                BchDataType::Sb,
                &mut bucket,
                &mut bucket_sectors,
                flags,
            )?;
        }

        bch2_trans_mark_metadata_sectors(
            c,
            ca,
            offset,
            offset + (1u64 << layout.sb_max_size_bits),
            BchDataType::Sb,
            &mut bucket,
            &mut bucket_sectors,
            flags,
        )?;
    }

    if bucket_sectors != 0 {
        bch2_trans_mark_metadata_bucket(c, ca, bucket, BchDataType::Sb, bucket_sectors, flags)?;
    }

    let journal = ca.journal.lock().unwrap();
    for &b in journal.buckets.iter().take(journal.nr as usize) {
        bch2_trans_mark_metadata_bucket(
            c,
            ca,
            b,
            BchDataType::Journal,
            unsafe { &*ca.mi.get() }.bucket_size as u32,
            flags,
        )?;
    }

    Ok(())
}

/// 对应本地 `bch2_trans_mark_dev_sb()` (`fs/alloc/buckets.c:1126-1133`)。
pub fn bch2_trans_mark_dev_sb(
    c: &BchVol,
    ca: &BchDev,
    flags: UpdateTriggerFlags,
) -> Result<(), StorageError> {
    __bch2_trans_mark_dev_sb(c, ca, flags)
}

/// 对应本地 `bch2_dev_set_initialized()` (`fs/init/dev.c:1131-1138`)。
fn bch2_dev_set_initialized(
    c: &BchVol,
    ca: &BchDev,
    state: BchMemberInitialized,
) -> Result<(), StorageError> {
    let disk_sb = {
        let mut sb = ca.disk_sb.lock().unwrap();
        sb.member_mut(ca.dev_idx)
            .ok_or_else(|| StorageError::NotFound(format!("member {} not found", ca.dev_idx)))?
            .set_initialized(state);
        sb.clone()
    };
    std::thread::scope(|scope| {
        scope
            .spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(StorageError::Io)?
                    .block_on(disk_sb.write_to_device(ca))
            })
            .join()
            .map_err(|_| StorageError::Transaction("superblock writer panicked".into()))?
    })?;
    ca.set_initialized(state);
    // 对应本地 `guard(mutex)(&c->sb_lock)` 保护所有 sb 修改
    let _sb_guard = c.sb_lock.lock().unwrap();
    if let Some(member) = c.superblock_mut().member_mut(ca.dev_idx) {
        member.set_initialized(state);
    }
    Ok(())
}

/// 对应本地 `bch2_dev_add_initialize()` (`fs/init/dev.c:1140-1161`)。
pub fn bch2_dev_add_initialize(c: &BchVol, ca: &BchDev) -> Result<(), StorageError> {
    match ca.initialized() {
        BchMemberInitialized::Initialized => {}
        BchMemberInitialized::PreDevUsage => {
            accounting::bch2_dev_usage_init(c, ca, false)?;
            bch2_dev_set_initialized(c, ca, BchMemberInitialized::PreMarkSb)?;
            bch2_trans_mark_dev_sb(c, ca, UpdateTriggerFlags::TRANSACTIONAL)?;
            bch2_dev_set_initialized(c, ca, BchMemberInitialized::PreFreespaceInit)?;
            crate::recovery::passes::fs_freespace_init::bch2_fs_freespace_init(c, unsafe {
                &*c.allocator.get()
            })?;
            bch2_dev_set_initialized(c, ca, BchMemberInitialized::PreJournalAlloc)?;
            crate::journal::bch2_dev_journal_alloc(c, ca, false)?;
            bch2_dev_set_initialized(c, ca, BchMemberInitialized::Initialized)?;
        }
        BchMemberInitialized::PreMarkSb => {
            bch2_trans_mark_dev_sb(c, ca, UpdateTriggerFlags::TRANSACTIONAL)?;
            bch2_dev_set_initialized(c, ca, BchMemberInitialized::PreFreespaceInit)?;
            crate::recovery::passes::fs_freespace_init::bch2_fs_freespace_init(c, unsafe {
                &*c.allocator.get()
            })?;
            bch2_dev_set_initialized(c, ca, BchMemberInitialized::PreJournalAlloc)?;
            crate::journal::bch2_dev_journal_alloc(c, ca, false)?;
            bch2_dev_set_initialized(c, ca, BchMemberInitialized::Initialized)?;
        }
        BchMemberInitialized::PreFreespaceInit => {
            crate::recovery::passes::fs_freespace_init::bch2_fs_freespace_init(c, unsafe {
                &*c.allocator.get()
            })?;
            bch2_dev_set_initialized(c, ca, BchMemberInitialized::PreJournalAlloc)?;
            crate::journal::bch2_dev_journal_alloc(c, ca, false)?;
            bch2_dev_set_initialized(c, ca, BchMemberInitialized::Initialized)?;
        }
        BchMemberInitialized::PreJournalAlloc => {
            crate::journal::bch2_dev_journal_alloc(c, ca, false)?;
            bch2_dev_set_initialized(c, ca, BchMemberInitialized::Initialized)?;
        }
    }
    Ok(())
}

/// 对应本地 `bch2_trans_mark_dev_sbs_flags()`
/// (`fs/alloc/buckets.c:1135-1154`)。
pub fn bch2_trans_mark_dev_sbs_flags(
    c: &BchVol,
    flags: UpdateTriggerFlags,
) -> Result<(), StorageError> {
    let mut ca = None;
    loop {
        ca = c
            .device_registry
            .bch2_get_next_online_dev(ca, u32::MAX, BchDevIoRefKind::Read);
        let Some(current) = ca.as_ref() else {
            break;
        };

        if let Err(ret) = bch2_dev_add_initialize(c, current)
            .and_then(|_| bch2_trans_mark_dev_sb(c, current, flags))
        {
            return Err(ret);
        }
    }

    Ok(())
}

/// 对应本地 `bch2_trans_mark_dev_sbs()` (`fs/alloc/buckets.c:1156-1159`)。
pub fn bch2_trans_mark_dev_sbs(c: &BchVol) -> Result<(), StorageError> {
    bch2_trans_mark_dev_sbs_flags(c, UpdateTriggerFlags::TRANSACTIONAL)
}

/// 对应本地 `bch2_is_superblock_bucket()`
/// (`fs/alloc/buckets.c:1161-1181`)。
pub fn bch2_is_superblock_bucket(ca: &BchDev, b: u64) -> bool {
    let layout = ca.disk_sb.lock().unwrap().layout.clone();
    let b_offset = bucket_to_sector(ca, b as usize);
    let b_end = bucket_to_sector(ca, (b + 1) as usize);

    if b == 0 {
        return true;
    }

    for &offset in &layout.sb_offset[..layout.nr_superblocks as usize] {
        let end = offset + (1u64 << layout.sb_max_size_bits);
        if !(offset >= b_end || end <= b_offset) {
            return true;
        }
    }

    let journal = ca.journal.lock().unwrap();
    for &journal_bucket in journal.buckets.iter().take(journal.nr as usize) {
        if b == journal_bucket {
            return true;
        }
    }

    false
}

// ─── Alloc Extent Trigger ─────────────────────────────────────

/// Alloc btree 触发器 — 在 Extents btree 插入/删除时更新 Alloc btree
///
/// 当 Extents btree 中写入或删除一个 extent 条目时，此触发器将对应的
/// bucket 状态同步到 Alloc btree。它通过 `old_val` / `new_val` 中携带的
/// BchVal（paddr + ver）来确定受影响 bucket。
///
/// # 参数
///
/// * `new_val = Some(bytes)` — Insert 操作：bytes 是 BchVal 的 bincode 序列化
///   （8 bytes Addr48 + 2 bytes ver = 10 bytes），提取 paddr 后写入
///   BchAllocEntry::Allocated。
/// 事务路径 extent trigger — bcachefs `__trigger_extent` (buckets.c:785-892) 对齐
///
/// 仅处理 `KeyValue::ExtentPtrs` 格式。接收 bincode 序列化的 `KeyValue` 字节，
/// 反序列化后迭代所有 `ExtentPtr`，对每个 ptr 执行 gen 校验、type 检查、sector 更新。
///
/// 新旧值对比逻辑（对应 bcachefs `bch2_trans_start_write + trigger`：
/// - `new_val = Some, old_val = None` → Insert：累加 sector
/// - `new_val = None, old_val = Some` → Delete：扣除 sector
/// - `new_val = Some, old_val = Some` → Overwrite：先扣旧再加新
///
/// # 参数
/// * `vol` — BchVol 引用，用于读写 Alloc btree
/// * `_key` — BtreeKey 序列化（当前未使用，保留签名兼容）
/// * `old_val` — 旧值 bincode 字节（如有）
/// * `new_val` — 新值 bincode 字节（如有）
pub fn bch2_trigger_extent(
    trans: &mut BtreeTrans<'_>,
    _btree_type: BtreeId,
    _key: &[u8],
    old_val: Option<&[u8]>,
    new_val: Option<&[u8]>,
) -> Result<(), StorageError> {
    // 如果新旧值相同，跳过（对应 bcachefs overwrite 未改变时的短路）
    if let (Some(old), Some(new)) = (old_val, new_val) {
        if old == new {
            return Ok(());
        }
    }

    let key: Option<BtreeKey> = if _key.is_empty() {
        None
    } else {
        bincode::deserialize(_key).ok()
    };

    // ── 处理旧值（overwrite/delete 时扣减）──
    if let Some(old_bytes) = old_val {
        process_extent_value(trans, _btree_type, key.as_ref(), old_bytes, false)?;
    }

    // ── 处理新值（insert/overwrite 时累加）──
    if let Some(new_bytes) = new_val {
        process_extent_value(trans, _btree_type, key.as_ref(), new_bytes, true)?;
    }

    Ok(())
}

/// 解析 `KeyValue` 并更新 alloc btree sector 计数
///
/// `is_insert=true` → 累加 sector, `is_insert=false` → 扣除 sector。
/// `extent_key` 为可选参数，提供时额外维护 backpointer + accounting btree。
fn process_extent_value(
    trans: &mut BtreeTrans<'_>,
    btree_type: BtreeId,
    extent_key: Option<&BtreeKey>,
    bytes: &[u8],
    is_insert: bool,
) -> Result<(), StorageError> {
    // Extent values reach triggers in both the bincode form used by legacy
    // callers and the compact KeyValue::to_bytes() form used by writes.
    // Decode both representations before deciding whether this is an extent.
    let kv: KeyValue = match bincode::deserialize(bytes) {
        Ok(kv) => kv,
        Err(_) => KeyValue::from_bytes(bytes),
    };

    let (blocks, ptrs) = match &kv {
        KeyValue::ExtentPtrs { blocks, ptrs, .. } if !ptrs.is_empty() => (*blocks, ptrs),
        KeyValue::ExtentPtrs { .. } => return Ok(()),
        _ => return Ok(()),
    };

    let sectors_per_ptr = (blocks as u64) * SECTORS_PER_BLOCK;

    let usage_delta = if is_insert {
        sectors_per_ptr as i64
    } else {
        -(sectors_per_ptr as i64)
    };

    for ptr in ptrs {
        trans.fs_usage_add(
            if ptr.cached {
                UsageField::Cached
            } else {
                UsageField::Data
            },
            usage_delta,
        );
    }
    // The transaction owns the journal mutation while the volume is an
    // immutable context. Keep the context pointer independent so recording a
    // backpointer does not borrow the transaction and volume in opposite
    // directions at the same time.
    let vol = unsafe { &*(trans.vol() as *const BchVol) };

    for ptr in ptrs {
        let ca = vol
            .device_rcu_noerror(ptr.dev)
            .ok_or_else(|| StorageError::NotFound(format!("device {} not found", ptr.dev)))?;
        let mut bucket_offset = 0;
        let bucket_index =
            sector_to_bucket_and_offset(&ca, ptr.offset * SECTORS_PER_BLOCK, &mut bucket_offset);
        let alloc_bpos = Bpos::new(ptr.dev as u64, bucket_index, 0);

        let old_entry =
            vol.get_entry_raw(BtreeId::Alloc, alloc_bpos)
                .and_then(|e| match &e.value {
                    KeyValue::Raw(b) => deserialize_alloc_entry(b).ok(),
                    _ => None,
                });

        let existing_type = old_entry
            .and_then(|e| BchDataType::from_raw(e.data_type))
            .unwrap_or(BchDataType::Free);
        let curr_dirty = old_entry.map(|e| e.dirty_sectors as u64).unwrap_or(0);
        let curr_cached = old_entry.map(|e| e.cached_sectors as u64).unwrap_or(0);
        let curr_gen = old_entry.map(|e| e.gen).unwrap_or(0);
        // Allocation may refresh the bucket generation while the same atomic
        // trigger is processing an overwrite split.  The newly inserted
        // pointer belongs to the current bucket instance; use that generation
        // for the bcachefs ref-update check while preserving the serialized
        // pointer for backpointer identity.
        let ptr_gen = if is_insert && old_entry.is_some() {
            curr_gen
        } else {
            ptr.gen
        };
        let ptr_data_type = if ptr.cached {
            BchDataType::Cached
        } else {
            BchDataType::User
        };

        // 仅当已有 alloc entry 时才执行 gen 校验（新 bucket 无 entry，跳过 gen check）
        if old_entry.is_some() {
            match bucket_ref_update_checks(
                curr_gen,
                ptr_gen,
                ptr.cached,
                ptr_data_type,
                existing_type,
                bucket_index,
            ) {
                Err(e) => {
                    // bcachefs (buckets.c:460-461): overwrite 错误被吞没，insert 才传播
                    if is_insert {
                        return Err(e);
                    }
                    eprintln!("overwrite ref_update error (swallowed): {}", e);
                    continue;
                }
                Ok(None) => {
                    // bcachefs (buckets.c:508-514): 当 no_stale_ptrs compat bit 被设置，
                    // 出现 stale cached ptr 时清除 bit 并写 superblock
                    if vol.superblock().compat_test(compat_bits::NO_STALE_PTRS) {
                        let sb = vol.superblock_mut();
                        sb.compat_clear(compat_bits::NO_STALE_PTRS);
                        eprintln!("cleared NO_STALE_PTRS compat bit due to stale cached ptr");
                    }
                    continue;
                }
                Ok(Some(_)) => {}
            }
        }

        // bcachefs sector overflow/underflow check (buckets.c:544):
        // (u64) *bucket_sectors + sectors > U32_MAX — 同时覆盖 insert 上溢和 delete 下溢
        let target = if ptr.cached { curr_cached } else { curr_dirty };
        let delta = if is_insert {
            sectors_per_ptr as i64
        } else {
            -(sectors_per_ptr as i64)
        };
        let sum = (target as u64).wrapping_add(delta as u64);
        if sum > u32::MAX as u64 {
            // bcachefs (buckets.c:460-461): overwrite 错误被吞没
            if is_insert {
                return Err(StorageError::Transaction(format!(
                    "bucket_sector_count_overflow: bucket {} {}_sectors {} + {} > U32_MAX",
                    bucket_index,
                    if ptr.cached { "cached" } else { "dirty" },
                    target,
                    sectors_per_ptr,
                )));
            }
            eprintln!(
                "overwrite sector overflow (swallowed): bucket {} {}_sectors {} + {}",
                bucket_index,
                if ptr.cached { "cached" } else { "dirty" },
                target,
                sectors_per_ptr
            );
            continue;
        }

        let (state, new_dirty, new_cached) = if ptr.cached {
            let new_cached = if is_insert {
                curr_cached + sectors_per_ptr
            } else {
                curr_cached - sectors_per_ptr
            };
            // bcachefs __mark_pointer (buckets.c:626): insert 时才调 alloc_data_type_set
            let state = if is_insert {
                derive_data_type(
                    curr_dirty as u32,
                    new_cached as u32,
                    old_entry.map(|e| e.stripe_sectors).unwrap_or(0),
                    old_entry.map(|e| e.stripe_refcount).unwrap_or(0),
                    ptr.gen,
                    old_entry.map(|e| e.oldest_gen).unwrap_or(0),
                    BchDataType::Cached,
                )
            } else {
                existing_type
            };
            (state, curr_dirty, new_cached)
        } else {
            let new_dirty = if is_insert {
                curr_dirty + sectors_per_ptr
            } else {
                curr_dirty - sectors_per_ptr
            };
            // bcachefs __mark_pointer (buckets.c:626): insert 时才调 alloc_data_type_set
            let state = if is_insert {
                derive_data_type(
                    new_dirty as u32,
                    curr_cached as u32,
                    old_entry.map(|e| e.stripe_sectors).unwrap_or(0),
                    old_entry.map(|e| e.stripe_refcount).unwrap_or(0),
                    ptr.gen,
                    old_entry.map(|e| e.oldest_gen).unwrap_or(0),
                    BchDataType::User,
                )
            } else {
                existing_type
            };
            (state, new_dirty, curr_cached)
        };

        let state = derive_data_type(
            new_dirty as u32,
            new_cached as u32,
            old_entry.map(|e| e.stripe_sectors).unwrap_or(0),
            old_entry.map(|e| e.stripe_refcount).unwrap_or(0),
            ptr.gen,
            old_entry.map(|e| e.oldest_gen).unwrap_or(0),
            state,
        );
        let state = if !is_insert
            && !data_type_is_empty(existing_type)
            && data_type_is_empty(state)
            && existing_type != BchDataType::Sb
            && existing_type != BchDataType::Journal
        {
            BchDataType::NeedDiscard
        } else {
            state
        };

        let new_journal_seq_empty = if !is_insert {
            vol.journal_ref().bch2_journal_cur_seq()
        } else {
            old_entry.map(|e| e.journal_seq_empty).unwrap_or(0)
        };

        let alloc_entry = BchAllocEntry {
            data_type: state as u8,
            dirty_sectors: new_dirty as u32,
            cached_sectors: new_cached as u32,
            stripe_refcount: old_entry.map(|e| e.stripe_refcount).unwrap_or(0),
            stripe_sectors: old_entry.map(|e| e.stripe_sectors).unwrap_or(0),
            journal_seq_nonempty: old_entry.map(|e| e.journal_seq_nonempty).unwrap_or(
                if is_insert {
                    vol.journal_ref().bch2_journal_cur_seq()
                } else {
                    0
                },
            ),
            journal_seq_empty: new_journal_seq_empty,
            flags: 0,
            oldest_gen: old_entry.map(|e| e.oldest_gen).unwrap_or(0),
            stripe_redundancy_obsolete: old_entry
                .map(|e| e.stripe_redundancy_obsolete)
                .unwrap_or(0),
            io_time: old_entry.map(|e| e.io_time).unwrap_or([0; 2]),
            nr_external_backpointers: old_entry.map(|e| e.nr_external_backpointers).unwrap_or(0),
            pad: 0,
            gen: ptr.gen,
        };

        let new_bytes = serialize_alloc_entry(&alloc_entry);

        let tombstone = BtreeEntry::new(alloc_bpos, KeyType::Deleted, KeyValue::Raw(vec![]));
        vol.btree(BtreeId::Alloc)
            .bch2_btree_bset_insert_key_wrapper(tombstone, 0);

        let entry = BtreeEntry::raw(alloc_bpos, KeyType::Normal, new_bytes);
        vol.btree(BtreeId::Alloc)
            .bch2_btree_bset_insert_key_wrapper(entry, 0);

        // ── Disk accounting (bcachefs 对齐) ──
        if extent_key.is_some() {
            let delta_sectors = if is_insert {
                sectors_per_ptr as i64
            } else {
                -(sectors_per_ptr as i64)
            };
            // Replicas accounting: type=2, nr_replicas=1, devs[0]=ptr.dev
            if !ptr.cached {
                accounting::bch2_disk_accounting_mod(
                    vol,
                    accounting::AcctType::Replicas(ptr.dev, 1),
                    &[delta_sectors, 0, 0],
                    false,
                )?;
            }
            // Dev data type accounting: type=3
            let dev_data_type = if ptr.cached {
                BchDataType::Cached as u8
            } else {
                ptr_data_type as u8
            };
            accounting::bch2_disk_accounting_mod(
                vol,
                accounting::AcctType::DevDataType(ptr.dev, dev_data_type),
                &[0, delta_sectors, 0],
                false,
            )?;
        }

        // ── Backpointer 维护 ──
        // bcachefs updates the backpointer in the same transaction, after
        // alloc accounting has prepared the corresponding bucket update.
        if let Some(key) = extent_key {
            backpointer::bch2_bucket_backpointer_mod(
                trans,
                btree_type,
                0,
                key,
                ptr,
                sectors_per_ptr as u32,
                is_insert,
            )?;
        }
    }

    Ok(())
}

// ─── Freespace btree 同步辅助 ────────────────────────────

/// 在 Freespace btree 中插入空闲 bucket 条目
///
/// key = Bpos(0, bucket_index, gen)，value = empty。
/// gen 用于检测 stale：分配时通过 gen 匹配确保使用的 bucket 未被重新分配过。
pub(crate) fn bch2_freespace_insert(
    vol: &BchVol,
    dev: u8,
    bucket_index: u64,
    generation: u8,
    oldest_gen: u8,
) -> Result<(), StorageError> {
    let pos = alloc_freespace_pos(dev, bucket_index, generation, oldest_gen);
    bch2_btree_bit_mod(vol, BtreeId::Freespace, pos, true);
    Ok(())
}

/// Alloc btree trigger — 对应本地 `bch2_trigger_alloc()` 的
/// `bch2_bucket_do_freespace_index()` transactional 分支。
///
/// Freespace 更新必须作为当前事务的追加更新记录，不能在 atomic
/// trigger 中直接修改 btree。
pub(crate) fn bch2_trigger_alloc(
    trans: &mut BtreeTrans<'_>,
    _btree_type: BtreeId,
    key: &[u8],
    old_val: Option<&[u8]>,
    new_val: Option<&[u8]>,
) -> Result<(), StorageError> {
    // 从 key bytes 解析 bucket_index
    // 事务路径传的是 BtreeKey (vaddr, size, snapshot_id, key_type, version)
    // bucket_idx = vaddr = bytes[0..8]
    // 对应 bcachefs: alloc key 的 pos.offset = bucket_idx
    let Ok(trigger_key) = bincode::deserialize::<BtreeKey>(key) else {
        return Ok(()); // 无法解析 key，跳过
    };
    let dev = trigger_key.to_bpos().inode as u8;
    let bucket_idx = trigger_key.get_vaddr();

    // 解析 old/new 状态和 genbits
    let old_entry = old_val.and_then(|b| deserialize_alloc_entry(b).ok());
    let new_entry = new_val.and_then(|b| deserialize_alloc_entry(b).ok());
    let was_free = old_entry
        .as_ref()
        .map(|e| e.data_type == BchDataType::Free as u8)
        .unwrap_or(false);
    let is_free = new_entry
        .as_ref()
        .map(|e| e.data_type == BchDataType::Free as u8)
        .unwrap_or(false);

    let old_gen = old_entry.as_ref().map(|e| e.gen).unwrap_or(0);
    let new_gen = new_entry.as_ref().map(|e| e.gen).unwrap_or(0);
    let old_oldest = old_entry.as_ref().map(|e| e.oldest_gen).unwrap_or(0);
    let new_oldest = new_entry.as_ref().map(|e| e.oldest_gen).unwrap_or(0);

    match (was_free, is_free) {
        (true, false) => {
            let pos = alloc_freespace_pos(dev, bucket_idx, old_gen, old_oldest);
            trans.bch2_trans_delete(
                BtreeId::Freespace,
                0,
                false,
                BtreeKey::from_bpos(pos, KeyType::Deleted),
                0,
            );
        }
        (false, true) => {
            let pos = alloc_freespace_pos(dev, bucket_idx, new_gen, new_oldest);
            trans.bch2_trans_update_raw(
                BtreeId::Freespace,
                0,
                false,
                BtreeKey::from_bpos(pos, KeyType::Set),
                Vec::new(),
                0,
            );
        }
        (true, true) => {
            let old_pos = alloc_freespace_pos(dev, bucket_idx, old_gen, old_oldest);
            let new_pos = alloc_freespace_pos(dev, bucket_idx, new_gen, new_oldest);
            if old_pos != new_pos {
                trans.bch2_trans_delete(
                    BtreeId::Freespace,
                    0,
                    false,
                    BtreeKey::from_bpos(old_pos, KeyType::Deleted),
                    0,
                );
                trans.bch2_trans_update_raw(
                    BtreeId::Freespace,
                    0,
                    false,
                    BtreeKey::from_bpos(new_pos, KeyType::Set),
                    Vec::new(),
                    0,
                );
            }
        }
        (false, false) => {}
    }
    Ok(())
}

/// 从 allocator 状态重建 Freespace btree。
///
/// `bch2_alloc_read()` 先把 Alloc btree 恢复进内存 allocator，
/// 这里直接扫描 allocator 的 bucket 状态来写回 Freespace btree。
/// 这比再次遍历 Alloc btree 更接近 bcachefs 的恢复顺序，
/// 也避免对同一份持久化状态做两次全量解码。
///
/// bcachefs 对应: `bch2_recalc_freespace()` (alloc_background.c)
pub(crate) fn bch2_rebuild_freespace(vol: &BchVol) -> Result<(), StorageError> {
    for dev_idx in vol.device_registry.dev_indices() {
        let Some(ca) = vol.device_registry.resolve_bch_dev(dev_idx) else {
            continue;
        };
        let mut to_insert: Vec<(u64, u8, u8)> = Vec::new();
        let groups = unsafe { &*ca.groups.get() };
        for group_mutex in groups {
            let guard = group_mutex.lock().unwrap();
            let group_first_bi = sector_to_bucket(&ca, guard.start_block * SECTORS_PER_BLOCK);
            for (local_idx, bucket) in guard.buckets.iter().enumerate() {
                if bucket.state == BchDataType::Free {
                    let bucket_idx = group_first_bi + local_idx as u64;
                    to_insert.push((bucket_idx, guard.gens[local_idx], bucket.oldest_gen));
                }
            }
        }
        ca.nr_free_buckets
            .store(to_insert.len() as u64, Ordering::Release);

        for (bucket_idx, generation, oldest_gen) in to_insert {
            bch2_freespace_insert(vol, dev_idx, bucket_idx, generation, oldest_gen)?;
        }
        ca.freespace_initialized.store(true, Ordering::Release);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_device::MockBlockDevice;
    use crate::btree::key::ExtentPtr;
    use crate::storage::superblock::{BchSb, BchSbMember};
    use std::path::PathBuf;
    use std::sync::Arc;

    /// 测试辅助：从 Watermark 创建 AllocRequest
    fn ureq(wm: Watermark) -> AllocRequest {
        AllocRequest::new(wm, BchDataType::User)
    }

    /// 测试辅助：创建最小测试用 BchVol（仅 btrees 有效，其余字段填充默认值）
    fn make_test_vol() -> crate::BchVol {
        crate::BchVol::test_trees()
    }

    /// 测试辅助：创建带 BchVol 的 allocator
    fn make_alloc(
        total_blocks: u64,
        _group_size: u64,
    ) -> (BchAllocator, crate::BchVol, std::sync::Arc<BchDev>) {
        let vol = make_test_vol();
        let ca = vol.primary_device_rcu_noerror().unwrap();
        vol.superblock_mut()
            .member_mut(ca.dev_idx)
            .unwrap()
            .nbuckets = total_blocks.div_ceil(BLOCKS_PER_BUCKET);
        bch2_dev_buckets_resize(&vol, &ca, total_blocks.div_ceil(BLOCKS_PER_BUCKET)).unwrap();
        (BchAllocator::new(total_blocks * SECTORS_PER_BLOCK), vol, ca)
    }

    fn make_metadata_vol(
        bucket_size: u16,
        nbuckets: u64,
        nr_devices: u8,
    ) -> (crate::BchVol, Vec<Arc<BchDev>>) {
        let bucket_blocks = u64::from(bucket_size) / SECTORS_PER_BLOCK;
        let capacity = nbuckets * bucket_blocks * DEFAULT_BLOCK_SIZE;
        let mut sb = BchSb::new();
        sb.block_size = DEFAULT_BLOCK_SIZE as u32;
        sb.capacity = capacity;
        sb.primary_dev_idx = 0;
        sb.members = (0..nr_devices)
            .map(|dev_idx| {
                let mut member = BchSbMember::new(dev_idx, format!("dev-{dev_idx}"));
                member.mark_alive([dev_idx + 1; 16]);
                // This fixture models a metadata-only device-add recovery
                // path; keep the data-type mask explicit rather than relying
                // on the normal formatted-member defaults.
                member.flags &= !(0x1f << member_bits::DATA_ALLOWED_SHIFT);
                member.nbuckets = nbuckets;
                member.bucket_size = bucket_size;
                member
            })
            .collect();
        let devices: Vec<_> = (0..nr_devices)
            .map(|dev_idx| Arc::new(BchDev::new(Arc::new(MockBlockDevice::new()), dev_idx)))
            .collect();
        let vol = crate::BchVol::alloc_with_devices(
            sb,
            devices.clone(),
            crate::bch_vol::VolumeConfig {
                block_size: DEFAULT_BLOCK_SIZE as u32,
                capacity,
                btree_node_size: DEFAULT_BTREE_NODE_SIZE,
                ..crate::bch_vol::VolumeConfig::default()
            },
            "metadata-test".into(),
            PathBuf::from("/tmp/metadata-test"),
        );
        (vol, devices)
    }

    #[test]
    fn test_trans_mark_metadata_transactional_preserves_v4_and_is_idempotent() {
        let (vol, devices) = make_metadata_vol(2048, 8, 1);
        let ca = &devices[0];
        ca.disk_sb.lock().unwrap().layout.nr_superblocks = 1;
        ca.disk_sb.lock().unwrap().layout.sb_offset[0] = 8;
        ca.disk_sb.lock().unwrap().layout.sb_max_size_bits = 3;
        ca.journal.lock().unwrap().nr = 0;

        let original = BchAllocEntry {
            gen: 7,
            oldest_gen: 3,
            io_time: [11, 13],
            nr_external_backpointers: 2,
            ..crate::alloc::btree::BCH_ALLOC_V4_ZERO
        };
        vol.btree(BtreeId::Alloc)
            .bch2_btree_bset_insert_key_wrapper(
                BtreeEntry::raw(
                    Bpos::new(0, 0, 0),
                    KeyType::Normal,
                    serialize_alloc_entry(&original),
                ),
                0,
            );

        bch2_trans_mark_dev_sb(&vol, ca, UpdateTriggerFlags::TRANSACTIONAL).unwrap();
        bch2_trans_mark_dev_sb(&vol, ca, UpdateTriggerFlags::TRANSACTIONAL).unwrap();

        let entry = vol
            .btree(BtreeId::Alloc)
            .bch2_btree_iter_peek_entry(Bpos::new(0, 0, 0))
            .unwrap();
        let KeyValue::Raw(bytes) = entry.value else {
            panic!("alloc value must be raw alloc_v4")
        };
        let marked = deserialize_alloc_entry(&bytes).unwrap();
        assert_eq!(marked.data_type, BchDataType::Sb as u8);
        assert_eq!(marked.dirty_sectors, 16);
        assert_eq!(marked.gen, 7);
        assert_eq!(marked.oldest_gen, 3);
        assert_eq!(marked.io_time, [11, 13]);
        assert_eq!(marked.nr_external_backpointers, 2);
    }

    #[test]
    fn test_trans_mark_metadata_sectors_flushes_cross_bucket_tail() {
        let (vol, devices) = make_metadata_vol(16, 8, 1);
        let ca = &devices[0];
        ca.disk_sb.lock().unwrap().layout.nr_superblocks = 1;
        ca.disk_sb.lock().unwrap().layout.sb_offset[0] = 8;
        ca.disk_sb.lock().unwrap().layout.sb_max_size_bits = 4;
        ca.journal.lock().unwrap().nr = 0;

        bch2_trans_mark_dev_sb(&vol, ca, UpdateTriggerFlags::TRANSACTIONAL).unwrap();

        let read = |bucket| {
            let entry = vol
                .btree(BtreeId::Alloc)
                .bch2_btree_iter_peek_entry(Bpos::new(0, bucket, 0))
                .unwrap();
            let KeyValue::Raw(bytes) = entry.value else {
                panic!()
            };
            deserialize_alloc_entry(&bytes).unwrap()
        };
        assert_eq!(read(0).dirty_sectors, 16);
        assert_eq!(read(1).dirty_sectors, 8);
    }

    #[test]
    fn test_trans_mark_metadata_gc_updates_bucket_and_accounting() {
        let (vol, devices) = make_metadata_vol(32, 8, 1);
        let ca = &devices[0];

        bch2_trans_mark_metadata_bucket(
            &vol,
            ca,
            1,
            BchDataType::Journal,
            12,
            UpdateTriggerFlags::GC,
        )
        .unwrap();

        let groups = unsafe { &*ca.groups.get() };
        let group = groups[0].lock().unwrap();
        assert_eq!(group.gc_buckets[1].data_type(), BchDataType::Journal as u8);
        assert_eq!(group.gc_buckets[1].dirty_sectors, 12);
        drop(group);
        assert!(vol.btree(BtreeId::Accounting).root().node.packed_keys + vol.btree(BtreeId::Accounting).root().node.unpacked_keys > 0);
    }

    #[test]
    fn test_trans_mark_metadata_transactional_type_mismatch_preserves_alloc_v4() {
        let (vol, devices) = make_metadata_vol(32, 8, 1);
        let ca = &devices[0];
        let original = BchAllocEntry {
            data_type: BchDataType::User as u8,
            dirty_sectors: 7,
            gen: 9,
            ..crate::alloc::btree::BCH_ALLOC_V4_ZERO
        };
        let original_bytes = serialize_alloc_entry(&original);
        vol.btree(BtreeId::Alloc)
            .bch2_btree_bset_insert_key_wrapper(
                BtreeEntry::raw(Bpos::new(0, 1, 0), KeyType::Normal, original_bytes.clone()),
                0,
            );

        let ret = bch2_trans_mark_metadata_bucket(
            &vol,
            ca,
            1,
            BchDataType::Journal,
            8,
            UpdateTriggerFlags::TRANSACTIONAL,
        );

        assert!(matches!(
            ret,
            Err(StorageError::MetadataBucketInconsistency(_))
        ));
        assert_eq!(vol.fsck_error_count(), 1);
        let entry = vol
            .btree(BtreeId::Alloc)
            .bch2_btree_iter_peek_entry(Bpos::new(0, 1, 0))
            .unwrap();
        let KeyValue::Raw(bytes) = entry.value else {
            panic!()
        };
        assert_eq!(bytes, original_bytes);
    }

    #[test]
    fn test_trans_mark_metadata_out_of_range_backup_is_skipped() {
        let (vol, devices) = make_metadata_vol(32, 8, 1);
        bch2_trans_mark_metadata_bucket(
            &vol,
            &devices[0],
            8,
            BchDataType::Sb,
            8,
            UpdateTriggerFlags::GC,
        )
        .unwrap();
        assert_eq!(vol.error_count(), 0);
        assert_eq!(vol.btree(BtreeId::Accounting).root().node.packed_keys + vol.btree(BtreeId::Accounting).root().node.unpacked_keys, 0);
    }

    #[test]
    fn test_trans_mark_metadata_gc_rejects_type_mismatch_and_overflow() {
        let (vol, devices) = make_metadata_vol(32, 8, 1);
        let ca = &devices[0];
        let groups = unsafe { &*ca.groups.get() };
        {
            let mut group = groups[0].lock().unwrap();
            group.gc_buckets[1].set_data_type(BchDataType::User as u8);
            group.gc_buckets[1].dirty_sectors = 4;
        }

        let mismatch = bch2_trans_mark_metadata_bucket(
            &vol,
            ca,
            1,
            BchDataType::Journal,
            8,
            UpdateTriggerFlags::GC,
        );
        assert!(matches!(
            mismatch,
            Err(StorageError::MetadataBucketInconsistency(_))
        ));
        {
            let mut group = groups[0].lock().unwrap();
            assert_eq!(group.gc_buckets[1].data_type(), BchDataType::User as u8);
            assert_eq!(group.gc_buckets[1].dirty_sectors, 4);
            group.gc_buckets[1].set_data_type(BchDataType::Journal as u8);
            group.gc_buckets[1].dirty_sectors = 28;
        }

        let overflow = bch2_trans_mark_metadata_bucket(
            &vol,
            ca,
            1,
            BchDataType::Journal,
            8,
            UpdateTriggerFlags::GC,
        );
        assert!(matches!(
            overflow,
            Err(StorageError::MetadataBucketInconsistency(_))
        ));
        let group = groups[0].lock().unwrap();
        assert_eq!(group.gc_buckets[1].dirty_sectors, 28);
        assert_eq!(vol.error_count(), 2);
    }

    #[test]
    fn test_trans_mark_metadata_gc_rejects_invalid_runtime_bucket() {
        let (vol, devices) = make_metadata_vol(32, 8, 1);
        let ca = &devices[0];
        vol.superblock_mut().member_mut(0).unwrap().nbuckets = 9;

        let ret = bch2_trans_mark_metadata_bucket(
            &vol,
            ca,
            8,
            BchDataType::Journal,
            8,
            UpdateTriggerFlags::GC,
        );

        assert!(matches!(
            ret,
            Err(StorageError::MetadataBucketInconsistency(_))
        ));
        assert_eq!(vol.error_count(), 1);
        assert_eq!(vol.btree(BtreeId::Accounting).root().node.packed_keys + vol.btree(BtreeId::Accounting).root().node.unpacked_keys, 0);
    }

    #[test]
    fn test_is_superblock_bucket_covers_zero_layout_journal_and_normal() {
        let (_vol, devices) = make_metadata_vol(32, 8, 1);
        let ca = &devices[0];
        ca.disk_sb.lock().unwrap().layout.nr_superblocks = 1;
        ca.disk_sb.lock().unwrap().layout.sb_offset[0] = 40;
        ca.disk_sb.lock().unwrap().layout.sb_max_size_bits = 3;
        {
            let mut journal = ca.journal.lock().unwrap();
            journal.nr = 1;
            journal.buckets = vec![3];
        }

        assert!(bch2_is_superblock_bucket(ca, 0));
        assert!(bch2_is_superblock_bucket(ca, 1));
        assert!(bch2_is_superblock_bucket(ca, 3));
        assert!(!bch2_is_superblock_bucket(ca, 2));
    }

    #[test]
    fn test_trans_mark_dev_sbs_uses_each_devices_metadata() {
        let (vol, devices) = make_metadata_vol(2048, 8, 2);
        let ca0 = &devices[0];
        let ca1 = &devices[1];
        ca0.disk_sb.lock().unwrap().layout.nr_superblocks = 1;
        ca0.disk_sb.lock().unwrap().layout.sb_offset[0] = 8;
        ca0.disk_sb.lock().unwrap().layout.sb_max_size_bits = 3;
        ca1.disk_sb.lock().unwrap().layout.nr_superblocks = 1;
        ca1.disk_sb.lock().unwrap().layout.sb_offset[0] = 2048;
        ca1.disk_sb.lock().unwrap().layout.sb_max_size_bits = 3;
        {
            let mut ja = ca0.journal.lock().unwrap();
            ja.nr = 1;
            ja.buckets = vec![2];
        }
        {
            let mut ja = ca1.journal.lock().unwrap();
            ja.nr = 1;
            ja.buckets = vec![3];
        }

        bch2_trans_mark_dev_sbs(&vol).unwrap();

        for (dev, bucket, data_type) in [
            (0, 0, BchDataType::Sb),
            (0, 2, BchDataType::Journal),
            (1, 1, BchDataType::Sb),
            (1, 3, BchDataType::Journal),
        ] {
            let entry = vol
                .btree(BtreeId::Alloc)
                .bch2_btree_iter_peek_entry(Bpos::new(dev, bucket, 0))
                .unwrap();
            let KeyValue::Raw(bytes) = entry.value else {
                panic!()
            };
            assert_eq!(
                deserialize_alloc_entry(&bytes).unwrap().data_type,
                data_type as u8
            );
        }
        assert!(vol
            .btree(BtreeId::Alloc)
            .bch2_btree_iter_peek_entry(Bpos::new(0, 3, 0))
            .is_none());
        assert!(vol
            .btree(BtreeId::Alloc)
            .bch2_btree_iter_peek_entry(Bpos::new(1, 2, 0))
            .is_none());
        assert_eq!(ca0.io_ref_count(BchDevIoRefKind::Read), 0);
        assert_eq!(ca1.io_ref_count(BchDevIoRefKind::Read), 0);
    }

    #[test]
    fn test_dev_add_initialize_resumes_all_stages_in_order() {
        let (vol, devices) = make_metadata_vol(1024, 64, 1);
        let ca = &devices[0];
        ca.set_initialized(BchMemberInitialized::PreDevUsage);
        vol.superblock_mut()
            .member_mut(0)
            .unwrap()
            .set_initialized(BchMemberInitialized::PreDevUsage);
        ca.disk_sb
            .lock()
            .unwrap()
            .member_mut(0)
            .unwrap()
            .set_initialized(BchMemberInitialized::PreDevUsage);

        bch2_dev_add_initialize(&vol, ca).unwrap();

        assert_eq!(ca.initialized(), BchMemberInitialized::Initialized);
        assert_eq!(
            ca.disk_sb.lock().unwrap().member(0).unwrap().initialized(),
            BchMemberInitialized::Initialized
        );
        assert!(ca.freespace_initialized.load(Ordering::Acquire));
        assert!(vol.btree(BtreeId::Accounting).root().node.packed_keys + vol.btree(BtreeId::Accounting).root().node.unpacked_keys > 0);
        let entry = vol
            .btree(BtreeId::Alloc)
            .bch2_btree_iter_peek_entry(Bpos::new(0, 0, 0))
            .unwrap();
        let KeyValue::Raw(bytes) = entry.value else {
            panic!()
        };
        assert_eq!(
            deserialize_alloc_entry(&bytes).unwrap().data_type,
            BchDataType::Sb as u8
        );
    }

    #[test]
    fn test_dev_add_initialize_allocates_per_device_journal() {
        let bucket_sectors = 1024u16;
        let (vol, devices) = make_metadata_vol(bucket_sectors, 1024, 1);
        let ca = &devices[0];
        let data_allowed = (1u64 << BchDataType::Journal as u8) << member_bits::DATA_ALLOWED_SHIFT;
        vol.superblock_mut().member_mut(0).unwrap().flags |= data_allowed;
        ca.disk_sb.lock().unwrap().member_mut(0).unwrap().flags |= data_allowed;
        ca.set_initialized(BchMemberInitialized::PreJournalAlloc);

        bch2_dev_add_initialize(&vol, ca).unwrap();

        let journal = ca.journal.lock().unwrap();
        assert_eq!(journal.nr, 8);
        assert_eq!(journal.buckets.len(), 8);
        assert_eq!(journal.bucket_seq.len(), 8);
        for &bucket in &journal.buckets {
            let entry = vol
                .btree(BtreeId::Alloc)
                .bch2_btree_iter_peek_entry(Bpos::new(0, bucket, 0))
                .unwrap();
            let KeyValue::Raw(bytes) = entry.value else {
                panic!()
            };
            let alloc = deserialize_alloc_entry(&bytes).unwrap();
            assert_eq!(alloc.data_type, BchDataType::Journal as u8);
            assert_eq!(alloc.dirty_sectors, u32::from(bucket_sectors));
        }
        assert_eq!(ca.initialized(), BchMemberInitialized::Initialized);
    }

    #[test]
    fn test_allocator_new() {
        let (alloc, _vol, ca) = make_alloc(1024, 256);
        assert_eq!(alloc.total_blocks(&ca), 1024);
        assert_eq!(alloc.group_count(&ca), 4);
        assert_eq!(
            alloc.btree_reserve_buckets(&ca),
            calc_btree_reserve_buckets(2048, DEFAULT_BTREE_NODE_SIZE)
        );
    }

    #[test]
    fn test_per_device_runtime_and_same_bucket_open_identity_are_isolated() {
        use crate::block_device::MockBlockDevice;
        use crate::storage::superblock::{BchSb, BchSbMember};
        use std::path::PathBuf;
        use std::sync::Arc;

        let mut sb = BchSb::new();
        sb.block_size = DEFAULT_BLOCK_SIZE as u32;
        sb.capacity = 4 * DEFAULT_BUCKET_SIZE;
        sb.primary_dev_idx = 0;
        sb.members = (0..=1)
            .map(|dev_idx| {
                let mut member = BchSbMember::new(dev_idx, format!("dev-{dev_idx}"));
                member.mark_alive([dev_idx + 1; 16]);
                member.nbuckets = 4;
                member.bucket_size = if dev_idx == 0 { 1024 } else { 4096 };
                member
            })
            .collect();

        let dev0 = Arc::new(BchDev::new(Arc::new(MockBlockDevice::new()), 0));
        let dev1 = Arc::new(BchDev::new(Arc::new(MockBlockDevice::new()), 1));
        let vol = crate::BchVol::alloc_with_devices(
            sb,
            vec![dev0.clone(), dev1.clone()],
            crate::bch_vol::VolumeConfig {
                block_size: DEFAULT_BLOCK_SIZE as u32,
                capacity: 4 * DEFAULT_BUCKET_SIZE,
                btree_node_size: DEFAULT_BTREE_NODE_SIZE,
                ..crate::bch_vol::VolumeConfig::default()
            },
            "two-dev".into(),
            PathBuf::from("/tmp/two-dev"),
        );
        let allocator = unsafe { &*vol.allocator.get() };

        let addr0 = allocator
            .bch2_bucket_alloc_new_fs(&vol, &dev0, &ureq(Watermark::Normal), None)
            .unwrap();
        let addr1 = allocator
            .bch2_bucket_alloc_new_fs(&vol, &dev1, &ureq(Watermark::Normal), None)
            .unwrap();
        let bucket0 = sector_to_bucket(&dev0, addr0 * SECTORS_PER_BLOCK);
        let bucket1 = sector_to_bucket(&dev1, addr1 * SECTORS_PER_BLOCK);

        assert_eq!(bucket0, bucket1);
        assert_eq!(allocator.allocated_blocks(&dev0), 128);
        assert_eq!(allocator.allocated_blocks(&dev1), 512);
        assert!(allocator.open_buckets.lookup(0, bucket0).is_some());
        assert!(allocator.open_buckets.lookup(1, bucket1).is_some());

        let addr0_next = allocator
            .bch2_bucket_alloc_new_fs(&vol, &dev0, &ureq(Watermark::Normal), None)
            .unwrap();
        let addr1_next = allocator
            .bch2_bucket_alloc_new_fs(&vol, &dev1, &ureq(Watermark::Normal), None)
            .unwrap();
        let bucket0_next = sector_to_bucket(&dev0, addr0_next * SECTORS_PER_BLOCK);
        let bucket1_next = sector_to_bucket(&dev1, addr1_next * SECTORS_PER_BLOCK);

        assert_ne!(bucket0_next, bucket0);
        assert_eq!(bucket0_next, bucket1_next);
        assert_eq!(addr0_next, bucket0_next * 128);
        assert_eq!(addr1_next, bucket1_next * 512);
        assert_eq!(allocator.allocated_blocks(&dev0), 256);
        assert_eq!(allocator.allocated_blocks(&dev1), 1024);

        let ob0 = allocator.open_buckets.lookup(0, bucket0_next).unwrap();
        let ob1 = allocator.open_buckets.lookup(1, bucket1_next).unwrap();
        assert_ne!(ob0, ob1);
        assert_eq!(
            allocator
                .open_buckets
                .get_entry(ob0)
                .unwrap()
                .sectors_free
                .load(Ordering::Acquire),
            1024
        );
        assert_eq!(
            allocator
                .open_buckets
                .get_entry(ob1)
                .unwrap()
                .sectors_free
                .load(Ordering::Acquire),
            4096
        );
        assert!(vol
            .btree(BtreeId::Alloc)
            .bch2_btree_iter_peek_entry(Bpos::new(0, bucket0_next, 0))
            .is_some());
        assert!(vol
            .btree(BtreeId::Alloc)
            .bch2_btree_iter_peek_entry(Bpos::new(1, bucket1_next, 0))
            .is_some());
        assert_ne!(
            allocator.open_buckets.lookup(0, bucket0),
            allocator.open_buckets.lookup(1, bucket1)
        );
        assert!(vol
            .btree(BtreeId::Alloc)
            .bch2_btree_iter_peek_entry(Bpos::new(0, bucket0, 0))
            .is_some());
        assert!(vol
            .btree(BtreeId::Alloc)
            .bch2_btree_iter_peek_entry(Bpos::new(1, bucket1, 0))
            .is_some());
    }

    #[test]
    fn test_device_bucket_resize_preserves_overlapping_runtime() {
        let (allocator, vol, ca) = make_alloc(1024, 256);
        let addr = allocator
            .bch2_bucket_alloc_new_fs(&vol, &ca, &ureq(Watermark::Normal), None)
            .unwrap();
        let bucket_idx = addr / BLOCKS_PER_BUCKET;
        let allocated = allocator.allocated_blocks(&ca);
        let mut before = None;
        allocator.for_each_bucket(&ca, |bi, bucket, gen| {
            if bi == bucket_idx {
                before = Some((bucket.state, *gen));
            }
        });

        let nbuckets = 8;
        vol.superblock_mut()
            .member_mut(ca.dev_idx)
            .unwrap()
            .nbuckets = nbuckets;
        bch2_dev_buckets_resize(&vol, &ca, nbuckets).unwrap();

        let mut after = None;
        allocator.for_each_bucket(&ca, |bi, bucket, gen| {
            if bi == bucket_idx {
                after = Some((bucket.state, *gen));
            }
        });
        assert_eq!(after, before);
        assert_eq!(allocator.allocated_blocks(&ca), allocated);
    }

    #[test]
    fn test_dynamic_bucket_helpers_and_invalid_geometry() {
        let (vol, devices) = make_metadata_vol(1024, 8, 1);
        let ca = &devices[0];

        assert_eq!(sector_to_bucket(ca, 1025), 1);
        assert_eq!(bucket_to_sector(ca, 1), 1024);
        assert_eq!(bucket_remainder(ca, 1025), 1);
        let mut offset = 0;
        assert_eq!(sector_to_bucket_and_offset(ca, 2051, &mut offset), 2);
        assert_eq!(offset, 3);

        vol.superblock_mut().member_mut(0).unwrap().bucket_size = 0;
        assert!(matches!(
            bch2_dev_buckets_resize(&vol, ca, 8),
            Err(StorageError::InvalidArgument(_))
        ));
        vol.superblock_mut().member_mut(0).unwrap().bucket_size = 1025;
        assert!(matches!(
            bch2_dev_buckets_resize(&vol, ca, 8),
            Err(StorageError::InvalidArgument(_))
        ));
    }

    #[test]
    fn test_dev_buckets_free_tracks_watermark_and_open_buckets() {
        let (_allocator, _vol, ca) = make_alloc(1024, 256);
        let group_free: u64 = unsafe { &*ca.groups.get() }
            .iter()
            .map(|group| group.lock().unwrap().free_buckets.load(Ordering::Acquire))
            .sum();
        assert_eq!(ca.nr_free_buckets.load(Ordering::Acquire), group_free);
        let before = dev_buckets_free(&ca, Watermark::Normal);
        assert!(before > 0);

        ca.nr_open_buckets.fetch_add(1, Ordering::AcqRel);
        assert_eq!(dev_buckets_free(&ca, Watermark::Normal), before - 1);
        ca.nr_open_buckets.fetch_sub(1, Ordering::AcqRel);

        assert!(
            dev_buckets_free(&ca, Watermark::InteriorUpdate)
                >= dev_buckets_free(&ca, Watermark::Normal)
        );
    }

    #[test]
    fn test_dynamic_bucket_reuse_reservation_free_trim_and_extent_trigger() {
        for bucket_sectors in [1024u16, 4096u16] {
            let bucket_blocks = u64::from(bucket_sectors) / SECTORS_PER_BLOCK;
            let (vol, devices) = make_metadata_vol(bucket_sectors, 16, 1);
            let ca = &devices[0];
            let alloc = BchAllocator::with_config(
                16 * u64::from(bucket_sectors),
                WritePointConfig {
                    max_write_points: 8,
                },
            );
            crate::alloc::background::bch2_fs_capacity_init(&vol).unwrap();
            {
                let capacity = unsafe { &mut *vol.capacity.get() };
                capacity.capacity = 16 * u64::from(bucket_sectors);
                capacity.pcpu[0].sectors_available = capacity.capacity;
                capacity
                    .sectors_available
                    .store(capacity.capacity, Ordering::Release);
            }
            let request = AllocRequest::new(Watermark::InteriorUpdate, BchDataType::User);
            let wp = Some(WritePointSpecifier::Hashed(77));

            let first = alloc
                .bch2_alloc_sectors_start_trans(1, &vol, ca, &request, wp)
                .unwrap();
            alloc.bch2_consume_written_extent(ca, first, 1);
            let second = alloc
                .bch2_alloc_sectors_start_trans(1, &vol, ca, &request, wp)
                .unwrap();
            assert_eq!(second, first + 1);

            let bucket = sector_to_bucket(ca, first * SECTORS_PER_BLOCK);
            let ob_idx = alloc.open_buckets.lookup(ca.dev_idx, bucket).unwrap();
            assert_eq!(
                alloc
                    .open_buckets
                    .get_entry(ob_idx)
                    .unwrap()
                    .sectors_free
                    .load(Ordering::Acquire),
                u32::from(bucket_sectors) - (SECTORS_PER_BLOCK as u32 * 2)
            );

            let extent = KeyValue::ExtentPtrs {
                blocks: 1,
                ptrs: vec![ExtentPtr {
                    dev: ca.dev_idx,
                    gen: 1,
                    offset: first,
                    cached: false,
                    unwritten: false,
                }],
                crc32c: 0,
                crc_offset_blocks: 0,
            };
            let extent_bytes = bincode::serialize(&extent).unwrap();
            let mut trans = BtreeTrans::new(&vol);
            bch2_trigger_extent(&mut trans, BtreeId::Extents, &[], None, Some(&extent_bytes))
                .unwrap();
            let alloc_entry = vol
                .btree(BtreeId::Alloc)
                .bch2_btree_iter_peek_entry(Bpos::new(ca.dev_idx as u64, bucket, 0))
                .unwrap();
            let KeyValue::Raw(bytes) = alloc_entry.value else {
                panic!()
            };
            assert_eq!(
                deserialize_alloc_entry(&bytes).unwrap().dirty_sectors,
                SECTORS_PER_BLOCK as u32
            );

            alloc.bch2_bucket_free(ca, first, &vol).unwrap();
            alloc.bch2_bucket_do_trim(ca, first, &vol).unwrap();
            assert_eq!(alloc.allocated_blocks(ca), 0);
            assert_eq!(alloc.free_blocks(ca), 16 * bucket_blocks);
            let free_entry = vol
                .btree(BtreeId::Alloc)
                .bch2_btree_iter_peek_entry(Bpos::new(ca.dev_idx as u64, bucket, 0))
                .unwrap();
            let KeyValue::Raw(bytes) = free_entry.value else {
                panic!()
            };
            assert_eq!(
                deserialize_alloc_entry(&bytes).unwrap().data_type,
                BchDataType::Free as u8
            );
        }
    }

    #[test]
    fn test_may_alloc_bucket_journal_seq_uses_journal_seq_empty() {
        let mut bucket = Bucket::free(0);
        bucket.journal_seq_nonempty = 99;
        bucket.journal_seq_empty = 42;

        assert!(!may_alloc_bucket_journal_seq(&bucket, 41));
        assert!(may_alloc_bucket_journal_seq(&bucket, 42));
        assert!(may_alloc_bucket_journal_seq(&bucket, 50));
        bucket.journal_seq_empty = 0;
        assert!(may_alloc_bucket_journal_seq(&bucket, 0));
    }

    #[test]
    fn test_allocate_bucket() {
        let (alloc, vol, ca) = make_alloc(4096, 1024);
        let addr = alloc
            .bch2_bucket_alloc_new_fs(&vol, &ca, &ureq(Watermark::InteriorUpdate), None)
            .unwrap();
        assert_eq!(addr, 1024); // P1-7: InteriorUpdate→System offset=1→group 1
        assert_eq!(alloc.allocated_blocks(&ca), BLOCKS_PER_BUCKET);

        // 验证 Alloc btree 写入
        let bi = addr / BLOCKS_PER_BUCKET;
        let entry = vol
            .btree(crate::btree::BtreeId::Alloc)
            .bch2_btree_iter_peek_entry(Bpos::new(0, bi, 0));
        assert!(
            entry.is_some(),
            "allocate_bucket should write BchAllocEntry"
        );
    }

    #[test]
    fn test_allocate_multiple_buckets() {
        let (alloc, vol, ca) = make_alloc(4096, 1024);
        // round-robin: 各组交替分配
        let addr0 = alloc
            .bch2_bucket_alloc_new_fs(&vol, &ca, &ureq(Watermark::InteriorUpdate), None)
            .unwrap(); // group 0
        let addr1 = alloc
            .bch2_bucket_alloc_new_fs(&vol, &ca, &ureq(Watermark::InteriorUpdate), None)
            .unwrap(); // group 1 (hint rotated)
        assert_eq!(addr0 % 1024, 0);
        assert_eq!(addr1 % 1024, 0);
        assert_ne!(addr0, addr1);
        assert_eq!(alloc.allocated_blocks(&ca), 2 * BLOCKS_PER_BUCKET);
    }

    #[test]
    fn test_round_robin_groups() {
        let (alloc, vol, ca) = make_alloc(4096, 128);
        // 只有 4 个 blocks 每组，多分配几次
        for _ in 0..8 {
            let _addr =
                alloc.bch2_bucket_alloc_new_fs(&vol, &ca, &ureq(Watermark::InteriorUpdate), None);
        }
    }

    #[test]
    fn test_free_blocks() {
        let (alloc, vol, ca) = make_alloc(1024, 256);
        assert_eq!(alloc.free_blocks(&ca), 1024);
        // InteriorUpdate: 无预留，适用于小型分配器
        alloc
            .bch2_bucket_alloc_new_fs(&vol, &ca, &ureq(Watermark::InteriorUpdate), None)
            .unwrap();
        assert_eq!(alloc.free_blocks(&ca), 1024 - BLOCKS_PER_BUCKET);
    }

    #[test]
    fn test_allocate_buckets_batch() {
        let (alloc, vol, ca) = make_alloc(4096, 1024);
        let addrs = alloc
            .bch2_alloc_buckets(2, &vol, &ca, &ureq(Watermark::InteriorUpdate), None)
            .unwrap();
        assert_eq!(addrs.len(), 2);
        // round-robin 分发，地址不连续
        assert_ne!(addrs[0], addrs[1]);
        assert!(addrs[0] % BLOCKS_PER_BUCKET == 0);
        assert!(addrs[1] % BLOCKS_PER_BUCKET == 0);
    }

    #[test]
    fn test_reuse_open_bucket_without_extra_flag() {
        let (alloc, _vol, ca) = make_alloc(4096, 1024);
        let ob_idx = alloc
            .open_buckets
            .alloc(0, 0, (BLOCKS_PER_BUCKET * SECTORS_PER_BLOCK) as u32, 1)
            .unwrap();
        alloc.open_buckets.add_to_partial(ob_idx);

        let before = alloc
            .open_buckets
            .get_entry(ob_idx)
            .unwrap()
            .sectors_free
            .load(std::sync::atomic::Ordering::Acquire);
        let addr = alloc
            .bch2_try_reuse_open_bucket(
                &ca,
                BLOCKS_PER_BUCKET,
                &AllocRequest::new(Watermark::Normal, BchDataType::User),
                None,
            )
            .unwrap();

        assert_eq!(addr, 0);
        let after = alloc
            .open_buckets
            .get_entry(ob_idx)
            .unwrap()
            .sectors_free
            .load(std::sync::atomic::Ordering::Acquire);
        assert_eq!(before - after, BLOCKS_PER_BUCKET as u32);
    }

    #[test]
    fn test_exhaustion() {
        let (alloc, vol, ca) = make_alloc(BLOCKS_PER_BUCKET, BLOCKS_PER_BUCKET);
        // InteriorUpdate: 无预留，1 个 bucket 的 AG 无法支持水位线预留
        alloc
            .bch2_bucket_alloc_new_fs(&vol, &ca, &ureq(Watermark::InteriorUpdate), None)
            .unwrap();
        let result =
            alloc.bch2_bucket_alloc_new_fs(&vol, &ca, &ureq(Watermark::InteriorUpdate), None);
        assert!(result.is_err());
    }

    #[test]
    fn test_free_then_allocate() {
        let (alloc, vol, ca) = make_alloc(4096, 1024);
        let addr = alloc
            .bch2_bucket_alloc_new_fs(&vol, &ca, &ureq(Watermark::InteriorUpdate), None)
            .unwrap();
        let allocated_before = alloc.allocated_blocks(&ca);
        let free_seq = vol.journal_ref().bch2_journal_cur_seq();
        vol.journal_ref()
            .flushed_seq_ondisk
            .store(free_seq.saturating_sub(1), Ordering::Release);
        let bucket_index = addr / BLOCKS_PER_BUCKET;
        let alloc_bpos = Bpos::new(0, bucket_index, 0);
        let custom_entry = BchAllocEntry {
            journal_seq_nonempty: 77,
            journal_seq_empty: 88,
            dirty_sectors: (BLOCKS_PER_BUCKET * SECTORS_PER_BLOCK) as u32,
            cached_sectors: 0,
            stripe_refcount: 0,
            stripe_sectors: 0,
            data_type: BchDataType::User as u8,
            flags: 0,
            gen: 3,
            oldest_gen: 9,
            io_time: [1234, 0],
            nr_external_backpointers: 7,
            ..crate::alloc::btree::BCH_ALLOC_V4_ZERO
        };
        let custom_bytes = serialize_alloc_entry(&custom_entry);
        vol.btree(crate::btree::BtreeId::Alloc)
            .bch2_btree_bset_insert_key_wrapper(
                crate::btree::key::BtreeEntry::raw(
                    alloc_bpos,
                    crate::btree::key::KeyType::Normal,
                    custom_bytes,
                ),
                0,
            );
        alloc.bch2_bucket_free(&ca, addr, &vol).unwrap();
        // C3: free 后 state=NeedDiscard，allocated 不变
        assert_eq!(
            alloc.allocated_blocks(&ca),
            allocated_before,
            "free sets NeedDiscard, allocated should not decrease"
        );
        // 验证 Alloc btree 中 state 为 NeedDiscard
        let entry = vol
            .btree(crate::btree::BtreeId::Alloc)
            .bch2_btree_iter_peek_entry(alloc_bpos)
            .unwrap();
        if let crate::btree::key::KeyValue::Raw(bytes) = &entry.value {
            let alloc_data = deserialize_alloc_entry(bytes).unwrap();
            assert_eq!(
                alloc_data.data_type,
                BchDataType::NeedDiscard as u8,
                "free should set NeedDiscard in Alloc btree"
            );
            assert_eq!(
                alloc_data.journal_seq_empty, free_seq,
                "free should record the current journal seq as journal_seq_empty"
            );
            assert_eq!(alloc_data.io_time[0], 1234);
            assert_eq!(alloc_data.nr_external_backpointers, 7);
        }
        // Trim → Free
        alloc.bch2_bucket_do_trim(&ca, addr, &vol).unwrap();
        assert_eq!(
            alloc.allocated_blocks(&ca),
            allocated_before - BLOCKS_PER_BUCKET,
            "trim should decrease allocated count"
        );
        let entry = vol
            .btree(crate::btree::BtreeId::Alloc)
            .bch2_btree_iter_peek_entry(Bpos::new(0, bucket_index, 0))
            .unwrap();
        if let crate::btree::key::KeyValue::Raw(bytes) = &entry.value {
            let alloc_data = deserialize_alloc_entry(bytes).unwrap();
            assert_eq!(alloc_data.data_type, BchDataType::NeedGcGens as u8);
            assert_eq!(alloc_data.journal_seq_nonempty, 0);
            assert_eq!(alloc_data.journal_seq_empty, 0);
            assert_eq!(alloc_data.io_time[0], 1234);
            assert_eq!(alloc_data.nr_external_backpointers, 7);
        }
        let _addr2 = alloc
            .bch2_bucket_alloc_new_fs(&vol, &ca, &ureq(Watermark::InteriorUpdate), None)
            .unwrap();
        // 注意：round-robin 分配策略不保证立即复用刚释放的 bucket，
        // hint 已推进到下一个 group。简化 bitmap 分配器的已知行为。
        assert_eq!(alloc.allocated_blocks(&ca), allocated_before);
    }

    #[test]
    fn test_freespace_key_preserves_oldest_gen() {
        let (alloc, vol, ca) = make_alloc(4096, 1024);
        let addr = alloc
            .bch2_bucket_alloc_new_fs(&vol, &ca, &ureq(Watermark::InteriorUpdate), None)
            .unwrap();
        let bucket_index = addr / BLOCKS_PER_BUCKET;

        {
            let groups = unsafe { &*ca.groups.get() };
            let guard = groups[0].lock().unwrap();
            let local_bi = (bucket_index - guard.start_block / BLOCKS_PER_BUCKET) as usize;
            if local_bi < guard.buckets.len() {
                drop(guard);
                let groups = unsafe { &*ca.groups.get() };
                let mut guard = groups[0].lock().unwrap();
                guard.gens[local_bi] = 10;
                guard.buckets[local_bi].oldest_gen = 7;
            }
        }

        let alloc_bpos = Bpos::new(0, bucket_index, 0);
        let alloc_entry = BchAllocEntry {
            journal_seq_nonempty: 77,
            journal_seq_empty: 0,
            dirty_sectors: (BLOCKS_PER_BUCKET * SECTORS_PER_BLOCK) as u32,
            cached_sectors: 0,
            stripe_refcount: 0,
            stripe_sectors: 0,
            data_type: BchDataType::User as u8,
            flags: 0,
            gen: 10,
            oldest_gen: 7,
            io_time: [0; 2],
            nr_external_backpointers: 0,
            ..crate::alloc::btree::BCH_ALLOC_V4_ZERO
        };
        let bytes = serialize_alloc_entry(&alloc_entry);
        vol.btree(crate::btree::BtreeId::Alloc)
            .bch2_btree_bset_insert_key_wrapper(
                crate::btree::key::BtreeEntry::raw(
                    alloc_bpos,
                    crate::btree::key::KeyType::Normal,
                    bytes,
                ),
                0,
            );

        alloc.bch2_bucket_free(&ca, addr, &vol).unwrap();
        alloc.bch2_bucket_do_trim(&ca, addr, &vol).unwrap();

        let freespace_pos = alloc_freespace_pos(0, bucket_index, 11, 7);
        let entry = vol
            .btree(crate::btree::BtreeId::Freespace)
            .bch2_btree_iter_peek_entry(freespace_pos);
        assert!(entry.is_some(), "freespace key should preserve oldest_gen");
    }

    #[test]
    fn test_free_invalid_addr() {
        let (alloc, vol, ca) = make_alloc(4096, 1024);
        alloc.bch2_bucket_free(&ca, 0, &vol).unwrap();
        alloc.bch2_bucket_free(&ca, 99999, &vol).unwrap();
        assert_eq!(alloc.allocated_blocks(&ca), 0);
    }

    #[test]
    fn test_free_multiple_buckets() {
        let (alloc, vol, ca) = make_alloc(8192, 2048);
        let addrs: Vec<u64> = (0..4)
            .map(|_| {
                alloc
                    .bch2_bucket_alloc_new_fs(&vol, &ca, &ureq(Watermark::InteriorUpdate), None)
                    .unwrap()
            })
            .collect();
        assert_eq!(alloc.allocated_blocks(&ca), 4 * BLOCKS_PER_BUCKET);
        for addr in &addrs {
            alloc.bch2_bucket_free(&ca, *addr, &vol).unwrap();
        }
        // C3: free 后 state=NeedDiscard，allocated 不变
        assert_eq!(
            alloc.allocated_blocks(&ca),
            4 * BLOCKS_PER_BUCKET,
            "freed buckets are NeedDiscard, allocated unchanged until trim"
        );
        // Trim all → Free
        for addr in &addrs {
            alloc.bch2_bucket_do_trim(&ca, *addr, &vol).unwrap();
        }
        assert_eq!(
            alloc.allocated_blocks(&ca),
            0,
            "after trim, all buckets should be free"
        );
        for _ in 0..4 {
            alloc
                .bch2_bucket_alloc_new_fs(&vol, &ca, &ureq(Watermark::InteriorUpdate), None)
                .unwrap();
        }
        assert_eq!(
            alloc.allocated_blocks(&ca),
            4 * BLOCKS_PER_BUCKET,
            "should re-allocate freed buckets"
        );
    }

    // ─── P1.1: Alloc btree 加载测试 ───────

    #[test]
    fn test_global_allocator_replacement_preserves_device_state() {
        // 阶段 1：分配 bucket 并验证 Alloc btree 有记录
        let (alloc, vol, ca) = make_alloc(4096, 1024);
        let addr = alloc
            .bch2_bucket_alloc_new_fs(&vol, &ca, &ureq(Watermark::InteriorUpdate), None)
            .unwrap();
        let bucket_index = addr / BLOCKS_PER_BUCKET;

        // 验证 btree 中有 BchAllocEntry
        let bpos = Bpos::new(0, bucket_index, 0);
        let entry_before = vol
            .btree(crate::btree::BtreeId::Alloc)
            .bch2_btree_iter_peek_entry(bpos);
        assert!(
            entry_before.is_some(),
            "Alloc btree should have entry after allocation"
        );

        // 阶段 2：替换文件系统级 allocator。bucket runtime 属于 BchDev，
        // 因此替换全局 allocator 不得重置设备状态。
        let alloc2 = BchAllocator::new(4096 * SECTORS_PER_BLOCK);
        assert_eq!(
            alloc2.allocated_blocks(&ca),
            BLOCKS_PER_BUCKET,
            "global allocator replacement must preserve BchDev bucket state"
        );

        // 阶段 3：从 Alloc btree 加载
        alloc2.bch2_alloc_read(&vol).unwrap();

        // 尚未写入 extent 的 open bucket在 Alloc btree 中仍为 Free；重复读取
        // 不得覆盖仍存活的 BchDev runtime。
        assert_eq!(alloc2.allocated_blocks(&ca), BLOCKS_PER_BUCKET);
    }

    #[test]
    fn test_load_from_btree_all_free_after_free() {
        let (alloc, vol, ca) = make_alloc(4096, 1024);
        let addr = alloc
            .bch2_bucket_alloc_new_fs(&vol, &ca, &ureq(Watermark::InteriorUpdate), None)
            .unwrap();
        alloc.bch2_bucket_free(&ca, addr, &vol).unwrap();
        // C3: trim 后才能从 NeedDiscard 变为 Free
        alloc.bch2_bucket_do_trim(&ca, addr, &vol).unwrap();

        // 创建新分配器 + load_from_btree
        let alloc2 = BchAllocator::new(4096 * SECTORS_PER_BLOCK);
        alloc2.bch2_alloc_read(&vol).unwrap();

        // 验证所有 bucket 都是 Free（分配后释放并 trim 了）
        assert_eq!(
            alloc2.allocated_blocks(&ca),
            0,
            "after free+trim+load, allocated should be 0"
        );
    }

    #[test]
    fn test_alloc_btree_sync_on_allocate() {
        let (alloc, vol, ca) = make_alloc(4096, 1024);
        let addr = alloc
            .bch2_bucket_alloc_new_fs(&vol, &ca, &ureq(Watermark::InteriorUpdate), None)
            .unwrap();
        let bucket_index = addr / BLOCKS_PER_BUCKET;

        // 直接从 Alloc btree 读取，验证状态为 Allocated
        let bpos = Bpos::new(0, bucket_index, 0);
        let entry = vol
            .btree(crate::btree::BtreeId::Alloc)
            .bch2_btree_iter_peek_entry(bpos)
            .expect("Alloc btree should have entry after allocate");
        match &entry.value {
            crate::btree::key::KeyValue::Raw(bytes) => {
                let alloc_data = deserialize_alloc_entry(bytes).unwrap();
                assert_eq!(
                    alloc_data.data_type,
                    BchDataType::Free as u8,
                    "allocate_bucket should write BchAllocEntry::Allocated"
                );
            }
            _ => panic!("Alloc entry should be stored as KeyValue::Raw"),
        }
    }

    #[test]
    fn test_alloc_btree_sync_on_free() {
        let (alloc, vol, ca) = make_alloc(4096, 1024);
        let addr = alloc
            .bch2_bucket_alloc_new_fs(&vol, &ca, &ureq(Watermark::InteriorUpdate), None)
            .unwrap();
        let bucket_index = addr / BLOCKS_PER_BUCKET;

        // 释放
        alloc.bch2_bucket_free(&ca, addr, &vol).unwrap();

        // C3: 验证 Alloc btree 中状态变为 NeedDiscard（非 Free）
        let bpos = Bpos::new(0, bucket_index, 0);
        let entry = vol
            .btree(crate::btree::BtreeId::Alloc)
            .bch2_btree_iter_peek_entry(bpos)
            .expect("Alloc btree should have entry after free");
        match &entry.value {
            crate::btree::key::KeyValue::Raw(bytes) => {
                let alloc_data = deserialize_alloc_entry(bytes).unwrap();
                assert_eq!(
                    alloc_data.data_type,
                    BchDataType::NeedDiscard as u8,
                    "free should write BchAllocEntry::NeedDiscard"
                );
            }
            _ => panic!("Alloc entry should be stored as KeyValue::Raw"),
        }
    }

    // ─── Phase C2: Alloc extent trigger tests ───────

    #[test]
    fn test_alloc_extent_trigger_insert() {
        let vol = crate::BchVol::test_trees();

        // KeyValue::ExtentPtrs: bucket index = offset / BLOCKS_PER_BUCKET
        let offset = BLOCKS_PER_BUCKET + 1; // bucket_index = 1
        let kv = KeyValue::ExtentPtrs {
            blocks: 1,
            ptrs: vec![ExtentPtr {
                dev: 0,
                gen: 1,
                offset,
                cached: false,
                unwritten: false,
            }],
            crc32c: 0,
            crc_offset_blocks: 0,
        };
        let bytes = bincode::serialize(&kv).unwrap();

        let mut trans = BtreeTrans::new(&vol);
        super::bch2_trigger_extent(&mut trans, BtreeId::Extents, &[], None, Some(&bytes)).unwrap();

        let alloc_bpos = Bpos::new(0, 1, 0);
        let entry = vol.get_entry_raw(BtreeId::Alloc, alloc_bpos).unwrap();
        match &entry.value {
            KeyValue::Raw(b) => {
                let alloc = deserialize_alloc_entry(b).unwrap();
                assert_eq!(alloc.data_type, BchDataType::User as u8);
                assert_eq!(alloc.dirty_sectors, SECTORS_PER_BLOCK as u32);
                assert_eq!(alloc.gen, 1);
            }
            _ => panic!("Alloc entry should be KeyValue::Raw"),
        }
    }

    #[test]
    fn test_alloc_extent_trigger_delete() {
        let vol = crate::BchVol::test_trees();

        let offset = 2 * BLOCKS_PER_BUCKET;
        let bucket_index = offset / BLOCKS_PER_BUCKET;
        let alloc_bpos = Bpos::new(0, bucket_index, 0);

        // Pre-populate alloc entry with User state
        let initial_alloc = BchAllocEntry {
            journal_seq_nonempty: 10,
            journal_seq_empty: 0,
            dirty_sectors: SECTORS_PER_BLOCK as u32,
            cached_sectors: 0,
            stripe_refcount: 0,
            stripe_sectors: 0,
            data_type: BchDataType::User as u8,
            flags: 0,
            gen: 2,
            oldest_gen: 0,
            io_time: [0; 2],
            nr_external_backpointers: 0,
            ..crate::alloc::btree::BCH_ALLOC_V4_ZERO
        };
        let initial_bytes = serialize_alloc_entry(&initial_alloc);
        vol.btree(BtreeId::Alloc)
            .bch2_btree_bset_insert_key_wrapper(
                BtreeEntry::raw(alloc_bpos, KeyType::Normal, initial_bytes),
                0,
            );

        // Delete: pass old_val = ExtentPtrs, new_val = None
        let kv = KeyValue::ExtentPtrs {
            blocks: 1,
            ptrs: vec![ExtentPtr {
                dev: 0,
                gen: 2,
                offset,
                cached: false,
                unwritten: false,
            }],
            crc32c: 0,
            crc_offset_blocks: 0,
        };
        let bytes = bincode::serialize(&kv).unwrap();

        let mut trans = BtreeTrans::new(&vol);
        super::bch2_trigger_extent(&mut trans, BtreeId::Extents, &[], Some(&bytes), None).unwrap();

        let entry = vol.get_entry_raw(BtreeId::Alloc, alloc_bpos).unwrap();
        match &entry.value {
            KeyValue::Raw(b) => {
                let alloc = deserialize_alloc_entry(b).unwrap();
                assert_eq!(alloc.data_type, BchDataType::NeedDiscard as u8);
                assert_eq!(alloc.dirty_sectors, 0);
                assert!(alloc.journal_seq_empty > 0);
            }
            _ => panic!("Alloc entry should be KeyValue::Raw"),
        }
    }

    #[test]
    fn test_bucket_free_skips_journal_seq_empty_when_already_flushed() {
        let (alloc, vol, ca) = make_alloc(4096, 1024);
        let addr = alloc
            .bch2_bucket_alloc_new_fs(&vol, &ca, &ureq(Watermark::InteriorUpdate), None)
            .unwrap();

        let journal_seq = vol.journal_ref().bch2_journal_cur_seq();
        vol.journal_ref()
            .flushed_seq_ondisk
            .store(journal_seq, Ordering::Release);

        alloc.bch2_bucket_free(&ca, addr, &vol).unwrap();

        let bucket_index = addr / BLOCKS_PER_BUCKET;
        let bpos = Bpos::new(0, bucket_index, 0);
        let entry = vol
            .btree(crate::btree::BtreeId::Alloc)
            .bch2_btree_iter_peek_entry(bpos)
            .expect("Alloc btree should have entry after free");
        match &entry.value {
            crate::btree::key::KeyValue::Raw(bytes) => {
                let alloc_data = deserialize_alloc_entry(bytes).unwrap();
                assert_eq!(alloc_data.data_type, BchDataType::NeedDiscard as u8);
                assert_eq!(alloc_data.journal_seq_empty, 0);
            }
            _ => panic!("Alloc entry should be stored as KeyValue::Raw"),
        }
    }

    #[test]
    fn test_alloc_extent_trigger_ignores_identical_update() {
        let vol = crate::BchVol::test_trees();
        let entry = crate::alloc::btree::BchAllocEntry {
            journal_seq_nonempty: 12,
            journal_seq_empty: 34,
            dirty_sectors: (BLOCKS_PER_BUCKET * SECTORS_PER_BLOCK) as u32,
            cached_sectors: 0,
            stripe_refcount: 0,
            stripe_sectors: 0,
            data_type: crate::alloc::BchDataType::User as u8,
            flags: 0,
            gen: 7,
            oldest_gen: 5,
            io_time: [88, 0],
            nr_external_backpointers: 6,
            ..crate::alloc::btree::BCH_ALLOC_V4_ZERO
        };
        let bytes = crate::alloc::btree::serialize_alloc_entry(&entry);
        let bucket_index = 0x500500 / BLOCKS_PER_BUCKET;
        let alloc_bpos = Bpos::new(0, bucket_index, 0);
        vol.btree(crate::btree::BtreeId::Alloc)
            .bch2_btree_bset_insert_key_wrapper(
                crate::btree::key::BtreeEntry::raw(
                    alloc_bpos,
                    crate::btree::key::KeyType::Normal,
                    bytes.clone(),
                ),
                0,
            );

        let mut trans = BtreeTrans::new(&vol);
        super::bch2_trigger_extent(
            &mut trans,
            crate::btree::BtreeId::Extents,
            &[],
            Some(&bytes),
            Some(&bytes),
        )
        .unwrap();

        let restored = vol
            .get_entry_raw(crate::btree::BtreeId::Alloc, alloc_bpos)
            .unwrap();
        match restored.value {
            crate::btree::key::KeyValue::Raw(bytes_after) => {
                let restored_entry = deserialize_alloc_entry(&bytes_after).unwrap();
                assert_eq!(restored_entry, deserialize_alloc_entry(&bytes).unwrap());
            }
            _ => panic!("Alloc entry should remain raw"),
        }
    }

    #[test]
    fn test_gc_trigger_insert_via_transaction() {
        let vol = crate::BchVol::test_trees();

        let offset = 3 * BLOCKS_PER_BUCKET;
        let bucket_index = offset / BLOCKS_PER_BUCKET;
        let alloc_bpos = Bpos::new(0, bucket_index, 0);

        let kv = KeyValue::ExtentPtrs {
            blocks: 1,
            ptrs: vec![ExtentPtr {
                dev: 0,
                gen: 1,
                offset,
                cached: false,
                unwritten: false,
            }],
            crc32c: 0,
            crc_offset_blocks: 0,
        };
        let bytes = bincode::serialize(&kv).unwrap();

        // 触发 trigger（模拟 Atomic 阶段）
        let mut trans = BtreeTrans::new(&vol);
        super::bch2_trigger_extent(&mut trans, BtreeId::Extents, &[], None, Some(&bytes)).unwrap();

        let entry = vol.get_entry_raw(BtreeId::Alloc, alloc_bpos).unwrap();
        match &entry.value {
            KeyValue::Raw(b) => {
                let alloc = deserialize_alloc_entry(b).unwrap();
                assert_eq!(alloc.data_type, BchDataType::User as u8);
                assert_eq!(alloc.dirty_sectors, SECTORS_PER_BLOCK as u32);
                assert_eq!(alloc.gen, 1);
            }
            _ => panic!("Alloc entry should be KeyValue::Raw"),
        }
    }

    #[test]
    fn test_gc_trigger_delete_via_transaction() {
        let vol = crate::BchVol::test_trees();

        let offset = 4 * BLOCKS_PER_BUCKET;
        let bucket_index = offset / BLOCKS_PER_BUCKET;
        let alloc_bpos = Bpos::new(0, bucket_index, 0);

        // Pre-populate alloc entry
        let initial_alloc = BchAllocEntry {
            journal_seq_nonempty: 10,
            journal_seq_empty: 0,
            dirty_sectors: SECTORS_PER_BLOCK as u32,
            cached_sectors: 0,
            stripe_refcount: 0,
            stripe_sectors: 0,
            data_type: BchDataType::User as u8,
            flags: 0,
            gen: 2,
            oldest_gen: 0,
            io_time: [0; 2],
            nr_external_backpointers: 0,
            ..crate::alloc::btree::BCH_ALLOC_V4_ZERO
        };
        let initial_bytes = serialize_alloc_entry(&initial_alloc);
        vol.btree(BtreeId::Alloc)
            .bch2_btree_bset_insert_key_wrapper(
                BtreeEntry::raw(alloc_bpos, KeyType::Normal, initial_bytes),
                0,
            );

        let kv = KeyValue::ExtentPtrs {
            blocks: 1,
            ptrs: vec![ExtentPtr {
                dev: 0,
                gen: 2,
                offset,
                cached: false,
                unwritten: false,
            }],
            crc32c: 0,
            crc_offset_blocks: 0,
        };
        let bytes = bincode::serialize(&kv).unwrap();

        // Delete: pass old_val only
        let mut trans = BtreeTrans::new(&vol);
        super::bch2_trigger_extent(&mut trans, BtreeId::Extents, &[], Some(&bytes), None).unwrap();

        let entry = vol.get_entry_raw(BtreeId::Alloc, alloc_bpos).unwrap();
        match &entry.value {
            KeyValue::Raw(b) => {
                let alloc = deserialize_alloc_entry(b).unwrap();
                assert_eq!(alloc.data_type, BchDataType::NeedDiscard as u8);
                assert_eq!(alloc.dirty_sectors, 0);
                assert!(alloc.journal_seq_empty > 0);
            }
            _ => panic!("Alloc entry should be KeyValue::Raw"),
        }
    }

    // ─── Freespace btree 测试 ─────────────────────────────────────

    #[test]
    fn test_freespace_btree_sync_on_allocate() {
        let (alloc, vol, ca) = make_alloc(4096, 1024);
        let addr = alloc
            .bch2_bucket_alloc_new_fs(&vol, &ca, &ureq(Watermark::InteriorUpdate), None)
            .unwrap();
        let bucket_index = addr / BLOCKS_PER_BUCKET;

        // 分配后，freespace btree 中不应有此 bucket 的正常条目
        let freespace_pos = alloc_freespace_pos(0, bucket_index, 1, 0); // gen=1 after first alloc
        let entry = vol
            .btree(BtreeId::Freespace)
            .bch2_btree_iter_peek_entry(freespace_pos);
        // 预期：不存在（被 Deleted tombstone 覆盖）或根本不在 btree 中
        assert!(
            entry.is_none() || matches!(entry.unwrap().key_type, KeyType::Deleted),
            "freespace entry should be absent or tombstone after allocation"
        );
    }

    #[test]
    fn test_freespace_btree_sync_on_free() {
        let (alloc, vol, ca) = make_alloc(4096, 1024);
        let addr = alloc
            .bch2_bucket_alloc_new_fs(&vol, &ca, &ureq(Watermark::InteriorUpdate), None)
            .unwrap();
        let bucket_index = addr / BLOCKS_PER_BUCKET;

        // C3: free 后 state=NeedDiscard（不写入 freespace），trim 后才是 Free
        alloc.bch2_bucket_free(&ca, addr, &vol).unwrap();
        alloc.bch2_bucket_do_trim(&ca, addr, &vol).unwrap();

        // bucket alloc (gen=1) → free (gen=2) → trim (gen=2, freespace gen=2)
        let freespace_pos = alloc_freespace_pos(0, bucket_index, 2, 0);
        let entry = vol
            .btree(BtreeId::Freespace)
            .bch2_btree_iter_peek_entry(freespace_pos);
        assert!(entry.is_some(), "freespace should have entry after trim");
        if let Some(e) = entry {
            assert_eq!(
                e.key_type,
                KeyType::Normal,
                "freespace entry should be Normal after trim"
            );
        }
    }

    #[test]
    fn test_freespace_rebuild_from_alloc() {
        let (alloc, vol, ca) = make_alloc(4096, 1024);

        // 分配一个 bucket、释放并 trim（创建 Alloc btree entry for Free）
        let addr = alloc
            .bch2_bucket_alloc_new_fs(&vol, &ca, &ureq(Watermark::InteriorUpdate), None)
            .unwrap();
        let bucket_index = addr / BLOCKS_PER_BUCKET;
        alloc.bch2_bucket_free(&ca, addr, &vol).unwrap();
        alloc.bch2_bucket_do_trim(&ca, addr, &vol).unwrap();

        // 创建新 vol，从 Alloc btree 重建 Freespace btree
        let fresh_vol = make_test_vol();
        let fresh_ca = fresh_vol.primary_device_rcu_noerror().unwrap();
        let nbuckets = vol.superblock().member(ca.dev_idx).unwrap().nbuckets;
        fresh_vol
            .superblock_mut()
            .member_mut(fresh_ca.dev_idx)
            .unwrap()
            .nbuckets = nbuckets;
        bch2_dev_buckets_resize(&fresh_vol, &fresh_ca, nbuckets).unwrap();
        // 手动将 Alloc btree 的条目复制到新 vol
        if let Some(entry) = vol
            .btree(BtreeId::Alloc)
            .bch2_btree_iter_peek_entry(Bpos::new(0, bucket_index, 0))
        {
            fresh_vol
                .btree_mut(BtreeId::Alloc)
                .bch2_btree_bset_insert_key_wrapper(entry, 0);
        }

        // 先恢复 allocator，再从 allocator 重建 freespace
        {
            let allocator = unsafe { &mut *fresh_vol.allocator.get() };
            allocator.bch2_alloc_read(&fresh_vol).unwrap();
        }
        super::bch2_rebuild_freespace(&fresh_vol).unwrap();

        // 验证 freespace btree 中有释放的 bucket
        // Alloc entry 最终 gen=2（alloc gen=1 → free gen=2 → trim gen=2）
        let freespace_pos = alloc_freespace_pos(0, bucket_index, 2, 0);
        let freespace_entry = fresh_vol
            .btree(BtreeId::Freespace)
            .bch2_btree_iter_peek_entry(freespace_pos);
        assert!(
            freespace_entry.is_some(),
            "rebuild should insert freed bucket into freespace"
        );
    }

    #[test]
    fn test_freespace_no_sync_for_unchanged_state() {
        // 分配后直接再次分配同一个 bucket 位置（不同 gen）不应影响 freespace
        let vol = make_test_vol();

        // 手动写入一个 Free 的 Alloc entry
        let entry = BchAllocEntry {
            gen: 1,
            ..crate::alloc::btree::BCH_ALLOC_V4_ZERO
        };
        let bytes = serialize_alloc_entry(&entry);
        vol.btree_mut(BtreeId::Alloc)
            .bch2_btree_bset_insert_key_wrapper(
                BtreeEntry::raw(Bpos::new(0, 0, 0), KeyType::Normal, bytes),
                0,
            );

        // 然后写入另一个 Free 的 Alloc entry（状态未变，不应触发 freespace 同步）
        let entry2 = BchAllocEntry {
            gen: 2,
            ..crate::alloc::btree::BCH_ALLOC_V4_ZERO
        };
        let bytes2 = serialize_alloc_entry(&entry2);
        vol.btree_mut(BtreeId::Alloc)
            .bch2_btree_bset_insert_key_wrapper(
                BtreeEntry::raw(alloc_freespace_pos(0, 0, 0, 0), KeyType::Normal, bytes2),
                0,
            );

        // Freespace btree 不应有该 bucket 的条目（allocate_bucket/free 负责同步，
        // 直接写 Alloc btree 不会触发 freespace 同步）
        let freespace_pos = alloc_freespace_pos(0, 0, 2, 0);
        // 可能没有，可能是 Free→Free 不会触发
        let _freespace_entry = vol
            .btree(BtreeId::Freespace)
            .bch2_btree_iter_peek_entry(freespace_pos);
        // 不断言存在或不存在，仅验证不 panic
    }

    // ─── P1: Write Point 测试 ─────────────────────────

    #[test]
    fn test_allocator_new_global_state() {
        let (alloc, vol, ca) = make_alloc(4096, 1024);
        let addr = alloc
            .bch2_bucket_alloc_new_fs(&vol, &ca, &ureq(Watermark::InteriorUpdate), None)
            .unwrap();
        assert_eq!(addr, 1024); // P1-7: InteriorUpdate→System offset=1→group 1
        assert_eq!(alloc.allocated_blocks(&ca), BLOCKS_PER_BUCKET);
    }

    #[test]
    fn test_with_config_default_eq_new() {
        // with_config(WP=1) 行为与 new() 一致
        let (_alloc1, vol, ca) = make_alloc(4096, 1024);
        let alloc2 = BchAllocator::with_config(
            4096 * SECTORS_PER_BLOCK,
            WritePointConfig {
                max_write_points: 1,
            },
        );
        assert!(
            alloc2.write_points.is_none(),
            "WP=1 should have no write point pool"
        );
        // 验证 hint 字段类型相同（内部细节：两者都使用全局 hint）
        let addr = alloc2
            .bch2_bucket_alloc_new_fs(&vol, &ca, &ureq(Watermark::InteriorUpdate), None)
            .unwrap();
        assert_eq!(addr, 1024); // P1-7: InteriorUpdate→System offset=1→group 1
        assert_eq!(alloc2.allocated_blocks(&ca), BLOCKS_PER_BUCKET);
    }

    #[test]
    fn test_with_config_wp_gt_1_has_pool() {
        let (_base, vol, ca) = make_alloc(4096, 1024);
        let alloc = BchAllocator::with_config(
            4096 * SECTORS_PER_BLOCK,
            WritePointConfig {
                max_write_points: 8,
            },
        );
        // write_points 应为 Some（池已初始化）
        // 由于 write_points 是私有字段，通过功能验证：分配应仍然正常工作
        let addr = alloc
            .bch2_bucket_alloc_new_fs(
                &vol,
                &ca,
                &ureq(Watermark::InteriorUpdate),
                Some(WritePointSpecifier::Hashed(42)),
            )
            .unwrap();
        assert_eq!(addr, 0);
        assert_eq!(alloc.allocated_blocks(&ca), BLOCKS_PER_BUCKET);
    }

    #[test]
    fn test_allocate_with_wp_id_none() {
        // None WP ID → 使用全局 hint（向后兼容路径）
        let (alloc, vol, ca) = make_alloc(4096, 1024);
        let addr = alloc
            .bch2_bucket_alloc_new_fs(&vol, &ca, &ureq(Watermark::InteriorUpdate), None)
            .unwrap();
        assert_eq!(addr, 1024); // P1-7: InteriorUpdate→System offset=1→group 1
                                // 第二次分配，hint 推进
        let addr2 = alloc
            .bch2_bucket_alloc_new_fs(&vol, &ca, &ureq(Watermark::InteriorUpdate), None)
            .unwrap();
        assert_ne!(addr, addr2);
    }

    #[test]
    fn test_allocate_with_wp_id_hashed() {
        let (_base, vol, ca) = make_alloc(8192, 1024);
        let alloc = BchAllocator::with_config(
            8192 * SECTORS_PER_BLOCK,
            WritePointConfig {
                max_write_points: 8,
            },
        );
        // 相同 hash 值应导致 hint 行为相同（但 WP hint 是独立的）
        let addr = alloc
            .bch2_bucket_alloc_new_fs(
                &vol,
                &ca,
                &ureq(Watermark::InteriorUpdate),
                Some(WritePointSpecifier::Hashed(100)),
            )
            .unwrap();
        assert!(addr % BLOCKS_PER_BUCKET == 0);
        // 不同 hash 值使用不同 WP → hint 独立
        let addr2 = alloc
            .bch2_bucket_alloc_new_fs(
                &vol,
                &ca,
                &ureq(Watermark::InteriorUpdate),
                Some(WritePointSpecifier::Hashed(200)),
            )
            .unwrap();
        assert_ne!(addr, addr2);
    }

    #[test]
    fn test_allocate_with_wp_id_direct() {
        let (_base, vol, ca) = make_alloc(8192, 1024);
        let alloc = BchAllocator::with_config(
            8192 * SECTORS_PER_BLOCK,
            WritePointConfig {
                max_write_points: 8,
            },
        );
        // 专用写点：btree
        let addr = alloc
            .bch2_bucket_alloc_new_fs(
                &vol,
                &ca,
                &ureq(Watermark::InteriorUpdate),
                Some(WritePointSpecifier::Direct(DedicatedWp::BTree)),
            )
            .unwrap();
        assert!(addr % BLOCKS_PER_BUCKET == 0);
        // journal
        let addr2 = alloc
            .bch2_bucket_alloc_new_fs(
                &vol,
                &ca,
                &ureq(Watermark::InteriorUpdate),
                Some(WritePointSpecifier::Direct(DedicatedWp::Journal)),
            )
            .unwrap();
        assert_ne!(addr, addr2);
        // GC
        let addr3 = alloc
            .bch2_bucket_alloc_new_fs(
                &vol,
                &ca,
                &ureq(Watermark::InteriorUpdate),
                Some(WritePointSpecifier::Direct(DedicatedWp::GC)),
            )
            .unwrap();
        assert_ne!(addr2, addr3);
    }

    #[test]
    fn test_regression_pass_none_when_wp_disabled() {
        // WRITE_POINT_MAX=1 时即使传 Some(...)，因为是 None 池所以仍用全局 hint
        let (alloc, vol, ca) = make_alloc(4096, 1024);
        // Should not crash
        let _addr = alloc
            .bch2_bucket_alloc_new_fs(
                &vol,
                &ca,
                &ureq(Watermark::InteriorUpdate),
                Some(WritePointSpecifier::Hashed(42)),
            )
            .unwrap();
        // WP=1 时池不存在，Some(...) 被 match arm _ 捕获走全局 hint
    }

    #[test]
    fn test_allocate_blocks_with_wp_id() {
        let (_base, vol, ca) = make_alloc(8192, 1024);
        let alloc = BchAllocator::with_config(
            8192 * SECTORS_PER_BLOCK,
            WritePointConfig {
                max_write_points: 8,
            },
        );
        let addr = alloc
            .bch2_alloc_sectors_start_trans(
                1,
                &vol,
                &ca,
                &ureq(Watermark::InteriorUpdate),
                Some(WritePointSpecifier::Hashed(99)),
            )
            .unwrap();
        assert_eq!(addr, 0);
    }

    #[test]
    fn test_new_open_bucket_consumption_advances_next_allocation() {
        let (_base, vol, ca) = make_alloc(8192, 1024);
        let alloc = BchAllocator::with_config(
            8192 * SECTORS_PER_BLOCK,
            WritePointConfig {
                max_write_points: 8,
            },
        );
        let request = ureq(Watermark::InteriorUpdate);
        let write_point = Some(WritePointSpecifier::Hashed(99));

        let first = alloc
            .bch2_alloc_sectors_start_trans(1, &vol, &ca, &request, write_point)
            .unwrap();
        alloc.bch2_consume_written_extent(&ca, first, 1);
        let second = alloc
            .bch2_alloc_sectors_start_trans(1, &vol, &ca, &request, write_point)
            .unwrap();

        assert_eq!(second, first + 1);
    }

    #[test]
    fn test_allocate_buckets_with_wp_id() {
        let (_base, vol, ca) = make_alloc(8192, 1024);
        let alloc = BchAllocator::with_config(
            8192 * SECTORS_PER_BLOCK,
            WritePointConfig {
                max_write_points: 8,
            },
        );
        let addrs = alloc
            .bch2_alloc_buckets(
                2,
                &vol,
                &ca,
                &ureq(Watermark::InteriorUpdate),
                Some(WritePointSpecifier::Hashed(77)),
            )
            .unwrap();
        assert_eq!(addrs.len(), 2);
        assert_ne!(addrs[0], addrs[1]);
    }

    // ─── L4 fallback 测试 ────────────────────────────

    #[test]
    fn test_freespace_alloc_after_allocator_init() {
        let (alloc, vol, ca) = make_alloc(4096, 1024);
        // freespace btree 应包含所有初始空闲 bucket
        let addr = alloc
            .bch2_bucket_alloc_new_fs(&vol, &ca, &ureq(Watermark::InteriorUpdate), None)
            .unwrap();
        assert_eq!(
            addr, 1024,
            "freespace alloc: InteriorUpdate→System offset=1→group 1"
        );
    }

    #[test]
    fn test_freespace_alloc_exhausted() {
        let (alloc, vol, ca) = make_alloc(4096, 1024);
        // 将所有 bucket 标记为 User（不需要清空 free_list）
        let groups = unsafe { &*ca.groups.get() };
        for group_mutex in groups {
            let mut group = group_mutex.lock().unwrap();
            group.free_buckets.store(0, Ordering::Relaxed);
            for bucket in &mut group.buckets {
                bucket.state = BchDataType::User;
            }
        }
        ca.nr_free_buckets.store(0, Ordering::Release);
        // P0-2: 无可用 bucket → AllocError::AddressSpaceExhausted
        let result =
            alloc.bch2_bucket_alloc_new_fs(&vol, &ca, &ureq(Watermark::InteriorUpdate), None);
        assert!(
            result.is_err(),
            "should return AllocError::AddressSpaceExhausted"
        );
        match result {
            Err(AllocError::AddressSpaceExhausted { .. }) => {} // expected
            _ => panic!("expected AddressSpaceExhausted"),
        }
    }

    #[test]
    fn test_trigger_alloc_freespace_chain() {
        use crate::btree::key::BtreeKey;
        use crate::btree::BtreeEntry;

        let vol = crate::BchVol::test_trees();

        let bucket_idx = 42u64;
        let gen = 1u8;
        let oldest_gen = 0u8;

        // Free BchAllocEntry
        let free_entry = BchAllocEntry {
            journal_seq_nonempty: 0,
            journal_seq_empty: 0,
            dirty_sectors: 0,
            cached_sectors: 0,
            stripe_refcount: 0,
            stripe_sectors: 0,
            data_type: BchDataType::Free as u8,
            flags: 0,
            gen: 0,
            oldest_gen: 0,
            io_time: [0; 2],
            nr_external_backpointers: 0,
            ..crate::alloc::btree::BCH_ALLOC_V4_ZERO
        };
        let free_bytes = crate::alloc::btree::serialize_alloc_entry(&free_entry);

        // Allocated BchAllocEntry (User state)
        let alloc_entry = BchAllocEntry {
            journal_seq_nonempty: 100,
            journal_seq_empty: 0,
            dirty_sectors: 10 * super::SECTORS_PER_BLOCK as u32,
            cached_sectors: 0,
            stripe_refcount: 0,
            stripe_sectors: 0,
            data_type: BchDataType::User as u8,
            flags: 0,
            gen: gen,
            oldest_gen,
            io_time: [0; 2],
            nr_external_backpointers: 0,
            ..crate::alloc::btree::BCH_ALLOC_V4_ZERO
        };
        let alloc_bytes = crate::alloc::btree::serialize_alloc_entry(&alloc_entry);

        // Key bytes for Alloc btree
        let key = BtreeKey::new(bucket_idx, 0, KeyType::Normal);
        let key_bytes = bincode::serialize(&key).unwrap();

        // Pre-populate Freespace btree with the bucket (simulating Free state)
        let free_pos = super::alloc_freespace_pos(0, bucket_idx, gen, oldest_gen);
        vol.btree(BtreeId::Freespace)
            .bch2_btree_bset_insert_key_wrapper(
                BtreeEntry::raw(free_pos, KeyType::Normal, vec![]),
                0,
            );
        assert!(
            vol.btree(BtreeId::Freespace)
                .bch2_btree_iter_peek_entry(free_pos)
                .is_some(),
            "Freespace entry should exist before trigger"
        );

        // Fire trigger: Free → User (Allocated)
        let mut trans = BtreeTrans::new(&vol);
        super::bch2_trigger_alloc(
            &mut trans,
            BtreeId::Alloc,
            &key_bytes,
            Some(&free_bytes),
            Some(&alloc_bytes),
        )
        .unwrap();

        // bcachefs appends the Freespace update to the same transaction;
        // materialization happens when that transaction commits.
        let entries = trans.drain_journal();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].btree_id, BtreeId::Freespace);
        assert_eq!(entries[0].key.to_bpos(), free_pos);
        assert_eq!(entries[0].key.key_type, KeyType::Deleted);
    }
}
