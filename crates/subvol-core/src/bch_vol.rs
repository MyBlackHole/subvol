use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::RwLock;
use std::time::Instant;

use futures::future::{join_all, try_join_all};
use tokio::sync::Notify;

use crate::alloc::{AllocRequest, BchAllocator, BchFsCapacity, DedicatedWp, WritePointSpecifier};
use crate::block_device::{BchDev, BchDevIoRefGuard, BchDevIoRefKind, BchDevsMask, BlockDevice};

use crate::btree::key::{BtreeKey, ExtentPtr, ExtentValue, KeyType, KeyValue};
use crate::btree::write_buffer;
use crate::btree::{
    BchVal, Bpos, Btree, BtreeEntry, BtreeId, BtreeNode, BtreeTrans, NodeCache, BTREE_ID_NR,
};
use crate::io::{
    BchDevIoFailure, BchIoFailures, BchReadBio, BchReadFlags, BchWriteOp, BkeyBuf, BvecIter,
    SubvolInum,
};
use crate::replicas::BchReplicasCpu;
use crate::config::StorageConfig;
use crate::journal::{Journal, JournalSuperblockState};
use crate::recovery;
use crate::snap::snapshot::{
    bch2_snapshot_list, bch2_snapshot_node_create, bch2_snapshot_node_set_deleted,
    bch2_snapshot_read_value, 
};
use crate::storage::superblock::BchSb;
use crate::subvol::{
    bch2_initialize_subvolumes, bch2_subvolume_create, bch2_subvolume_delete, bch2_subvolume_get,
    bch2_subvolume_get_snapshot, bch2_subvolume_snapshot, BCACHEFS_ROOT_INO, BCACHEFS_ROOT_SUBVOL,
};
use crate::types::{BackendType, BlockAddr, StorageError, Watermark};

// ─── 卷配置（原 crate::volume） ───

/// 默认块大小 (4KB)
const DEFAULT_BLOCK_SIZE: u32 = 4096;
/// 默认卷容量 (1GB)
const DEFAULT_CAPACITY: u64 = 1024 * 1024 * 1024;

// bcachefs `BCH_EXTENT_FLAG_poisoned` is a key-side extent flag
// (`fs/data/extents_format.h`, `fs/data/read.c:541-590`).  The new subvol
// extent encoding has one reserved metadata bit, so keep the flag persistent
// without changing the public read/write API.
const EXTENT_CRC_POISONED_BIT: u64 = 1 << 63;
const EXTENT_CRC_ORIGINAL_BLOCKS_MASK: u32 = 0x7fff_ffff;

/// 卷配置
#[derive(Debug, Clone)]
pub struct VolumeConfig {
    /// 逻辑块大小（默认 4096）
    pub block_size: u32,
    /// 卷容量（字节，默认 1GB）
    pub capacity: u64,
    /// btree 节点大小（与 superblock 的 storage config 对齐）
    pub btree_node_size: u32,
    /// 对应本地 `c->opts.metadata_replicas`。
    pub metadata_replicas: u8,
    /// 对应本地 `c->opts.data_replicas`。
    pub data_replicas: u8,
    /// 对应本地 `c->opts.metadata_target`。
    pub metadata_target: u16,
    /// 对应本地 `c->opts.foreground_target`。
    pub foreground_target: u16,
    /// 对应本地 `c->opts.nochanges`。
    pub nochanges: bool,
    /// 对应本地 `c->opts.read_only`。
    pub read_only: bool,
    /// 对应本地 `c->opts.journal_rewind_discard_buffer_percent`。
    pub journal_rewind_discard_buffer_percent: u8,
}

impl Default for VolumeConfig {
    fn default() -> Self {
        Self {
            block_size: DEFAULT_BLOCK_SIZE,
            capacity: DEFAULT_CAPACITY,
            btree_node_size: crate::alloc::DEFAULT_BTREE_NODE_SIZE,
            metadata_replicas: 1,
            data_replicas: 1,
            metadata_target: 0,
            foreground_target: 0,
            nochanges: false,
            read_only: false,
            journal_rewind_discard_buffer_percent: 4,
        }
    }
}

/// journal allocation 使用的本地 `struct bch_opts` 字段子集。
#[derive(Debug, Clone, Copy)]
pub struct BchOpts {
    pub metadata_replicas: u8,
    pub data_replicas: u8,
    pub metadata_target: u16,
    pub foreground_target: u16,
    pub journal_flush_delay: u32,
    pub nochanges: bool,
    pub journal_rewind_discard_buffer_percent: u8,
}

impl From<&VolumeConfig> for BchOpts {
    fn from(config: &VolumeConfig) -> Self {
        Self {
            metadata_replicas: config.metadata_replicas,
            data_replicas: config.data_replicas,
            metadata_target: config.metadata_target,
            foreground_target: config.foreground_target,
            journal_flush_delay: StorageConfig::default().journal_flush_delay_ms,
            nochanges: config.nochanges,
            journal_rewind_discard_buffer_percent: config.journal_rewind_discard_buffer_percent,
        }
    }
}

/// 卷生命周期状态（对应 bcachefs BCH_FS_* 标志位系统）
///
/// 命名对齐 bcachefs 风格：
///   - `New` ↔ BCH_FS_new_fs        — 新创建
///   - `Rw`  ↔ BCH_FS_rw             — 可读写（bcachefs 用 rw 而非 "running"）
///   - `Error` ↔ BCH_FS_error
///   - `Stopping` ↔ BCH_FS_stopping
///   - `Stopped` ↔ BCH_FS_clean_shutdown
///
/// 状态转换顺序：New → Starting → Rw → Stopping → Stopped
/// 从任何非终止状态可转入 Error。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum VolumeState {
    /// 卷已创建但未启动（BCH_FS_new_fs）
    New = 0,
    /// 卷正在启动（恢复/初始化中）
    Starting = 1,
    /// 卷正常运行中，可读写（BCH_FS_rw）
    Rw = 2,
    /// 卷可读写但后台恢复尚未完成（Rw 子状态）
    ///
    /// 对应 bcachefs BCH_FS_rw 下仍有 recovery passes 在执行的场景。
    /// 恢复完成后应转回 Rw；恢复失败则转入 Error。
    RwWithPendingRecovery = 6,
    /// 卷处于错误状态（非终止，可尝试恢复；BCH_FS_error）
    Error = 3,
    /// 卷正在关闭（BCH_FS_stopping）
    Stopping = 4,
    /// 卷已停止，终止状态（BCH_FS_clean_shutdown）
    Stopped = 5,
    /// 卷只读模式（BCH_FS_going_ro 后已清 BCH_FS_rw）
    ReadOnly = 7,
    /// 正在切换到只读（BCH_FS_going_ro）— 禁止新写入，等待飞行写入完成
    ///
    /// 对应 bcachefs BCH_FS_going_ro 标志。
    /// 从 Rw 或 Stopping 转入，完成后进入 ReadOnly 或 Stopped。
    GoingRo = 8,
    /// 紧急只读（BCH_FS_emergency_ro）— 错误触发的只读状态
    ///
    /// 对应 bcachefs BCH_FS_emergency_ro 标志。
    /// 从任何状态（除终止态）转入，不可逆转。
    EmergencyRo = 9,
}

/// 卷统计信息
#[derive(Debug, Clone)]
pub struct VolumeStats {
    pub block_size: u32,
    pub capacity: u64,
    pub total_blocks: u64,
    pub allocated_blocks: u64,
    pub mapping_entries: usize,
    pub btree_keys: u32,
    pub snapshot_count: usize,
    pub snapshot_tree_depth: usize,
}

/// 卷级设备注册表 — 通过 superblock 中的 `dev_idx` 解析运行时设备
pub struct BchDeviceRegistry {
    devices: HashMap<u8, Arc<BchDev>>,
}

impl std::fmt::Debug for BchDeviceRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BchDeviceRegistry")
            .field("dev_indices", &self.dev_indices())
            .field("online_dev_indices", &self.online_dev_indices())
            .finish()
    }
}

impl Clone for BchDeviceRegistry {
    fn clone(&self) -> Self {
        Self {
            devices: self.devices.clone(),
        }
    }
}

impl Default for BchDeviceRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl BchDeviceRegistry {
    pub fn new() -> Self {
        Self {
            devices: HashMap::new(),
        }
    }

    pub(crate) fn from_devices(devices: impl IntoIterator<Item = Arc<BchDev>>) -> Self {
        let mut registry = Self::new();
        registry.insert_devices(devices);
        registry
    }

    pub(crate) fn with_bch_dev(dev: Arc<BchDev>) -> Self {
        let mut registry = Self::new();
        registry.insert_bch_dev(dev);
        registry
    }

    pub(crate) fn insert_devices(&mut self, devices: impl IntoIterator<Item = Arc<BchDev>>) {
        for dev in devices {
            self.insert_bch_dev(dev);
        }
    }

    pub(crate) fn insert_bch_dev(&mut self, dev: Arc<BchDev>) {
        self.devices.insert(dev.dev_idx, dev);
    }

    pub(crate) fn resolve(&self, dev_idx: u8) -> Option<Arc<BchDev>> {
        self.devices.get(&dev_idx).cloned()
    }

    pub(crate) fn dev_indices(&self) -> Vec<u8> {
        let mut devs: Vec<u8> = self.devices.keys().copied().collect();
        devs.sort_unstable();
        devs
    }

    pub(crate) fn online_dev_indices(&self) -> Vec<u8> {
        let mut devs: Vec<u8> = self
            .devices
            .iter()
            .filter_map(|(&dev_idx, dev)| dev.is_online().then_some(dev_idx))
            .collect();
        devs.sort_unstable();
        devs
    }

    pub(crate) fn resolve_bch_dev(&self, dev_idx: u8) -> Option<Arc<BchDev>> {
        self.resolve(dev_idx)
    }

    /// 对齐本地 `bch2_get_next_online_dev()`：先释放前一个设备的 IO ref，
    /// 再按 dev_idx 前进、过滤 member state，并尝试取得下一个 IO ref。
    pub(crate) fn bch2_get_next_online_dev(
        &self,
        ca: Option<BchDevIoRefGuard>,
        state_mask: u32,
        rw: BchDevIoRefKind,
    ) -> Option<BchDevIoRefGuard> {
        let previous = ca.as_ref().map(|ca| ca.dev_idx);
        drop(ca);

        for dev_idx in self.dev_indices() {
            if previous.is_some_and(|previous| dev_idx <= previous) {
                continue;
            }

            let dev = self.resolve_bch_dev(dev_idx)?;
            if state_mask & (1u32 << dev.member_state() as u8) == 0 {
                continue;
            }
            if let Some(ca) = dev.try_get_io_ref_guard(rw) {
                return Some(ca);
            }
        }

        None
    }

    /// 返回所有注册设备的掩码。
    pub(crate) fn devs_mask(&self) -> BchDevsMask {
        let mut mask = BchDevsMask::new();
        for &dev_idx in self.devices.keys() {
            mask.set(dev_idx);
        }
        mask
    }

    /// 返回所有在线设备的掩码。
    pub(crate) fn online_mask(&self) -> BchDevsMask {
        let mut mask = BchDevsMask::new();
        for (&dev_idx, dev) in &self.devices {
            if dev.is_online() {
                mask.set(dev_idx);
            }
        }
        mask
    }

    /// 返回所有指定状态的设备掩码。
    pub(crate) fn devices_by_state(
        &self,
        state: crate::storage::superblock::BchMemberState,
    ) -> BchDevsMask {
        let mut mask = BchDevsMask::new();
        for (&dev_idx, dev) in &self.devices {
            if dev.member_state() == state {
                mask.set(dev_idx);
            }
        }
        mask
    }

    /// 将掩码解析为设备引用列表。
    pub(crate) fn resolve_mask(&self, mask: BchDevsMask) -> Vec<Arc<BchDev>> {
        mask.iter()
            .filter_map(|dev_idx| self.devices.get(&dev_idx).cloned())
            .collect()
    }

    /// 获取指定设备的 durability（副本计数贡献）。
    /// 默认返回 1（单副本），从 member 元数据中解析。
    pub(crate) fn durability(&self, dev_idx: u8) -> u32 {
        self.devices.get(&dev_idx).map_or(1, |dev| {
            // Match local `bch2_mi_to_cpu()` (`fs/sb/members.h:416-439`):
            // the on-disk value is encoded as durability + 1, with zero
            // retaining the historical single-copy default.
            unsafe { &*dev.mi.get() }.durability.max(1) as u32
        })
    }
}

/// bcachefs 对齐统一上下文 — 对应 `bch_fs`
///
/// 聚合所有子系统 + 生命周期管理。
///
/// # 不变字段（outer）
/// - 卷固定属性、`backend` — 初始化后不变
/// - `state`, `recovery_*` — 原子类型，无需锁
/// - `name`, `vol_dir` — 标识
/// - `config` — 完全不可变
///
/// # 可变字段（各带内部同步）
/// - `trees` — UnsafeCell 内部可变（与旧引擎设计的 Sync 论证一致）
/// - `allocator` — UnsafeCell 内部可变
/// - `journal` — UnsafeCell<Arc<Journal>>，Arc 共享 + 可替换
/// - `root_snapshot_id` — AtomicU32 无锁并发
///
/// struct bch_fs 对齐：无外层锁，字段直接暴露，各子系统内部维护同步。
pub struct BchVol {
    block_size: u32,
    logical_capacity: u64,
    /// 对应本地 `bch_fs.capacity`，以扇区计。
    pub capacity: UnsafeCell<BchFsCapacity>,
    pub device_registry: BchDeviceRegistry,
    pub superblock: UnsafeCell<BchSb>,
    /// Superblock 更新锁；对应本地 `struct mutex sb_lock`。
    /// 所有 `superblock_mut()` 运行时调用方必须先获取此锁。
    pub sb_lock: std::sync::Mutex<()>,
    /// 设备 bucket arrays resize 锁；对应本地 `bch_fs.state_lock`。
    pub state_lock: Mutex<()>,

    // ──── 内化自旧引擎的字段 ────
    /// 所有 BtreeId 对应的 btree 实例
    pub trees: UnsafeCell<[Btree; 28]>,
    /// 对应本地 `c->btree.foreground_merge_threshold`。
    pub(crate) btree_foreground_merge_threshold: u16,
    /// 运行时 trim 洞记录（基本 NBD 语义兜底）
    pub trim_holes: RwLock<HashMap<u32, Vec<(u64, u64)>>>,
    /// inode → 子卷 ID 列表映射（SubvolInoMap 兼容）
    pub subvol_ino_map: std::sync::Mutex<HashMap<u64, Vec<u32>>>,
    /// bcachefs 对齐: btree write buffer set
    pub write_buffer_set: UnsafeCell<write_buffer::BtreeWriteBufferSet>,
    /// Journal replica entry 引用表；对应本地 `bch_fs.replicas`。
    pub replicas: Mutex<BchReplicasCpu>,

    // 可变字段（各带内部同步，类似 bcachefs 各子系统自锁）
    pub journal: UnsafeCell<Arc<Journal>>,
    pub allocator: UnsafeCell<BchAllocator>,
    pub root_snapshot_id: AtomicU32,
    /// 根子卷 ID（创建卷时自动分配，对齐 bcachefs root subvol = 1）
    pub root_subvol_id: AtomicU32,
    pub config: VolumeConfig,
    /// 对应本地 `struct bch_fs::opts`；当前只承载已接入的 journal allocation 字段。
    pub opts: BchOpts,

    // 原子状态（对应 bch_fs->flags）
    pub state: AtomicU8,
    pub recovery_pass_done: AtomicU8,
    pub recovery_passes_complete: AtomicU64,
    pub passes_failing: AtomicU64,
    pub error_count: AtomicU64,
    pub fsck_error: AtomicU64,
    /// 对应本地 `c->key_version`，写入每个 journal entry 的 usage 记录。
    pub(crate) key_version: AtomicU64,
    /// 对应本地 `c->io_clock[READ/WRITE].now`。
    pub(crate) io_clock: [AtomicU64; 2],

    // ──── 写引用追踪（对应 bcachefs enumerated_ref writes） ────
    /// 当前飞行中的写入计数
    pub write_ref_count: AtomicU64,
    /// 写入 drain 通知（用于 bch2_fs_read_only/close 等待飞行写入完成）
    pub write_drain_notify: Notify,

    // daemon 属性
    pub name: String,
    pub vol_dir: PathBuf,
}

/// # Safety
///
/// `BchVol` 各字段使用内部可变性（`UnsafeCell`）而不是外层大锁，
/// 直接对齐 bcachefs `struct bch_fs` 的设计：各子系统自有锁保护可变状态。
///
/// 每个 `UnsafeCell` 字段的安全保证：
/// - `capacity` — `mark_lock` + `sectors_available_lock` 保护
/// - `superblock` — `sb_lock`（`std::sync::Mutex`）保护
/// - `trees` — 各 Btree 内部 SIX 锁保护节点
/// - `write_buffer_set` — per-buffer `inc.lock`（Mutex）保护
/// - `journal` — init 替换后运行期只读 Arc clone
/// - `allocator` — 内部 `Mutex` 保护
unsafe impl Sync for BchVol {}

// ─── UnsafeCell 内部可变帮助方法 ───

impl BchVol {
    // ─── Btree 访问 ───

    /// 将外部字节偏移/长度转换为内部 extent 块单位。
    ///
    /// bcachefs 的 extent key 以 sectors/blocks 计数，`size` 不是字节数。
    /// NBD 入口仍然按字节提供偏移与长度，因此这里统一转换。
    fn extent_bytes_to_blocks(&self, bytes: u64, what: &str) -> Result<u64, StorageError> {
        let block_size = self.block_size as u64;
        if bytes == 0 {
            return Ok(0);
        }
        if bytes % block_size != 0 {
            return Err(StorageError::InvalidArgument(format!(
                "{what} must be a multiple of block size {block_size} (got {bytes})"
            )));
        }
        Ok(bytes / block_size)
    }

    fn trim_hole_exists(&self, snapshot_id: u32, block: u64) -> bool {
        let holes = self.trim_holes.read().unwrap();
        holes
            .get(&snapshot_id)
            .map(|ranges| {
                ranges
                    .iter()
                    .any(|(start, end)| *start <= block && block < *end)
            })
            .unwrap_or(false)
    }

    fn add_trim_hole(&self, snapshot_id: u32, start: u64, end: u64) {
        let mut holes = self.trim_holes.write().unwrap();
        holes.entry(snapshot_id).or_default().push((start, end));
    }

    fn clear_trim_holes_overlapping(&self, snapshot_id: u32, start: u64, end: u64) {
        let mut holes = self.trim_holes.write().unwrap();
        if let Some(ranges) = holes.get_mut(&snapshot_id) {
            ranges.retain(|(hs, he)| *he <= start || *hs >= end);
        }
    }

    /// 获取指定 btree 的不可变引用
    pub fn btree(&self, id: BtreeId) -> &Btree {
        let trees = unsafe { &*self.trees.get() };
        &trees[id as usize]
    }

    /// 获取指定 btree 的可变引用（通过 UnsafeCell 内部可变性）
    #[allow(clippy::mut_from_ref)]
    pub fn btree_mut(&self, id: BtreeId) -> &mut Btree {
        let trees = unsafe { &mut *self.trees.get() };
        &mut trees[id as usize]
    }

    // ─── 内化自旧引擎的上提方法 ───

    pub fn for_each<F>(&self, mut f: F)
    where
        F: FnMut(BtreeId, &Btree),
    {
        for ty in BTREE_ID_NR {
            f(ty, self.btree(ty));
        }
    }

    pub async fn load_root(
        &self,
        ty: BtreeId,
        root_addr: u64,
        level: Option<u8>,
    ) -> Result<(), StorageError> {
        let trees = unsafe { &*self.trees.get() };
        trees[ty as usize]
            .bch2_btree_root_read(root_addr, level)
            .await
    }

    pub fn cache_arc(&self, ty: BtreeId) -> Arc<NodeCache> {
        let trees = unsafe { &*self.trees.get() };
        trees[ty as usize].node_cache_arc()
    }

    pub fn insert_entry_raw(&self, ty: BtreeId, entry: BtreeEntry, journal_seq: u64) -> bool {
        self.flush_cache_dirty_keys(journal_seq);
        self.btree(ty)
            .bch2_btree_bset_insert_key_wrapper(entry, journal_seq)
    }

    pub fn get_entry(&self, ty: BtreeId, key: &BtreeKey) -> Option<(BtreeKey, BchVal)> {
        let pos = Bpos::from_key(key);
        self.get_entry_raw(ty, pos)
            .map(|entry| entry.to_key_value())
    }

    /// 在指定 type 的 btree 上通过 Bpos 查询（支持 KeyValue::Raw）。
    ///
    /// 与 get_entry 不同，返回 BtreeEntry 而非 (BtreeKey, BchVal)。
    pub fn get_entry_raw(&self, ty: BtreeId, pos: Bpos) -> Option<BtreeEntry> {
        self.btree(ty).bch2_btree_iter_peek_entry(pos)
    }


    pub fn flush_dirty_nodes(&self) -> Vec<(BtreeId, Vec<(u64, Arc<BtreeNode>)>)> {
        self.flush_cache_dirty_keys(0);
        let mut result = Vec::new();
        for ty in BTREE_ID_NR {
            let serialized = self.btree(ty).bch2_btree_flush_all();
            if !serialized.is_empty() {
                result.push((ty, serialized));
            }
        }
        result
    }

    pub fn flush_write_buffers_going_ro(
        &self,
        journal: Option<&Journal>,
    ) -> Result<bool, StorageError> {
        use write_buffer::bch2_btree_write_buffer_flush_going_ro;
        let mut pass_dirty = false;
        let set = unsafe { &mut *self.write_buffer_set.get() };
        for wb in set.buffers.iter_mut() {
            if bch2_btree_write_buffer_flush_going_ro(wb, self, journal)? {
                pass_dirty = true;
            }
        }
        Ok(pass_dirty)
    }

    pub fn flush_cache_dirty_keys(&self, journal_seq: u64) -> usize {
        let trees = unsafe { &*self.trees.get() };
        type DirtyEntry = (BtreeId, Bpos, BtreeEntry);
        let all_dirty: Vec<DirtyEntry> = BTREE_ID_NR
            .iter()
            .map(|&ty| {
                let idx = ty as usize;
                (ty, trees[idx].key_cache.collect_dirty())
            })
            .flat_map(|(ty, entries)| {
                entries
                    .into_iter()
                    .map(move |(pos, entry)| (ty, pos, entry))
            })
            .collect();
        let total = all_dirty.len();
        if total == 0 {
            return 0;
        }
        for (ty, pos, entry) in &all_dirty {
            if trees[*ty as usize]
                .bch2_btree_bset_insert_key_wrapper(entry.clone(), journal_seq)
            {
                trees[*ty as usize].key_cache.mark_clean(pos);
                // bch2_btree_bset_insert_key_wrapper 会使 key cache 失效，
                // 但 flush 场景下条目数据一致，应保留 cache 中 clean 的条目
                trees[*ty as usize].key_cache.insert(*pos, entry.clone());
            }
        }
        total
    }

    pub fn register_ino_map(&self, inode: u64, subvol_id: u32) {
        let mut map = self.subvol_ino_map.lock().unwrap();
        map.entry(inode).or_default().push(subvol_id);
    }

    pub fn cleanup_ino_map(&self, inode: u64, subvol_id: u32) {
        let mut map = self.subvol_ino_map.lock().unwrap();
        if let std::collections::hash_map::Entry::Occupied(mut entry) = map.entry(inode) {
            entry.get_mut().retain(|&id| id != subvol_id);
            if entry.get().is_empty() {
                entry.remove();
            }
        }
    }

    /// 测试用：创建仅含 btrees 的轻量 BchVol（替代旧引擎构造）
    #[cfg(test)]
    pub fn test_trees() -> Self {
        use crate::block_device::MockBlockDevice;
        use crate::btree::write_buffer;
        use std::cell::UnsafeCell;
        use std::collections::HashMap;
        use std::sync::atomic::AtomicU32;
        let dev = Arc::new(BchDev::new(Arc::new(MockBlockDevice::new()), 0));
        let mut sb = crate::storage::superblock::BchSb::new();
        let mut member = crate::storage::superblock::BchSbMember::new(0, "dev-0");
        member.mark_alive([1; 16]);
        // Keep the tiny in-memory fixture free of journal allocation side
        // effects; production-formatted members receive the normal defaults.
        member.flags &= !(0x1f << crate::storage::superblock::member_bits::DATA_ALLOWED_SHIFT);
        member.bucket_size =
            (crate::alloc::BLOCKS_PER_BUCKET * crate::alloc::SECTORS_PER_BLOCK) as u16;
        member.nbuckets = 1;
        sb.primary_dev_idx = 0;
        sb.members = vec![member];
        let device_registry = BchDeviceRegistry::with_bch_dev(dev.clone());

        let vol = Arc::new(Self {
            block_size: 4096,
            logical_capacity: 0,
            capacity: UnsafeCell::new(BchFsCapacity::default()),
            superblock: UnsafeCell::new(sb),
            sb_lock: std::sync::Mutex::new(()),
            state_lock: Mutex::new(()),
            trees: UnsafeCell::new(Self::fresh_trees()),
            btree_foreground_merge_threshold: (crate::btree::node::btree_max_u64s(
                VolumeConfig::default().btree_node_size,
            ) / 3) as u16,
            trim_holes: RwLock::new(HashMap::new()),
            subvol_ino_map: std::sync::Mutex::new(HashMap::new()),
            write_buffer_set: UnsafeCell::new(write_buffer::BtreeWriteBufferSet::new()),
            replicas: Mutex::new(BchReplicasCpu::new()),
            journal: UnsafeCell::new(Arc::new(crate::journal::Journal::new(vec![]))),
            allocator: UnsafeCell::new(crate::alloc::BchAllocator::new(
                4096 * crate::alloc::SECTORS_PER_BLOCK,
            )),
            root_snapshot_id: AtomicU32::new(0),
            root_subvol_id: AtomicU32::new(0),
            config: VolumeConfig::default(),
            opts: BchOpts::from(&VolumeConfig::default()),
            state: AtomicU8::new(VolumeState::New as u8),
            recovery_pass_done: AtomicU8::new(0),
            recovery_passes_complete: AtomicU64::new(0),
            passes_failing: AtomicU64::new(0),
            error_count: AtomicU64::new(0),
            fsck_error: AtomicU64::new(0),
            key_version: AtomicU64::new(0),
            io_clock: std::array::from_fn(|_| AtomicU64::new(0)),
            write_ref_count: AtomicU64::new(0),
            write_drain_notify: tokio::sync::Notify::new(),
            device_registry,
            name: String::new(),
            vol_dir: std::path::PathBuf::new(),
        });
        crate::alloc::background::bch2_fs_capacity_init(&vol)
            .expect("capacity slot allocation failed");
        if let Some(primary_dev) = vol.primary_device_rcu_noerror() {
            for ty in BTREE_ID_NR {
                vol.btree(ty).set_device_ref(primary_dev.clone());
            }
            vol.journal_ref().set_device_ref(primary_dev);
        }
        let dev = vol
            .primary_device_rcu_noerror()
            .expect("test device registered");
        crate::alloc::bch2_dev_buckets_alloc(&vol, &dev).expect("valid test bucket geometry");
        let _state_lock = vol.state_lock.lock().unwrap();
        crate::alloc::background::bch2_recalc_capacity(&vol);
        drop(_state_lock);
        Arc::try_unwrap(vol).expect("BchVol::test_trees should have a single Arc owner")
    }

    // ─── allocator / journal 访问 ───

    /// 获取 allocator 不可变引用
    pub(crate) fn allocator(&self) -> &BchAllocator {
        unsafe { &*self.allocator.get() }
    }

    /// 获取 journal 不可变引用
    pub fn journal_ref(&self) -> &Journal {
        unsafe { &**self.journal.get() }
    }

    /// 克隆 journal 的 Arc（用于传递所有权）
    pub(crate) fn journal_arc(&self) -> Arc<Journal> {
        unsafe { (*self.journal.get()).clone() }
    }

}

impl std::fmt::Debug for BchVol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let snaps = {
            let t = BtreeTrans::new_ro(self);
            bch2_snapshot_list(&t).len()
        };
        f.debug_struct("BchVol")
            .field("name", &self.name)
            .field("state", &self.state())
            .field("snapshots", &snaps)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// 生命周期状态机
// ---------------------------------------------------------------------------

impl BchVol {
    pub fn state(&self) -> VolumeState {
        match self.state.load(Ordering::Acquire) {
            0 => VolumeState::New,
            1 => VolumeState::Starting,
            2 => VolumeState::Rw,
            3 => VolumeState::Error,
            4 => VolumeState::Stopping,
            5 => VolumeState::Stopped,
            6 => VolumeState::RwWithPendingRecovery,
            7 => VolumeState::ReadOnly,
            8 => VolumeState::GoingRo,
            9 => VolumeState::EmergencyRo,
            _ => VolumeState::Error,
        }
    }

    pub fn is_rw(&self) -> bool {
        let s = self.state.load(Ordering::Acquire);
        s == VolumeState::Rw as u8 || s == VolumeState::RwWithPendingRecovery as u8
    }

    pub fn is_read_only(&self) -> bool {
        self.state.load(Ordering::Acquire) == VolumeState::ReadOnly as u8
    }

    // ──── 写引用追踪（对应 bcachefs enumerated_ref writes） ────

    /// 尝试获取写引用。
    ///
    /// 如果卷处于 GoingRo 状态（正在切换只读），返回 false 阻止新写入。
    /// 对应 bcachefs `bch2_write_ref_tryget` + `BCH_FS_going_ro` 检查。
    fn try_begin_write(&self) -> bool {
        match self.state.load(Ordering::Acquire) {
            s if s == VolumeState::Rw as u8 || s == VolumeState::RwWithPendingRecovery as u8 => {
                self.write_ref_count.fetch_add(1, Ordering::AcqRel);
                true
            }
            _ => false,
        }
    }

    /// 释放写引用。
    ///
    /// 如果这是最后一个活跃写入，通知所有等待 drain 的消费者。
    /// 对应 bcachefs `bch2_write_ref_put`。
    fn end_write(&self) {
        if self.write_ref_count.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.write_drain_notify.notify_waiters();
        }
    }

    /// 等待所有飞行写入完成。
    ///
    /// 必须在 GoingRo 状态设置后调用，新写入会被 try_begin_write 阻止。
    /// 对应 bcachefs `bch2_wait_event` + `enumerated_ref_stop_async`。
    async fn drain_writes(&self) {
        loop {
            // 先创建 Notified future，再检查计数值，避免丢失通知
            let notified = self.write_drain_notify.notified();
            if self.write_ref_count.load(Ordering::Acquire) == 0 {
                return;
            }
            notified.await;
        }
    }
}

/// 对应本地 `bch2_dev_set_state()` 的 filesystem-level 状态发布边界。
pub fn bch2_dev_set_state(
    c: &BchVol,
    ca: &BchDev,
    new_state: crate::storage::superblock::BchMemberState,
) -> Result<(), StorageError> {
    let _state_lock = c.state_lock.lock().unwrap();
    let was_rw =
        unsafe { &*ca.mi.get() }.state == crate::storage::superblock::BchMemberState::Rw as u8;

    if unsafe { &*ca.mi.get() }.state == new_state as u8 {
        return Ok(());
    }

    // 对应本地 `scoped_guard(mutex, &c->sb_lock)` 保护 sb 修改
    let member = {
        let _sb_guard = c.sb_lock.lock().unwrap();
        c.superblock_mut().set_member_state(ca.dev_idx, new_state)?;
        c.superblock()
            .member(ca.dev_idx)
            .ok_or_else(|| StorageError::NotFound(format!("member {} not found", ca.dev_idx)))?
            .clone()
    };
    let mi = crate::block_device::bch_dev::bch2_mi_to_cpu(&member);
    unsafe {
        *ca.mi.get() = mi;
    }
    ca.set_member_state(new_state);

    if new_state == crate::storage::superblock::BchMemberState::Rw || was_rw {
        crate::alloc::background::bch2_recalc_capacity(c);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// alloc / start
// ---------------------------------------------------------------------------

impl BchVol {
    /// 纯内存初始化（无 I/O）— 对应 bcachefs `bch2_fs_alloc()`
    pub fn alloc(
        sb: BchSb,
        dev: Arc<BchDev>,
        config: VolumeConfig,
        name: String,
        vol_dir: PathBuf,
    ) -> Self {
        Self::alloc_with_registry(
            sb,
            BchDeviceRegistry::with_bch_dev(dev),
            config,
            name,
            vol_dir,
        )
    }

    pub fn alloc_with_registry(
        mut sb: BchSb,
        device_registry: BchDeviceRegistry,
        config: VolumeConfig,
        name: String,
        vol_dir: PathBuf,
    ) -> Self {
        sb.normalize_members();
        let allocator = BchAllocator::new(config.capacity / crate::types::SECTOR_SIZE as u64);
        let journal = Journal::new(vec![]);
        let mut opts = BchOpts::from(&config);
        if let Some(storage_config) = &sb.storage_config {
            opts.journal_flush_delay = storage_config.journal_flush_delay_ms;
        }
        let btree_foreground_merge_threshold =
            (crate::btree::node::btree_max_u64s(config.btree_node_size) / 3) as u16;

        let vol = Self {
            block_size: sb.block_size,
            logical_capacity: sb.capacity,
            capacity: UnsafeCell::new(BchFsCapacity::default()),
            device_registry,
            superblock: UnsafeCell::new(sb.clone()),
            sb_lock: std::sync::Mutex::new(()),
            state_lock: Mutex::new(()),
            trees: UnsafeCell::new(std::array::from_fn(|i| {
                Btree::new_with_type(crate::btree::BTREE_ID_NR[i])
            })),
            btree_foreground_merge_threshold,
            trim_holes: RwLock::new(HashMap::new()),
            subvol_ino_map: std::sync::Mutex::new(HashMap::new()),
            write_buffer_set: UnsafeCell::new(write_buffer::BtreeWriteBufferSet::new()),
            replicas: Mutex::new(BchReplicasCpu::new()),
            journal: UnsafeCell::new(Arc::new(journal)),
            allocator: UnsafeCell::new(allocator),
            root_snapshot_id: AtomicU32::new(0),
            root_subvol_id: AtomicU32::new(0),
            opts,
            config,
            state: AtomicU8::new(VolumeState::New as u8),
            recovery_pass_done: AtomicU8::new(0),
            recovery_passes_complete: AtomicU64::new(0),
            passes_failing: AtomicU64::new(0),
            error_count: AtomicU64::new(0),
            fsck_error: AtomicU64::new(0),
            key_version: AtomicU64::new(0),
            io_clock: std::array::from_fn(|_| AtomicU64::new(0)),
            write_ref_count: AtomicU64::new(0),
            write_drain_notify: Notify::new(),
            name,
            vol_dir,
        };
        crate::alloc::background::bch2_fs_capacity_init(&vol)
            .expect("capacity slot allocation failed");
        if let Some(primary_dev) = vol.primary_device_rcu_noerror() {
            for ty in BTREE_ID_NR {
                vol.btree(ty).set_device_ref(primary_dev.clone());
            }
            vol.journal_ref().set_device_ref(primary_dev);
        }
        for dev_idx in vol.device_registry.dev_indices() {
            let dev = vol
                .device_registry
                .resolve_bch_dev(dev_idx)
                .expect("registered device disappeared");
            if let Some(member) = sb.member(dev_idx) {
                dev.set_initialized(member.initialized());
                dev.set_member_state(member.state());
            }
            *dev.disk_sb.lock().unwrap() = sb.clone();
            {
                let member = sb
                    .member(dev_idx)
                    .expect("registered device has no superblock member");
                let bucket_blocks = u64::from(member.bucket_size) / crate::alloc::SECTORS_PER_BLOCK;
                let mut ja = dev.journal.lock().unwrap();
                ja.bucket_seq = if sb.journal_bucket_seq.len() == sb.journal_buckets.len() {
                    sb.journal_bucket_seq.clone()
                } else {
                    vec![0; sb.journal_buckets.len()]
                };
                ja.discard_idx = sb.journal_discard_idx;
                ja.dirty_idx_ondisk = sb.journal_dirty_idx_ondisk;
                ja.dirty_idx = sb.journal_dirty_idx;
                ja.nr = sb.journal_buckets.len() as u32;
                ja.buckets = sb
                    .journal_buckets
                    .iter()
                    .map(|addr| addr / bucket_blocks)
                    .collect();
                // 对应本地 bcachefs
                // `bch2_journal_pos_from_member_info_resume()`
                // (`journal/read.c:36-50`)：先恢复 bucket index，再恢复
                // 当前 bucket 的剩余 sector 数；无效字段保持初始化值。
                if member.last_journal_bucket < ja.nr {
                    ja.cur_idx = member.last_journal_bucket;
                }
                if member.last_journal_bucket_offset <= u32::from(member.bucket_size) {
                    ja.sectors_free =
                        u32::from(member.bucket_size) - member.last_journal_bucket_offset;
                }
            }
            crate::alloc::bch2_dev_buckets_alloc(&vol, &dev)
                .expect("validated member bucket geometry");
        }
        let _state_lock = vol.state_lock.lock().unwrap();
        crate::alloc::background::bch2_recalc_capacity(&vol);
        drop(_state_lock);
        vol
    }

    pub fn alloc_with_devices(
        sb: BchSb,
        devices: impl IntoIterator<Item = Arc<BchDev>>,
        config: VolumeConfig,
        name: String,
        vol_dir: PathBuf,
    ) -> Self {
        Self::alloc_with_registry(
            sb,
            BchDeviceRegistry::from_devices(devices),
            config,
            name,
            vol_dir,
        )
    }

    /// 获取当前卷 superblock 的只读引用。
    pub fn superblock(&self) -> &BchSb {
        unsafe { &*self.superblock.get() }
    }

    /// 获取当前卷 superblock 的可变引用。
    #[allow(clippy::mut_from_ref)]
    pub fn superblock_mut(&self) -> &mut BchSb {
        unsafe { &mut *self.superblock.get() }
    }

    /// 对齐 bcachefs `bch2_fs_start()` (fs.c:1565-1580)
    ///
    /// 薄包装：由 `bch2_fs_start()` 调用 `__bch2_fs_start()` 并在出错时记录日志。
    /// subvol 将日志处理留给调用方，函数体只做委托。
    pub async fn start(&self) -> Result<(), StorageError> {
        // 对齐 bch2_fs_start(): 委托给 __bch2_fs_start (c->recovery_task = NULL 等无关)
        self.__bch2_fs_start().await
    }

    /// 对齐 bcachefs `__bch2_fs_start()` (fs.c:1496-1562)
    async fn __bch2_fs_start(&self) -> Result<(), StorageError> {
        // 对齐 BUG_ON(test_bit(BCH_FS_started, &c->flags));
        self.state
            .compare_exchange(
                VolumeState::New as u8,
                VolumeState::Starting as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(|_| StorageError::AlreadyExists("BchVol::__bch2_fs_start"))?;

        // ── 对齐 bch2_dev_allocator_add + bch2_recalc_capacity (fs.c:1500-1507) ──
        // subvol: allocator 已在 BchVol::alloc 时初始化，无需重复操作。

        // ── 对齐 bch2_fs_may_start (fs.c:1509) ──
        // subvol: 设备在线状态由 open_pool() 的调用方保证，跳过此检查。

        // ── 对齐 bch2_fs_reconcile_init (fs.c:1511) ──
        // subvol: 无 reconciler 逻辑，跳过。

        // ── 对齐 bch2_fs_counters_init_late (fs.c:1512) ──
        // subvol: 无性能计数器，跳过。

        // ── 对齐 bch2_request_incompat_feature (fs.c:1524) ──
        // subvol: 无须请求不兼容特性，跳过。

        // ── 对齐 go_rw_in_recovery → bch2_fs_init_rw (fs.c:1530-1531) ──
        self.bch2_fs_init_rw()?;

        // ── 对齐 bch2_opts_hooks_pre_set (fs.c:1537) ──
        // subvol: 无挂载选项校验，跳过。

        // ── Dispatch: INITIALIZED ? bch2_fs_recovery : bch2_fs_initialize (fs.c:1542-1544) ──
        // bcachefs 通过 c->disk_sb.sb 获取 superblock，subvol 通过 self.superblock()
        let result = {
            let sb = self.superblock();
            if sb.clean_shutdown && sb.journal_seq == 0 {
                self.bch2_fs_initialize().await
            } else {
                self.bch2_fs_recovery().await
            }
        };

        if let Err(e) = result {
            self.state
                .store(VolumeState::Error as u8, Ordering::Release);
            return Err(e);
        }

        // ── 对齐 bch2_opts_hooks_pre_set 第2次 (fs.c:1546) ──
        // subvol: 跳过（同上）

        // ── 对齐 set_bit(BCH_FS_started) + wake_up(&c->ro_ref_wait) (fs.c:1552-1553) ──
        // subvol: "started" 由 state ≥ ReadOnly 隐式标识，无需显式标记。

        // ── 对齐 bch2_fs_read_only / bch2_fs_read_write (fs.c:1555-1559) ──
        // bcachefs 通过 c->opts.read_only 判断；subvol 通过 config.read_only
        if self.config.read_only {
            self.bch2_fs_read_only().await?;
        } else if !self.is_rw() {
            self.bch2_fs_read_write().await?;
        }

        Ok(())
    }

    /// 对齐 bcachefs `bch2_fs_initialize()` (recovery.c:1023-1148)
    /// 对应 rust 版本 — 不接受 sb 参数，bcachefs 通过 c->disk_sb.sb 获取 superblock。
    async fn bch2_fs_initialize(&self) -> Result<(), StorageError> {
        let sb = self.superblock();
        let n = sb.journal_buckets.len();
        let journal_state = JournalSuperblockState {
            bucket_addrs: sb.journal_buckets.clone(),
            last_seq: sb.journal_last_seq,
            last_seq_ondisk: sb.journal_seq,
            last_bucket: sb.journal_last_bucket,
            discard_idx: sb.journal_discard_idx,
            dirty_idx: sb.journal_dirty_idx,
            dirty_idx_ondisk: sb.journal_dirty_idx_ondisk,
            bucket_seq: if sb.journal_bucket_seq.len() == n {
                sb.journal_bucket_seq.clone()
            } else {
                vec![0; n]
            },
            replayed_seqs: sb.replayed_seqs.clone(),
        };
        let _ = sb;
        let journal = Journal::from_superblock(&journal_state);
        unsafe {
            *self.journal.get() = Arc::new(journal);
        }
        self.state
            .store(VolumeState::ReadOnly as u8, Ordering::Release);

        if let Some(vol) = self.btree(crate::btree::BTREE_ID_NR[0]).vol_arc() {
            self.journal_ref().set_vol_ref(&vol);
            if let Some(dev) = vol.primary_device_rcu_noerror() {
                self.journal_ref().set_device_ref(dev);
            }
        }

        crate::snap::table::bch2_fs_snapshots_init(self)?;

        {
            let mut t = BtreeTrans::new(self);
            t.bch2_trans_begin();
            bch2_initialize_subvolumes(&mut t)?;

            let subvol_val = crate::subvol::BchSubvolume::new(u32::MAX, BCACHEFS_ROOT_INO, 0, 0);
            crate::subvol::bch2_subvolume_validate(
                &t, BCACHEFS_ROOT_SUBVOL as u32, &subvol_val,
            )?;
            t.vol().register_ino_map(BCACHEFS_ROOT_INO, BCACHEFS_ROOT_SUBVOL as u32);

            t.bch2_trans_commit()?;
        }
        self.root_subvol_id.store(BCACHEFS_ROOT_SUBVOL as u32, Ordering::Release);
        self.root_snapshot_id.store(u32::MAX, Ordering::Release);

        crate::snap::table::bch2_snapshots_read(self);

        crate::btree::gc::bch2_presplit_shard_boundaries(self)?;

        self.flush().await?;

        self.journal_ref().bch2_journal_advance_rewind_seq(
            self.journal_ref().last_seq_ondisk.load(Ordering::Acquire).wrapping_add(1),
        );

        if let Some(dev) = self.primary_device_rcu_noerror() {
            let mut sb = BchSb::read_from_device(&dev).await?;
            let journal_state = self.journal_ref().to_superblock_state();
            sb.journal_last_seq = journal_state.last_seq;
            sb.journal_seq = journal_state.last_seq_ondisk;
            sb.journal_last_bucket = journal_state.last_bucket;
            sb.journal_discard_idx = journal_state.discard_idx;
            sb.journal_dirty_idx = journal_state.dirty_idx;
            sb.journal_dirty_idx_ondisk = journal_state.dirty_idx_ondisk;
            sb.journal_bucket_seq = journal_state.bucket_seq;
            sb.replayed_seqs = journal_state.replayed_seqs;
            sb.clean_shutdown = false;
            sb.write_to_device(dev.as_ref()).await?;
        }
        Ok(())
    }

    /// 对齐 bcachefs `bch2_fs_recovery()` (recovery.c:576-1019)
    /// 对应 rust 版本 — 不接受 sb 参数，bcachefs 通过 c->disk_sb.sb 获取 superblock。
    async fn bch2_fs_recovery(&self) -> Result<(), StorageError> {
        let sb = self.superblock();
        if sb.clean_shutdown {
            // 正常关闭：读取 btree roots、alloc、freespace、journal
            for ty in crate::btree::BTREE_ID_NR {
                let idx = ty as usize;
                let root_addr = sb.root_addrs.get(idx).copied().unwrap_or(0);
                let root_level = sb.root_levels.get(idx).copied();
                self.btree(ty)
                    .bch2_btree_root_read(root_addr, root_level)
                    .await?;
            }

            let allocator = unsafe { &*self.allocator.get() };
            allocator.bch2_alloc_read(self)?;
            crate::alloc::bch2_rebuild_freespace(self)?;

            let root_snapshot_id = {
                let t2 = BtreeTrans::new_ro(self);
                bch2_subvolume_get_snapshot(&t2, BCACHEFS_ROOT_SUBVOL as u32)
                    .unwrap_or(u32::MAX)
            };

            let n = sb.journal_buckets.len();
            let journal_state = JournalSuperblockState {
                bucket_addrs: sb.journal_buckets.clone(),
                last_seq: sb.journal_last_seq,
                last_seq_ondisk: sb.journal_seq,
                last_bucket: sb.journal_last_bucket,
                discard_idx: sb.journal_discard_idx,
                dirty_idx: sb.journal_dirty_idx,
                dirty_idx_ondisk: sb.journal_dirty_idx_ondisk,
                bucket_seq: if sb.journal_bucket_seq.len() == n {
                    sb.journal_bucket_seq.clone()
                } else {
                    vec![0; n]
                },
                replayed_seqs: sb.replayed_seqs.clone(),
            };
            let journal = Journal::from_superblock(&journal_state);

            unsafe {
                *self.journal.get() = Arc::new(journal);
            }
            self.root_snapshot_id
                .store(root_snapshot_id, Ordering::Release);
            self.state
                .store(VolumeState::ReadOnly as u8, Ordering::Release);
            Ok(())
        } else {
            // 不洁关闭：journal recovery
            let n = sb.journal_buckets.len();
            let journal_state = JournalSuperblockState {
                bucket_addrs: sb.journal_buckets.clone(),
                last_seq: sb.journal_last_seq,
                last_seq_ondisk: sb.journal_seq,
                last_bucket: sb.journal_last_bucket,
                discard_idx: sb.journal_discard_idx,
                dirty_idx: sb.journal_dirty_idx,
                dirty_idx_ondisk: sb.journal_dirty_idx_ondisk,
                bucket_seq: if sb.journal_bucket_seq.len() == n {
                    sb.journal_bucket_seq.clone()
                } else {
                    vec![0; n]
                },
                replayed_seqs: sb.replayed_seqs.clone(),
            };
            let journal = Journal::from_superblock(&journal_state);
            let recovery_vol = Box::new(BchVol::alloc_with_registry(
                sb.clone(),
                self.device_registry.clone(),
                self.config.clone(),
                self.name.clone(),
                self.vol_dir.clone(),
            ));
            if let Some(dev) = recovery_vol.primary_device_rcu_noerror() {
                journal.set_device_ref(dev);
            }
            let mut state = recovery::RecoveryState::new(recovery_vol, journal, sb.clone());
            recovery::bch2_fs_recovery(&mut state).await?;

            let mut sb = state.superblock.clone();
            let jss = state.journal.to_superblock_state();
            sb.pass_done = state.pass_done as u64;
            sb.replayed_seqs.clone_from(&jss.replayed_seqs);
            sb.journal_last_seq = jss.last_seq;
            sb.journal_seq = jss.last_seq_ondisk;
            sb.journal_last_bucket = jss.last_bucket;
            sb.journal_discard_idx = jss.discard_idx;
            sb.journal_dirty_idx = jss.dirty_idx;
            sb.journal_dirty_idx_ondisk = jss.dirty_idx_ondisk;
            sb.journal_bucket_seq = jss.bucket_seq;
            let mut first_error = None;
            for dev_idx in self.device_registry.dev_indices() {
                let Some(dev) = self.device_registry.resolve_bch_dev(dev_idx) else {
                    continue;
                };
                if !dev.is_online() {
                    continue;
                }
                if let Err(error) = sb.write_to_device(dev.as_ref()).await {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
            if let Some(error) = first_error {
                return Err(error);
            }

            let root_snapshot_id = unsafe {
                *self.trees.get() =
                    std::mem::replace(&mut *state.vol.trees.get(), Self::fresh_trees());
                let t2 = BtreeTrans::new_ro(self);
                bch2_subvolume_get_snapshot(&t2, BCACHEFS_ROOT_SUBVOL as u32)
                    .unwrap_or(u32::MAX)
            };
            unsafe {
                *self.write_buffer_set.get() = std::mem::replace(
                    &mut *state.vol.write_buffer_set.get(),
                    write_buffer::BtreeWriteBufferSet::new(),
                );
            }

            let primary_dev = self.primary_device_rcu_noerror().ok_or_else(|| {
                StorageError::NotFound("BchVol::recovery: no registered device".into())
            })?;
            let sb = BchSb::read_from_device(&primary_dev).await?;
            let n = sb.journal_buckets.len();
            let journal_state = JournalSuperblockState {
                bucket_addrs: sb.journal_buckets.clone(),
                last_seq: sb.journal_last_seq,
                last_seq_ondisk: sb.journal_seq,
                last_bucket: sb.journal_last_bucket,
                discard_idx: sb.journal_discard_idx,
                dirty_idx: sb.journal_dirty_idx,
                dirty_idx_ondisk: sb.journal_dirty_idx_ondisk,
                bucket_seq: if sb.journal_bucket_seq.len() == n {
                    sb.journal_bucket_seq.clone()
                } else {
                    vec![0; n]
                },
                replayed_seqs: sb.replayed_seqs.clone(),
            };
            let journal = Journal::from_superblock(&journal_state);

            unsafe {
                *self.journal.get() = Arc::new(journal);
            }
            self.root_snapshot_id
                .store(root_snapshot_id, Ordering::Release);
            self.state
                .store(VolumeState::ReadOnly as u8, Ordering::Release);
            Ok(())
        }
    }

    /// 对齐 bcachefs `bch2_fs_init_rw()` (fs.c:884-908)
    ///
    /// 在 init/recovery 前预分配 RW 基础设施（journal reclaim、auto flush 等）。
    /// 幂等：通过检查 reclaim_interval_ms > 0 判断是否已启动。
    fn bch2_fs_init_rw(&self) -> Result<(), StorageError> {
        if self.journal_ref().reclaim_interval_ms.load(Ordering::Acquire) > 0 {
            return Ok(());
        }
        let j = self.journal_arc();
        j.start_background_reclaim(j.clone(), 100);
        j.start_auto_flush(j.clone());
        Ok(())
    }

    fn fresh_trees() -> [Btree; 28] {
        std::array::from_fn(|i| Btree::new_with_type(crate::btree::BTREE_ID_NR[i]))
    }

    pub(crate) fn attach_tree_refs(&self, vol: &Arc<BchVol>) {
        let primary_dev = vol.primary_device_rcu_noerror();
        for tree in unsafe { &*self.trees.get() } {
            tree.set_vol_ref(vol);
            if let Some(dev) = primary_dev.as_ref() {
                tree.set_device_ref(dev.clone());
            }
        }
        if let Some(dev) = primary_dev {
            self.journal_ref().set_device_ref(dev);
        }
    }

    /// 直接解析 member 记录对应的设备后端，不检查在线状态。
    pub(crate) fn device_rcu_noerror(&self, dev_idx: u8) -> Option<Arc<BchDev>> {
        if !self.superblock().member_exists(dev_idx) {
            return None;
        }
        self.device_registry.resolve_bch_dev(dev_idx)
    }

    pub(crate) fn primary_device_rcu_noerror(&self) -> Option<Arc<BchDev>> {
        let primary_idx = self.superblock().primary_dev_idx;
        if let Some(primary) = self.device_registry.resolve_bch_dev(primary_idx) {
            if primary.is_online() {
                return Some(primary);
            }
        }

        // 对齐本地 bcachefs `for_each_online_member_rcu()`
        // (`fs/sb/members.h:110-145`)：主设备故障后，元数据路径继续使用
        // 按成员索引排序的在线设备；仅在整个卷没有在线成员时保留主设备回退。
        self.device_registry
            .online_dev_indices()
            .into_iter()
            .find_map(|dev_idx| self.device_registry.resolve_bch_dev(dev_idx))
            .or_else(|| self.device_registry.resolve_bch_dev(primary_idx))
    }
}

// ---------------------------------------------------------------------------
// open / create
// ---------------------------------------------------------------------------

impl BchVol {
    /// 打开已有卷：read super → alloc → start。
    ///
    /// 对应本地 bcachefs `bch2_fs_open()` (`fs/init/fs.c:1689-1779`)：
    /// 读取 superblock 后分配文件系统上下文，再进入统一 start/recovery 路径。
    pub async fn open(
        backend: Arc<dyn BlockDevice>,
        vol_dir: &Path,
        name: &str,
    ) -> Result<Arc<Self>, StorageError> {
        let dev = Arc::new(BchDev::new(backend, 0));
        let sb = BchSb::read_from_device(&dev).await?;
        let storage = sb.storage_config.clone().unwrap_or_default();
        let config = VolumeConfig {
            block_size: sb.block_size,
            capacity: sb.capacity,
            btree_node_size: storage.btree_node_size,
            data_replicas: storage.data_replicas,
            read_only: storage.read_only,
            ..VolumeConfig::default()
        };
        let vol = Arc::new(Self::alloc(
            sb,
            dev,
            config,
            name.to_owned(),
            vol_dir.to_owned(),
        ));
        vol.attach_tree_refs(&vol);
        vol.journal_ref().set_vol_ref(&vol);
        vol.start().await?;
        vol.attach_tree_refs(&vol);
        vol.journal_ref().set_vol_ref(&vol);
        Ok(vol)
    }

    /// 打开内部池（自动创建如果不存在）
    ///
    /// `pool_dir`: `~/.subvol/pool/` — 存放 superblock + blocks/
    /// 若目录不存在或尚未初始化，自动执行初始化流程。
    pub async fn open_pool(
        pool_dir: &Path,
        pool_name: &str,
    ) -> Result<Arc<Self>, StorageError> {
        let blocks_dir = pool_dir.join("blocks");
        if blocks_dir.exists() {
            let backend = open_backend(BackendType::Nfs, pool_dir, 4096).await?;
            Self::open(backend, pool_dir, pool_name).await
        } else {
            let capacity = 1 << 30; // 1GB — 稀疏分配，实际物理增长
            let block_size = 4096u32;

            let bucket_count = capacity / crate::alloc::DEFAULT_BUCKET_SIZE;
            if bucket_count < 1 << 9 {
                return Err(StorageError::InvalidArgument(format!(
                    "capacity too small: {} buckets, need at least {}",
                    bucket_count,
                    1 << 9
                )));
            }

            let backend = create_backend(BackendType::Nfs, pool_dir, capacity).await?;
            let dev = Arc::new(BchDev::new(backend, 0));
            let mut sb = BchSb::with_volume_info(
                pool_name.to_string(),
                1,
                "default".to_string(),
                block_size,
                capacity,
                BackendType::Nfs,
            );

            let journal_bucket_count = (bucket_count >> 7).max(8).min(128);
            let journal_buckets: Vec<u64> = (0..journal_bucket_count)
                .map(|i| (i + 1) * crate::alloc::BLOCKS_PER_BUCKET)
                .collect();
            sb.journal_buckets = journal_buckets;
            sb.journal_last_seq = 0;
            sb.journal_seq = 0;
            sb.journal_last_bucket = 0;
            sb.journal_discard_idx = 0;
            sb.journal_dirty_idx = 0;
            sb.journal_dirty_idx_ondisk = 0;
            sb.journal_bucket_seq = vec![0; journal_bucket_count as usize];
            sb.clean_shutdown = true;
            sb.storage_config = Some(StorageConfig::default());
            sb.init_quota_config();
            sb.write_to_device(dev.as_ref()).await?;

            let sb = BchSb::read_from_device(&dev).await?;
            let storage = sb.storage_config.clone().unwrap_or_default();
            let config = VolumeConfig {
                block_size: sb.block_size,
                capacity: sb.capacity,
                btree_node_size: storage.btree_node_size,
                data_replicas: storage.data_replicas,
                ..VolumeConfig::default()
            };
            let vol = Arc::new(Self::alloc(
                sb,
                dev.clone(),
                config,
                pool_name.to_owned(),
                pool_dir.to_owned(),
            ));
            vol.attach_tree_refs(&vol);
            vol.journal_ref().set_vol_ref(&vol);
            vol.start().await?;
            vol.attach_tree_refs(&vol);
            vol.journal_ref().set_vol_ref(&vol);

            Ok(vol)
        }
    }
}

// ---------------------------------------------------------------------------
// bch2_fs_read_write / bch2_fs_read_only
// ---------------------------------------------------------------------------

impl BchVol {
    /// 对齐 bcachefs `bch2_fs_read_write()` (fs.c:647)
    ///
    /// 启动 journal reclaim 后台线程 + 一次性后台操作。
    /// 对齐 bcachefs `__bch2_fs_read_write()` 的启动顺序。
    pub async fn bch2_fs_read_write(&self) -> Result<(), StorageError> {
        let current = self.state.load(Ordering::Acquire);
        if current != VolumeState::ReadOnly as u8 {
            return Err(StorageError::AlreadyExists(
                "BchVol::bch2_fs_read_write: not in ReadOnly state",
            ));
        }
        // 1. 标记 superblock dirty（对齐 bcachefs bch2_fs_mark_dirty, sb/clean.c:259）
        //    在 journal 启动前标记，确保 clean_shutdown = false 已持久化，
        //    使 recovery 在干净关闭后能正确判断是否需要 journal 回放。
        let primary_dev = self.primary_device_rcu_noerror().ok_or_else(|| {
            StorageError::NotFound("BchVol::bch2_fs_read_write: no registered device".into())
        })?;
        let mut sb = BchSb::read_from_device(&primary_dev).await?;
        sb.clean_shutdown = false;
        let mut first_error = None;
        for dev_idx in self.device_registry.dev_indices() {
            let Some(dev) = self.device_registry.resolve_bch_dev(dev_idx) else {
                continue;
            };
            if !dev.is_online() {
                continue;
            }
            if let Err(error) = sb.write_to_device(dev.as_ref()).await {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
        if let Some(error) = first_error {
            return Err(error);
        }

        // 对齐 __bch2_fs_read_write (fs.c:582): pre-alloc RW infrastructure
        self.bch2_fs_init_rw()?;

        // 对齐 __bch2_fs_read_write (fs.c:587): set RW state + enable writes
        self.state.store(VolumeState::Rw as u8, Ordering::Release);
        for dev_idx in self.device_registry.dev_indices() {
            if let Some(dev) = self.device_registry.resolve_bch_dev(dev_idx) {
                dev.set_write_enabled(true);
            }
        }

        // bcachefs 在工作线程中由 alloc trigger 安排 discard、invalidate 和
        // gc_gens；此处不添加 subvol 自有的启动扫描。
        Ok(())
    }

    /// 对齐 bcachefs `bch2_fs_read_only()` (fs.c:415)
    ///
    /// 对齐 bcachefs `__bch2_fs_read_only` (fs.c:317-457) 的完整流程：
    ///   1. `BCH_FS_going_ro` + `enumerated_ref_stop_async(&c->writes)` — 阻止新写入 + 等待飞行写入
    ///   2. `__bch2_fs_read_only` — flush btree + journal
    ///   3. 停止后台线程
    ///   4. 清除 `BCH_FS_going_ro`，切换为全只读
    pub async fn bch2_fs_read_only(&self) -> Result<(), StorageError> {
        // 对齐 bcachefs: 如果当前并不处于 RW，直接停 journal reclaim 后返回。
        // 这会保留当前状态，不会错误进入 GoingRo/ReadOnly。
        if !self.is_rw() {
            self.journal_ref().stop_background_reclaim().await;
            return Ok(());
        }

        // Step 1: 设置 GoingRo 状态（禁止新写入）
        // 对应 bcachefs set_bit(BCH_FS_going_ro, &c->flags) (fs.c:333)
        self.state
            .store(VolumeState::GoingRo as u8, Ordering::Release);

        // Step 2: 等待飞行写入完成（对应 enumerated_ref_stop_async + wait_event, fs.c:334-349）
        // 所有已开始的写入会正常完成，新写入被 GoingRo 阻止
        self.drain_writes().await;

        // Step 3: flush btree + journal + 写 superblock
        // 对应 __bch2_fs_read_only 的 flush 循环 (fs.c:352-402)
        let flush_result = self.flush_read_only().await;

        // Step 4: 停止 journal 后台线程（对齐 fs.c:418 + 对应 __bch2_fs_read_only 停止顺序）
        self.journal_ref().stop_background_reclaim().await;
        self.journal_ref().stop_auto_flush().await;

        // Match `__bch2_dev_read_only()` stopping each device's WRITE refs
        // after draining the filesystem write refs (`fs/init/dev.c:370-430`).
        for dev_idx in self.device_registry.dev_indices() {
            if let Some(dev) = self.device_registry.resolve_bch_dev(dev_idx) {
                dev.set_write_enabled(false);
            }
        }

        // Step 5: 即使 flush 失败也必须完成后台线程收口；本地
        // `__bch2_fs_read_only()` (`fs/init/fs.c:317-457`) 在错误/紧急只读
        // 路径同样会停止 journal 与设备写引用，不能把卷永久留在 GoingRo。
        match flush_result {
            Ok(()) => {
                self.state
                    .store(VolumeState::ReadOnly as u8, Ordering::Release);
                Ok(())
            }
            Err(error) => {
                self.state
                    .store(VolumeState::Error as u8, Ordering::Release);
                Err(error)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// read-only flush / close / delete
// ---------------------------------------------------------------------------

impl BchVol {
    async fn flush_pending_root_journals(&self) -> Result<(), StorageError> {
        for ty in crate::btree::BTREE_ID_NR {
            if let Some(root) = self.btree(ty).take_pending_root_journal() {
                self.journal_ref()
                    .append_btree_root(ty, root.root_addr, root.level, false)
                    .await
                    .map_err(|e| StorageError::JournalError(e.to_string()))?;
            }
        }
        Ok(())
    }

    /// 只读收口：flush dirty btree + journal blacklist + 写 superblock
    ///
    /// 对应 bcachefs `__bch2_fs_read_only()` (fs.c:317)
    /// 包含 flush 循环（2+ 轮干净 pass）和 journal error 处理
    async fn flush_read_only(&self) -> Result<(), StorageError> {
        let primary_dev = self.primary_device_rcu_noerror().ok_or_else(|| {
            StorageError::NotFound("BchVol::flush_read_only: no registered device".into())
        })?;
        let mut sb = BchSb::read_from_device(&primary_dev).await?;

        // flush 循环：对齐 bcachefs __bch2_fs_read_only 的 2+ 轮 clean pass
        let mut clean_passes = 0u32;
        while clean_passes < 2 {
            let mut pass_dirty = false;

            // bcachefs `bch2_btree_interior_updates_flush()` 必须在
            // dirty-node/journal 收口前排空异步内部更新，避免父节点在
            // 子节点持久化完成前被当作可写回节点处理。
            for ty in crate::btree::BTREE_ID_NR {
                self.btree(ty).bch2_btree_interior_updates_flush().await;
            }

            // 1. flush dirty btree nodes
            let per_type = self.flush_dirty_nodes();
            let nodes: Vec<(BtreeId, u64, Arc<BtreeNode>)> = per_type
                .into_iter()
                .flat_map(|(ty, list)| list.into_iter().map(move |(id, node)| (ty, id, node)))
                .collect();
            if !nodes.is_empty() {
                pass_dirty = true;
            }
            let mut write_futs = Vec::with_capacity(nodes.len());
            for (ty, _node_id, node) in nodes {
                let req = AllocRequest::new(Watermark::Btree, crate::alloc::BchDataType::Btree);
                let blocks = u64::from(node.node_size).div_ceil(crate::alloc::DEFAULT_BLOCK_SIZE);
                let block_addr = self.alloc_btree_sectors(&req, blocks)?;
                let cache = self.btree(ty).cache();
                node.try_set_block_addr(block_addr);
                write_futs.push(crate::btree::io::btree_node_write_mut(node, cache, None));
            }
            try_join_all(write_futs).await?;

            // 2. flush key cache
            for ty in crate::btree::BTREE_ID_NR {
                let btree = self.btree(ty);
                let still_dirty = btree
                    .key_cache
                    .bch2_btree_key_cache_flush_going_ro(|_, _| true);
                if still_dirty {
                    pass_dirty = true;
                }
            }

            // Phase-2a: write buffer flush（对应 bcachefs write_buffer.c:601）
            // 遍历所有 write buffer 将 pending 条目刷入 btree
            let wb_dirty = self.flush_write_buffers_going_ro(None)?;
            if wb_dirty {
                pass_dirty = true;
            }

            // 3. flush root journal entries and journal
            self.flush_pending_root_journals().await?;
            self.journal_ref()
                .bch2_journal_flush()
                .await
                .map_err(|e| StorageError::JournalError(e.to_string()))?;

            if pass_dirty {
                clean_passes = 0;
            } else {
                clean_passes += 1;
            }
        }

        // bcachefs persists btree roots through root journal entries and the
        // superblock root arrays; it does not serialize a separate data image.
        sb.root_addrs.clear();
        sb.root_levels.clear();
        for ty in crate::btree::BTREE_ID_NR {
            let root = self.btree(ty).root();
            let (addr, level) = self.btree(ty).current_root_disk_info().unwrap_or((0, root.depth));
            sb.root_addrs.push(addr);
            sb.root_levels.push(level);
        }

        // journal flush
        self.journal_ref()
            .bch2_journal_flush()
            .await
            .map_err(|e| StorageError::JournalError(e.to_string()))?;

        let new_state = self.journal_ref().to_superblock_state();
        sb.journal_last_seq = new_state.last_seq;
        sb.journal_seq = new_state.last_seq_ondisk;
        sb.journal_last_bucket = new_state.last_bucket;
        sb.journal_discard_idx = new_state.discard_idx;
        sb.journal_dirty_idx = new_state.dirty_idx;
        sb.journal_dirty_idx_ondisk = new_state.dirty_idx_ondisk;
        sb.journal_bucket_seq = new_state.bucket_seq;
        sb.replayed_seqs = new_state.replayed_seqs;

        // 对应本地 bcachefs `bch2_journal_pos_from_member_info_set()`
        // (`journal/read.c:24-33`)：在写 superblock 前逐 member 保存
        // 当前 journal bucket 与已使用 sector 偏移。
        for dev_idx in self.device_registry.dev_indices() {
            let Some(dev) = self.device_registry.resolve_bch_dev(dev_idx) else {
                continue;
            };
            let ja = dev.journal.lock().unwrap();
            let Some(member) = sb.member_mut(dev_idx) else {
                continue;
            };
            member.last_journal_bucket = ja.cur_idx;
            member.last_journal_bucket_offset =
                u32::from(member.bucket_size).saturating_sub(ja.sectors_free);
        }

        // journal error 时不设 clean_shutdown（对齐 bcachefs __bch2_fs_read_only）
        let journal_ok = self.journal_ref().bch2_journal_error_check().is_none();
        if journal_ok {
            sb.clean_shutdown = true;
        }
        // 对齐本地 bcachefs `__bch2_write_super()` (`fs/sb/io.c:1390-1430`)：
        // 每个在线成员都必须尝试写入自己的 superblock 副本，不能只更新
        // 当前主设备；所有写入完成后再返回按成员顺序遇到的首个错误。
        let sb_ref = &sb;
        let writes = self
            .device_registry
            .dev_indices()
            .into_iter()
            .filter_map(|dev_idx| self.device_registry.resolve_bch_dev(dev_idx))
            .filter(|dev| dev.is_online())
            .map(move |dev| {
                let sb = sb_ref;
                async move { sb.write_to_device(dev.as_ref()).await }
            });
        let results = join_all(writes).await;
        let mut first_error = None;
        for result in results {
            if let Err(error) = result {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
        if let Some(error) = first_error {
            return Err(error);
        }

        Ok(())
    }

    /// 关闭卷 — 对应 bcachefs `bch2_fs_stop()` (fs.c:738)
    ///
    /// 对齐 bcachefs 关闭流程：
    ///   1. Stopping — 通知系统卷正在关闭
    ///   2. GoingRo + drain_writes — 阻止新写入并等待飞行写入完成（写引用追踪）
    ///   3. flush_all_reads — 等待 btree 读 I/O 完成（对应 bcachefs fs.c:696）
    ///   4. flush btree + journal
    ///   5. backend.flush — 确保所有数据落盘
    ///   6. Stopped — 标记终止
    pub async fn close(&self) -> Result<(), StorageError> {
        self.state
            .store(VolumeState::Stopping as u8, Ordering::Release);
        // 设置 GoingRo 阻止新写入 + 等待飞行写入完成
        self.state
            .store(VolumeState::GoingRo as u8, Ordering::Release);
        self.drain_writes().await;
        // 对应 bcachefs __bch2_fs_stop (fs.c:696): bch2_btree_flush_all_reads
        crate::btree::io::bch2_btree_flush_all_reads();
        self.journal_ref().stop_background_reclaim().await;
        self.journal_ref().stop_auto_flush().await;
        // bcachefs stop 先完成只读收口并写 superblock，再对所有在线成员
        // 执行设备级 flush，确保 roots/journal 元数据和设备缓存顺序一致。
        let readonly_result = self.flush_read_only().await;
        let flush_result = self.flush().await;
        // 即使任一持久化阶段失败，也必须发布终态；本地
        // `bch2_fs_stop()` (`fs/init/fs.c:738-817`) 不会把卷遗留在
        // Stopping/GoingRo。
        self.state
            .store(VolumeState::Stopped as u8, Ordering::Release);
        match readonly_result {
            Err(error) => Err(error),
            Ok(()) => flush_result,
        }
    }

    /// 删除卷（静态方法）
    pub async fn delete(vol_dir: &Path) -> Result<(), StorageError> {
        tokio::fs::remove_dir_all(vol_dir)
            .await
            .map_err(StorageError::Io)
    }
}

// ---------------------------------------------------------------------------
// 子卷 / 快照 / btree 操作
// ---------------------------------------------------------------------------

impl BchVol {
    pub async fn create_snapshot(&self, _description: &str) -> Result<u32, StorageError> {
        let mut t = BtreeTrans::new(self);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        // bcachefs `bch2_subvolume_create(src_subvolid != 0)` creates the
        // read-only snapshot subvolume and advances the source subvolume to
        // the sibling snapshot node in the same transaction.
        let snapshot_subvol = bch2_subvolume_snapshot(
            &mut t,
            BCACHEFS_ROOT_SUBVOL as u32,
            0,
            self.logical_capacity,
            now,
        )?;
        let snapshot_id = bch2_subvolume_get(&t, snapshot_subvol, true)?.snapshot;
        let current_snapshot =
            bch2_subvolume_get(&t, BCACHEFS_ROOT_SUBVOL as u32, true)?.snapshot;
        t.bch2_trans_commit()?;
        self.root_snapshot_id
            .store(current_snapshot, Ordering::Release);
        Ok(snapshot_id)
    }

    pub async fn list_snapshots(&self) -> Vec<crate::snap::SnapshotMeta> {
        let t = BtreeTrans::new_ro(self);
        bch2_snapshot_list(&t)
            .into_iter()
            .map(|(id, val)| crate::snap::SnapshotMeta {
                id,
                parent: val.parent,
                subvol: val.subvol,
                depth: val.depth,
                created_at: val.btime,
                deleted: val.deleted,
            })
            .collect()
    }

    pub async fn rollback(&self, snap_id: u32) -> Result<(), StorageError> {
        let mut t = BtreeTrans::new(self);
        let snap = bch2_snapshot_read_value(&t, snap_id)
            .ok_or_else(|| StorageError::NotFound(format!("snapshot {} not found", snap_id)))?;
        if snap.deleted {
            return Err(StorageError::NotFound(format!(
                "snapshot {} has been deleted",
                snap_id
            )));
        }

        let source_subvol_id = snap.subvol;
        if source_subvol_id == 0 {
            return Err(StorageError::InvalidArgument(
                "rollback target is not a snapshot subvolume".into(),
            ));
        }
        let mut root_subvol = bch2_subvolume_get(&t, BCACHEFS_ROOT_SUBVOL as u32, true)?;
        bch2_subvolume_get(&t, source_subvol_id, true)?;

        // Keep the selected snapshot subvolume immutable: create a new
        // writable root leaf below the selected snapshot, using the same
        // 1->2 snapshot-node split as bcachefs
        // `bch2_subvolume_create(src_subvolid != 0)`, but do not advance the
        // selected snapshot subvolume itself.
        if source_subvol_id == BCACHEFS_ROOT_SUBVOL as u32 {
            self.root_snapshot_id.store(snap_id, Ordering::Release);
            return Ok(());
        }
        let mut new_snapids = [0u32; 2];
        let snapshot_subvols = [BCACHEFS_ROOT_SUBVOL as u32, source_subvol_id];
        bch2_snapshot_node_create(
            &mut t,
            snap_id,
            &mut new_snapids,
            &snapshot_subvols,
            2,
        )?;
        let new_root_snapshot = new_snapids[0];
        root_subvol.snapshot = new_root_snapshot;
        t.bch2_trans_delete(
            BtreeId::Subvolumes,
            0,
            false,
            BtreeKey::new(BCACHEFS_ROOT_SUBVOL as u64, 0, KeyType::Normal),
            0,
        );
        t.bch2_trans_update_raw(
            BtreeId::Subvolumes,
            0,
            false,
            BtreeKey::new(BCACHEFS_ROOT_SUBVOL as u64, 0, KeyType::Normal),
            root_subvol.to_bytes(),
            0,
        );
        t.bch2_trans_commit()?;
        self.root_snapshot_id
            .store(new_root_snapshot, Ordering::Release);
        Ok(())
    }

    pub async fn delete_snapshot(&self, snap_id: u32) -> Result<(), StorageError> {
        let mut t = BtreeTrans::new(self);
        bch2_snapshot_node_set_deleted(&mut t, snap_id)?;
        t.bch2_trans_commit()?;
        Ok(())
    }

    pub async fn clone_snapshot(
        &self,
        snap_id: u32,
        size: u64,
    ) -> Result<(u32, u32), StorageError> {
        let snap = {
            let t = BtreeTrans::new_ro(self);
            let s = bch2_snapshot_read_value(&t, snap_id)
                .ok_or_else(|| StorageError::NotFound(format!("snapshot {} not found", snap_id)))?;
            s
        };
        let parent_subvol = snap.subvol;
        if parent_subvol == 0 {
            return Err(StorageError::NotFound(
                "snapshot has no owning subvol".into(),
            ));
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let mut t = BtreeTrans::new(self);
        let subvol_id = bch2_subvolume_snapshot(&mut t, parent_subvol, 0, size, now)?;
        let snapshot_id = bch2_subvolume_get(&t, subvol_id, true)?.snapshot;
        t.bch2_trans_commit()?;
        Ok((subvol_id, snapshot_id))
    }

    pub async fn create_subvol(&self, _name: &str, size: u64) -> Result<u32, StorageError> {
        let mut t = BtreeTrans::new(self);
        let mut new_subvolid = 0;
        let mut new_snapshotid = 0;
        let mut new_subvol = crate::subvol::BchSubvolume::new(0, 0, size, 0);
        bch2_subvolume_create(
            &mut t,
            0,
            0,
            0,
            &mut new_subvolid,
            &mut new_snapshotid,
            &mut new_subvol,
            false,
        )?;
        t.bch2_trans_commit()?;
        Ok(new_subvolid)
    }

    pub async fn delete_subvol(&self, subvol_id: u32) -> Result<(), StorageError> {
        let mut t = BtreeTrans::new(self);
        bch2_subvolume_delete(&mut t, subvol_id)?;
        t.bch2_trans_commit()?;
        Ok(())
    }

    pub async fn list_subvols(&self) -> Vec<(u32, crate::subvol::BchSubvolume)> {
        let t = BtreeTrans::new_ro(self);
        crate::subvol::bch2_subvolume_list(&t)
    }

    pub async fn btree_insert(&self, key: BtreeKey, value: BchVal) -> bool {
        let pos = Bpos::from_key(&key);
        let entry = BtreeEntry::new(
            pos,
            key.key_type,
            KeyValue::Extent(ExtentValue {
                paddr: value.paddr.get(),
                size: 1,
                ver: value.ver,
                dev_idx: 0,
                crc32c: 0,
                crc_offset_blocks: 0,
            }),
        );
        self.insert_entry_raw(BtreeId::Extents, entry, 0)
    }

    pub async fn btree_get(&self, key: &BtreeKey) -> Option<(BtreeKey, BchVal)> {
        self.get_entry(BtreeId::Extents, key)
    }

    // ──── extent I/O ────

    /// bcachefs 对齐: bch2_read (fs/data/read.h:152-154)
    ///
    /// 签名映射: `int bch2_read(struct btree_trans *, struct bch_read_bio *,
    /// struct bvec_iter, subvol_inum, struct bch_io_failures *,
    /// struct bkey_buf *, enum bch_read_flags)`。
    ///
    /// ⚠️ Rust 类型不同但语义等价: trans → BtreeTrans<'_> (subvol 内部管理)，
    /// rbio → BchReadBio (数据缓冲区), iter → BvecIter (扇区偏移+大小),
    /// inum → SubvolInum (子卷+inode), failed → BchIoFailures,
    /// prev_read → BkeyBuf, flags → BchReadFlags。
    ///
    /// 内部委托给 read_extent_with_snapshot，并先按 bcachefs 语义把 subvol
    /// 解析为本次读取使用的 snapshot。
    pub async fn bch2_read(
        &self,
        trans: &mut BtreeTrans<'_>,
        rbio: &mut BchReadBio,
        iter: BvecIter,
        inum: SubvolInum,
        failed: &mut BchIoFailures,
        prev_read: &mut BkeyBuf,
        flags: BchReadFlags,
    ) -> Result<(), StorageError> {
        let mut snapshot_id =
            crate::subvol::bch2_subvolume_get_snapshot(trans, inum.subvol as u32)?;
        let vaddr = (iter.bi_sector << 9) + rbio.offset_into_extent as u64;
        let buf: &mut [u8] = &mut rbio.data;
        // Keep the public rbio flag state identical to the flags supplied to
        // bch2_read; the async backend does not reinterpret these bits.
        rbio.flags = flags.bits();
        // 对应 bcachefs read.c:1705-1707: bch2_bkey_buf_init(&sk) — 临时键值缓冲区
        // subvol 在 peek_visible_range_with_entry 中直接返回 entry_value 引用，
        // 无需手工管理 key buffer 生命周期。
        if buf.is_empty() {
            return Ok(());
        }

        let block_size = self.block_size as u64;
        let start = self.extent_bytes_to_blocks(vaddr, "extent read offset")?;
        let nblocks = self.extent_bytes_to_blocks(buf.len() as u64, "extent read length")?;
        let target_end = start.checked_add(nblocks).ok_or_else(|| {
            StorageError::InvalidArgument("extent read range overflows key space".into())
        })?;
        let mut cursor = start;

        // 本地 bcachefs data/read.c:1765-1832 在提交 IO 前把当前 extent
        // 重组到独立的 key buffer，后续 IO 不再依赖 btree node 中的 key。
        // Rust 的 BlockDevice 接口在 `.await` 时可能把 task 迁移到另一 worker；
        // 因此先在持有 transaction path lock 时复制完整 IO 计划，释放 transaction
        // 后再等待设备 IO，避免把 six lock 的 task-local 持有关系跨 await 携带。
        let mut reads: Vec<(
            Vec<(Arc<BchDev>, u64)>,
            u32,
            usize,
            usize,
            u32,
            bool,
            u32,
            BtreeKey,
            Vec<u8>,
        )> = Vec::new();

        let btree = self.btree(BtreeId::Extents);
        let target = BtreeKey::new(cursor, snapshot_id, KeyType::Normal);
        let mut trans = BtreeTrans::new_ro(self);
        let iter_idx = trans.iter_count();
        trans.bch2_trans_get_iter(btree.root(), &target, false, BtreeId::Extents);

        loop {
            // 对应 bcachefs read.c:1728-1744：每个 extent 重新开始事务，
            // 重新解析 subvolume → snapshot，并将 iterator 定位到 cursor。
            trans.bch2_trans_begin();
            snapshot_id = crate::subvol::bch2_subvolume_get_snapshot(
                &trans,
                inum.subvol as u32,
            )?;
            trans
                .iter_mut(iter_idx)
                .expect("read iterator disappeared")
                .set_snapshot_filter(snapshot_id);
            trans.bch2_btree_iter_set_pos(
                iter_idx,
                Bpos::new(inum.inum, cursor, 0),
            );

            {
                let iter = trans
                    .iter_mut(iter_idx)
                    .expect("read iterator disappeared");
                match iter.peek() {
                    Some((key, _)) => {
                        let key_vaddr = unsafe { std::ptr::addr_of!(key.vaddr).read_unaligned() };
                        if key_vaddr > cursor {
                            iter.prev_slot();
                        }
                    }
                    None => {
                        iter.prev_slot();
                    }
                }
            }

            if self.trim_hole_exists(snapshot_id, cursor) {
                let byte_off = ((cursor - start) * block_size) as usize;
                buf[byte_off..byte_off + block_size as usize].fill(0);
                cursor += 1;
                if cursor >= target_end {
                    break;
                }
                continue;
            }

            let (entry_key, entry_val, entry_value, visible_start, visible_end) = {
                match trans
                    .iter_mut(iter_idx)
                    .expect("read iterator disappeared")
                    .peek_visible_range_with_entry(self)
                {
                    Some((k, v, raw_value, vs, ve)) if vs < target_end && ve > cursor => {
                        (k, v, raw_value, vs, ve)
                    }
                    _ => {
                        // 空洞：零填充剩余部分
                        let byte_off = ((cursor - start) * block_size) as usize;
                        buf[byte_off..].fill(0);
                        break;
                    }
                }
            };

            // 对应 bcachefs read.c:1784-1788：retry 期间若当前 extent
            // 已经变化，旧设备失败记录不再适用于新 key，必须清空。
            if flags.contains(BchReadFlags::IN_RETRY) {
                if !prev_read.bkey_and_val_eq(&entry_key, &entry_value) {
                    failed.nr = 0;
                    failed.data.clear();
                }
                prev_read.k = Some(entry_key);
                prev_read.v = Some(entry_value.clone());
            }

            if entry_key.key_type != KeyType::Normal {
                let byte_off = ((cursor - start) * block_size) as usize;
                let hole_blocks = (visible_end - cursor) as usize;
                buf[byte_off..byte_off + hole_blocks * block_size as usize].fill(0);
                cursor = visible_end;
                if cursor >= target_end {
                    break;
                }
                trans
                    .iter_mut(iter_idx)
                    .expect("read iterator disappeared")
                    .advance_visible(self);
                continue;
            }

            if visible_start > cursor {
                // 空洞：零填充
                let hole_blocks = (visible_start - cursor) as usize;
                let byte_off = ((cursor - start) * block_size) as usize;
                buf[byte_off..byte_off + hole_blocks * block_size as usize].fill(0);
                cursor = visible_start;
                // 推进 iter 到空洞之后
                if visible_start >= entry_key.end() {
                    trans
                        .iter_mut(iter_idx)
                        .expect("read iterator disappeared")
                        .advance_visible(self);
                }
                continue;
            }

            // 对应 bcachefs read.c:1773-1776: 间接 extent 处理
            // TODO(indirect_extent): bcachefs 调用 `bch2_read_indirect_extent(trans,
            // &data_btree, &offset_into_extent, &sk)` 处理 reflink 间接 extent
            // (KEY_TYPE_reflink_p)。此调用可能修改 data_btree（切换到 BTREE_ID_reflink）
            // 和 offset_into_extent。subvol 无 reflink btree，不支持间接 extent。
            // 当 subvol 未来添加 reflink 支持时，在此处添加:
            //   1. 检查 entry_value 是否为间接 extent 类型
            //   2. 在 reflink btree 中查找实际数据 extent
            //   3. 更新 data_btree、offset_into_extent 和 reads 条目

            // 读取覆盖范围
            let read_start = cursor;
            let read_end = visible_end.min(target_end);
            let live_read_blocks = (read_end - read_start) as u32;
            let byte_off = ((read_start - start) * block_size) as usize;
            let byte_len = live_read_blocks as usize * block_size as usize;

            if byte_len > 0 {
                let paddr = entry_val.paddr.get() + (read_start - entry_key.vaddr);
                let delta = read_start - entry_key.vaddr;
                let (crc32c, extent_blocks, crc_offset_blocks) = match &entry_value {
                    KeyValue::Extent(extent) => {
                        (extent.crc32c, extent.size, extent.crc_offset_blocks)
                    }
                    KeyValue::ExtentPtrs { blocks, crc32c, crc_offset_blocks, .. } => {
                        (*crc32c, *blocks, *crc_offset_blocks)
                    }
                    KeyValue::Raw(bytes) => match KeyValue::from_bytes(bytes) {
                        KeyValue::Extent(extent) => {
                            (extent.crc32c, extent.size, extent.crc_offset_blocks)
                        }
                        KeyValue::ExtentPtrs { blocks, crc32c, crc_offset_blocks, .. } => {
                            (crc32c, blocks, crc_offset_blocks)
                        }
                        _ => (0, live_read_blocks, 0),
                    },
                    KeyValue::BtreePtr(_) => (0, live_read_blocks, 0),
                };
                let has_crc32c = crc_offset_blocks >> 32 != 0 || crc32c != 0;
                let poisoned = crc_offset_blocks & EXTENT_CRC_POISONED_BIT != 0;
                // bcachefs `__bch2_read_extent()` rejects a poisoned extent
                // before device selection (`fs/data/read.c:1369-1392`), while
                // BCH_READ_no_poison_check is the explicit recovery escape.
                if poisoned && !flags.contains(BchReadFlags::NO_POISON_CHECK) {
                    return Err(StorageError::ExtentPoisoned);
                }
                let read_blocks = if has_crc32c {
                    (((crc_offset_blocks >> 32) as u32) & EXTENT_CRC_ORIGINAL_BLOCKS_MASK)
                        .max(extent_blocks)
                        .max(live_read_blocks)
                } else {
                    live_read_blocks
                };
                let mut candidates = Vec::new();
                let mut had_extent_pointer = false;
                match &entry_value {
                    KeyValue::Extent(extent) => {
                        had_extent_pointer = true;
                        if let Some(dev) = self.device_rcu_noerror(extent.dev_idx) {
                            candidates.push((
                                dev,
                                if has_crc32c { extent.paddr } else { extent.paddr + delta },
                            ));
                        }
                    }
                    KeyValue::ExtentPtrs { ptrs, .. } => {
                        had_extent_pointer = !ptrs.is_empty();
                        for ptr in ptrs {
                            if let Some(dev) = self.device_rcu_noerror(ptr.dev) {
                                candidates.push((
                                    dev,
                                    if has_crc32c { ptr.offset } else { ptr.offset + delta },
                                ));
                            }
                        }
                    }
                    KeyValue::Raw(_) => {
                        had_extent_pointer = entry_value.nr_ptrs() != 0;
                        entry_value.for_each_ptr(|ptr| {
                            if let Some(dev) = self.device_rcu_noerror(ptr.dev) {
                                candidates.push((
                                    dev,
                                    if has_crc32c { ptr.offset } else { ptr.offset + delta },
                                ));
                            }
                        });
                    }
                    KeyValue::BtreePtr(_) => {}
                }
                if candidates.is_empty() && !had_extent_pointer {
                    let dev = self.primary_device_rcu_noerror().ok_or_else(|| {
                        StorageError::NotFound(
                            "BchVol::read_extent_with_snapshot: no registered device".into(),
                        )
                    })?;
                    candidates.push((dev, if has_crc32c { paddr - delta } else { paddr }));
                }
                // bcachefs `bch2_bkey_pick_read_device()` consults
                // `bch_io_failures` before selecting a replica. A recorded
                // device I/O error is not retried for the same extent.
                candidates.retain(|(dev, _)| {
                    !failed.data.iter().any(|failure| {
                        failure.dev == dev.dev_idx
                            && (failure.errcode != 0
                                || failure.ec_errcode != 0
                                || failure.csum_nr != 0)
                    })
                });
                // bcachefs `bch2_bkey_pick_read_device()` prefers the
                // lower-latency replica while retaining every other pointer
                // for retry (`fs/data/extents.c:202-310`).  Keep the same
                // fallback set, but order it by the per-device EWMA; zero
                // means the device has not been sampled and is tried first.
                candidates.sort_unstable_by_key(|(dev, _)| {
                    let latency = dev.io_read_latency.load(Ordering::Acquire);
                    (latency != 0, latency.saturating_mul(latency))
                });
                reads.push((
                    candidates,
                    read_blocks,
                    byte_off,
                    byte_len,
                    crc32c,
                    has_crc32c,
                    if has_crc32c {
                        (crc_offset_blocks & 0xffff_ffff)
                            .saturating_add(delta)
                            as u32
                    } else {
                        0
                    },
                    entry_key,
                    entry_value.to_bytes(),
                ));
            }

            cursor = read_end;
            if cursor >= target_end {
                break;
            }
            trans
                .iter_mut(iter_idx)
                .expect("read iterator disappeared")
                .advance_visible(self);
        }

        drop(trans);

        // 对应 bcachefs read.c:1812-1815: fragment 标志
        // TODO(fragment_flags): bcachefs 在调用 `__bch2_read_extent` 前根据是否
        // 为最后一段 extent 设置 `BCH_READ_last_fragment` 或 `BCH_READ_must_clone`
        // 标志。subvol 在 `drop(trans)` 后的循环中统一执行 IO，无逐 extent 的标志
        // 传递。如未来需要 `__bch2_read_extent` 式的分片 IO（如 bounce buffer），
        // 可将 reads 条目拆分为逐个 extent 的独立 future 并添加标志位。
        //
        // 对应 bcachefs read.c:1847: bio_advance_iter — subvol 的 byte_off 已由
        // 之前 extent 遍历时计算，无需在 IO 循环中推进 bio iter。
        //
        // 对应 bcachefs read.c:1852-1859: error classification retry
        // 对应 bcachefs read.c:592-597: data_read_err_should_retry
        // subvol 的候选设备列表重试等价于 bcachefs 的多副本重试路径；
        // 元数据扫描阶段已经按 bcachefs 的每 extent 事务循环执行
        // BCH_ERR_transaction_restart 等价的 begin/set_pos 路径；
        // 这里的设备 IO 在 drop(trans) 后执行，因此只保留副本级重试，
        // 不再重复元数据扫描。
        for (
            candidates,
            read_blocks,
            byte_off,
            byte_len,
            crc32c,
            has_crc32c,
            data_offset_blocks,
            entry_key,
            entry_raw_value,
        ) in reads
        {
            let mut last_err = None;
            let mut completed = false;
            let mut extent_checksum_failed = false;
            let mut extent_non_checksum_failed = false;
            for (dev, paddr) in candidates {
                let Some(_io_ref) = dev.try_get_io_ref_guard(BchDevIoRefKind::Read) else {
                    continue;
                };
                let started = Instant::now();
                let mut extent_buf = if has_crc32c {
                    vec![0u8; read_blocks as usize * self.block_size as usize]
                } else {
                    Vec::new()
                };
                let read_result = if has_crc32c {
                    dev.bdev()
                        .read_blocks(BlockAddr::new(paddr), read_blocks, &mut extent_buf)
                        .await
                } else {
                    dev.bdev()
                        .read_blocks(
                            BlockAddr::new(paddr),
                            read_blocks,
                            &mut buf[byte_off..byte_off + byte_len],
                        )
                        .await
                };
                let result = match read_result {
                    Ok(()) if has_crc32c => {
                        let actual = crate::block_device::block_crc32c(&extent_buf);
                        if actual != crc32c {
                            Err(StorageError::ChecksumMismatch {
                                expected: crc32c,
                                actual,
                            })
                        } else {
                            let src_off = data_offset_blocks as usize * self.block_size as usize;
                            buf[byte_off..byte_off + byte_len]
                                .copy_from_slice(&extent_buf[src_off..src_off + byte_len]);
                            Ok(())
                        }
                    }
                    other => other,
                };
                // bcachefs `bch2_latency_acct()` maintains an EWMA in
                // `cur_latency[READ]`; update it after every completion,
                // including errors, so a slow failing device is deprioritized.
                let elapsed = started.elapsed().as_nanos().min(u64::MAX as u128) as u64;
                let latency = &dev.io_read_latency;
                let mut old = latency.load(Ordering::Acquire);
                loop {
                    let next = if old == 0 {
                        elapsed
                    } else if elapsed >= old {
                        old.saturating_add((elapsed - old) / 8)
                    } else {
                        old.saturating_sub((old - elapsed) / 8)
                    };
                    match latency.compare_exchange(old, next, Ordering::AcqRel, Ordering::Acquire) {
                        Ok(_) => break,
                        Err(current) => old = current,
                    }
                }
                match result {
                    Ok(()) => {
                        completed = true;
                        break;
                    }
                    Err(err) => {
                        // 对应 bcachefs `bch2_mark_io_failure()`：记录设备级
                        // 失败，使同一 extent 的后续 retry 跳过该副本。
                        if let Some(failure) =
                            failed.data.iter_mut().find(|failure| failure.dev == dev.dev_idx)
                        {
                            match err {
                                StorageError::ChecksumMismatch { .. } => {
                                    extent_checksum_failed = true;
                                    failure.csum_nr = failure.csum_nr.saturating_add(1);
                                }
                                _ => {
                                    extent_non_checksum_failed = true;
                                    failure.errcode = -1;
                                }
                            }
                        } else {
                            failed.data.push(BchDevIoFailure {
                                dev: dev.dev_idx,
                                csum_nr: match err {
                                    StorageError::ChecksumMismatch { .. } => {
                                        extent_checksum_failed = true;
                                        1
                                    }
                                    _ => {
                                        extent_non_checksum_failed = true;
                                        0
                                    }
                                },
                                ec_errcode: 0,
                                errcode: match err {
                                    StorageError::ChecksumMismatch { .. } => 0,
                                    _ => {
                                        extent_non_checksum_failed = true;
                                        -1
                                    }
                                },
                            });
                            failed.nr = failed.data.len().min(u8::MAX as usize) as u8;
                        }
                        last_err = Some(err);
                    }
                }
            }
            if !completed {
                // 对应 bcachefs read.c:1864-1882: 错误日志
                // bcachefs 使用 `bch2_read_err_msg_trans` + `bch_err_ratelimited`
                // 记录日志，并检查是否在重试中及 extent 是否标记为 poisoned。
                // subvol 使用 tracing::warn! 记录 IO 错误。
                let err_msg = last_err
                    .as_ref()
                    .map(|e| format!("{e:?}"))
                    .unwrap_or_else(|| "no online extent replica".into());
                tracing::warn!(
                    vaddr,
                    buf_len = buf.len(),
                    read_blocks,
                    byte_off,
                    error = %err_msg,
                    "bch2_read: all replicas failed"
                );

                // bcachefs `read_extent_pick_err()` calls
                // `maybe_poison_extent()` for a checksum failure in the retry
                // path (`fs/data/read.c:1408-1422`); that helper rechecks the
                // current key/value before committing the poisoned flag
                // (`fs/data/read.c:541-590`).  Device replicas are retried in
                // this loop, so an exhausted checksum-only retry is the
                // equivalent retry boundary for the async Rust path.
                if extent_checksum_failed
                    && !extent_non_checksum_failed
                    && !flags.contains(BchReadFlags::NO_POISON_CHECK)
                    && self.is_rw()
                {
                    if let Some(current) = self.get_entry_raw(
                        BtreeId::Extents,
                        Bpos::from_key(&entry_key),
                    ) {
                        let (current_key, _) = current.to_key_value();
                        if current_key == entry_key
                            && current.value.to_bytes() == entry_raw_value
                        {
                            let mut poisoned_value = entry_raw_value.clone();
                            match KeyValue::from_bytes(&poisoned_value) {
                                KeyValue::Extent(_) if poisoned_value.len() >= 26 => {
                                    let mut metadata = [0u8; 8];
                                    metadata.copy_from_slice(&poisoned_value[18..26]);
                                    let metadata =
                                        u64::from_le_bytes(metadata) | EXTENT_CRC_POISONED_BIT;
                                    poisoned_value[18..26]
                                        .copy_from_slice(&metadata.to_le_bytes());
                                }
                                KeyValue::ExtentPtrs { .. } if poisoned_value.len() >= 20 => {
                                    let mut metadata = [0u8; 8];
                                    metadata.copy_from_slice(&poisoned_value[12..20]);
                                    let metadata =
                                        u64::from_le_bytes(metadata) | EXTENT_CRC_POISONED_BIT;
                                    poisoned_value[12..20]
                                        .copy_from_slice(&metadata.to_le_bytes());
                                }
                                _ => {}
                            }
                            if poisoned_value != entry_raw_value {
                                self.trans_update_commit_raw(
                                    BtreeId::Extents,
                                    0,
                                    false,
                                    entry_key,
                                    poisoned_value,
                                )
                                .await?;
                            }
                        }
                    }
                }
                return Err(last_err
                    .unwrap_or_else(|| StorageError::NotFound("no online extent replica".into())));
            }
        }
        Ok(())
    }

    /// bcachefs 对齐: bch2_write (fs/data/write.h:63, CLOSURE_CALLBACK)
    ///
    /// 签名映射: `CLOSURE_CALLBACK(bch2_write)` → `void bch2_write(struct bch_write_op *)`
    /// → Rust `pub async fn bch2_write(&self, op: &mut BchWriteOp)`。
    ///
    /// # 两层结构（对齐 bcachefs）
    ///
    /// 本函数实现入口校验层，对应 bcachefs `CLOSURE_CALLBACK(bch2_write)` (write.c:2919-2988):
    /// - 对齐检查 (line 2947-2952)、nochanges 检查 (line 2954-2958)、write_ref 获取 (line 2960-2965)
    /// - 校验通过后调用 `__bch2_write()` 执行主写循环
    ///
    /// `__bch2_write()` 实现主分配/写入层，对应 bcachefs `__bch2_write()` (write.c:2703-2838):
    /// - 分配扇区 → 写 extent → 提交 bio → 索引更新
    ///
    /// ⚠️ Rust 类型不同但语义等价: BchWriteOp 包含 flags/subvol/pos/data 等字段，
    /// 对应 bcachefs `struct bch_write_op` 的子集。
    pub async fn bch2_write(&self, op: &mut BchWriteOp) -> Result<(), StorageError> {
        // 对应 bcachefs write.c:2947-2952: 检查数据大小是否为 block 对齐
        // extent_bytes_to_blocks 已做 bytes % block_size != 0 对齐检查，
        // 等价于 bcachefs 的 `bio->bi_iter.bi_size & (block_size - 1)` 检查。
        let buf: &[u8] = &op.data;
        if buf.is_empty() {
            return Ok(());
        }

        // bcachefs write.c:1122-1149 carries the source subvolume in
        // `op->subvol` and resolves its current snapshot before building the
        // extent key.  Do not treat the subvolume ID as a snapshot ID.
        let trans = BtreeTrans::new_ro(self);
        let snapshot_id = crate::subvol::bch2_subvolume_get_snapshot(&trans, op.subvol)?;
        let key = BtreeKey::new(op.pos.offset, snapshot_id, KeyType::Normal);

        // 对应 bcachefs write.c:2931-2934: BUG_ON 检查 op 的初始状态
        // Rust 类型系统已保证 key 字段的有效性，无需运行时断言。
        //
        // 对应 bcachefs write.c:2936: async_object_list_add — Rust 版在 write_ref
        // 的引用计数追踪中实现了等价的生命周期管理（end_write 时 implicit drop）。
        //
        // 对应 bcachefs write.c:2938-2939: BCH_WRITE_only_specified_devs → alloc_nowait
        // TODO(deviation): subvol 未实现 `BCH_WRITE_only_specified_devs` / `alloc_nowait`
        // 标志。如有定向设备写入需求，需在此处添加等效设置。

        // Validate the externally supplied byte geometry before acquiring a
        // write reference. This keeps the bcachefs-style write-ref drain
        // balanced on rejected requests.
        let vaddr = self.extent_bytes_to_blocks(key.vaddr, "extent write offset")?;
        let nblocks = self.extent_bytes_to_blocks(buf.len() as u64, "extent write length")?;
        let extent_end = vaddr.checked_add(nblocks).ok_or_else(|| {
            StorageError::InvalidArgument("extent write range overflows key space".into())
        })?;

        // 对应 bcachefs write.c:2954-2958: 只读检查 — c->opts.nochanges
        // 对应 bcachefs write.c:2960-2965: enumerated_ref_tryget(BCH_WRITE_REF_write)
        // 写引用：获取写引用后才允许写入，GoingRo 时拒绝
        if !self.try_begin_write() {
            return Err(StorageError::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "volume is going read-only",
            )));
        }

        // 对应 bcachefs write.c:2967: bch2_increment_clock(c, bio_sectors(bio), WRITE)
        // TODO(known_deviation): subvol 无 io_clock 基础设施（无 percpu 计数器/IO 调度器
        // 公平性机制）。bcachefs 的 `bch2_increment_clock` 用于 IO 调度器的带宽追踪；
        // subvol 的块设备层由内核 block layer 直接管理 IO 调度，因此此偏差不影响正确性。
        //
        // 对应 bcachefs write.c:2942: op->start_time = local_clock()
        // 写延迟追踪由底层 dev.bdev().write_blocks() 的完成时间覆盖，无需此处记录。
        //
        // 对应 bcachefs write.c:2943: bch2_keylist_init — subvol 的 extent 插入在
        // handle_partial_overlap + trans_update_commit_raw 中统一完成。
        //
        // 对应 bcachefs write.c:2944-2945: wbio_init(bio) — Rust BlockDevice 的
        // write_blocks 自带异步 bio 生命周期管理。

        // 对应 bcachefs write.c:2969-2970: data_len = min(bi_size, new_i_size - pos)
        // 对应 bcachefs write.c:2972-2977: 内联写入路径
        // TODO(inline_data): bcachefs 在 `c->opts.inline_data && data_len <= block_bytes(c)/2`
        // 且 `data_len <= 1024U` 时使用 `bch2_write_data_inline()` 将小数据嵌入 extent key
        // 中（write.c:2972-2977）。subvol 目前总是走完整分配 + IO 路径。如需小 IO 优化，
        // 可在此处添加检查：当 buf.len() <= min(self.block_size/2, 1024) 时使用内联 extent，
        // 跳过磁盘分配和 IO。

        // 部分覆盖 split is deferred until the new data replicas have
        // completed. bcachefs indexes the newly written bio only after IO
        // completion (`fs/data/write.c:1541-1588`); deleting the old COW
        // extent first would turn an allocation/device failure into data loss.
        let result = self
            .__bch2_write(vaddr, nblocks, extent_end, buf, key.snapshot_id)
            .await;

        // 对应 bcachefs write.c:2981-2988: 错误路径
        // bcachefs 在错误时执行:
        //   bch2_disk_reservation_put(c, &op->res) — 释放预分配
        //   closure_debug_destroy(&op->cl) — 销毁闭包
        //   async_object_list_del(c, write_op, op->list_idx) — 从全局写列表移除
        //   op->end_io(op) — 通知上层写完成（含错误）
        // subvol 等价:
        //   - 预分配: 由 `cleanup_allocations` (bch2_bucket_free) 在每个错误返回点前完成
        //   - 闭包清理: async block 作用域结束 + `drop(write_refs)` 自动释放
        //   - 写引用: `self.end_write()` 在 result block 之外确保无论成功/失败都释放
        //   - 通知: Result 返回给调用方，调用方自行处理（NBD/FUSE 层处理错误传播）
        self.end_write();
        result
    }

    /// bcachefs 对齐: __bch2_write (fs/data/write.c:2703-2838)
    ///
    /// 主写循环：分配扇区 → 写 extent → 提交 bio → 索引更新。
    /// 对应 bcachefs `static void __bch2_write(struct bch_write_op *op)`。
    ///
    /// 由 `bch2_write` 入口校验层在通过对齐/RO/写引用检查后调用。
    ///
    /// # bcachefs 控制流映射
    ///
    /// | bcachefs (write.c) | subvol |
    /// |---|---|
    /// | line 2730-2823: do-while 主循环 (alloc→write_extent→submit) | 一次性分配全部副本 → join_all IO → 索引更新 |
    /// | line 2825-2833: sync completion (__bch2_write_index + bch2_write_done) | Result 返回后由调用方 `bch2_write` 处理 |
    /// | line 2717-2719: `wait_on_allocator_sync` 标志 | 全部分配/IO 同步等待，等价的背压语义 |
    ///
    /// # 偏差说明
    /// - subvol 不区分 `BCH_WRITE_sync` / `BCH_WRITE_submitted`：所有写入为同步完成，通过 Rust Result 传播错误
    /// - subvol 无 nocow 路径（对应 write.c:2723-2726）：总是走 COW 分配 + IO 管线
    /// - subvol 无 `bch2_write_endio` 回调链：IO 由 join_all + Result 同步等待
    /// - subvol 无 open_bucket 追踪（对应 write.c:2736-2738, 2746-2747）：采用一次性分配全部副本模型
    async fn __bch2_write(
        &self,
        vaddr: u64,
        nblocks: u64,
        extent_end: u64,
        buf: &[u8],
        snapshot_id: u32,
    ) -> Result<(), StorageError> {
        // Local bcachefs defaults data extents to a key-side checksum when
        // data checksum is enabled (`fs/data/write.c:2140-2159`).  This
        // implementation enables the CRC32C data checksum for every extent;
        // the checksum is computed over the complete uncompressed extent
        // before any replica is published.
        let extent_crc32c = crate::block_device::block_crc32c(buf);
        // 范围分配
        let request = AllocRequest::new(Watermark::Normal, crate::alloc::BchDataType::User);
        // 本地 bcachefs `alloc_request` 为每个有效 extent ptr 分配独立
        // open bucket，然后 `bch2_submit_wbio_replicas()` 在等待前提交
        // 所有副本（`fs/alloc/foreground.c:1653-1661`,
        // `fs/data/write.c:1341-1478`）。先完成全部分配和消费，再开始
        // 任意数据 IO，避免副本数量不足时留下部分写入的元数据。
        let wanted_replicas = u32::from(self.opts.data_replicas.max(1));
        let mut allocated: Vec<(Arc<BchDev>, u64)> = Vec::new();
        let mut write_refs: Vec<BchDevIoRefGuard> = Vec::new();
        let mut allocated_replicas = 0u32;
        let mut first_alloc_error: Option<StorageError> = None;
        let target_devs = crate::alloc::target_rw_devs(
            self,
            crate::alloc::BchDataType::User,
            self.opts.foreground_target,
        );
        for dev_idx in target_devs.iter() {
            if allocated_replicas >= wanted_replicas {
                break;
            }
            let Some(ca) = self.device_rcu_noerror(dev_idx) else {
                continue;
            };
            let Some(io_ref) = ca.try_get_io_ref_guard(BchDevIoRefKind::Write) else {
                continue;
            };
            let paddr = match {
                // bch2_alloc_sectors_start_trans 需要 &BchAllocator + &BchVol
                let alloc_ptr = self.allocator.get();
                unsafe {
                    (*alloc_ptr).bch2_alloc_sectors_start_trans(
                        nblocks,
                        self,
                        &ca,
                        &request,
                        Some(WritePointSpecifier::Hashed(vaddr)),
                    )
                }
            } {
                Ok(paddr) => paddr,
                Err(err) => {
                    // bcachefs retries allocation across all eligible
                    // members before failing the request
                    // (`fs/alloc/foreground.c:1498-1540`).  Keep the
                    // first error for the all-devices-failed case, but
                    // continue so a later member can provide a degraded
                    // replica set.
                    first_alloc_error.get_or_insert(err.into());
                    continue;
                }
            };
            // 对应本地 bcachefs alloc/foreground.h:407-429：先从 write point /
            // open bucket 消费本次分配空间，再提交数据 IO。
            let alloc_ptr = self.allocator.get();
            unsafe {
                (*alloc_ptr).bch2_consume_written_extent(&ca, paddr, nblocks);
            }
            allocated_replicas += unsafe { &*ca.mi.get() }.durability as u32;
            allocated.push((ca, paddr));
            write_refs.push(io_ref);
        }
        if allocated.is_empty() {
            return Err(first_alloc_error
                .unwrap_or_else(|| StorageError::NotFound("no writable extent device".into())));
        }

        // 与 bcachefs closure 聚合语义一致：全部副本先提交，再按提交顺序
        // 传播首个错误；一个副本失败不能取消其他已提交副本。
        let writes = allocated.iter().map(|(dev, paddr)| {
            self.write_blocks_on_device(dev.clone(), BlockAddr::new(*paddr), buf)
        });
        let write_results = join_all(writes).await;
        let mut successful = Vec::with_capacity(allocated.len());
        let mut first_error = None;
        for ((dev, paddr), result) in allocated.into_iter().zip(write_results) {
            match result {
                Ok(()) => successful.push((dev, paddr)),
                Err(err) => {
                    first_error.get_or_insert(err);
                    // bcachefs write completion drops the failed write
                    // point's allocation before publishing the extent
                    // (`fs/data/write.c:bch2_write_done`).  Keep failed
                    // replicas out of allocator accounting as well;
                    // otherwise a transient device error permanently
                    // strands the bucket until a later full GC pass.
                    let alloc_ptr = self.allocator.get();
                    if let Err(cleanup_err) =
                        unsafe { (*alloc_ptr).bch2_bucket_free(&dev, paddr, self) }
                    {
                        tracing::warn!(
                            device = dev.dev_idx,
                            paddr,
                            error = ?cleanup_err,
                            "failed replica allocation cleanup"
                        );
                    }
                }
            }
        }
        if successful.is_empty() {
            return Err(first_error.unwrap_or_else(|| {
                StorageError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "all extent replicas failed",
                ))
            }));
        }
        let successful_durability: u32 = successful
            .iter()
            .map(|(dev, _)| unsafe { (&*dev.mi.get()).durability as u32 })
            .sum();

        // Keep successful allocations owned until the extent metadata is
        // durable.  bcachefs aborts the write point on a later transaction
        // error; dropping this ownership early would strand the buckets
        // whenever overlap handling or journal insertion fails.
        let cleanup_allocations = |allocations: &[(Arc<BchDev>, u64)]| {
            let alloc_ptr = self.allocator.get();
            for (dev, paddr) in allocations {
                if let Err(cleanup_err) =
                    unsafe { (*alloc_ptr).bch2_bucket_free(dev, *paddr, self) }
                {
                    tracing::warn!(
                        device = dev.dev_idx,
                        paddr = *paddr,
                        error = ?cleanup_err,
                        "extent metadata rollback cleanup failed"
                    );
                }
            }
        };

        // Only now remove/split the old mapping: the new physical data is
        // available and can be made visible by the following transaction.
        if let Err(err) = self
            .handle_partial_overlap(vaddr, nblocks, snapshot_id)
            .await
        {
            cleanup_allocations(&successful);
            return Err(err);
        }
        self.clear_trim_holes_overlapping(snapshot_id, vaddr, extent_end);

        // 插入 range extent（key 在起始位置，size = nblocks）
        let range_key = BtreeKey {
            inode: 0,
            vaddr,
            size: nblocks as u32,
            snapshot_id,
            key_type: KeyType::Normal,
            version: 0,
        };
        // The legacy BchVal shape has no device field and therefore is
        // only safe for the historical primary device (index 0).  Local
        // bcachefs always keeps the extent pointer's device alongside
        // its offset (`fs/data/write.c:1341-1478`); preserve that field
        // whenever allocation selected another member.
        if successful_durability == 1 {
            let (dev, paddr) = &successful[0];
            let raw_value = KeyValue::Extent(ExtentValue {
                paddr: *paddr,
                size: nblocks as u32,
                ver: 0,
                dev_idx: dev.dev_idx,
                crc32c: extent_crc32c,
                crc_offset_blocks: (nblocks as u64) << 32,
            })
            .to_bytes();
            if let Err(err) = self
                .trans_update_commit_raw(BtreeId::Extents, 0, false, range_key, raw_value)
                .await
            {
                cleanup_allocations(&successful);
                return Err(err);
            }
        } else {
            let ptrs = successful
                .iter()
                .map(|(dev, paddr)| {
                    let bucket_index = crate::alloc::sector_to_bucket(
                        dev,
                        *paddr * crate::alloc::SECTORS_PER_BLOCK,
                    );
                    let alloc_bpos = Bpos::new(dev.dev_idx as u64, bucket_index, 0);
                    let gen = self
                        .get_entry_raw(BtreeId::Alloc, alloc_bpos)
                        .and_then(|entry| match entry.value {
                            KeyValue::Raw(bytes) => {
                                crate::alloc::btree::deserialize_alloc_entry(&bytes).ok()
                            }
                            _ => None,
                        })
                        .map_or(0, |entry| entry.gen);
                    crate::btree::key::ExtentPtr {
                        dev: dev.dev_idx,
                        gen,
                        offset: *paddr,
                        cached: false,
                        unwritten: false,
                    }
                })
                .collect();
            let raw_value = KeyValue::ExtentPtrs {
                blocks: nblocks as u32,
                ptrs,
                crc32c: extent_crc32c,
                crc_offset_blocks: (nblocks as u64) << 32,
            }
            .to_bytes();
            if let Err(err) = self
                .trans_update_commit_raw(BtreeId::Extents, 0, false, range_key, raw_value)
                .await
            {
                cleanup_allocations(&successful);
                return Err(err);
            }
        }
        drop(write_refs);
        Ok(())
    }

    async fn handle_partial_overlap(
        &self,
        new_start: u64,
        nblocks: u64,
        snapshot_id: u32,
    ) -> Result<(), StorageError> {
        let new_end = new_start.checked_add(nblocks).ok_or_else(|| {
            StorageError::InvalidArgument("extent overlap range overflows key space".into())
        })?;

        let entries_to_split: Vec<(BtreeKey, BchVal, KeyValue)> = {
            let btree = self.btree(BtreeId::Extents);
            let target = BtreeKey::new(new_start, snapshot_id, KeyType::Normal);
            let mut trans = BtreeTrans::new_ro(self);
            let iter = trans.bch2_trans_get_iter(btree.root(), &target, false, BtreeId::Extents);
            iter.snapshot = snapshot_id;
            match iter.peek() {
                Some((key, _)) => {
                    let key_vaddr = unsafe { std::ptr::addr_of!(key.vaddr).read_unaligned() };
                    if key_vaddr > new_start {
                        iter.prev_slot();
                    }
                }
                None => {
                    iter.prev_slot();
                }
            }

            let mut results = Vec::new();
            loop {
                let found = match iter.peek_visible_range_with_entry(self) {
                    Some((k, v, raw_value, vs, ve)) if vs < new_end && ve > new_start => {
                        if k.snapshot_id == snapshot_id {
                            Some((k, v, raw_value))
                        } else {
                            None
                        }
                    }
                    _ => None,
                };
                match found {
                    Some(entry) => results.push(entry),
                    None => break,
                }
                if !iter.advance_visible(self) {
                    break;
                }
            }
            results
        };

        // Keep every split/delete in one btree transaction. Local bcachefs
        // `__bch2_trans_commit()` applies the complete update set as one
        // journaled unit (`fs/btree/commit.c:1381-1519`); separate commits
        // could expose a half-split extent when a later update fails.
        let has_entries = !entries_to_split.is_empty();
        let mut trans = BtreeTrans::new(self);
        if has_entries {
            trans.bch2_trans_begin();
        }

        for (old_key, old_val, raw_value) in entries_to_split {
            let old_start = old_key.vaddr;
            let old_size = unsafe { std::ptr::addr_of!(old_key.size).read_unaligned() };
            let old_effective = if old_size == 0 { 1 } else { old_size } as u64;
            let old_end = old_start + old_effective;
            let old_paddr = old_val.paddr.get();
            let old_ver = old_val.ver;

            // `peek_entry()` may expose a leaf extent as Raw. Decode it through
            // the same packed value parser used by bcachefs extent pointers so
            // split ranges retain every device mapping and pointer order.
            let extent_value = match raw_value {
                KeyValue::Raw(bytes) => KeyValue::from_bytes(&bytes),
                value => value,
            };

            let trimmed_value = |start: u64, size: u64| -> KeyValue {
                let shift = start - old_start;
                match &extent_value {
                    KeyValue::Extent(value) => {
                        let has_crc32c = value.crc_offset_blocks >> 32 != 0 || value.crc32c != 0;
                        KeyValue::Extent(ExtentValue {
                            paddr: if has_crc32c { value.paddr } else { value.paddr + shift },
                            size: size as u32,
                            ver: value.ver,
                            dev_idx: value.dev_idx,
                            crc32c: value.crc32c,
                            crc_offset_blocks: (value.crc_offset_blocks & !0xffff_ffff)
                                | ((value.crc_offset_blocks as u32 as u64)
                                    .saturating_add(if has_crc32c { shift } else { 0 })
                                    & 0xffff_ffff),
                        })
                    }
                    KeyValue::ExtentPtrs { ptrs, crc32c, blocks: _, crc_offset_blocks } => KeyValue::ExtentPtrs {
                        // The upper metadata word, rather than crc32c itself,
                        // records that this is a checksummed original extent;
                        // a valid CRC32C may be zero.
                        blocks: size as u32,
                        ptrs: ptrs
                            .iter()
                            .map(|ptr| crate::btree::key::ExtentPtr {
                                offset: if *crc_offset_blocks >> 32 != 0 || *crc32c != 0 {
                                    ptr.offset
                                } else {
                                    ptr.offset + shift
                                },
                                ..*ptr
                            })
                            .collect(),
                        crc32c: *crc32c,
                        crc_offset_blocks: (*crc_offset_blocks & !0xffff_ffff)
                            | (((*crc_offset_blocks as u32 as u64)
                                .saturating_add(if *crc_offset_blocks >> 32 != 0 || *crc32c != 0 {
                                    shift
                                } else {
                                    0
                                }))
                                & 0xffff_ffff),
                    },
                    KeyValue::BtreePtr(_) | KeyValue::Raw(_) => KeyValue::Extent(ExtentValue {
                        paddr: old_paddr + shift,
                        size: size as u32,
                        ver: old_ver,
                        dev_idx: 0,
                        crc32c: 0,
                        crc_offset_blocks: 0,
                    }),
                }
            };

            // 删除旧 extent
            trans.bch2_trans_delete(BtreeId::Extents, 0, false, old_key, 0);

            // 左段：[old_start, new_start)
            if old_start < new_start {
                let left_end = new_start.min(old_end);
                let left_size = left_end - old_start;
                let mut left_key = old_key;
                left_key.vaddr = old_start;
                left_key.size = left_size as u32;
                let left_value = trimmed_value(old_start, left_size);
                trans.bch2_trans_update_raw(
                    BtreeId::Extents,
                    0,
                    false,
                    left_key,
                    left_value.to_bytes(),
                    0,
                );
            }

            // 右段：[new_end, old_end)
            if new_end < old_end {
                let right_start = new_end.max(old_start);
                let right_size = old_end - right_start;
                let mut right_key = old_key;
                right_key.vaddr = right_start;
                right_key.size = right_size as u32;
                let right_value = trimmed_value(right_start, right_size);
                trans.bch2_trans_update_raw(
                    BtreeId::Extents,
                    0,
                    false,
                    right_key,
                    right_value.to_bytes(),
                    0,
                );
            }
        }
        if has_entries {
            trans
                .bch2_trans_commit()
                .map_err(|e| StorageError::JournalError(e.to_string()))?;
        }
        Ok(())
    }

    /// 批量写入 block
    async fn write_blocks_on_device(
        &self,
        dev: Arc<BchDev>,
        addr: BlockAddr,
        buf: &[u8],
    ) -> Result<(), StorageError> {
        let block_size = self.block_size as usize;
        let nblocks = buf.len() / block_size;
        let writes = (0..nblocks).map(|i| {
            let dev = dev.clone();
            let off = i * block_size;
            let data = buf[off..off + block_size].to_vec();
            async move {
                let _io_ref = dev
                    .try_get_io_ref_guard(BchDevIoRefKind::Write)
                    .ok_or_else(|| StorageError::NotFound("device offline".into()))?;
                dev.bdev()
                    .write_block_with_csum(BlockAddr::new(addr.raw + i as u64), &data)
                    .await
                    .map(|_| ())
            }
        });

        // bcachefs submits all bios in the write point before waiting for the
        // closure. Await every completion so one failed bio cannot cancel
        // still-running writes; return the first error in submission order.
        let results = join_all(writes).await;
        for result in results {
            result?;
        }
        Ok(())
    }

    /// bcachefs 对齐: bch2_btree_delete_range (fs/btree/update.h:262)
    ///
    /// 删除给定范围内所有 extent 键。范围是半开区间。
    /// `start` 和 `end` 为块级别 Bpos（offset = block 号，snapshot = 快照 ID）。
    ///
    /// 已知偏差: `start.snapshot` 作为快照过滤器，`_flags` 暂未使用。
    pub async fn bch2_btree_delete_range(
        &self,
        _btree_id: BtreeId,
        start: Bpos,
        end: Bpos,
        _flags: u32,
    ) -> Result<(), StorageError> {
        let start_block = start.offset;
        let target_end = end.offset;
        let snapshot_id = start.snapshot;
        if start_block >= target_end {
            return Ok(());
        }

        // Match bcachefs `bch2_btree_delete_range()`/`delete_range_one()`:
        // enter the write reference before taking an intent iterator, so a
        // concurrent read-only transition cannot pass the range scan and
        // leave a trim update outside the write lifetime.
        if !self.try_begin_write() {
            return Err(StorageError::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "volume is going read-only",
            )));
        }
        let mut cursor = start_block;

        let btree = self.btree(BtreeId::Extents);
        let target = BtreeKey::new(cursor, snapshot_id, KeyType::Normal);
        let mut trans = BtreeTrans::new(self);
        let iter = trans.bch2_trans_get_iter(btree.root(), &target, true, BtreeId::Extents);
        iter.snapshot = snapshot_id;
        match iter.peek() {
            Some((key, _)) => {
                let key_vaddr = unsafe { std::ptr::addr_of!(key.vaddr).read_unaligned() };
                if key_vaddr > cursor {
                    iter.prev_slot();
                }
            }
            None => {
                iter.prev_slot();
            }
        }

        let mut entries_to_trim: Vec<(BtreeKey, KeyValue, Vec<(u8, u64)>, u64, u64, bool)> =
            Vec::new();
        loop {
            let entries: Vec<BtreeKey> = {
                let mut results = Vec::new();
                match iter.peek_visible_range(self) {
                    Some((k, _v, vs, ve)) if vs < target_end && ve > cursor => {
                        if k.snapshot_id == snapshot_id {
                            results.push(k);
                        }
                    }
                    _ => break,
                }
                results
            };

            for key in entries {
                let entry = match self.get_entry_raw(
                    BtreeId::Extents,
                    Bpos::new(key.inode, key.vaddr, key.snapshot_id),
                ) {
                    Some(entry) => entry,
                    None => {
                        self.end_write();
                        return Err(StorageError::NotFound("trim extent not found".into()));
                    }
                };
                let value = match entry.value {
                    KeyValue::Raw(bytes) => KeyValue::from_bytes(&bytes),
                    value => value,
                };
                let key_end = key.vaddr + u64::from(key.size.max(1));
                let overlap_start = key.vaddr.max(start_block);
                let overlap_end = key_end.min(target_end);
                let fully_trimmed = overlap_start <= key.vaddr && overlap_end >= key_end;
                let pointers = if fully_trimmed {
                    match &value {
                        KeyValue::Extent(value) => vec![(value.dev_idx, value.paddr)],
                        KeyValue::ExtentPtrs { ptrs, .. } => {
                            ptrs.iter().map(|ptr| (ptr.dev, ptr.offset)).collect()
                        }
                        KeyValue::BtreePtr(_) | KeyValue::Raw(_) => Vec::new(),
                    }
                } else {
                    Vec::new()
                };
                entries_to_trim.push((
                    key,
                    value,
                    pointers,
                    overlap_start,
                    overlap_end,
                    fully_trimmed,
                ));
            }
            cursor += 1;
            if cursor >= target_end {
                break;
            }
            if !iter.advance_visible(self) {
                break;
            }
        }

        if entries_to_trim.is_empty() {
            self.end_write();
            return Ok(());
        }

        drop(trans);

        // bcachefs keeps all updates from one trim operation in the same
        // btree transaction (`bch2_trans_commit()`); one journal reservation
        // and one lock pass is both faster and prevents a half-trimmed range
        // from becoming visible when a later delete fails.
        let result = async {
            let mut trans = BtreeTrans::new(self);
            trans.bch2_trans_begin();
            for (key, value, _, overlap_start, overlap_end, fully_trimmed) in &entries_to_trim {
                trans.bch2_trans_delete(BtreeId::Extents, 0, false, *key, 0);
                    if !fully_trimmed {
                        let trimmed_value = |range_start: u64, range_size: u64| match value {
                        KeyValue::Extent(extent) => {
                            let has_crc32c =
                                extent.crc_offset_blocks >> 32 != 0 || extent.crc32c != 0;
                            KeyValue::Extent(ExtentValue {
                                paddr: if has_crc32c {
                                    extent.paddr
                                } else {
                                    extent.paddr + range_start - key.vaddr
                                },
                                size: range_size as u32,
                                ver: extent.ver,
                                dev_idx: extent.dev_idx,
                                crc32c: extent.crc32c,
                                crc_offset_blocks: (extent.crc_offset_blocks & !0xffff_ffff)
                                    | ((extent.crc_offset_blocks as u32 as u64)
                                        .saturating_add(if has_crc32c {
                                            range_start - key.vaddr
                                        } else {
                                            0
                                        })
                                        & 0xffff_ffff),
                            })
                        }
                        KeyValue::ExtentPtrs { ptrs, crc32c, blocks: _, crc_offset_blocks } => KeyValue::ExtentPtrs {
                            blocks: range_size as u32,
                            ptrs: ptrs
                                .iter()
                                .map(|ptr| ExtentPtr {
                                    offset: if *crc_offset_blocks >> 32 != 0 || *crc32c != 0 {
                                        ptr.offset
                                    } else {
                                        ptr.offset + range_start - key.vaddr
                                    },
                                    ..*ptr
                                })
                                .collect(),
                            crc32c: *crc32c,
                            crc_offset_blocks: (*crc_offset_blocks & !0xffff_ffff)
                                | (((*crc_offset_blocks as u32 as u64)
                                    .saturating_add(if *crc_offset_blocks >> 32 != 0 || *crc32c != 0 {
                                        range_start - key.vaddr
                                    } else {
                                        0
                                    }))
                                    & 0xffff_ffff),
                        },
                        KeyValue::BtreePtr(_) | KeyValue::Raw(_) => value.clone(),
                    };
                    if *overlap_start > key.vaddr {
                        let left_key = BtreeKey {
                            size: (*overlap_start - key.vaddr) as u32,
                            ..*key
                        };
                        trans.bch2_trans_update_raw(
                            BtreeId::Extents,
                            0,
                            false,
                            left_key,
                            trimmed_value(key.vaddr, left_key.size as u64).to_bytes(),
                            0,
                        );
                    }
                    if *overlap_end < key.vaddr + u64::from(key.size.max(1)) {
                        let right_key = BtreeKey {
                            vaddr: *overlap_end,
                            size: (key.vaddr + u64::from(key.size.max(1)) - *overlap_end) as u32,
                            ..*key
                        };
                        trans.bch2_trans_update_raw(
                            BtreeId::Extents,
                            0,
                            false,
                            right_key,
                            trimmed_value(right_key.vaddr, right_key.size as u64).to_bytes(),
                            0,
                        );
                    }
                }
            }
            trans
                .bch2_trans_commit()
                .map_err(|e| StorageError::JournalError(e.to_string()))?;

            let alloc_ptr = self.allocator.get();
            for (_key, _, _, overlap_start, overlap_end, _) in &entries_to_trim {
                self.add_trim_hole(snapshot_id, *overlap_start, *overlap_end);
            }
            for (_, _, pointers, _, _, _) in &entries_to_trim {
                for (dev_idx, paddr) in pointers {
                    let ca = self.device_rcu_noerror(*dev_idx).ok_or_else(|| {
                        StorageError::NotFound(format!(
                            "delete_extent: device {} not found",
                            dev_idx
                        ))
                    })?;
                    unsafe {
                        (*alloc_ptr).bch2_bucket_free(&ca, *paddr, self)?;
                    }
                }
            }
            Ok(())
        }
        .await;
        self.end_write();
        result
    }

    // ──── 统计 ────

    pub async fn stats(&self) -> VolumeStats {
        let snapshot_count = {
            let t = BtreeTrans::new_ro(self);
            bch2_snapshot_list(&t).len()
        };
        let (total_blocks, allocated_blocks) = self
            .device_registry
            .dev_indices()
            .into_iter()
            .filter_map(|dev_idx| self.device_registry.resolve_bch_dev(dev_idx))
            .fold((0, 0), |(total, allocated), ca| {
                (
                    total + self.allocator().total_blocks(&ca),
                    allocated + self.allocator().allocated_blocks(&ca),
                )
            });
        VolumeStats {
            block_size: self.block_size,
            capacity: self.logical_capacity,
            total_blocks,
            allocated_blocks,
            mapping_entries: (self.btree(BtreeId::Extents).root().node.packed_keys + self.btree(BtreeId::Extents).root().node.unpacked_keys) as usize,
            btree_keys: BTREE_ID_NR
                .iter()
                .map(|ty| self.btree(*ty).root().node.packed_keys as u32 + self.btree(*ty).root().node.unpacked_keys as u32)
                .sum::<u32>(),
            snapshot_count,
            snapshot_tree_depth: 0,
        }
    }

    pub fn block_size(&self) -> u32 {
        self.block_size
    }

    pub fn capacity(&self) -> u64 {
        self.logical_capacity
    }
}

// ─── BchVol 内部帮助方法（可直接访问 UnsafeCell 字段）───

impl BchVol {
    /// 分配 btree 扇区（只读 flush 场景）
    pub(crate) fn alloc_btree_sectors(
        &self,
        req: &AllocRequest,
        blocks: u64,
    ) -> Result<u64, StorageError> {
        let alloc_ptr = self.allocator.get();
        let ca = self.primary_device_rcu_noerror().ok_or_else(|| {
            StorageError::NotFound("alloc_btree_sectors: no registered device".into())
        })?;
        unsafe {
            (*alloc_ptr).bch2_alloc_sectors_start_trans(
                blocks,
                self,
                &ca,
                req,
                Some(WritePointSpecifier::Direct(DedicatedWp::BTree)),
            )
        }
        .map_err(StorageError::from)
    }

    /// journal 事务提交：insert（带写引用追踪）
    ///
    /// 获取写引用后才允许提交 journal 写入。如果卷处于 GoingRo 状态，
    /// 返回 PermissionDenied 错误阻止新写入。
    async fn trans_update_commit(
        &self,
        btree_id: BtreeId,
        level: u8,
        gc: bool,
        key: BtreeKey,
        value: BchVal,
    ) -> Result<u64, StorageError> {
        if !self.try_begin_write() {
            return Err(StorageError::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "volume is going read-only",
            )));
        }
        let mut trans = BtreeTrans::new(self);
        trans.bch2_trans_begin();
        trans.bch2_trans_update(btree_id, level, gc, key, value, 0);
        let result = trans
            .bch2_trans_commit()
            .map_err(|e| StorageError::JournalError(e.to_string()));
        self.end_write();
        result
    }

    /// 提交带完整 extent value 的事务更新。
    ///
    /// `BchVal` 是历史单指针投影；多副本 extent 必须通过本地
    /// bcachefs `bkey` 的完整指针列表进入 journal。`bch2_trans_update_raw`
    /// 保留原始 value 字节，随后由 commit/replay 路径按同一格式重建 entry。
    async fn trans_update_commit_raw(
        &self,
        btree_id: BtreeId,
        level: u8,
        gc: bool,
        key: BtreeKey,
        raw_value: Vec<u8>,
    ) -> Result<u64, StorageError> {
        if !self.try_begin_write() {
            return Err(StorageError::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "volume is going read-only",
            )));
        }
        let mut trans = BtreeTrans::new(self);
        trans.bch2_trans_begin();
        trans.bch2_trans_update_raw(btree_id, level, gc, key, raw_value, 0);
        let result = trans
            .bch2_trans_commit()
            .map_err(|e| StorageError::JournalError(e.to_string()));
        self.end_write();
        result
    }

    /// journal 事务提交：delete（带写引用追踪）
    ///
    /// 获取写引用后才允许提交 journal 删除。如果卷处于 GoingRo 状态，
    /// 返回 PermissionDenied 错误阻止新写入。
    async fn trans_delete_commit(
        &self,
        btree_id: BtreeId,
        level: u8,
        gc: bool,
        key: BtreeKey,
    ) -> Result<u64, StorageError> {
        if !self.try_begin_write() {
            return Err(StorageError::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "volume is going read-only",
            )));
        }
        let mut trans = BtreeTrans::new(self);
        trans.bch2_trans_begin();
        trans.bch2_trans_delete(btree_id, level, gc, key, 0);
        let result = trans
            .bch2_trans_commit()
            .map_err(|e| StorageError::JournalError(e.to_string()));
        self.end_write();
        result
    }
}

// ---------------------------------------------------------------------------
// flush — 设备级持久化屏障
// ---------------------------------------------------------------------------

impl BchVol {
    /// 刷新所有在线设备的后端缓存到持久介质。
    ///
    /// 对应 bcachefs 每个设备的 FLUSH 操作（最终通过 journal FLUSH 持久化）。
    /// 对齐本地 bcachefs `bch2_journal_flush()` (`fs/journal/journal.c:1255`)
    /// 后再执行设备级屏障：先提交当前 journal entry，再在多设备上对所有
    /// 在线成员发送 FLUSH。这样 FUSE fsync 和 NBD FUA 不会只刷设备缓存而
    /// 把仍停留在 journal buffer 中的元数据留给下一次 journal flush。
    pub async fn flush(&self) -> Result<(), StorageError> {
        for ty in crate::btree::BTREE_ID_NR {
            self.btree(ty).bch2_btree_interior_updates_flush().await;
        }
        self.flush_pending_root_journals().await?;
        self.journal_ref()
            .bch2_journal_flush()
            .await
            .map_err(|e| StorageError::JournalError(e.to_string()))?;

        let mut flushes = Vec::new();
        for dev_idx in self.device_registry.dev_indices() {
            let Some(dev) = self.device_rcu_noerror(dev_idx) else {
                continue;
            };
            if !dev.is_online() {
                continue;
            }
            let Some(io_ref) = dev.try_get_io_ref_guard(BchDevIoRefKind::Write) else {
                continue;
            };
            flushes.push(async move {
                let _io_ref = io_ref;
                dev.bdev().flush().await
            });
        }
        if flushes.is_empty() {
            return Err(StorageError::NotFound(
                "no online device available for flush".into(),
            ));
        }
        let results = join_all(flushes).await;
        for result in results {
            result?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 后端创建
// ---------------------------------------------------------------------------

pub async fn create_backend(
    backend_type: BackendType,
    vol_dir: &Path,
    _capacity: u64,
) -> Result<Arc<dyn BlockDevice>, StorageError> {
    let vol_name = vol_dir
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let base_path = vol_dir.parent().unwrap_or(Path::new("")).to_path_buf();
    match backend_type {
        BackendType::Nfs => {
            let config = crate::block_device::NfsConfig {
                base_path,
                vol_name,
                block_size: 4096,
            };
            let backend = crate::block_device::NfsBlockDevice::new(config).await?;
            Ok(Arc::new(backend))
        }
        BackendType::S3 => {
            let config = crate::block_device::S3Config {
                bucket: format!("subvolmount-{vol_name}"),
                key_prefix: String::new(),
                region: String::from("us-east-1"),
                endpoint_url: None,
                ..Default::default()
            };
            let backend = crate::block_device::S3BlockDevice::new(config).await?;
            Ok(Arc::new(backend))
        }
    }
}

/// 打开已有后端（不创建目录）
///
/// 用于启动时根据配置直接加载已有卷，避免先做路径探测再决定。
pub async fn open_backend(
    backend_type: BackendType,
    vol_dir: &Path,
    block_size: u32,
) -> Result<Arc<dyn BlockDevice>, StorageError> {
    let vol_name = vol_dir
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let base_path = vol_dir.parent().unwrap_or(Path::new("")).to_path_buf();
    match backend_type {
        BackendType::Nfs => {
            let config = crate::block_device::NfsConfig {
                base_path,
                vol_name,
                block_size: block_size as u64,
            };
            let backend = crate::block_device::NfsBlockDevice::open(config).await?;
            Ok(Arc::new(backend))
        }
        BackendType::S3 => {
            let config = crate::block_device::S3Config {
                bucket: format!("subvolmount-{vol_name}"),
                key_prefix: String::new(),
                region: String::from("us-east-1"),
                endpoint_url: None,
                ..Default::default()
            };
            let backend = crate::block_device::S3BlockDevice::new(config).await?;
            Ok(Arc::new(backend))
        }
    }
}

// ---------------------------------------------------------------------------
// 恢复跟踪
// ---------------------------------------------------------------------------

impl BchVol {
    pub fn recovery_progress(&self) -> (u8, u64, u64) {
        (
            self.recovery_pass_done.load(Ordering::Acquire),
            self.recovery_passes_complete.load(Ordering::Acquire),
            self.passes_failing.load(Ordering::Acquire),
        )
    }

    pub fn set_recovery_progress(&self, pass_done: u8, passes_complete: u64, passes_failing: u64) {
        self.recovery_pass_done.store(pass_done, Ordering::Release);
        self.recovery_passes_complete
            .store(passes_complete, Ordering::Release);
        self.passes_failing.store(passes_failing, Ordering::Release);
    }

    pub fn record_error(&self) {
        self.error_count.fetch_add(1, Ordering::Release);
    }

    pub fn error_count(&self) -> u64 {
        self.error_count.load(Ordering::Acquire)
    }

    pub fn record_fsck_error(&self) {
        self.fsck_error.fetch_add(1, Ordering::Release);
    }

    pub fn fsck_error_count(&self) -> u64 {
        self.fsck_error.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_device::MockBlockDevice;
    use crate::io::{
        BchIoFailures, BchReadBio, BchReadFlags, BchWriteFlags, BchWriteOp, BkeyBuf, BvecIter,
        SubvolInum,
    };
    use crate::storage::superblock::BchSbMember;
    use std::path::PathBuf;

    // ─── 测试 helper：bcachefs 对齐 API 的便利封装 ───

    /// 用 bcachefs 对齐的 bch2_write 快速写入 extent。
    /// 原签名: `vol.bch2_write(key, buf)` → 新签名需 BchWriteOp。
    async fn test_write(vol: &BchVol, key: BtreeKey, buf: &[u8]) -> Result<(), StorageError> {
        let mut op = BchWriteOp {
            flags: BchWriteFlags::SYNC,
            subvol: BCACHEFS_ROOT_SUBVOL as u32,
            pos: Bpos::new(key.inode, key.vaddr, 0),
            data: buf.to_vec(),
            csum_type: 0,
            compression_opt: 0,
            nr_replicas: 1,
            watermark: 0,
        };
        vol.bch2_write(&mut op).await
    }

    /// 用 bcachefs 对齐的 bch2_read 快速读取 extent。
    /// 原签名: `test_read(&vol, offset, buf)` → 新签名需 7 个参数。
    async fn test_read(vol: &BchVol, offset: u64, buf: &mut [u8]) -> Result<(), StorageError> {
        let mut rbio = BchReadBio {
            data: buf.to_vec(),
            offset_into_extent: 0,
            flags: 0,
        };
        let iter = BvecIter {
            bi_sector: offset >> 9,
            bi_size: buf.len() as u32,
        };
        let inum = SubvolInum {
            subvol: BCACHEFS_ROOT_SUBVOL,
            inum: 0,
        };
        let mut failed = BchIoFailures {
            nr: 0,
            data: vec![],
        };
        let mut prev_read = BkeyBuf { k: None, v: None };
        let mut trans = BtreeTrans::new_ro(vol);
        vol.bch2_read(
            &mut trans,
            &mut rbio,
            iter,
            inum,
            &mut failed,
            &mut prev_read,
            BchReadFlags::empty(),
        )
        .await?;
        buf.copy_from_slice(&rbio.data[..buf.len()]);
        Ok(())
    }

    fn make_vol() -> BchVol {
        let sb = BchSb::with_volume_info(
            "test-vol".to_string(),
            1,
            "default".to_string(),
            4096,
            4096 * 1024,
            BackendType::Nfs,
        );
        let vol = BchVol::alloc(
            sb,
            Arc::new(BchDev::new(Arc::new(MockBlockDevice::new()), 0)),
            VolumeConfig {
                block_size: 4096,
                capacity: 4096 * 1024,
                btree_node_size: crate::alloc::DEFAULT_BTREE_NODE_SIZE,
                ..VolumeConfig::default()
            },
            "test-vol".to_string(),
            PathBuf::from("/tmp/test-vol"),
        );
        // bcachefs bch2_write()/bch2_read() take a real subvolume ID and
        // resolve its snapshot internally.  Keep this low-level fixture
        // initialized with the same root subvolume before exercising those
        // APIs.
        let mut trans = BtreeTrans::new(&vol);
        bch2_initialize_subvolumes(&mut trans).expect("initialize root subvolume");
        trans.bch2_trans_commit()
            .expect("commit root subvolume");
        drop(trans);
        vol
    }

    #[test]
    fn create_persists_root_subvolume_for_reopen() {
        std::thread::Builder::new()
            .stack_size(32 * 1024 * 1024)
            .spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(async {
                        let dir = tempfile::TempDir::new().unwrap();
                        let vol_dir = dir.path().join("reopen-root");
                        std::fs::create_dir(&vol_dir).unwrap();

                        let created = BchVol::open_pool(&vol_dir, "reopen-root")
                            .await
                            .unwrap();
                        let backend = open_backend(BackendType::Nfs, &vol_dir, 4096)
                            .await
                            .unwrap();
                        let reopened = BchVol::open(backend, &vol_dir, "reopen-root")
                            .await
                            .unwrap();
                        let trans = BtreeTrans::new_ro(&reopened);

                        assert!(bch2_subvolume_get(
                            &trans,
                            BCACHEFS_ROOT_SUBVOL as u32,
                            true
                        )
                        .is_ok());
                        drop(created);
                    });
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[tokio::test]
    async fn poisoned_extent_is_persisted_and_rejected_before_io() {
        let vol = make_vol();
        vol.state.store(VolumeState::Rw as u8, Ordering::Release);
        test_write(&vol, BtreeKey::new(0, 0, KeyType::Normal), &[0x4a; 4096])
            .await
            .unwrap();

        let pos = Bpos::new(0, 0, u32::MAX);
        let entry = vol.get_entry_raw(BtreeId::Extents, pos).unwrap();
        let (key, _) = entry.to_key_value();
        let mut raw = entry.value.to_bytes();
        let mut metadata = [0u8; 8];
        match KeyValue::from_bytes(&raw) {
            KeyValue::Extent(_) => {
                metadata.copy_from_slice(&raw[18..26]);
                raw[18..26].copy_from_slice(
                    &(u64::from_le_bytes(metadata) | EXTENT_CRC_POISONED_BIT).to_le_bytes(),
                );
            }
            KeyValue::ExtentPtrs { .. } => {
                metadata.copy_from_slice(&raw[12..20]);
                raw[12..20].copy_from_slice(
                    &(u64::from_le_bytes(metadata) | EXTENT_CRC_POISONED_BIT).to_le_bytes(),
                );
            }
            value => panic!("unexpected extent value: {value:?}"),
        }
        vol.trans_update_commit_raw(BtreeId::Extents, 0, false, key, raw)
            .await
            .unwrap();

        let mut buf = vec![0u8; 4096];
        assert!(matches!(
            test_read(&vol, 0, &mut buf).await,
            Err(StorageError::ExtentPoisoned)
        ));
    }

    #[test]
    fn write_refs_require_active_rw_state() {
        let vol = make_vol();

        vol.state
            .store(VolumeState::ReadOnly as u8, Ordering::Release);
        assert!(!vol.try_begin_write());

        vol.state
            .store(VolumeState::Stopped as u8, Ordering::Release);
        assert!(!vol.try_begin_write());

        vol.state
            .store(VolumeState::GoingRo as u8, Ordering::Release);
        assert!(!vol.try_begin_write());

        vol.state
            .store(VolumeState::RwWithPendingRecovery as u8, Ordering::Release);
        assert!(vol.try_begin_write());
        assert_eq!(vol.write_ref_count.load(Ordering::Acquire), 1);
        vol.end_write();

        vol.state.store(VolumeState::Rw as u8, Ordering::Release);
        assert!(vol.try_begin_write());
        assert_eq!(vol.write_ref_count.load(Ordering::Acquire), 1);
        vol.end_write();
    }

    #[tokio::test]
    async fn write_extent_rejects_member_without_user_data_permission() {
        let vol = make_vol();
        vol.state.store(VolumeState::Rw as u8, Ordering::Release);
        let dev = vol.primary_device_rcu_noerror().unwrap();
        unsafe {
            (*dev.mi.get()).data_allowed = 1 << crate::alloc::BchDataType::Journal as u8;
        }

        let result = test_write(&vol, BtreeKey::new(0, 0, KeyType::Normal), &[0x5a; 4096]).await;
        assert!(
            matches!(result, Err(StorageError::NotFound(message)) if message == "no writable extent device")
        );
    }

    #[tokio::test]
    async fn write_extent_invalid_geometry_does_not_leak_write_ref() {
        let vol = make_vol();
        vol.state.store(VolumeState::Rw as u8, Ordering::Release);

        let result = test_write(&vol, BtreeKey::new(1, 0, KeyType::Normal), &[0x5a; 4096]).await;
        assert!(matches!(result, Err(StorageError::InvalidArgument(_))));
        assert_eq!(vol.write_ref_count.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn write_extent_preserves_durable_pointer_list() {
        let mut vol = make_vol();
        vol.state.store(VolumeState::Rw as u8, Ordering::Release);
        vol.opts.data_replicas = 2;
        let dev = vol.primary_device_rcu_noerror().unwrap();
        unsafe {
            (*dev.mi.get()).durability = 2;
        }

        test_write(&vol, BtreeKey::new(0, 0, KeyType::Normal), &[0x6b; 4096])
            .await
            .unwrap();
        let entry = vol
            .get_entry_raw(BtreeId::Extents, Bpos::new(0, 0, u32::MAX))
            .unwrap();
        let value = match entry.value {
            KeyValue::Raw(bytes) => KeyValue::from_bytes(&bytes),
            value => value,
        };
        let KeyValue::ExtentPtrs { ptrs, .. } = value else {
            panic!("durable device should be persisted as an extent pointer list");
        };
        assert_eq!(ptrs.len(), 1);
    }

    #[tokio::test]
    async fn trim_removes_extent_entry() {
        let vol = make_vol();
        vol.state.store(VolumeState::Rw as u8, Ordering::Release);
        test_write(
            &vol,
            BtreeKey::new(8 * 4096, 0, KeyType::Normal),
            &[0x77; 4096],
        )
        .await
        .unwrap();
        vol.bch2_btree_delete_range(
            BtreeId::Extents,
            Bpos::new(0, 8, u32::MAX),
            Bpos::new(0, 9, u32::MAX),
            0,
        )
            .await
            .unwrap();
        assert!(vol
            .get_entry_raw(BtreeId::Extents, Bpos::new(0, 8, u32::MAX))
            .is_none());
    }

    #[tokio::test]
    async fn trim_partial_extent_preserves_untrimmed_ranges() {
        let vol = make_vol();
        vol.state.store(VolumeState::Rw as u8, Ordering::Release);
        let mut data = vec![0u8; 3 * 4096];
        data[..4096].fill(0x11);
        data[4096..8192].fill(0x22);
        data[8192..].fill(0x33);
        test_write(&vol, BtreeKey::new(0, 0, KeyType::Normal), &data)
            .await
            .unwrap();
        vol.bch2_btree_delete_range(
            BtreeId::Extents,
            Bpos::new(0, 1, u32::MAX),
            Bpos::new(0, 2, u32::MAX),
            0,
        )
            .await
            .unwrap();

        let mut readback = vec![0u8; 3 * 4096];
        test_read(&vol, 0, &mut readback).await.unwrap();
        assert_eq!(&readback[..4096], &[0x11; 4096]);
        assert_eq!(&readback[4096..8192], &[0; 4096]);
        assert_eq!(&readback[8192..], &[0x33; 4096]);
    }

    #[test]
    fn device_registry_resolves_multiple_members() {
        let dev0 = Arc::new(BchDev::new(Arc::new(MockBlockDevice::new()), 0));
        let dev1 = Arc::new(BchDev::new(Arc::new(MockBlockDevice::new()), 1));
        let registry = BchDeviceRegistry::from_devices(vec![dev0.clone(), dev1.clone()]);

        assert_eq!(registry.dev_indices(), vec![0, 1]);
        assert!(Arc::ptr_eq(&registry.resolve_bch_dev(0).unwrap(), &dev0));
        assert!(Arc::ptr_eq(&registry.resolve_bch_dev(1).unwrap(), &dev1));
    }

    #[test]
    fn device_registry_reports_member_durability() {
        let dev = Arc::new(BchDev::new(Arc::new(MockBlockDevice::new()), 3));
        unsafe {
            (*dev.mi.get()).durability = 2;
        }
        let registry = BchDeviceRegistry::from_devices(vec![dev]);
        assert_eq!(registry.durability(3), 2);
        assert_eq!(registry.durability(99), 1);
    }

    #[test]
    fn alloc_resumes_journal_position_from_member_info() {
        let mut sb = BchSb::with_volume_info(
            "test-vol".to_string(),
            1,
            "default".to_string(),
            4096,
            4096 * 1024,
            BackendType::Nfs,
        );
        sb.journal_buckets = vec![256, 512];
        let member = sb.member_mut(0).unwrap();
        member.last_journal_bucket = 1;
        member.last_journal_bucket_offset = 16;
        let bucket_size = u32::from(member.bucket_size);
        let dev = Arc::new(BchDev::new(Arc::new(MockBlockDevice::new()), 0));

        let _vol = BchVol::alloc(
            sb,
            dev.clone(),
            VolumeConfig {
                block_size: 4096,
                capacity: 4096 * 1024,
                btree_node_size: crate::alloc::DEFAULT_BTREE_NODE_SIZE,
                ..VolumeConfig::default()
            },
            "test-vol".to_string(),
            PathBuf::from("/tmp/test-vol"),
        );

        let ja = dev.journal.lock().unwrap();
        assert_eq!(ja.cur_idx, 1);
        assert_eq!(ja.sectors_free, bucket_size - 16);
    }

    #[test]
    fn devices_own_independent_disk_sb_and_journal_metadata() {
        let mut sb = BchSb::with_volume_info(
            "test-vol".to_string(),
            1,
            "default".to_string(),
            4096,
            4096 * 4096,
            BackendType::Nfs,
        );
        let mut member1 = BchSbMember::new(1, "dev-1");
        member1.mark_alive([2; 16]);
        member1.nbuckets = sb.members[0].nbuckets;
        member1.bucket_size = sb.members[0].bucket_size;
        sb.members.push(member1);
        sb.journal_buckets = vec![9, 10];

        let dev0 = Arc::new(BchDev::new(Arc::new(MockBlockDevice::new()), 0));
        let dev1 = Arc::new(BchDev::new(Arc::new(MockBlockDevice::new()), 1));
        let vol = BchVol::alloc_with_devices(
            sb,
            vec![dev0.clone(), dev1.clone()],
            VolumeConfig {
                block_size: 4096,
                capacity: 4096 * 4096,
                btree_node_size: crate::alloc::DEFAULT_BTREE_NODE_SIZE,
                ..VolumeConfig::default()
            },
            "test-vol".to_string(),
            PathBuf::from("/tmp/test-vol"),
        );

        dev0.disk_sb.lock().unwrap().layout.nr_superblocks = 2;
        dev0.disk_sb.lock().unwrap().layout.sb_offset[..2].copy_from_slice(&[8, 32]);
        dev1.disk_sb.lock().unwrap().layout.nr_superblocks = 2;
        dev1.disk_sb.lock().unwrap().layout.sb_offset[..2].copy_from_slice(&[16, 48]);
        dev0.journal.lock().unwrap().buckets = vec![9];
        dev1.journal.lock().unwrap().buckets = vec![11];

        assert_eq!(
            &dev0.disk_sb.lock().unwrap().layout.sb_offset[..2],
            &[8, 32]
        );
        assert_eq!(
            &dev1.disk_sb.lock().unwrap().layout.sb_offset[..2],
            &[16, 48]
        );
        assert_eq!(dev0.journal.lock().unwrap().buckets, vec![9]);
        assert_eq!(dev1.journal.lock().unwrap().buckets, vec![11]);
        assert_eq!(vol.device_registry.dev_indices(), vec![0, 1]);
    }

    #[test]
    fn online_member_iteration_orders_filters_and_releases_refs() {
        let dev0 = Arc::new(BchDev::new(Arc::new(MockBlockDevice::new()), 0));
        let dev1 = Arc::new(BchDev::new(Arc::new(MockBlockDevice::new()), 1));
        let dev2 = Arc::new(BchDev::new(Arc::new(MockBlockDevice::new()), 2));
        dev1.set_offline();
        let registry =
            BchDeviceRegistry::from_devices(vec![dev2.clone(), dev0.clone(), dev1.clone()]);

        let ca0 = registry
            .bch2_get_next_online_dev(None, u32::MAX, BchDevIoRefKind::Read)
            .unwrap();
        assert_eq!(ca0.dev_idx, 0);
        assert_eq!(dev0.io_ref_count(BchDevIoRefKind::Read), 1);

        let ca2 = registry
            .bch2_get_next_online_dev(Some(ca0), u32::MAX, BchDevIoRefKind::Read)
            .unwrap();
        assert_eq!(dev0.io_ref_count(BchDevIoRefKind::Read), 0);
        assert_eq!(ca2.dev_idx, 2);
        assert_eq!(dev1.io_ref_count(BchDevIoRefKind::Read), 0);

        assert!(registry
            .bch2_get_next_online_dev(Some(ca2), u32::MAX, BchDevIoRefKind::Read)
            .is_none());
        assert_eq!(dev2.io_ref_count(BchDevIoRefKind::Read), 0);

        let early = registry
            .bch2_get_next_online_dev(None, u32::MAX, BchDevIoRefKind::Read)
            .unwrap();
        drop(early);
        assert_eq!(dev0.io_ref_count(BchDevIoRefKind::Read), 0);
    }

    #[test]
    fn online_member_iteration_applies_state_mask_before_tryget() {
        let rw = Arc::new(BchDev::new(Arc::new(MockBlockDevice::new()), 0));
        let ro = Arc::new(BchDev::new(Arc::new(MockBlockDevice::new()), 1));
        ro.set_member_state(crate::storage::superblock::BchMemberState::Ro);
        let registry = BchDeviceRegistry::from_devices(vec![rw.clone(), ro.clone()]);
        let ro_mask = 1u32 << crate::storage::superblock::BchMemberState::Ro as u8;

        let ca = registry
            .bch2_get_next_online_dev(None, ro_mask, BchDevIoRefKind::Read)
            .unwrap();
        assert_eq!(ca.dev_idx, 1);
        drop(ca);
        assert_eq!(rw.io_ref_count(BchDevIoRefKind::Read), 0);
        assert_eq!(ro.io_ref_count(BchDevIoRefKind::Read), 0);
    }

    #[test]
    fn primary_device_falls_back_to_online_member_after_primary_loss() {
        let mut sb = BchSb::with_volume_info(
            "test-vol".to_string(),
            1,
            "default".to_string(),
            4096,
            4096 * 4096,
            BackendType::Nfs,
        );
        let mut member1 = BchSbMember::new(1, "dev-1");
        member1.mark_alive([2; 16]);
        member1.nbuckets = sb.members[0].nbuckets;
        member1.bucket_size = sb.members[0].bucket_size;
        sb.members.push(member1);

        let dev0 = Arc::new(BchDev::new(Arc::new(MockBlockDevice::new()), 0));
        let dev1 = Arc::new(BchDev::new(Arc::new(MockBlockDevice::new()), 1));
        let vol = BchVol::alloc_with_devices(
            sb,
            vec![dev0.clone(), dev1.clone()],
            VolumeConfig {
                block_size: 4096,
                capacity: 4096 * 4096,
                btree_node_size: crate::alloc::DEFAULT_BTREE_NODE_SIZE,
                ..VolumeConfig::default()
            },
            "test-vol".to_string(),
            PathBuf::from("/tmp/test-vol"),
        );

        assert_eq!(vol.primary_device_rcu_noerror().unwrap().dev_idx, 0);
        dev0.set_offline();
        assert_eq!(vol.primary_device_rcu_noerror().unwrap().dev_idx, 1);
    }

    #[tokio::test]
    async fn bch2_fs_read_write_switches_to_rw_before_background_work() {
        let dev = Arc::new(BchDev::new(Arc::new(MockBlockDevice::new()), 0));
        let sb = BchSb::with_volume_info(
            "test-vol".to_string(),
            1,
            "default".to_string(),
            4096,
            4096 * 1024,
            BackendType::Nfs,
        );
        sb.write_to_device(dev.as_ref()).await.unwrap();
        let vol = BchVol::alloc(
            sb,
            dev,
            VolumeConfig {
                block_size: 4096,
                capacity: 4096 * 1024,
                btree_node_size: crate::alloc::DEFAULT_BTREE_NODE_SIZE,
                ..VolumeConfig::default()
            },
            "test-vol".to_string(),
            PathBuf::from("/tmp/test-vol"),
        );
        vol.state
            .store(VolumeState::ReadOnly as u8, Ordering::Release);

        vol.bch2_fs_read_write().await.unwrap();

        assert_eq!(vol.state(), VolumeState::Rw);
    }

    #[tokio::test]
    async fn bch2_fs_read_write_marks_all_online_superblocks_dirty() {
        let mut sb = BchSb::with_volume_info(
            "test-vol".to_string(),
            1,
            "default".to_string(),
            4096,
            4096 * 1024,
            BackendType::Nfs,
        );
        let mut member1 = BchSbMember::new(1, "dev-1");
        member1.mark_alive([2; 16]);
        member1.nbuckets = sb.members[0].nbuckets;
        member1.bucket_size = sb.members[0].bucket_size;
        sb.members.push(member1);

        let dev0 = Arc::new(BchDev::new(Arc::new(MockBlockDevice::new()), 0));
        let dev1 = Arc::new(BchDev::new(Arc::new(MockBlockDevice::new()), 1));
        sb.write_to_device(dev0.as_ref()).await.unwrap();
        sb.write_to_device(dev1.as_ref()).await.unwrap();
        let vol = BchVol::alloc_with_devices(
            sb,
            vec![dev0, dev1.clone()],
            VolumeConfig {
                block_size: 4096,
                capacity: 4096 * 1024,
                btree_node_size: crate::alloc::DEFAULT_BTREE_NODE_SIZE,
                ..VolumeConfig::default()
            },
            "test-vol".to_string(),
            PathBuf::from("/tmp/test-vol"),
        );
        vol.state
            .store(VolumeState::ReadOnly as u8, Ordering::Release);

        vol.bch2_fs_read_write().await.unwrap();

        assert!(!BchSb::read_from_device(&dev1).await.unwrap().clean_shutdown);
    }

    #[tokio::test]
    async fn bch2_fs_read_only_short_circuits_when_not_rw() {
        let vol = make_vol();

        vol.state
            .store(VolumeState::ReadOnly as u8, Ordering::Release);

        vol.bch2_fs_read_only().await.unwrap();

        assert_eq!(vol.state(), VolumeState::ReadOnly);
    }

    #[tokio::test]
    async fn start_failure_transitions_to_error_state() {
        let vol = make_vol();
        // 将 state 设为 Error（非 New），compare_exchange(New→Starting) 失败。
        // compare_exchange 失败不会触发内部 Error 状态设置（这是 API 误用而非 init 错误），
        // 但 start() 会返回 Err，state 保持原始值 Error。
        vol.state
            .store(VolumeState::Error as u8, Ordering::Release);

        assert!(vol.start().await.is_err());
        assert_eq!(vol.state(), VolumeState::Error);
    }

    #[tokio::test]
    async fn failed_cow_write_keeps_old_extent_visible() {
        let backend = Arc::new(MockBlockDevice::new());
        let sb = BchSb::with_volume_info(
            "cow-failure".to_string(),
            1,
            "default".to_string(),
            4096,
            4096 * 1024,
            BackendType::Nfs,
        );
        let vol = BchVol::alloc(
            sb,
            Arc::new(BchDev::new(backend.clone(), 0)),
            VolumeConfig {
                block_size: 4096,
                capacity: 4096 * 1024,
                btree_node_size: crate::alloc::DEFAULT_BTREE_NODE_SIZE,
                ..VolumeConfig::default()
            },
            "cow-failure".to_string(),
            PathBuf::from("/tmp/cow-failure"),
        );
        let mut trans = BtreeTrans::new(&vol);
        bch2_initialize_subvolumes(&mut trans).unwrap();
        trans.bch2_trans_commit().unwrap();
        drop(trans);
        vol.state.store(VolumeState::Rw as u8, Ordering::Release);

        let old = vec![0x11u8; 4096];
        test_write(&vol, BtreeKey::new(0, 0, KeyType::Normal), &old)
            .await
            .unwrap();
        backend.set_write_error(true);
        assert!(
            test_write(&vol, BtreeKey::new(0, 0, KeyType::Normal), &[0x22u8; 4096])
                .await
                .is_err()
        );
        backend.set_write_error(false);

        let mut readback = vec![0u8; 4096];
        test_read(&vol, 0, &mut readback).await.unwrap();
        assert_eq!(readback, old);
    }

    #[tokio::test]
    async fn data_replicas_write_persists_extent_ptrs_on_each_device() {
        let mut sb = BchSb::new();
        sb.block_size = 4096;
        sb.capacity = 16 * 1024 * 1024;
        sb.primary_dev_idx = 0;
        sb.members = (0..2)
            .map(|dev_idx| {
                let mut member = BchSbMember::new(dev_idx, format!("dev-{dev_idx}"));
                member.mark_alive([dev_idx + 1; 16]);
                member.nbuckets = 16;
                member.bucket_size =
                    (crate::alloc::BLOCKS_PER_BUCKET * crate::alloc::SECTORS_PER_BLOCK) as u16;
                member
            })
            .collect();
        let backends: Vec<_> = (0..2).map(|_| Arc::new(MockBlockDevice::new())).collect();
        let devices: Vec<_> = backends
            .iter()
            .enumerate()
            .map(|(dev_idx, backend)| Arc::new(BchDev::new(backend.clone(), dev_idx as u8)))
            .collect();
        let mut vol = BchVol::alloc_with_devices(
            sb,
            devices.clone(),
            VolumeConfig {
                block_size: 4096,
                capacity: 16 * 1024 * 1024,
                btree_node_size: crate::alloc::DEFAULT_BTREE_NODE_SIZE,
                data_replicas: 2,
                ..VolumeConfig::default()
            },
            "replica-vol".into(),
            PathBuf::from("/tmp/replica-vol"),
        );
        let mut trans = BtreeTrans::new(&vol);
        bch2_initialize_subvolumes(&mut trans).unwrap();
        trans.bch2_trans_commit().unwrap();
        drop(trans);
        vol.state.store(VolumeState::Rw as u8, Ordering::Release);
        let data = vec![0x5au8; 8192];
        test_write(&vol, BtreeKey::new(0, 0, KeyType::Normal), &data)
            .await
            .expect("replica write");

        let entry = vol
            .get_entry_raw(BtreeId::Extents, Bpos::new(0, 0, u32::MAX))
            .expect("extent entry");
        let value = match entry.value {
            KeyValue::Raw(bytes) => KeyValue::from_bytes(&bytes),
            value => value,
        };
        let KeyValue::ExtentPtrs { ptrs, .. } = value else {
            panic!("expected multi-pointer extent");
        };
        assert_eq!(ptrs.len(), 2);
        for ptr in ptrs {
            let mut readback = vec![0u8; 4096];
            devices[ptr.dev as usize]
                .bdev()
                .read_block(BlockAddr::new(ptr.offset), &mut readback)
                .await
                .unwrap();
            assert_eq!(readback, vec![0x5au8; 4096]);
        }
        assert!(vol.flush().await.is_ok());

        // A failed replica must be dropped from the committed extent while
        // the surviving device remains readable (bcachefs degraded write).
        backends[0].set_write_error(true);
        let degraded = vec![0x3cu8; 4096];
        test_write(&vol, BtreeKey::new(4 * 4096, 0, KeyType::Normal), &degraded)
            .await
            .expect("surviving replica should commit");
        let mut degraded_read = vec![0u8; 4096];
        test_read(&vol, 4 * 4096, &mut degraded_read)
            .await
            .expect("surviving replica should read");
        assert_eq!(degraded_read, degraded);
        backends[0].set_write_error(false);

        backends[0].set_read_error(true);
        let mut transient_read = vec![0u8; 8192];
        test_read(&vol, 0, &mut transient_read)
            .await
            .expect("online IO failure should retry another replica");
        assert_eq!(transient_read, data);
        backends[0].set_read_error(false);

        // A COW overwrite splits the old extent. The surviving right range
        // must retain both physical replica pointers and its old data.
        let replacement = vec![0xa5u8; 4096];
        test_write(&vol, BtreeKey::new(0, 0, KeyType::Normal), &replacement)
            .await
            .expect("split replica write");
        let right = vol
            .get_entry_raw(BtreeId::Extents, Bpos::new(0, 1, u32::MAX))
            .expect("right split extent");
        let right_value = match right.value {
            KeyValue::Raw(bytes) => KeyValue::from_bytes(&bytes),
            value => value,
        };
        let KeyValue::ExtentPtrs { ptrs, .. } = right_value else {
            panic!("split extent lost replicas");
        };
        assert_eq!(ptrs.len(), 2);
        for ptr in ptrs {
            let mut readback = vec![0u8; 4096];
            devices[ptr.dev as usize]
                .bdev()
                .read_block(BlockAddr::new(ptr.offset), &mut readback)
                .await
                .unwrap();
            assert_eq!(readback, vec![0x5au8; 4096]);
        }

        devices[0].set_offline();
        assert!(vol.flush().await.is_ok());
        let mut failover_read = vec![0u8; 8192];
        test_read(&vol, 0, &mut failover_read)
            .await
            .expect("online replica should satisfy read");
        assert_eq!(&failover_read[..4096], &replacement);
        assert_eq!(&failover_read[4096..], vec![0x5au8; 4096].as_slice());

        // A single-replica write must retain a non-primary member index;
        // otherwise the legacy BchVal encoding would make reads address
        // device 0 after allocation selected device 1.
        vol.opts.data_replicas = 1;
        let non_primary = vec![0x77u8; 4096];
        test_write(
            &vol,
            BtreeKey::new(8 * 4096, 0, KeyType::Normal),
            &non_primary,
        )
        .await
        .expect("single online non-primary replica should commit");
        let mut non_primary_read = vec![0u8; 4096];
        test_read(&vol, 8 * 4096, &mut non_primary_read)
            .await
            .expect("non-primary extent should remain addressable");
        assert_eq!(non_primary_read, non_primary);
        let entry = vol
            .get_entry_raw(BtreeId::Extents, Bpos::new(0, 8, u32::MAX))
            .expect("non-primary extent entry");
        let value = match entry.value {
            KeyValue::Raw(bytes) => KeyValue::from_bytes(&bytes),
            value => value,
        };
        let KeyValue::Extent(extent) = value else {
            panic!("single non-primary replica must retain an extent device");
        };
        assert_eq!(extent.dev_idx, 1);

        vol.bch2_btree_delete_range(
            BtreeId::Extents,
            Bpos::new(0, 8, u32::MAX),
            Bpos::new(0, 9, u32::MAX),
            0,
        )
            .await
            .expect("trim must release the non-primary extent device");
        assert!(vol
            .get_entry_raw(BtreeId::Extents, Bpos::new(0, 8, u32::MAX))
            .is_none());
    }
}
