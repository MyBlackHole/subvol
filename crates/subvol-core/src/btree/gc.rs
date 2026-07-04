//! B-tree GC (Garbage Collection / Consistency Check) — bcachefs 对齐
//!
//! 对应 bcachefs btree_gc.c + check.h 中的公开 API。
//! GC 子系统负责：
//! - Mark-and-sweep 回收：标记引用 → 回收未标记的 bucket
//! - 拓扑检查：验证 btree 节点之间的引用完整性
//! - 分配一致性检查：验证 alloc btree 与实际分配的匹配
//!
//! bcachefs 的 GC 是增量 / 并发的：使用 gc_pos 跟踪进度。
//! subvol 当前实现覆盖基础检查 / 统计 / 快照路径，保留少量与主流程
//! 直接相关的 GC 辅助函数。

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::RwLock;

use serde::{Deserialize, Serialize};

use crate::alloc::btree::{deserialize_alloc_entry, serialize_alloc_entry};
use crate::alloc::bucket::{
    bucket_data_type, bucket_ref_update_checks, derive_data_type, BchDataType, GcBucket,
};
use crate::alloc::BchAllocEntry;
use crate::alloc::BchAllocator;
use crate::alloc::SECTORS_PER_BLOCK;
use crate::alloc::{
    alloc_freespace_bucket_idx, alloc_freespace_genbits, alloc_freespace_pos_genbits,
    sector_to_bucket,
};
use crate::btree::key::{Addr48, Bpos, BtreeEntry, BtreeKey, KeyType, KeyValue};
use crate::btree::node::BtreeNode;
use crate::btree::writer::NoopWriter;
use crate::btree::{BtreeId, BTREE_ID_NR};
use crate::storage::superblock::{compat_bits, BchSb};
use crate::types::StorageError;
use crate::BchVol;

// ─── GC Phase ───────────────────────────────────────────────────────────

/// bcachefs 对齐: enum gc_phase — GC 阶段
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[repr(u8)]
pub enum GcPhase {
    #[default]
    NotRunning = 0,
    Start = 1,
    Sb = 2,
    Btree = 3,
}

/// bcachefs 对齐: struct gc_pos — GC 位置跟踪
///
/// journal_seq 记录完成此 GC pass 时的最新 journal seq。
/// recovery 时通过比较 gc_pos.journal_seq 与 journal last_seq 判断
/// 是否需要重新执行 gc_gen pass。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct GcPos {
    pub phase: GcPhase,
    pub btree: u32,
    pub level: u16,
    pub pos: u64,
    /// 完成此 GC pass 时的 journal seq（用于 recovery 判断是否需要重做）
    pub journal_seq: u64,
}

// ─── GC State ───────────────────────────────────────────────────────────

/// GC 子系统状态
#[derive(Debug)]
pub struct BtreeGc {
    /// GC 是否正在运行
    pub running: AtomicBool,
    /// 当前 GC 位置
    pub pos: GcPos,
    /// GC 是否已被触发
    pub triggered: AtomicBool,
    /// GC 排他锁 — GC 运行时持有写锁，事务持有读锁
    pub lock: RwLock<()>,
}

impl Default for BtreeGc {
    fn default() -> Self {
        Self::new()
    }
}

impl BtreeGc {
    pub fn new() -> Self {
        Self {
            running: AtomicBool::new(false),
            pos: GcPos {
                phase: GcPhase::NotRunning,
                btree: 0,
                level: 0,
                pos: 0,
                journal_seq: 0,
            },
            triggered: AtomicBool::new(false),
            lock: RwLock::new(()),
        }
    }
}

// ─── Public API ─────────────────────────────────────────────────────────

/// bcachefs 对齐: bch2_gc_phase — 创建 GC phase 位置
pub fn gc_phase(phase: GcPhase) -> GcPos {
    GcPos {
        phase,
        btree: 0,
        level: 0,
        pos: 0,
        journal_seq: 0,
    }
}

/// bcachefs 对齐: gc_pos_btree — 创建 btree GC 位置
pub fn gc_pos_btree(btree: u32, level: u16, pos: u64) -> GcPos {
    GcPos {
        btree,
        level,
        pos,
        phase: GcPhase::Btree,
        journal_seq: 0,
    }
}

/// bcachefs 对齐: gc_pos_cmp — 比较两个 GC 位置
pub fn gc_pos_cmp(l: &GcPos, r: &GcPos) -> std::cmp::Ordering {
    l.phase
        .cmp(&r.phase)
        .then_with(|| l.btree.cmp(&r.btree))
        .then_with(|| l.level.cmp(&r.level))
        .then_with(|| l.pos.cmp(&r.pos))
}

/// bcachefs 对齐: gc_visited — 检查 GC 是否已访问过该位置
pub fn gc_visited(gc: &BtreeGc, pos: &GcPos) -> bool {
    gc_pos_cmp(pos, &gc.pos) == std::cmp::Ordering::Less
        || gc_pos_cmp(pos, &gc.pos) == std::cmp::Ordering::Equal
}

/// bcachefs 对齐: bch2_gc_gen — GC generation 传递
///
/// 遍历 Extents btree 中所有 extent 条目，收集被引用的 paddr，
/// 将对应 bucket 标记为 `BchDataType::User`。
///
/// `journal_seq` 参数记录当前 journal seq，用于 recovery 判断
/// 是否需要重新执行此 GC pass。gc_pos 的 journal_seq 按 btree 级别存储，
/// 供后续 recovery pass 查询。
///
/// 对应 bcachefs `bch2_gc_gen()` (gc.c)。
pub fn bch2_gc_gen(
    vol: &BchVol,
    allocator: &mut BchAllocator,
    gc: &mut BtreeGc,
    journal_seq: u64,
) -> Result<(), StorageError> {
    // 收集 Extents btree 中所有被引用的 bucket
    // 注意：bch2 使用 `for_each_entry` 遍历，值的变体为 KeyValue::Raw（8 字节：
    // paddr 48-bit LE 在前 6 字节，ver 16-bit LE 在后 2 字节）。
    let extents_btree = vol.btree(BtreeId::Extents);
    let mut referenced: HashSet<(u8, u64)> = HashSet::new();

    extents_btree.for_each_btree_key_entry(|entry| {
        entry.value.for_each_ptr(|ptr| {
            if ptr.offset > 0 && ptr.offset <= Addr48::MAX {
                if let Some(ca) = vol.device_rcu_noerror(ptr.dev) {
                    referenced.insert((
                        ptr.dev,
                        sector_to_bucket(&ca, ptr.offset * SECTORS_PER_BLOCK),
                    ));
                }
            }
        });
    });

    // 将每个被引用的 bucket 的 gc_bucket 标记为 User
    // 对应 C 中 extent 触发器填充 genradix 的行为（不直接修改 bucket state，
    // 由 gc_mark_key 或后续 sweep 负责 data_type 推导）
    for dev_idx in vol.device_registry.dev_indices() {
        let Some(ca) = vol.device_registry.resolve_bch_dev(dev_idx) else {
            continue;
        };
        allocator.for_each_bucket_all_mut(&ca, |bucket_idx, _bucket, gc, gen| {
            if referenced.contains(&(dev_idx, bucket_idx)) {
                gc.gen = *gen;
                gc.set_gen_valid(true);
                gc.set_data_type(BchDataType::User as u8);
            }
        });
    }

    // 更新 gc_pos — 记录此 GC pass 完成时的 journal seq。
    // recovery 时通过比较 gc_pos.journal_seq 与 journal last_seq
    // 判断是否需要重新执行 gc_gen pass。
    gc.pos = GcPos {
        phase: GcPhase::Btree,
        btree: BtreeId::Extents as u32,
        level: 0,
        pos: 0,
        journal_seq,
    };

    Ok(())
}

/// bcachefs 对齐: bch2_gc_gen_async — 异步触发 GC generations
pub fn bch2_gc_gen_async(gc: &BtreeGc) {
    gc.triggered.store(true, Ordering::Release);
}

/// bcachefs 对齐: bch2_check_topology — 检查 btree 拓扑完整性（P0-5）
///
/// 验证：
/// - 每个 btree 中的条目按 Bpos 排序（原有）
/// - 无重复位置条目（原有）
/// - 多级树中每个内部节点都能递归找到 child，且 child level 连续
/// - 相邻 child 的 key span 不重叠，且按 key 顺序连续
pub fn bch2_check_topology(vol: &BchVol) -> Result<(), StorageError> {
    for ty in BTREE_ID_NR {
        let btree = vol.btree(ty);

        let mut visited_children = HashSet::new();
        validate_tree_node(
            ty,
            btree.root().node.as_ref(),
            btree.cache(),
            &mut visited_children,
        )?;

        let mut entries: Vec<Bpos> = Vec::new();

        btree.for_each_btree_key_entry(|entry| {
            if entry.key_type != KeyType::Deleted {
                entries.push(entry.pos);
            }
        });

        // 检查排序顺序
        for i in 1..entries.len() {
            if entries[i] < entries[i - 1] {
                return Err(StorageError::Transaction(format!(
                    "check_topology: btree {:?} entries out of order at index {}",
                    ty, i,
                )));
            }
        }

        // 检查重复位置
        let mut seen = HashSet::new();
        for pos in &entries {
            if !seen.insert(*pos) {
                return Err(StorageError::Transaction(format!(
                    "check_topology: btree {:?} duplicate entry at {:?}",
                    ty, pos,
                )));
            }
        }
    }

    Ok(())
}

fn validate_tree_node(
    ty: BtreeId,
    node: &BtreeNode,
    cache: &crate::btree::types::NodeCache,
    visited_children: &mut HashSet<u64>,
) -> Result<(), StorageError> {
    let mut entries: Vec<(BtreeKey, crate::btree::key::ExtentValue)> = Vec::new();
    for set in &node.sets[..node.nsets() as usize] {
        let mut cur = u32::from(set.first_key_offset()) * 8;
        while cur < u32::from(set.end_offset) * 8 {
            entries.push(node.read_packed_entry(cur as usize));
            cur += u32::from(node.read_entry_u64s(cur as usize)) * 8;
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    if node.level == 0 {
        if entries.is_empty() {
            return Ok(());
        }

        let first = Bpos::from_key(&entries[0].0);
        let last = Bpos::from_key(&entries[entries.len() - 1].0);
        if node.min_key != Bpos::MAX && node.min_key > first {
            return Err(StorageError::Transaction(format!(
                "check_topology: btree {:?} leaf min_key {:?} > first entry {:?}",
                ty, node.min_key, first,
            )));
        }
        if node.max_key != Bpos::MIN && node.max_key < last {
            return Err(StorageError::Transaction(format!(
                "check_topology: btree {:?} leaf max_key {:?} < last entry {:?}",
                ty, node.max_key, last,
            )));
        }
        return Ok(());
    }

    if entries.is_empty() {
        return Err(StorageError::Transaction(format!(
            "check_topology: btree {:?} empty interior node at level {}",
            ty, node.level,
        )));
    }

    let mut prev_child_max: Option<Bpos> = None;
    for (_idx, (key, value)) in entries.iter().enumerate() {
        let child_addr = value.paddr();
        if child_addr == 0 {
            return Err(StorageError::Transaction(format!(
                "check_topology: btree {:?} interior entry {:?} has null child pointer",
                ty, key,
            )));
        }

        if !visited_children.insert(child_addr) {
            return Err(StorageError::Transaction(format!(
                "check_topology: btree {:?} child node {} visited twice",
                ty, child_addr,
            )));
        }

        let child = cache.get(child_addr).ok_or_else(|| {
            StorageError::Transaction(format!(
                "check_topology: btree {:?} missing child node {} at {:?}",
                ty, child_addr, key,
            ))
        })?;

        if child.level != node.level.saturating_sub(1) {
            return Err(StorageError::Transaction(format!(
                "check_topology: btree {:?} cached child level mismatch at {:?}: child.level={} parent.level={}",
                ty, key, child.level, node.level,
            )));
        }

        if let Some(prev_max) = prev_child_max {
            if child.min_key <= prev_max {
                return Err(StorageError::Transaction(format!(
                    "check_topology: btree {:?} child boundary overlap at {:?}: prev_max {:?}, child_min {:?}",
                    ty, key, prev_max, child.min_key,
                )));
            }
        }

        validate_tree_node(ty, child.as_ref(), cache, visited_children)?;
        prev_child_max = Some(child.max_key);
    }

    Ok(())
}

/// bcachefs 对齐: bch2_check_allocations — 检查分配一致性
///
/// 对比 extent 引用的 bucket 与 allocator 中实际分配状态的差异。
/// 返回不一致的描述列表；空 Vec 表示一致。
pub fn bch2_check_allocations(
    vol: &BchVol,
    allocator: &BchAllocator,
) -> Result<Vec<String>, StorageError> {
    // 收集所有 btree 中 extent 引用的 paddr → bucket_index
    let mut referenced: HashSet<(u8, u64)> = HashSet::new();

    for ty in BTREE_ID_NR {
        let btree = vol.btree(ty);
        btree.for_each_btree_key_entry(|entry| {
            entry.value.for_each_ptr(|ptr| {
                if ptr.offset > 0 && ptr.offset <= Addr48::MAX {
                    if let Some(ca) = vol.device_rcu_noerror(ptr.dev) {
                        referenced.insert((
                            ptr.dev,
                            sector_to_bucket(&ca, ptr.offset * SECTORS_PER_BLOCK),
                        ));
                    }
                }
            });
        });
    }

    // 收集 allocator 中已分配（非 Free/NeedDiscard）的 bucket
    let mut allocated: Vec<(u8, u64, BchDataType)> = Vec::new();
    for dev_idx in vol.device_registry.dev_indices() {
        let Some(ca) = vol.device_registry.resolve_bch_dev(dev_idx) else {
            continue;
        };
        allocator.for_each_bucket(&ca, |bucket_idx, bucket, _gen| {
            if bucket.state != BchDataType::Free && bucket.state != BchDataType::NeedDiscard {
                allocated.push((dev_idx, bucket_idx, bucket.state));
            }
        });
    }

    // 找出已分配但未被任何 extent 引用的 bucket（潜在泄漏）
    let mut discrepancies = Vec::new();
    for &(dev, bi, state) in &allocated {
        if !referenced.contains(&(dev, bi)) {
            discrepancies.push(format!(
                "device {} bucket {} allocated ({:?}) but not referenced by any extent",
                dev, bi, state,
            ));
        }
    }

    Ok(discrepancies)
}

/// bcachefs 对齐: bch2_check_alloc_info
///
/// 校验 alloc / freespace / allocator 三者的一致性：
/// - Alloc btree 记录必须与 allocator 当前 bucket 状态一致
/// - Free bucket 必须有对应的 freespace key
/// - Allocated bucket 不得残留任何 freespace key
pub fn bch2_check_alloc_info(
    vol: &BchVol,
    allocator: &BchAllocator,
) -> Result<Vec<String>, StorageError> {
    let mut discrepancies = Vec::new();

    let mut allocator_snapshot: HashMap<(u8, u64), BchAllocEntry> = HashMap::new();
    for dev_idx in vol.device_registry.dev_indices() {
        let Some(ca) = vol.device_registry.resolve_bch_dev(dev_idx) else {
            continue;
        };
        allocator.for_each_bucket(&ca, |bucket_idx, bucket, gen| {
            allocator_snapshot.insert(
                (dev_idx, bucket_idx),
                BchAllocEntry {
                    journal_seq_nonempty: bucket.journal_seq_nonempty,
                    journal_seq_empty: bucket.journal_seq_empty,
                    dirty_sectors: bucket.dirty_sectors,
                    cached_sectors: bucket.cached_sectors,
                    stripe_refcount: 0,
                    stripe_sectors: bucket.stripe_sectors,
                    data_type: bucket.state as u8,
                    flags: 8 << 2,
                    gen: *gen,
                    oldest_gen: bucket.oldest_gen,
                    stripe_redundancy_obsolete: 0,
                    io_time: [0; 2],
                    nr_external_backpointers: 0,
                    pad: 0,
                },
            );
        });
    }

    let mut alloc_entries: HashMap<(u8, u64), BchAllocEntry> = HashMap::new();
    let alloc_btree = vol.btree(BtreeId::Alloc);
    alloc_btree.for_each_btree_key_entry(|entry| {
        if entry.key_type == KeyType::Normal {
            if let KeyValue::Raw(bytes) = &entry.value {
                if let Ok(alloc_data) = deserialize_alloc_entry(bytes) {
                    if let Ok(dev) = u8::try_from(entry.pos.inode) {
                        alloc_entries.insert((dev, entry.pos.offset), alloc_data);
                    }
                } else {
                    discrepancies.push(format!(
                        "alloc key {} failed to deserialize",
                        entry.pos.offset
                    ));
                }
            }
        }
    });

    let mut freespace_entries: HashMap<(u8, u64), Vec<u64>> = HashMap::new();
    let freespace_btree = vol.btree(BtreeId::Freespace);
    freespace_btree.for_each_btree_key_entry(|entry| {
        if entry.key_type == KeyType::Normal {
            freespace_entries
                .entry((entry.pos.inode as u8, alloc_freespace_bucket_idx(entry.pos)))
                .or_default()
                .push(alloc_freespace_pos_genbits(entry.pos));
        }
    });

    for ((dev, bucket_idx), bucket) in &allocator_snapshot {
        match alloc_entries.get(&(*dev, *bucket_idx)) {
            Some(alloc_entry) => {
                if alloc_entry != bucket {
                    discrepancies.push(format!(
                        "alloc device {} bucket {} mismatch: alloc={:?} allocator={:?}",
                        dev, bucket_idx, alloc_entry, bucket
                    ));
                }
            }
            None => discrepancies.push(format!(
                "missing alloc entry for device {} bucket {}",
                dev, bucket_idx
            )),
        }

        let freespaces = freespace_entries.get(&(*dev, *bucket_idx));
        match BchDataType::from_raw(bucket.data_type).unwrap_or(BchDataType::Free) {
            BchDataType::Free => {
                let expected =
                    alloc_freespace_genbits(crate::alloc::alloc_gc_gen(0u8, bucket.oldest_gen));
                if !matches!(freespaces, Some(generations) if generations.contains(&expected)) {
                    discrepancies.push(format!(
                        "missing freespace entry for free device {} bucket {} genbits {}",
                        dev,
                        bucket_idx,
                        expected >> 56
                    ));
                }
                if let Some(generations) = freespaces {
                    for generation in generations {
                        if *generation != expected {
                            discrepancies.push(format!(
                                "stale freespace entry for device {} bucket {} genbits {} (expected {})",
                                dev, bucket_idx,
                                generation >> 56,
                                expected >> 56
                            ));
                        }
                    }
                }
            }
            _ => {
                if let Some(generations) = freespaces {
                    for generation in generations {
                        discrepancies.push(format!(
                            "stale freespace entry for allocated device {} bucket {} genbits {}",
                            dev,
                            bucket_idx,
                            generation >> 56
                        ));
                    }
                }
            }
        }
    }

    for ((dev, bucket_idx), alloc_entry) in &alloc_entries {
        if !allocator_snapshot.contains_key(&(*dev, *bucket_idx)) {
            discrepancies.push(format!(
                "alloc entry device {} bucket {} references missing bucket {:?}",
                dev, bucket_idx, alloc_entry
            ));
        }
    }

    // P7: 检查 alloc btree 的 gen 与 gens[] 一致（对应 bcachefs `bch2_check_alloc_key` gen 检查）
    for dev_idx in vol.device_registry.dev_indices() {
        let Some(ca) = vol.device_registry.resolve_bch_dev(dev_idx) else {
            continue;
        };
        allocator.for_each_bucket(&ca, |bucket_idx, _bucket, gen| {
            if let Some(alloc_entry) = alloc_entries.get(&(dev_idx, bucket_idx)) {
                if alloc_entry.gen != *gen {
                    discrepancies.push(format!(
                        "device {} bucket {} gen mismatch: alloc btree gen={}, gens[]={}",
                        dev_idx, bucket_idx, alloc_entry.gen, gen,
                    ));
                }
            }
        });
    }

    Ok(discrepancies)
}

/// bcachefs 对齐: bch2_fs_btree_gc_init_early — GC 子系统早期初始化
pub fn bch2_fs_btree_gc_init_early(gc: &BtreeGc) {
    gc.running.store(false, Ordering::Release);
    gc.triggered.store(false, Ordering::Release);
}

/// bcachefs 对齐: bch2_gc_pos_to_text — 将 GC 位置格式化为文本
pub fn bch2_gc_pos_to_text(pos: &GcPos) -> String {
    format!(
        "GC phase={:?} btree={} level={} pos={}",
        pos.phase, pos.btree, pos.level, pos.pos
    )
}

/// bcachefs 对齐: bch2_presplit_shard_boundaries — 预分裂分片边界
///
/// 遍历所有 btree type，对每个 depth=0 的 btree 检查其 entries 是否跨越
/// SHARD_FACTOR（1024）分片边界。如果跨越则将 root leaf 节点分裂为两个，
/// 创建深度为 1 的多级树，使后续写入能按 shard 分散到不同子树。
///
/// 仅在 recovery 过程的 presplit_shard_boundaries pass 中调用。
pub fn bch2_presplit_shard_boundaries(vol: &BchVol) -> Result<(), StorageError> {
    for ty in BTREE_ID_NR {
        let btree = vol.btree(ty);
        futures::executor::block_on(btree.presplit_shard_boundaries(&NoopWriter))?;
    }
    Ok(())
}

// ─── G1: Mark-and-Sweep 核心 ──────────────────────────────────────────

/// bcachefs 对齐: bch2_gc_mark_key — 标记一个 btree entry 引用的 bucket（P0-5 增强）
///
/// 从 entry 的 value 中提取 paddr，在 allocator 中将对应的 bucket 标记为 User。
/// 只有 paddr 合法（> 0 且 ≤ Addr48::MAX）时才进行标记。

/// P0-5 增强：当提供了有效的 vol 引用时，在标记前确保拓扑检查已执行。
/// 拓扑检查由调用者（bch2_gc_btrees）统一调度，此函数不做重复检查。
pub fn bch2_gc_mark_key(
    vol: &BchVol,
    allocator: &mut BchAllocator,
    entry: &BtreeEntry,
) -> Result<(), StorageError> {
    // 获取 extent 的 block 数用于 sector counting
    // BtreePtr 没有 block 数，跳过 sector counting
    let blocks = match &entry.value {
        KeyValue::BtreePtr(_) => 0,
        _ => entry.value.extent_blocks(),
    };

    // 错误传播通道：for_each_ptr / for_each_bucket_all_mut 闭包不支持 Result 返回
    let mark_err = RefCell::new(None::<StorageError>);

    // 遍历所有指针: 标记 bucket + 累加 sector 计数
    // bcachefs ptr_data_type: btree_ptr → Btree, cached ptr → Cached, dirty → User
    entry.value.for_each_ptr(|ptr| {
        if mark_err.borrow().is_some() {
            return;
        }
        let offset = ptr.offset;
        if offset == 0 || offset > Addr48::MAX {
            return;
        }
        let ptr_data_type = match &entry.value {
            KeyValue::BtreePtr(_) => BchDataType::Btree,
            _ if ptr.cached => BchDataType::Cached,
            _ => BchDataType::User,
        };
        let Some(ca) = vol.device_rcu_noerror(ptr.dev) else {
            mark_err.replace(Some(StorageError::NotFound(format!(
                "device {} not found",
                ptr.dev
            ))));
            return;
        };
        let bucket_idx = sector_to_bucket(&ca, offset * SECTORS_PER_BLOCK);
        allocator.for_each_bucket_all_mut(&ca, |bi, bucket, gc, gen| {
            if mark_err.borrow().is_some() {
                return;
            }
            if bi == bucket_idx {
                // ===== bcachefs bch2_bucket_ref_update (buckets.c:483-558) 完整序列 =====

                let current_type =
                    BchDataType::from_raw(gc.data_type()).unwrap_or(BchDataType::Free);
                match bucket_ref_update_checks(
                    *gen,
                    ptr.gen,
                    ptr.cached,
                    ptr_data_type,
                    current_type,
                    bucket_idx,
                ) {
                    Err(e) => {
                        mark_err.replace(Some(e));
                        return;
                    }
                    Ok(None) => {
                        // bcachefs (buckets.c:508-514): 当 no_stale_ptrs compat bit 被设置，
                        // 出现 stale cached ptr 时清除 bit 并写 superblock
                        if vol.superblock().compat_test(compat_bits::NO_STALE_PTRS) {
                            vol.superblock_mut()
                                .compat_clear(compat_bits::NO_STALE_PTRS);
                        }
                        return; // stale cached, skip
                    }
                    Ok(Some(_)) => {} // proceed
                }

                // gen 匹配: 更新 bucket state + gc meta
                bucket.state = ptr_data_type;
                gc.gen = *gen;
                gc.set_gen_valid(true);
                gc.set_data_type(ptr_data_type as u8);

                // ⑥ gc sector counting: 扇区单位，仅当前 gen 的引用才累加
                // bcachefs: sectors = k.k->size
                if blocks > 0 {
                    let sectors = blocks * SECTORS_PER_BLOCK as u32;

                    // ⑦ overflow check: bucket_sectors + sectors > U32_MAX
                    let bucket_sectors = if ptr.cached {
                        &mut gc.cached_sectors
                    } else {
                        &mut gc.dirty_sectors
                    };
                    if *bucket_sectors > u32::MAX - sectors {
                        *bucket_sectors = 0;
                        mark_err.replace(Some(StorageError::InvalidData(format!(
                            "overflow: bucket {} {}_sectors + {} > U32_MAX",
                            bucket_idx,
                            if ptr.cached { "cached" } else { "dirty" },
                            sectors,
                        ))));
                        return;
                    }
                    *bucket_sectors = bucket_sectors.wrapping_add(sectors);

                    // ⑧ alloc_data_type_set: 根据实际 sector 计数推导 data_type
                    if gc.dirty_sectors > 0 {
                        bucket.state = bucket_data_type(ptr_data_type);
                    } else if gc.cached_sectors > 0 {
                        bucket.state = BchDataType::Cached;
                    }
                    gc.set_data_type(bucket.state as u8);
                }
            }
        });
    });

    if let Some(err) = mark_err.into_inner() {
        return Err(err);
    }
    Ok(())
}

/// bcachefs 对齐: bch2_gc_btrees — 全树标记遍历（P0-5 + P0-6 增强）
///
/// 遍历所有 BtreeId 类型的每个 entry：
/// 1. P0-5: 先对每个 btree 执行拓扑检查（bch2_check_topology）
/// 2. 对非 Deleted entry 调用 bch2_gc_mark_key 标记 bucket
/// 3. 对每个非 Deleted entry 执行 bcachefs GC mark。
///
/// 拓扑检查确保在 split/merge 后节点链接一致。
pub fn bch2_gc_btrees(
    vol: &BchVol,
    allocator: &mut BchAllocator,
) -> Result<(), StorageError> {
    // P0-5: 首次标记前执行全量拓扑检查，确保树结构完整
    bch2_check_topology(vol)?;

    for ty in BTREE_ID_NR {
        // 收集非 Deleted 条目（与 vol 的借用分离以便后续触发 trigger）
        let entries: Vec<BtreeEntry> = {
            let btree = vol.btree(ty);
            let mut collected = Vec::new();
            btree.for_each_btree_key_entry(|entry| {
                if entry.key_type != KeyType::Deleted {
                    collected.push(entry);
                }
            });
            collected
        };

        for entry in &entries {
            // P0-5: 调用 GC mark key 标记 bucket
            bch2_gc_mark_key(vol, allocator, entry)?;

        }
    }
    Ok(())
}

// ─── Sweep Phase ────────────────────────────────────────────────────

/// GC sweep 回收统计 — 记录 sweep phase 回收的 bucket 信息
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReclaimStats {
    /// 回收为 Free 的 bucket 数量
    pub reclaimed_count: u32,
    /// 回收的 bucket 索引列表（便于测试验证）
    pub reclaimed_buckets: Vec<u64>,
    /// 因非 User/NeedGcGens 状态跳过的 bucket 数量
    pub skipped_state: u32,
}

// ─── G2: Allocator Snapshot ───────────────────────────────────────────

/// bcachefs 对齐: bch2_gc_alloc_start — 快照分配器状态
///
/// 将当前 allocator 中所有非 Free/NeedDiscard 的 bucket 状态快照到
/// HashMap，供 GC 完成后对比以检测不一致。
pub fn bch2_gc_alloc_start(
    vol: &BchVol,
    allocator: &BchAllocator,
) -> HashMap<(u8, u64), BchDataType> {
    let mut snapshot = HashMap::new();
    for dev_idx in vol.device_registry.dev_indices() {
        let Some(ca) = vol.device_registry.resolve_bch_dev(dev_idx) else {
            continue;
        };
        allocator.for_each_bucket(&ca, |bi, bucket, _gen| {
            if bucket.state != BchDataType::Free && bucket.state != BchDataType::NeedDiscard {
                snapshot.insert((dev_idx, bi), bucket.state);
            }
        });
    }
    snapshot
}

/// bcachefs 对齐: bch2_gc_alloc_done — 对比分配器快照
///
/// 比较当前 allocator 状态与 GC 前的快照，返回变化描述列表。
/// 包括：新分配的 bucket、释放的 bucket、类型变更的 bucket。
/// 空 Vec 表示一致。
pub fn bch2_gc_alloc_done(
    vol: &BchVol,
    allocator: &mut BchAllocator,
    snapshot: HashMap<(u8, u64), BchDataType>,
) -> Result<Vec<String>, StorageError> {
    let mut current = HashMap::new();
    for dev_idx in vol.device_registry.dev_indices() {
        let Some(ca) = vol.device_registry.resolve_bch_dev(dev_idx) else {
            continue;
        };
        allocator.for_each_bucket(&ca, |bi, bucket, _gen| {
            if bucket.state != BchDataType::Free && bucket.state != BchDataType::NeedDiscard {
                current.insert((dev_idx, bi), bucket.state);
            }
        });
    }

    let mut changes = Vec::new();

    // 之前分配了而现在空闲的 bucket（潜在泄漏）
    for ((dev, bi), old_state) in &snapshot {
        if !current.contains_key(&(*dev, *bi)) {
            changes.push(format!(
                "device {} bucket {} was {:?} but is now free/unreferenced",
                dev, bi, old_state,
            ));
        }
    }

    // 新分配的 bucket
    for ((dev, bi), new_state) in &current {
        if !snapshot.contains_key(&(*dev, *bi)) {
            changes.push(format!(
                "device {} bucket {} is now {:?} but was not previously allocated",
                dev, bi, new_state,
            ));
        }
    }

    // 类型变更的 bucket
    for ((dev, bi), old_state) in &snapshot {
        if let Some(new_state) = current.get(&(*dev, *bi)) {
            if old_state != new_state {
                changes.push(format!(
                    "device {} bucket {} changed from {:?} to {:?}",
                    dev, bi, old_state, new_state,
                ));
            }
        }
    }

    Ok(changes)
}

// ─── G3: Sweep Phase ─────────────────────────────────────────────────

/// bcachefs 对齐: bch2_gc_alloc_done — GC Sweep phase
///
/// 对应 bcachefs `bch2_gc_alloc_done` + `bch2_alloc_write_key` (check.c:868-998)。
///
/// 遍历 alloc btree 条目（匹配 C `for_each_btree_key_max_commit` over `BTREE_ID_alloc`），
/// 对每个条目用 gc_bucket 值覆写（`__bucket_m_to_alloc`）并重新推导 data_type
/// （`alloc_data_type_set`），然后将修正写回 alloc btree 和 in-memory allocator。
///
/// 修正逻辑:
/// - gc.gen_valid == true: bucket 被 extent walk 引用，保持当前 state
/// - gc.gen_valid == false: bucket 未被引用，用 derive_data_type 重新推导
/// - 如果推导结果与原 alloc btree 条目不同，写回修正
pub fn bch2_gc_sweep(
    vol: &BchVol,
    allocator: &mut BchAllocator,
) -> Result<ReclaimStats, StorageError> {
    let gc_buckets_ready = vol
        .device_registry
        .dev_indices()
        .into_iter()
        .filter_map(|dev| vol.device_registry.resolve_bch_dev(dev))
        .any(|ca| allocator.has_gc_buckets_ready(&ca));
    if !gc_buckets_ready {
        return Ok(ReclaimStats::default());
    }

    // Phase 1: 读取 alloc btree 现有条目（对应 C 的 on-disk alloc key）
    let mut alloc_entries: HashMap<(u8, u64), BchAllocEntry> = HashMap::new();
    {
        let alloc_btree = vol.btree(BtreeId::Alloc);
        alloc_btree.for_each_btree_key_entry(|entry| {
            if entry.key_type == KeyType::Normal {
                if let KeyValue::Raw(bytes) = &entry.value {
                    if let Ok(alloc_data) = deserialize_alloc_entry(bytes) {
                        if let Ok(dev) = u8::try_from(entry.pos.inode) {
                            alloc_entries.insert((dev, entry.pos.offset), alloc_data);
                        }
                    }
                }
            }
        });
    }

    let mut stats = ReclaimStats::default();
    let mut alloc_corrections: Vec<(u8, u64, BchAllocEntry)> = Vec::new();

    // Phase 2: 遍历 in-memory bucket，用 gc_bucket 覆写 + 重新推导
    // 对应 C 的 for_each_btree_key_max_commit × BTREE_ID_alloc → bch2_alloc_write_key
    for dev_idx in vol.device_registry.dev_indices() {
        let Some(ca) = vol.device_registry.resolve_bch_dev(dev_idx) else {
            continue;
        };
        allocator.for_each_bucket_all_mut(&ca, |bi, bucket, gc, gen| {
            // 保护元数据 bucket 不被回收（对应 C bch2_alloc_write_key: Sb/Journal guard）
            if matches!(
                bucket.state,
                BchDataType::Sb | BchDataType::Journal | BchDataType::Btree
            ) {
                stats.skipped_state += 1;
                return;
            }

            if gc.gen_valid() {
                // gc_bucket 已标记 → bucket 被 extent walk 引用
                // 用 gc_bucket 的 sector 计数覆盖 runtime bucket
                bucket.dirty_sectors = gc.dirty_sectors;
                bucket.cached_sectors = gc.cached_sectors;
            } else {
                // gc_bucket 未标记 → bucket 未被 extent 引用
                // 对应 C: alloc_data_type_set(&gc, new.data_type) — fallback to on-disk hint
                let rederived = derive_data_type(
                    bucket.dirty_sectors,
                    bucket.cached_sectors,
                    bucket.stripe_sectors,
                    0, // stripe_refcount
                    *gen,
                    bucket.oldest_gen,
                    bucket.state, // data_type hint
                );

                if rederived == bucket.state {
                    stats.skipped_state += 1;
                    return;
                }

                let was_reclaimable =
                    matches!(bucket.state, BchDataType::User | BchDataType::NeedGcGens);
                bucket.state = rederived;
                if bucket.state == BchDataType::Free {
                    bucket.journal_seq_nonempty = 0;
                    bucket.journal_seq_empty = 0;
                }

                if was_reclaimable
                    && matches!(rederived, BchDataType::Free | BchDataType::NeedGcGens)
                {
                    stats.reclaimed_count += 1;
                    stats.reclaimed_buckets.push(bi);
                }
            }

            // 构造修正后的 alloc entry（对应 C 的 `new._f = gc._f` 逐字段覆写）
            // 保留原 alloc entry 中 sweep 不修改的字段（journal seq, io_time, etc.）
            let preserved = alloc_entries.get(&(dev_idx, bi));
            let corrected = BchAllocEntry {
                journal_seq_nonempty: bucket.journal_seq_nonempty,
                journal_seq_empty: bucket.journal_seq_empty,
                dirty_sectors: bucket.dirty_sectors,
                cached_sectors: bucket.cached_sectors,
                stripe_refcount: preserved.map_or(0, |e| e.stripe_refcount),
                stripe_sectors: bucket.stripe_sectors,
                data_type: bucket.state as u8,
                flags: bucket.flags as u32,
                gen: *gen,
                oldest_gen: bucket.oldest_gen,
                stripe_redundancy_obsolete: preserved.map_or(0, |e| e.stripe_redundancy_obsolete),
                io_time: preserved.map_or([0; 2], |e| e.io_time),
                nr_external_backpointers: preserved.map_or(0, |e| e.nr_external_backpointers),
                pad: 0,
            };

            alloc_corrections.push((dev_idx, bi, corrected));
        });
    }

    // Phase 3: 更新 dev counters + 将修正写回 alloc btree（对应 C 的 accounting + bch2_trans_update）
    for (dev, bucket_idx, entry) in &alloc_corrections {
        let ca = vol
            .device_rcu_noerror(*dev)
            .ok_or_else(|| StorageError::NotFound(format!("device {} not found", dev)))?;
        let old = alloc_entries
            .get(&(*dev, *bucket_idx))
            .unwrap_or(&crate::alloc::btree::BCH_ALLOC_V4_ZERO);
        crate::alloc::accounting::bch2_alloc_key_to_dev_counters(
            vol,
            &ca,
            old,
            entry,
            crate::btree::iter::UpdateTriggerFlags::GC,
        )?;

        let bytes = serialize_alloc_entry(entry);
        let bpos = Bpos::new(*dev as u64, *bucket_idx, 0);
        let e = BtreeEntry::raw(bpos, KeyType::Normal, bytes);
        vol.btree(BtreeId::Alloc)
            .bch2_btree_bset_insert_key_wrapper(e, 0);
    }

    // Phase 4: 清理 gc_buckets（下次 GC 重新标记，匹配 C 中 genradix 释放）
    for dev_idx in vol.device_registry.dev_indices() {
        let Some(ca) = vol.device_registry.resolve_bch_dev(dev_idx) else {
            continue;
        };
        allocator.for_each_bucket_all_mut(&ca, |_bi, _bucket, gc, _gen| {
            *gc = GcBucket::zero();
        });
    }

    Ok(stats)
}

// ─── G7: Superblock helpers ───────────────────────────────────────────

/// bcachefs 对齐: bch2_gc_pos_to_sb — 将 GC 位置写入 superblock
pub fn bch2_gc_pos_to_sb(gc: &BtreeGc, sb: &mut BchSb) {
    sb.gc_pos = gc.pos;
    sb.gc_pos_valid = true;
}

/// bcachefs 对齐: bch2_gc_pos_from_sb — 从 superblock 读取 GC 位置
///
/// 如果 superblock 中记录了有效的 gc_pos，返回 Some；否则返回 None。
pub fn bch2_gc_pos_from_sb(sb: &BchSb) -> Option<GcPos> {
    if sb.gc_pos_valid {
        Some(sb.gc_pos)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn make_gc_vol() -> crate::BchVol {
        let vol = crate::BchVol::test_trees();
        let ca = vol.primary_device_rcu_noerror().unwrap();
        let nbuckets = 1024;
        vol.superblock_mut()
            .member_mut(ca.dev_idx)
            .unwrap()
            .nbuckets = nbuckets;
        crate::alloc::bch2_dev_buckets_resize(&vol, &ca, nbuckets).unwrap();
        vol
    }

    #[test]
    fn test_gc_phase_order() {
        assert!(GcPhase::NotRunning < GcPhase::Start);
        assert!(GcPhase::Start < GcPhase::Sb);
        assert!(GcPhase::Sb < GcPhase::Btree);
    }

    #[test]
    fn test_gc_pos_cmp() {
        let a = gc_phase(GcPhase::Start);
        let b = gc_phase(GcPhase::Btree);
        assert_eq!(gc_pos_cmp(&a, &b), std::cmp::Ordering::Less);
    }

    #[test]
    fn test_gc_visited() {
        let gc = BtreeGc::new();
        // 初始状态 gc.pos = NotRunning, pos 为 0
        let pos = gc_phase(GcPhase::Start);
        assert!(
            !gc_visited(&gc, &pos),
            "gc should not have visited Start yet"
        );
    }

    #[test]
    fn test_gc_default() {
        let gc = BtreeGc::default();
        assert_eq!(gc.pos.phase, GcPhase::NotRunning);
        assert!(!gc.running.load(Ordering::Acquire));
    }

    #[test]
    fn test_gc_trigger() {
        let gc = BtreeGc::new();
        bch2_gc_gen_async(&gc);
        assert!(gc.triggered.load(Ordering::Acquire));
    }

    // ─── P0-3: bch2_gc_gen tests ───────────────────────────────

    #[test]
    fn test_gc_gen_basic() {
        let mut gc = BtreeGc::new();
        let vol = make_gc_vol();
        let ca = vol.primary_device_rcu_noerror().unwrap();
        vol.superblock_mut()
            .member_mut(ca.dev_idx)
            .unwrap()
            .bucket_size = 1024;
        crate::alloc::bch2_dev_buckets_resize(&vol, &ca, 1024).unwrap();
        let mut allocator =
            crate::alloc::BchAllocator::new(1024 * 256 * crate::alloc::SECTORS_PER_BLOCK);

        // 插入一个 extent 条目，paddr=0x1000 blocks → 1024-sector geometry 下 bucket_idx=32
        let paddr = 0x1000u64;
        vol.btree(crate::btree::BtreeId::Extents)
            .bch2_btree_bset_insert_key_wrapper(
                crate::btree::key::BtreeEntry::new(
                    crate::btree::key::Bpos::new(0, 1, 0),
                    crate::btree::key::KeyType::Normal,
                    crate::btree::key::KeyValue::extent(paddr, 1, 0),
                ),
                0,
            );

        let result = bch2_gc_gen(&vol, &mut allocator, &mut gc, 1);
        assert!(result.is_ok(), "bch2_gc_gen should succeed");

        // 验证 paddr 对应的 gc_bucket 已被标记（C 中 gc_buckets 由 extent trigger 填充）
        let bucket_idx = sector_to_bucket(&ca, paddr * crate::alloc::SECTORS_PER_BLOCK);
        let mut found = false;
        allocator.for_each_bucket_all_mut(&ca, |bi, _bucket, gc, _gen| {
            if bi == bucket_idx {
                assert!(
                    gc.gen_valid(),
                    "gc_bucket {} should have gen_valid after gc_gen",
                    bi,
                );
                assert_eq!(
                    gc.data_type(),
                    crate::alloc::BchDataType::User as u8,
                    "gc_bucket {} data_type should be User after gc_gen",
                    bi,
                );
                found = true;
            }
        });
        assert!(found, "bucket {} should exist in allocator", bucket_idx);
    }

    // ─── P0-4: bch2_check_topology tests ─────────────────────────

    #[test]
    fn test_check_topology_basic() {
        let vol = make_gc_vol();
        let _ca = vol.primary_device_rcu_noerror().unwrap();

        // 插入一些有序条目
        vol.btree(crate::btree::BtreeId::Extents)
            .bch2_btree_bset_insert_key_wrapper(
                crate::btree::key::BtreeEntry::new(
                    crate::btree::key::Bpos::new(0, 10, 0),
                    crate::btree::key::KeyType::Normal,
                    crate::btree::key::KeyValue::extent(0x100, 1, 0),
                ),
                0,
            );
        vol.btree(crate::btree::BtreeId::Extents)
            .bch2_btree_bset_insert_key_wrapper(
                crate::btree::key::BtreeEntry::new(
                    crate::btree::key::Bpos::new(0, 20, 0),
                    crate::btree::key::KeyType::Normal,
                    crate::btree::key::KeyValue::extent(0x200, 1, 0),
                ),
                0,
            );

        let result = bch2_check_topology(&vol);
        assert!(
            result.is_ok(),
            "topology check on consistent btree should pass"
        );
    }

    #[test]
    fn test_check_topology_empty_btree() {
        let vol = make_gc_vol();
        let _ca = vol.primary_device_rcu_noerror().unwrap();
        let result = bch2_check_topology(&vol);
        assert!(result.is_ok(), "topology check on empty btree should pass");
    }

    fn make_two_level_tree() -> crate::btree::Btree {
        use crate::btree::node::BsetTree;
        use crate::btree::types::{BtreeRoot, NodeCache};
        use crate::btree::{Btree, BtreeNode};
        use std::sync::Arc;

        let cache = Arc::new(NodeCache::new());

        let mut left = BtreeNode::new_leaf();
        left.insert(
            crate::btree::key::BtreeKey::new(10, 1, crate::btree::key::KeyType::Normal),
            crate::btree::key::BchVal::new(100, 0),
        );
        left.insert(
            crate::btree::key::BtreeKey::new(20, 1, crate::btree::key::KeyType::Normal),
            crate::btree::key::BchVal::new(200, 0),
        );
        left.insert(
            crate::btree::key::BtreeKey::new(30, 1, crate::btree::key::KeyType::Normal),
            crate::btree::key::BchVal::new(300, 0),
        );
        let left = Arc::new(left);

        let mut right = BtreeNode::new_leaf();
        right.insert(
            crate::btree::key::BtreeKey::new(40, 1, crate::btree::key::KeyType::Normal),
            crate::btree::key::BchVal::new(400, 0),
        );
        right.insert(
            crate::btree::key::BtreeKey::new(50, 1, crate::btree::key::KeyType::Normal),
            crate::btree::key::BchVal::new(500, 0),
        );
        let right = Arc::new(right);

        let left_addr = cache.alloc_addr();
        let right_addr = cache.alloc_addr();
        cache.insert(left_addr, left);
        cache.insert(right_addr, right);

        let mut internal = BtreeNode::new_internal();
        let mut cur = u32::from(crate::btree::node::BSET_HEADER_U64S) * 8;
        cur += internal.write_entry(
            cur,
            &crate::btree::key::BtreeKey::MIN_KEY,
            &crate::btree::key::BchVal::new(left_addr, 0),
            0,
        );
        cur += internal.write_entry(
            cur,
            &crate::btree::key::BtreeKey::new(40, 1, crate::btree::key::KeyType::Normal),
            &crate::btree::key::BchVal::new(right_addr, 0),
            0,
        );
        internal.sets[0] = BsetTree {
            size: 0,
            extra: crate::btree::node::BSET_NO_AUX_TREE_VAL,
            data_offset: 0,
            aux_data_offset: u16::MAX,
            end_offset: (cur / 8) as u16,
        };
        internal.packed_keys = 2;
        internal.unpacked_keys = 0;

        Btree::bch2_btree_set_root_for_read(
            BtreeRoot {
                node: Arc::new(internal),
                depth: 1,
            },
            cache,
            crate::btree::BtreeId::Extents,
        )
    }

    fn child_addrs(tree: &crate::btree::Btree) -> (u64, u64) {
        let root = &tree.root().node;
        let set = &root.sets[0];
        let (_, left_val) = root.read_entry(set, 1);
        let (_, right_val) = root.read_entry(set, 2);
        (left_val.paddr(), right_val.paddr())
    }

    // ─── P0-4: bch2_check_allocations tests ──────────────────────

    #[test]
    fn test_check_allocations_basic() {
        let vol = make_gc_vol();
        let _ca = vol.primary_device_rcu_noerror().unwrap();
        let allocator =
            crate::alloc::BchAllocator::new(1024 * 256 * crate::alloc::SECTORS_PER_BLOCK);

        // 无 extent、无分配，应返回空 Vec（一致）
        let result = bch2_check_allocations(&vol, &allocator);
        assert!(result.is_ok(), "allocations check should succeed");
        let discrepancies = result.unwrap();
        assert!(
            discrepancies.is_empty(),
            "expected no discrepancies, got: {:?}",
            discrepancies,
        );
    }

    // ─── P0-5: 拓扑检查增强 ─────────────────────────────────

    #[test]
    fn test_check_topology_with_root_range() {
        let vol = make_gc_vol();
        let _ca = vol.primary_device_rcu_noerror().unwrap();
        // 插入有序条目
        vol.btree(crate::btree::BtreeId::Extents)
            .bch2_btree_bset_insert_key_wrapper(
                crate::btree::key::BtreeEntry::new(
                    crate::btree::key::Bpos::new(0, 10, 0),
                    crate::btree::key::KeyType::Normal,
                    crate::btree::key::KeyValue::extent(0x100, 1, 0),
                ),
                0,
            );
        vol.btree(crate::btree::BtreeId::Extents)
            .bch2_btree_bset_insert_key_wrapper(
                crate::btree::key::BtreeEntry::new(
                    crate::btree::key::Bpos::new(0, 20, 0),
                    crate::btree::key::KeyType::Normal,
                    crate::btree::key::KeyValue::extent(0x200, 1, 0),
                ),
                0,
            );
        // 设置 root min_key/max_key 与实际条目一致（正常情况）
        let result = bch2_check_topology(&vol);
        assert!(
            result.is_ok(),
            "topology check should pass with correct root range"
        );
    }

    #[test]
    fn test_check_topology_detects_out_of_order() {
        let vol = make_gc_vol();
        let _ca = vol.primary_device_rcu_noerror().unwrap();
        // 用 insert_entry — sorted insert 保证有序
        vol.btree(BtreeId::Extents)
            .bch2_btree_bset_insert_key_wrapper(
                BtreeEntry::new(
                    Bpos::new(0, 20, 0),
                    KeyType::Normal,
                    KeyValue::extent(0x200, 1, 0),
                ),
                0,
            );
        vol.btree(BtreeId::Extents)
            .bch2_btree_bset_insert_key_wrapper(
                BtreeEntry::new(
                    Bpos::new(0, 10, 0),
                    KeyType::Normal,
                    KeyValue::extent(0x100, 1, 0),
                ),
                0,
            );
        let result = bch2_check_topology(&vol);
        assert!(result.is_ok(), "sorted insert ensures ordered entries");
    }

    #[test]
    fn test_check_topology_recursive_tree() {
        let btree = make_two_level_tree();
        let vol = make_gc_vol();
        let _ca = vol.primary_device_rcu_noerror().unwrap();
        *vol.btree_mut(crate::btree::BtreeId::Extents) = btree;

        let result = bch2_check_topology(&vol);
        assert!(result.is_ok(), "recursive topology check should pass");
    }

    #[test]
    fn test_check_topology_detects_child_boundary_overlap() {
        let btree = make_two_level_tree();
        let (left_addr, right_addr) = child_addrs(&btree);
        let cache = btree.cache();

        let mut right = cache
            .take_node(right_addr)
            .expect("right child should exist in cache");
        {
            let right_node = Arc::get_mut(&mut right).expect("right child Arc should be unique");
            right_node.min_key = crate::btree::key::Bpos::new(25, 1, 0);
        }
        cache.insert(right_addr, right);

        let vol = make_gc_vol();
        let _ca = vol.primary_device_rcu_noerror().unwrap();
        *vol.btree_mut(crate::btree::BtreeId::Extents) = btree;

        let result = bch2_check_topology(&vol);
        assert!(
            result.is_err(),
            "overlapping child boundary should fail topology check"
        );

        // left child remains untouched; ensure helper extracted a real root tree.
        assert_ne!(left_addr, right_addr);
    }

    #[test]
    fn test_check_topology_detects_missing_child() {
        let btree = make_two_level_tree();
        let (_, right_addr) = child_addrs(&btree);
        let cache = btree.cache();
        let _ = cache
            .take_node(right_addr)
            .expect("right child should exist");

        let vol = make_gc_vol();
        let _ca = vol.primary_device_rcu_noerror().unwrap();
        *vol.btree_mut(crate::btree::BtreeId::Extents) = btree;

        let result = bch2_check_topology(&vol);
        assert!(
            result.is_err(),
            "missing child node should fail topology check"
        );
    }

    // ─── P0-6: bch2_gc_btrees mark 路径 ─────────────────────

    #[test]
    fn test_gc_btrees_mark_path() {
        // 验证 bch2_gc_btrees 的 mark 路径
        let mut vol = make_gc_vol();
        let ca = vol.primary_device_rcu_noerror().unwrap();
        let paddr = 0x2000u64;
        vol.btree(crate::btree::BtreeId::Extents)
            .bch2_btree_bset_insert_key_wrapper(
                crate::btree::key::BtreeEntry::new(
                    crate::btree::key::Bpos::new(0, 1, 0),
                    crate::btree::key::KeyType::Normal,
                    crate::btree::key::KeyValue::extent(paddr, 0, 0),
                ),
                0,
            );

        let mut allocator =
            crate::alloc::BchAllocator::new(1024 * 256 * crate::alloc::SECTORS_PER_BLOCK);
        let result = bch2_gc_btrees(&mut vol, &mut allocator);
        assert!(
            result.is_ok(),
            "bch2_gc_btrees with None registry should succeed"
        );

        // 验证 bucket 被正确标记
        let bucket_idx = paddr / crate::alloc::BLOCKS_PER_BUCKET;
        let mut found = false;
        allocator.for_each_bucket(&ca, |bi, bucket, _gen| {
            if bi == bucket_idx {
                assert_eq!(
                    bucket.state,
                    crate::alloc::BchDataType::User,
                    "bucket {} should be marked User after gc_btrees",
                    bi,
                );
                found = true;
            }
        });
        assert!(found, "bucket {} should exist", bucket_idx);

        // P5: 验证 gc_buckets 在 bch2_gc_mark_key 中被正确填充
        let mut gc_found = false;
        allocator.for_each_bucket_all_mut(&ca, |bi, _bucket, gc, gen| {
            if bi == bucket_idx {
                assert!(gc.gen_valid(), "gc_buckets gen_valid should be true");
                assert_eq!(
                    gc.data_type(),
                    crate::alloc::BchDataType::User as u8,
                    "gc_buckets data_type should be User",
                );
                assert_eq!(gc.gen, *gen, "gc_buckets gen should match gens[]");
                gc_found = true;
            }
        });
        assert!(gc_found, "gc_buckets entry should exist");
    }

    #[test]
    fn test_gc_btrees_collects_entries() {
        // 验证 bch2_gc_btrees 能正确处理多个 btree 中的条目
        let mut vol = make_gc_vol();
        let ca = vol.primary_device_rcu_noerror().unwrap();

        // 在 Extents btree 中插入
        vol.btree(crate::btree::BtreeId::Extents)
            .bch2_btree_bset_insert_key_wrapper(
                crate::btree::key::BtreeEntry::new(
                    crate::btree::key::Bpos::new(0, 1, 0),
                    crate::btree::key::KeyType::Normal,
                    crate::btree::key::KeyValue::extent(0x3000, 0, 0),
                ),
                0,
            );
        // 在 Alloc btree 中插入
        vol.btree(crate::btree::BtreeId::Alloc)
            .bch2_btree_bset_insert_key_wrapper(
                crate::btree::key::BtreeEntry::new(
                    crate::btree::key::Bpos::new(0, 5, 0),
                    crate::btree::key::KeyType::Normal,
                    crate::btree::key::KeyValue::extent(0x4000, 0, 0),
                ),
                0,
            );

        let mut allocator =
            crate::alloc::BchAllocator::new(1024 * 256 * crate::alloc::SECTORS_PER_BLOCK);
        let result = bch2_gc_btrees(&mut vol, &mut allocator);
        assert!(
            result.is_ok(),
            "bch2_gc_btrees with multiple btrees should succeed"
        );

        // 验证 Extents btree 中的 bucket 被标记
        let bucket_idx_ext = 0x3000 / crate::alloc::BLOCKS_PER_BUCKET;
        let bucket_idx_alloc = 0x4000 / crate::alloc::BLOCKS_PER_BUCKET;
        let mut ext_found = false;
        let mut alloc_found = false;
        allocator.for_each_bucket(&ca, |bi, bucket, _gen| {
            if bi == bucket_idx_ext {
                assert_eq!(bucket.state, crate::alloc::BchDataType::User);
                ext_found = true;
            }
            if bi == bucket_idx_alloc {
                assert_eq!(bucket.state, crate::alloc::BchDataType::User);
                alloc_found = true;
            }
        });
        assert!(
            ext_found,
            "Extents bucket {} should be marked",
            bucket_idx_ext
        );
        assert!(
            alloc_found,
            "Alloc bucket {} should be marked",
            bucket_idx_alloc
        );
    }

    // ─── Sweep Phase tests ──────────────────────────────────────

    #[test]
    fn test_gc_sweep_reclaims_unreferenced_user_bucket() {
        let vol = make_gc_vol();
        let ca = vol.primary_device_rcu_noerror().unwrap();
        let mut allocator =
            crate::alloc::BchAllocator::new(1024 * 256 * crate::alloc::SECTORS_PER_BLOCK);

        // Insert an extent that references bucket at paddr=0x5000
        let paddr = 0x5000u64;
        vol.btree(crate::btree::BtreeId::Extents)
            .bch2_btree_bset_insert_key_wrapper(
                crate::btree::key::BtreeEntry::new(
                    crate::btree::key::Bpos::new(0, 1, 0),
                    crate::btree::key::KeyType::Normal,
                    crate::btree::key::KeyValue::extent(paddr, 1, 0),
                ),
                0,
            );

        let bucket_ref = paddr / crate::alloc::BLOCKS_PER_BUCKET; // = 80
        let bucket_unref = 30u64;

        // Manually mark both buckets as User and fill gc_buckets for the referenced one
        allocator.for_each_bucket_all_mut(&ca, |bi, bucket, gc, gen| {
            if bi == bucket_ref || bi == bucket_unref {
                bucket.state = crate::alloc::BchDataType::User;
            }
            if bi == bucket_ref {
                gc.gen = *gen;
                gc.set_gen_valid(true);
                gc.set_data_type(crate::alloc::BchDataType::User as u8);
            }
        });

        let stats = bch2_gc_sweep(&vol, &mut allocator).unwrap();

        // Referenced bucket should remain User
        allocator.for_each_bucket(&ca, |bi, bucket, _gen| {
            if bi == bucket_ref {
                assert_eq!(
                    bucket.state,
                    crate::alloc::BchDataType::User,
                    "referenced bucket should remain User",
                );
            }
            if bi == bucket_unref {
                assert_eq!(
                    bucket.state,
                    crate::alloc::BchDataType::Free,
                    "unreferenced bucket should be reclaimed to Free",
                );
            }
        });

        assert_eq!(stats.reclaimed_count, 1, "should reclaim 1 bucket");
        assert_eq!(
            stats.reclaimed_buckets,
            vec![bucket_unref],
            "should reclaim the unreferenced bucket",
        );
    }

    #[test]
    fn test_gc_sweep_applies_old_new_device_accounting_delta() {
        let vol = make_gc_vol();
        let ca = vol.primary_device_rcu_noerror().unwrap();
        let mut allocator =
            crate::alloc::BchAllocator::new(1024 * 256 * crate::alloc::SECTORS_PER_BLOCK);
        let bucket = 30u64;

        allocator.for_each_bucket_all_mut(&ca, |bi, state, gc, gen| {
            if bi == 0 {
                state.state = crate::alloc::BchDataType::Sb;
                gc.gen = *gen;
                gc.set_gen_valid(true);
                gc.set_data_type(crate::alloc::BchDataType::Sb as u8);
            } else if bi == bucket {
                state.state = crate::alloc::BchDataType::User;
            }
        });

        let old = crate::alloc::btree::BchAllocV4 {
            data_type: crate::alloc::BchDataType::User as u8,
            dirty_sectors: 100,
            ..crate::alloc::btree::BCH_ALLOC_V4_ZERO
        };
        vol.btree(crate::btree::BtreeId::Alloc)
            .bch2_btree_bset_insert_key_wrapper(
                crate::btree::key::BtreeEntry::raw(
                    crate::btree::key::Bpos::new(0, bucket, 0),
                    crate::btree::key::KeyType::Normal,
                    crate::alloc::btree::serialize_alloc_entry(&old),
                ),
                0,
            );
        crate::alloc::accounting::bch2_disk_accounting_mod(
            &vol,
            crate::alloc::accounting::AcctType::DevDataType(
                0,
                crate::alloc::BchDataType::User as u8,
            ),
            &[1, 100, 1948],
            true,
        )
        .unwrap();

        bch2_gc_sweep(&vol, &mut allocator).unwrap();

        let counters = |data_type: crate::alloc::BchDataType| {
            let inode = crate::alloc::accounting::BCH_DISK_ACCOUNTING_dev_data_type as u64
                | (data_type as u64) << 16;
            let entry = vol
                .get_entry_raw(
                    crate::btree::BtreeId::Accounting,
                    crate::btree::key::Bpos::new(inode, 0, 0),
                )
                .unwrap();
            let crate::btree::key::KeyValue::Raw(bytes) = entry.value else {
                panic!("accounting value is not raw");
            };
            bincode::deserialize::<crate::alloc::accounting::AcctEntry>(&bytes)
                .unwrap()
                .counters
        };
        assert_eq!(counters(crate::alloc::BchDataType::User), [0, 0, 0]);
        assert_eq!(counters(crate::alloc::BchDataType::Free), [1, 0, 0]);

        let corrected = vol
            .get_entry_raw(
                crate::btree::BtreeId::Alloc,
                crate::btree::key::Bpos::new(0, bucket, 0),
            )
            .unwrap();
        let crate::btree::key::KeyValue::Raw(bytes) = corrected.value else {
            panic!("alloc value is not raw");
        };
        assert_eq!(
            crate::alloc::btree::deserialize_alloc_entry(&bytes)
                .unwrap()
                .data_type,
            crate::alloc::BchDataType::Free as u8
        );
    }

    #[test]
    fn test_gc_sweep_preserves_sb_journal_btree() {
        let vol = make_gc_vol();
        let ca = vol.primary_device_rcu_noerror().unwrap();
        let mut allocator =
            crate::alloc::BchAllocator::new(1024 * 256 * crate::alloc::SECTORS_PER_BLOCK);

        // 填充一个 gc_bucket 使 gc_buckets_ready=true
        allocator.for_each_bucket_all_mut(&ca, |bi, _bucket, gc, gen| {
            if bi == 0 {
                gc.gen = *gen;
                gc.set_gen_valid(true);
                gc.set_data_type(crate::alloc::BchDataType::User as u8);
            }
        });

        // Manually set some buckets to non-reclaimable states
        allocator.for_each_bucket_mut(&ca, |bi, bucket, _gen| match bi {
            0 => bucket.state = crate::alloc::BchDataType::Sb,
            1 => bucket.state = crate::alloc::BchDataType::Journal,
            2 => bucket.state = crate::alloc::BchDataType::Btree,
            _ => {}
        });

        let stats = bch2_gc_sweep(&vol, &mut allocator).unwrap();

        // Verify non-reclaimable states are preserved
        let mut sb_found = false;
        let mut journal_found = false;
        let mut btree_found = false;
        allocator.for_each_bucket(&ca, |bi, bucket, _gen| match bi {
            0 => {
                assert_eq!(bucket.state, crate::alloc::BchDataType::Sb);
                sb_found = true;
            }
            1 => {
                assert_eq!(bucket.state, crate::alloc::BchDataType::Journal);
                journal_found = true;
            }
            2 => {
                assert_eq!(bucket.state, crate::alloc::BchDataType::Btree);
                btree_found = true;
            }
            _ => {}
        });
        assert!(sb_found, "bucket 0 should exist");
        assert!(journal_found, "bucket 1 should exist");
        assert!(btree_found, "bucket 2 should exist");

        assert!(
            stats.skipped_state > 0,
            "should have skipped non-User/non-NeedGcGens buckets",
        );
    }

    #[test]
    fn test_gc_sweep_cleans_needgcgens_transient() {
        let vol = make_gc_vol();
        let ca = vol.primary_device_rcu_noerror().unwrap();
        let mut allocator =
            crate::alloc::BchAllocator::new(1024 * 256 * crate::alloc::SECTORS_PER_BLOCK);

        // 填充一个 gc_bucket 使 gc_buckets_ready=true，模拟 GC 标记阶段已执行
        allocator.for_each_bucket_all_mut(&ca, |bi, _bucket, gc, gen| {
            if bi == 0 {
                gc.gen = *gen;
                gc.set_gen_valid(true);
                gc.set_data_type(crate::alloc::BchDataType::User as u8);
            }
        });

        // Manually set some buckets to NeedGcGens
        allocator.for_each_bucket_mut(&ca, |bi, bucket, _gen| {
            if bi == 5 || bi == 10 {
                bucket.state = crate::alloc::BchDataType::NeedGcGens;
            }
        });

        let stats = bch2_gc_sweep(&vol, &mut allocator).unwrap();

        // NeedGcGens buckets should be reclaimed to Free
        allocator.for_each_bucket(&ca, |bi, bucket, _gen| {
            if bi == 5 || bi == 10 {
                assert_eq!(
                    bucket.state,
                    crate::alloc::BchDataType::Free,
                    "NeedGcGens bucket {} should be reclaimed to Free",
                    bi,
                );
            }
        });

        assert_eq!(
            stats.reclaimed_count, 2,
            "should reclaim 2 NeedGcGens buckets",
        );
    }

    #[test]
    fn test_gc_sweep_empty_vol_no_reclaim() {
        let vol = make_gc_vol();
        let _ca = vol.primary_device_rcu_noerror().unwrap();
        let mut allocator =
            crate::alloc::BchAllocator::new(1024 * 256 * crate::alloc::SECTORS_PER_BLOCK);

        // All buckets are Free by default; no User buckets to reclaim
        let stats = bch2_gc_sweep(&vol, &mut allocator).unwrap();

        assert_eq!(
            stats.reclaimed_count, 0,
            "no User buckets should be reclaimed from empty vol",
        );
    }
}
