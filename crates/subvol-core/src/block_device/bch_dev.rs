//! bcachefs 对齐: `struct bch_dev` — 设备抽象
//!
//! 对应 bcachefs `struct bch_dev`（fs/bcachefs.h:479-603）。
//! 封装块后端 + 设备元数据，是所有 IO 路径的入口点。

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::BlockDevice;
use crate::alloc::AllocGroup;
use crate::storage::superblock::{
    member_bits, BchMemberInitialized, BchMemberState, BchSb, BchSbMember,
};

/// 对应本地 `struct bch_member_cpu` (`fs/sb/members_types.h:5-29`)。
///
/// 这是 `struct bch_dev.mi` 的运行时 CPU 端缓存；bucket 地址换算必须读取这里，
/// 与本地 `fs/alloc/buckets.h:18-39` 一致。
#[derive(Clone, Copy, Debug, Default)]
#[allow(dead_code)]
pub(crate) struct BchMemberCpu {
    pub(crate) nbuckets: u64,
    pub(crate) nbuckets_minus_first: u64,
    pub(crate) first_bucket: u16,
    pub(crate) bucket_size: u16,
    pub(crate) group: u16,
    pub(crate) state: u8,
    pub(crate) discard: u8,
    pub(crate) data_allowed: u8,
    pub(crate) durability: u8,
    pub(crate) freespace_initialized: u8,
    pub(crate) initialized: u8,
    pub(crate) resize_on_mount: u8,
    pub(crate) rotational: u8,
    pub(crate) valid: u8,
    pub(crate) btree_bitmap_shift: u8,
    pub(crate) btree_allocated_bitmap: u64,
}

/// 对应本地 `bch2_mi_to_cpu()` (`fs/sb/members.h:416-439`)。
#[inline]
pub(crate) fn bch2_mi_to_cpu(mi: &BchSbMember) -> BchMemberCpu {
    let durability = ((mi.flags >> member_bits::DURABILITY_SHIFT) & 0x3) as u8;

    BchMemberCpu {
        nbuckets: mi.nbuckets,
        nbuckets_minus_first: mi.nbuckets.wrapping_sub(mi.first_bucket as u64),
        first_bucket: mi.first_bucket,
        bucket_size: mi.bucket_size,
        group: ((mi.flags >> member_bits::GROUP_SHIFT) & 0xff) as u16,
        state: mi.state() as u8,
        discard: ((mi.flags >> member_bits::DISCARD_SHIFT) & 0x1) as u8,
        data_allowed: ((mi.flags >> member_bits::DATA_ALLOWED_SHIFT) & 0x1f) as u8,
        durability: if durability != 0 { durability - 1 } else { 1 },
        freespace_initialized: ((mi.flags >> member_bits::FREESPACE_INITIALIZED_SHIFT) & 0x1) as u8,
        initialized: mi.initialized() as u8,
        resize_on_mount: ((mi.flags >> member_bits::RESIZE_ON_MOUNT_SHIFT) & 0x1) as u8,
        rotational: ((mi.flags >> member_bits::ROTATIONAL_SHIFT) & 0x1) as u8,
        valid: mi.is_alive() as u8,
        btree_bitmap_shift: mi.btree_bitmap_shift,
        btree_allocated_bitmap: mi.btree_allocated_bitmap,
    }
}

/// 对应本地 `struct journal_device` (`fs/journal/types.h:429-462`) 中由
/// superblock journal field 初始化、供 metadata bucket 标记使用的状态。
#[derive(Debug, Default)]
pub(crate) struct JournalDevice {
    pub(crate) bucket_seq: Vec<u64>,
    pub(crate) sectors_free: u32,
    pub(crate) discard_idx: u32,
    pub(crate) dirty_idx_ondisk: u32,
    pub(crate) dirty_idx: u32,
    pub(crate) cur_idx: u32,
    pub(crate) nr: u32,
    pub(crate) buckets: Vec<u64>,
    pub(crate) highest_seq_found: u64,
}

/// bcachefs 对齐: `struct bch_dev`
///
/// bcachefs `struct bch_dev` 主要字段：
/// - `disk_sb.bdev` — 内核 `struct block_device *`（此处为 `backend: Arc<dyn BlockDevice>`）
/// - `dev_idx` — 设备在 superblock 中的索引
/// - `name` — 设备名称
/// - `io_ref[2]` — READ/WRITE IO 引用计数（待添加）
/// - `online` — 设备是否在线（对齐 `bch2_dev_is_online`）
///
/// 函数参数传递风格：所有 IO 函数接收 `&BchDev`，对齐 bcachefs 的 `struct bch_dev *ca`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BchDevIoRefKind {
    Read,
    Write,
}

pub struct BchDev {
    pub backend: Arc<dyn BlockDevice>,
    pub dev_idx: u8,
    pub name: String,
    online: AtomicBool,
    /// Runtime WRITE-ref gate, corresponding to bcachefs stopping the
    /// device's enumerated WRITE refs during `__bch2_dev_read_only()`.
    write_enabled: AtomicBool,
    member_state: std::sync::atomic::AtomicU8,
    initialized: std::sync::atomic::AtomicU8,
    io_read_refs: AtomicU32,
    io_write_refs: AtomicU32,
    /// EWMA read latency, corresponding to bcachefs `cur_latency[READ]`
    /// (`fs/data/write.c:837-863`).  A zero value means no sample yet.
    pub(crate) io_read_latency: AtomicU64,

    /// 本地 `struct bch_dev.mi`：由 superblock member 转换得到的运行时设备几何。
    /// 初始建立与 resize 都受 `BchVol::state_lock`/发布边界保护。
    pub(crate) mi: UnsafeCell<BchMemberCpu>,

    // Buckets: 对应本地 bcachefs `struct bch_dev` 的 per-device runtime。
    // 数组只在 `BchVol::state_lock` 写保护下 resize；正常分配仅锁单个 group。
    pub(crate) groups: UnsafeCell<Vec<Mutex<AllocGroup>>>,
    pub(crate) total_blocks: AtomicU64,
    /// Device-wide free-bucket fast counter, maintained alongside each
    /// allocation group's `free_buckets` counter.
    pub(crate) nr_free_buckets: AtomicU64,
    pub(crate) allocated: AtomicU64,
    pub(crate) freespace_initialized: AtomicBool,
    pub(crate) alloc_cursor: [AtomicU64; 3],
    pub(crate) nr_open_buckets: AtomicU64,
    pub(crate) nr_btree_reserve: AtomicU64,
    /// 对应本地 `ca->alloc_wake_counter`。
    pub(crate) alloc_wake_counter: AtomicU32,

    // Metadata ownership: 对应本地 `bch_dev.disk_sb` 与 `bch_dev.journal`。
    pub(crate) disk_sb: Mutex<BchSb>,
    pub(crate) journal: Mutex<JournalDevice>,
}

// SAFETY: bucket array replacement is serialized by `BchVol::state_lock` and
// happens before the device is exposed to allocation paths. Individual groups
// retain their own mutex for runtime bucket mutation.
unsafe impl Sync for BchDev {}

impl BchDev {
    /// 创建新设备
    pub fn new(backend: Arc<dyn BlockDevice>, dev_idx: u8) -> Self {
        Self {
            backend,
            dev_idx,
            name: String::new(),
            online: AtomicBool::new(true),
            write_enabled: AtomicBool::new(true),
            member_state: std::sync::atomic::AtomicU8::new(BchMemberState::Rw as u8),
            initialized: std::sync::atomic::AtomicU8::new(BchMemberInitialized::Initialized as u8),
            io_read_refs: AtomicU32::new(0),
            io_write_refs: AtomicU32::new(0),
            io_read_latency: AtomicU64::new(0),
            mi: UnsafeCell::new(BchMemberCpu::default()),
            groups: UnsafeCell::new(Vec::new()),
            total_blocks: AtomicU64::new(0),
            nr_free_buckets: AtomicU64::new(0),
            allocated: AtomicU64::new(0),
            freespace_initialized: AtomicBool::new(false),
            alloc_cursor: std::array::from_fn(|_| AtomicU64::new(0)),
            nr_open_buckets: AtomicU64::new(0),
            nr_btree_reserve: AtomicU64::new(0),
            alloc_wake_counter: AtomicU32::new(0),
            disk_sb: Mutex::new(BchSb::new()),
            journal: Mutex::new(JournalDevice::default()),
        }
    }

    /// 设置名称
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// 获取块后端 — 对齐 `bch_dev.disk_sb.bdev`
    pub fn bdev(&self) -> &Arc<dyn BlockDevice> {
        &self.backend
    }

    /// 设备当前是否在线。
    pub fn is_online(&self) -> bool {
        self.online.load(Ordering::Acquire)
    }

    /// 将设备标记为离线。
    pub(crate) fn set_offline(&self) -> bool {
        self.online.swap(false, Ordering::AcqRel)
    }

    /// Enable or stop new WRITE IO refs at the filesystem read-only boundary.
    pub(crate) fn set_write_enabled(&self, enabled: bool) {
        self.write_enabled.store(enabled, Ordering::Release);
    }

    /// 获取运行时 member state。
    pub(crate) fn member_state(&self) -> BchMemberState {
        match self.member_state.load(Ordering::Acquire) {
            x if x == BchMemberState::Rw as u8 => BchMemberState::Rw,
            x if x == BchMemberState::Ro as u8 => BchMemberState::Ro,
            x if x == BchMemberState::Evacuating as u8 => BchMemberState::Evacuating,
            x if x == BchMemberState::Spare as u8 => BchMemberState::Spare,
            _ => BchMemberState::Rw,
        }
    }

    /// 更新运行时 member state。
    pub(crate) fn set_member_state(&self, state: BchMemberState) {
        // SAFETY: callers serialize member state changes with the filesystem
        // state lock, matching the local bcachefs state transition boundary.
        unsafe {
            (*self.mi.get()).state = state as u8;
        }
        self.write_enabled
            .store(state == BchMemberState::Rw, Ordering::Release);
        self.member_state.store(state as u8, Ordering::Release);
    }

    pub(crate) fn initialized(&self) -> BchMemberInitialized {
        match self.initialized.load(Ordering::Acquire) {
            1 => BchMemberInitialized::PreDevUsage,
            2 => BchMemberInitialized::PreMarkSb,
            3 => BchMemberInitialized::PreFreespaceInit,
            4 => BchMemberInitialized::PreJournalAlloc,
            _ => BchMemberInitialized::Initialized,
        }
    }

    pub(crate) fn set_initialized(&self, state: BchMemberInitialized) {
        self.initialized.store(state as u8, Ordering::Release);
    }

    fn io_ref(&self, kind: BchDevIoRefKind) -> &AtomicU32 {
        match kind {
            BchDevIoRefKind::Read => &self.io_read_refs,
            BchDevIoRefKind::Write => &self.io_write_refs,
        }
    }

    /// 尝试获取一个 IO 引用。离线设备会返回 `false`。
    pub fn try_get_io_ref(&self, kind: BchDevIoRefKind) -> bool {
        if !self.is_online() {
            return false;
        }

        // Match local `bch2_dev_get_ioref()` (`fs/sb/members.h:377-390`):
        // take the enumerated IO ref first, then validate the member state.
        // A read-only transition can race this call; checking state before
        // incrementing would allow a write ref to slip through that boundary.
        self.io_ref(kind).fetch_add(1, Ordering::AcqRel);
        if self.is_online()
            && (kind == BchDevIoRefKind::Read
                || (self.write_enabled.load(Ordering::Acquire)
                    && self.member_state() == BchMemberState::Rw))
        {
            true
        } else {
            self.put_io_ref(kind);
            false
        }
    }

    /// 释放一个 IO 引用。
    pub fn put_io_ref(&self, kind: BchDevIoRefKind) {
        let old = self.io_ref(kind).fetch_sub(1, Ordering::AcqRel);
        debug_assert!(old > 0, "io_ref underflow");
    }

    /// 获取一个作用域内自动释放的 IO 引用。
    pub fn try_get_io_ref_guard(
        self: &Arc<Self>,
        kind: BchDevIoRefKind,
    ) -> Option<BchDevIoRefGuard> {
        if self.try_get_io_ref(kind) {
            Some(BchDevIoRefGuard {
                dev: self.clone(),
                kind,
            })
        } else {
            None
        }
    }

    pub fn io_ref_count(&self, kind: BchDevIoRefKind) -> u32 {
        self.io_ref(kind).load(Ordering::Acquire)
    }
}

impl std::fmt::Debug for BchDev {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BchDev")
            .field("dev_idx", &self.dev_idx)
            .field("name", &self.name)
            .field("online", &self.is_online())
            .field("member_state", &self.member_state())
            .field("io_read_refs", &self.io_ref_count(BchDevIoRefKind::Read))
            .field("io_write_refs", &self.io_ref_count(BchDevIoRefKind::Write))
            .field("mi", unsafe { &*self.mi.get() })
            .field("total_blocks", &self.total_blocks.load(Ordering::Relaxed))
            .field(
                "nr_free_buckets",
                &self.nr_free_buckets.load(Ordering::Relaxed),
            )
            .field("allocated", &self.allocated.load(Ordering::Relaxed))
            .finish()
    }
}

/// `bch_dev` IO 引用 RAII 保护，作用域结束时自动 `put`
pub struct BchDevIoRefGuard {
    dev: Arc<BchDev>,
    kind: BchDevIoRefKind,
}

impl std::ops::Deref for BchDevIoRefGuard {
    type Target = BchDev;

    fn deref(&self) -> &Self::Target {
        &self.dev
    }
}

impl Drop for BchDevIoRefGuard {
    fn drop(&mut self) {
        self.dev.put_io_ref(self.kind);
    }
}
