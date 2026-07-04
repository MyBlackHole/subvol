//! BchSb — 块设备超块区（对齐 bcachefs on-disk superblock）
//!
//! 存储在 512-byte sector 8（BlockAddr 1），固定 4096 字节。包含 Volume 元数据及所有子系统状态指针：
//!
//! ```text
//! BlockAddr 1:  BchSb (4KB) — 卷头信息 + 指针
//! BlockAddr 0, 2-7: superblock/layout 保留区域
//! BlockAddr 8+:  数据块（由 BchAllocator 管理）
//! ```
//!
//! # bcachefs 对齐
//!
//! bcachefs 的 superblock 位于设备起始处的固定偏移，
//! 包含 magic/version/backpointers/journal_buckets 等。
//! 本实现使用 BlockAddr 0 作为超块区，不使用文件系统层级。

use serde::{Deserialize, Serialize};

use crate::alloc::quota::types::{BchQuotaType, BchSbQuotaType};
use crate::block_device::{BchDev, BlockDevice};
use crate::btree::gc::GcPos;
use crate::journal::Crc32CHasher;
use crate::types::{BackendType, BlockAddr, StorageError, VolumeId};

/// 超块魔数
pub const SUPERBLOCK_MAGIC: [u8; 8] = *b"SUBVOL\0\0";
pub const BCHFS_MAGIC: [u8; 16] = [
    0xc6, 0x85, 0x73, 0xf6, 0x66, 0xce, 0x90, 0xa9, 0xd9, 0x6a, 0x60, 0xcf, 0x80, 0x3d, 0xf7, 0xef,
];
/// 当前超块格式版本
pub const SUPERBLOCK_VERSION: u32 = 1;
/// 对应本地 `BCH_SB_SECTOR` (`fs/bcachefs_format.h:1128`)。
pub const BCH_SB_SECTOR: u64 = 8;
/// 超块所在 BlockAddr。
pub const SUPERBLOCK_ADDR: u64 = BCH_SB_SECTOR / crate::alloc::SECTORS_PER_BLOCK;
/// 超块大小（固定 4KB，占一个完整 block）
pub const SUPERBLOCK_SIZE: usize = 4096;
/// 保留块数量（BlockAddr 0..RESERVED_BLOCKS 不纳入数据分配器）
pub const RESERVED_BLOCKS: u64 = 8;

/// 超块成员状态 — 对齐 bcachefs `bch_member_state`
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[repr(u8)]
pub enum BchMemberState {
    Rw = 0,
    Ro = 1,
    Evacuating = 2,
    Spare = 3,
}

impl Default for BchMemberState {
    fn default() -> Self {
        Self::Rw
    }
}

/// 设备新增的可恢复初始化阶段，对应本地 `enum bch_member_initialized`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum BchMemberInitialized {
    Initialized = 0,
    PreDevUsage = 1,
    PreMarkSb = 2,
    PreFreespaceInit = 3,
    PreJournalAlloc = 4,
}

/// `struct bch_member` 的 flags 位域
pub mod member_bits {
    pub const STATE_MASK: u64 = 0xF;
    pub const DISCARD_SHIFT: u64 = 14;
    pub const DATA_ALLOWED_SHIFT: u64 = 15;
    pub const GROUP_SHIFT: u64 = 20;
    pub const DURABILITY_SHIFT: u64 = 28;
    pub const FREESPACE_INITIALIZED_SHIFT: u64 = 30;
    pub const RESIZE_ON_MOUNT_SHIFT: u64 = 31;
    pub const ROTATIONAL_SHIFT: u64 = 32;
    pub const ROTATIONAL_SET_SHIFT: u64 = 33;
    pub const INITIALIZED_SHIFT: u64 = 34;
}

pub const MEMBER_DELETED_UUID: [u8; 16] = [
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xd9, 0x6a, 0x60, 0xcf,
];

/// 超块成员条目 — persisted device identity metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BchSbMember {
    pub dev_idx: u8,
    #[serde(default)]
    pub uuid: [u8; 16],
    #[serde(default)]
    /// Device size in buckets, matching local `struct bch_member`.
    pub nbuckets: u64,
    #[serde(default)]
    pub first_bucket: u16,
    #[serde(default)]
    /// Bucket size in 512-byte sectors, matching local `struct bch_member`.
    pub bucket_size: u16,
    #[serde(default)]
    pub btree_bitmap_shift: u8,
    #[serde(default)]
    pub last_mount: u64,
    #[serde(default)]
    pub flags: u64,
    #[serde(default)]
    pub iops: [u32; 4],
    #[serde(default)]
    pub errors: [u64; 3],
    #[serde(default)]
    pub errors_at_reset: [u64; 3],
    #[serde(default)]
    pub errors_reset_time: u64,
    #[serde(default)]
    pub seq: u64,
    #[serde(default)]
    pub btree_allocated_bitmap: u64,
    #[serde(default)]
    pub last_journal_bucket: u32,
    #[serde(default)]
    pub last_journal_bucket_offset: u32,
    #[serde(default)]
    pub device_name: String,
    #[serde(default)]
    pub device_model: String,
    #[serde(default)]
    pub flush_errors: u64,
    #[serde(default)]
    pub device_serial: String,
}

impl BchSbMember {
    pub fn new(dev_idx: u8, name: impl Into<String>) -> Self {
        let mut member = Self {
            dev_idx,
            uuid: [0; 16],
            nbuckets: 0,
            first_bucket: 0,
            bucket_size: 0,
            btree_bitmap_shift: 0,
            last_mount: 0,
            flags: 0,
            iops: [0; 4],
            errors: [0; 3],
            errors_at_reset: [0; 3],
            errors_reset_time: 0,
            seq: 0,
            btree_allocated_bitmap: 0,
            last_journal_bucket: 0,
            last_journal_bucket_offset: 0,
            device_name: name.into(),
            device_model: String::new(),
            flush_errors: 0,
            device_serial: String::new(),
        };
        member.set_state(BchMemberState::Rw);
        // Match the local bcachefs device option default (`fs/opts.h:561`):
        // newly formatted members accept journal, btree, and user data until
        // an explicit data-type restriction is persisted.
        member.flags |= ((1 << crate::alloc::BchDataType::Journal as u8)
            | (1 << crate::alloc::BchDataType::Btree as u8)
            | (1 << crate::alloc::BchDataType::User as u8))
            << member_bits::DATA_ALLOWED_SHIFT;
        member
    }

    pub fn is_alive(&self) -> bool {
        self.uuid != [0; 16] && self.uuid != MEMBER_DELETED_UUID
    }

    pub fn mark_alive(&mut self, uuid: [u8; 16]) {
        self.uuid = uuid;
    }

    pub fn state(&self) -> BchMemberState {
        match self.flags & member_bits::STATE_MASK {
            0 => BchMemberState::Rw,
            1 => BchMemberState::Ro,
            2 => BchMemberState::Evacuating,
            3 => BchMemberState::Spare,
            _ => BchMemberState::Rw,
        }
    }

    pub fn set_state(&mut self, state: BchMemberState) {
        self.flags = (self.flags & !member_bits::STATE_MASK) | state as u64;
    }

    pub fn initialized(&self) -> BchMemberInitialized {
        match (self.flags >> member_bits::INITIALIZED_SHIFT) & 0xf {
            1 => BchMemberInitialized::PreDevUsage,
            2 => BchMemberInitialized::PreMarkSb,
            3 => BchMemberInitialized::PreFreespaceInit,
            4 => BchMemberInitialized::PreJournalAlloc,
            _ => BchMemberInitialized::Initialized,
        }
    }

    pub fn set_initialized(&mut self, state: BchMemberInitialized) {
        const MASK: u64 = 0xf << member_bits::INITIALIZED_SHIFT;
        self.flags = (self.flags & !MASK) | ((state as u64) << member_bits::INITIALIZED_SHIFT);
    }
}

impl Default for BchSbMember {
    fn default() -> Self {
        Self::new(0, "")
    }
}

/// 超块 feature bits（bcachefs 对齐，对应 `BCH_FEATURE_*`）
///
/// 存储在 `BchSb::features[0]` 的 0-63 位。
///
/// # bcachefs 对齐
///
/// 位号 0-21 与 bcachefs `BCH_SB_FEATURES()` 完全一致：
/// - BIT(0)  = BCH_FEATURE_lz4
/// - BIT(1)  = BCH_FEATURE_gzip
/// - ...
/// - BIT(21) = BCH_FEATURE_no_alloc_info
///
/// 位号 22+ 为 subvol 自定义（位于 bcachefs BCH_FEATURE_NR 之上）。
pub mod feature_bits {
    /// BIT(5): journal sequence blacklist 存在于 superblock。
    /// 对应 bcachefs `BCH_FEATURE_journal_seq_blacklist_v3`。
    pub const JOURNAL_SEQ_BLACKLIST_V3: u32 = 5;
    /// BIT(16): journal 允许 noflush（bcachefs `BCH_FEATURE_journal_no_flush`）
    pub const JOURNAL_NO_FLUSH: u32 = 16;
    /// BIT(21): alloc 信息不可用（bcachefs `BCH_FEATURE_no_alloc_info`）
    ///
    /// 语义与 bcachefs 一致——否定式：
    /// - bit = 1 → alloc 信息不存在（旧格式）
    /// - bit = 0 → alloc 信息存在（新格式化）
    pub const NO_ALLOC_INFO: u32 = 21;
    /// BIT(22): 未扩容的小镜像文件，禁止 journal 分配。
    pub const SMALL_IMAGE: u32 = 22;
    /// BIT(23): 快照功能可用（subvol 自定义，非 bcachefs feature bit）
    pub const SNAPSHOTS: u32 = 23;
    /// BIT(24): 配额功能可用（subvol 自定义）
    pub const QUOTAS: u32 = 24;
}

/// bcachefs 对齐: compat 标志位
///
/// 对应 bcachefs `enum bch_sb_compat`（bcachefs_format.h:1395-1400）
pub mod compat_bits {
    /// BIT(0): alloc 信息兼容
    pub const ALLOC_INFO: u32 = 0;
    /// BIT(1): alloc 元数据兼容
    pub const ALLOC_METADATA: u32 = 1;
    /// BIT(4): 无 stale ptr（bcachefs BCH_COMPAT_no_stale_ptrs）
    ///
    /// 当此位被设置时，出现 stale cached ptr 会被视为 fsck 错误，
    /// 触发器会清除此位并写 superblock。
    pub const NO_STALE_PTRS: u32 = 4;
}

/// 超块 — 整个 Volume 的元数据入口
///
/// 以 bincode 序列化后写入 BlockAddr 0，固定 4096 字节。
///
/// # CRC 校验
///
/// `crc != 0` 时启用 CRC32 校验（使用 crc32fast）。
/// 校验方式：将 `crc` 字段清零后序列化，对序列化结果计算 CRC32，
/// 与存储值比对。`crc == 0` 时跳检验证（向后兼容旧版本）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BchSb {
    /// 文件格式魔数
    pub magic: [u8; 8],
    /// 格式版本
    pub version: u32,
    /// CRC32 校验和（0 = 未设置，向后兼容旧版）
    #[serde(default)]
    pub crc: u32,

    // === 扁平 superblock 字段 ===
    // 对应 bcachefs `sb.label[]` (`bcachefs_format.h:1176`)
    pub vol_name: String,
    /// 对应 bcachefs `sb.block_size` (`bcachefs_format.h:1182`)
    pub block_size: u32,
    pub capacity: u64,
    /// subvol 特有字段
    pub vol_id: VolumeId,
    pub pool_name: String,
    pub backend_type: BackendType,
    pub created_at: String,
    pub last_mount_at: Option<String>,

    // ─── WAL journal 状态 ───
    /// 当前 WAL seq
    pub journal_seq: u64,

    // ─── Flags ───
    /// 是否正常关闭
    pub clean_shutdown: bool,

    // ─── Journal 位置（Wave 1 新增，#[serde(default)] 向后兼容） ───
    /// 预分配的 journal bucket addrs（动态长度，不再固定 32）
    #[serde(default)]
    pub journal_buckets: Vec<u64>,
    /// 最近的 journal seq
    #[serde(default)]
    pub journal_last_seq: u64,
    /// 当前 journal bucket 索引
    #[serde(default)]
    pub journal_last_bucket: u32,

    // ─── Btree roots（Wave 3 使用，Wave 1 预占位） ───
    /// 每个 btree type 的 root node block addr（pre-Vec 以兼容 serde）
    #[serde(default)]
    pub root_addrs: Vec<u64>,
    /// 每个 btree type 的 root node level
    #[serde(default)]
    pub root_levels: Vec<u8>,

    // ─── Phase 3: Journal 索引持久化（完整 JournalBchSbState 覆盖） ───
    #[serde(default)]
    pub journal_discard_idx: u32,
    #[serde(default)]
    pub journal_dirty_idx: u32,
    #[serde(default)]
    pub journal_dirty_idx_ondisk: u32,
    #[serde(default)]
    pub journal_bucket_seq: Vec<u64>,
    /// Journal sequence blacklist；对应
    /// `BCH_SB_FIELD_journal_seq_blacklist` 中的区间数组。
    #[serde(default)]
    pub journal_seq_blacklist: Vec<crate::journal::BlacklistEntry>,
    #[serde(default)]
    pub replayed_seqs: Vec<u64>,

    // ─── Phase 3: Recovery passes 持久化 ───
    #[serde(default)]
    pub pass_done: u64,
    /// 必需的 recovery pass 位掩码（对应 bcachefs `bch_sb_field_ext.recovery_passes_required`）
    ///
    /// 位值使用 `BchRecoveryPassStable` ID。当某 pass 被标记为必需时，
    /// 下次 recovery 无论 flags 如何都会被纳入运行集合。
    /// 由 `bch2_reconstruct_alloc()` 等函数设置。
    #[serde(default)]
    pub recovery_passes_required: u64,

    // ─── GC 位置持久化 ───
    /// GC 完成时的位置标记（用于增量 GC 恢复）
    #[serde(default)]
    pub gc_pos: GcPos,
    /// gc_pos 是否有效（旧版本无此字段时 false）
    #[serde(default)]
    pub gc_pos_valid: bool,

    // ─── P2: UUID ───
    /// 卷 UUID（唯一标识，类似于 bcachefs sb.uuid）
    #[serde(default)]
    pub uuid: [u8; 16],
    /// 用户指定 UUID（类似于 bcachefs sb.user_uuid）
    #[serde(default)]
    pub user_uuid: [u8; 16],

    // ─── P2: Feature flags ───
    /// 功能标志位（类似于 bcachefs sb.features，bit 0-63）
    #[serde(default)]
    pub features: [u64; 2],
    /// 兼容标志位（类似于 bcachefs sb.compat）
    #[serde(default)]
    pub compat: [u64; 2],

    // ─── P2: Device membership metadata ───
    /// 主设备索引（用于从设备注册表解析 superblock / journal 入口）
    #[serde(default)]
    pub primary_dev_idx: u8,
    /// 成员设备元数据列表（dev_idx + name + state）
    #[serde(default)]
    pub members: Vec<BchSbMember>,

    // ─── P2: Backup superblock layout ───
    /// 超块副本布局（None = 仅 BlockAddr 0，无副本）
    #[serde(default)]
    pub layout: BackupSbLayout,

    // ─── P6: Quota 配置 ───
    /// 每配额类型的 superblock 配置 [Usr, Grp, Prj]
    #[serde(default)]
    pub sb_quota_type: Vec<BchSbQuotaType>,

    // ─── StorageConfig（Batch D 新增） ───
    /// 存储引擎配置（None = 使用默认值）
    #[serde(default)]
    pub storage_config: Option<crate::config::StorageConfig>,
}

/// 超块副本布局 — 定义主超块和备份副本的位置
///
/// bcachefs 在设备上保留多个 superblock 副本以提高可靠性。
/// `sb_offset` 和 `sb_max_size_bits` 的单位均为 512-byte sector，对应本地
/// `struct bch_sb_layout` (`fs/bcachefs_format.h:1132-1141`)。
///
/// # 默认值
///
/// primary = sector 8，replicas = sectors 32/64（BlockAddr 1/4/8）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupSbLayout {
    pub magic: [u8; 16],
    pub layout_type: u8,
    /// 以 512-byte sectors 表示的最大 superblock 大小的 base-2 对数。
    pub sb_max_size_bits: u8,
    pub nr_superblocks: u8,
    pub pad: [u8; 5],
    /// 有序 superblock sector offsets；仅前 `nr_superblocks` 项有效。
    #[serde(with = "sb_offsets")]
    pub sb_offset: [u64; 61],
}

mod sb_offsets {
    use serde::de::{Error, SeqAccess, Visitor};
    use serde::ser::SerializeTuple;
    use serde::{Deserializer, Serializer};
    use std::fmt;

    pub fn serialize<S>(offsets: &[u64; 61], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut tuple = serializer.serialize_tuple(61)?;
        for offset in offsets {
            tuple.serialize_element(offset)?;
        }
        tuple.end()
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<[u64; 61], D::Error>
    where
        D: Deserializer<'de>,
    {
        struct OffsetsVisitor;

        impl<'de> Visitor<'de> for OffsetsVisitor {
            type Value = [u64; 61];

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("61 superblock sector offsets")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut offsets = [0; 61];
                for (i, offset) in offsets.iter_mut().enumerate() {
                    *offset = seq
                        .next_element()?
                        .ok_or_else(|| A::Error::invalid_length(i, &self))?;
                }
                Ok(offsets)
            }
        }

        deserializer.deserialize_tuple(61, OffsetsVisitor)
    }
}

impl Default for BackupSbLayout {
    fn default() -> Self {
        Self {
            magic: BCHFS_MAGIC,
            layout_type: 0,
            sb_max_size_bits: 3,
            nr_superblocks: 3,
            pad: [0; 5],
            sb_offset: {
                let mut offsets = [0; 61];
                offsets[..3].copy_from_slice(&[BCH_SB_SECTOR, 32, 64]);
                offsets
            },
        }
    }
}

impl BchSb {
    /// 归一化成员元数据。
    ///
    /// 旧格式 superblock 可能没有持久化 `members`；此时为主设备补出单成员
    /// 记录，保持单设备卷可以继续挂载并且后续能进入 registry 解析路径。
    pub fn normalize_members(&mut self) {
        if !self.members.is_empty() {
            return;
        }

        let dev_idx = self.primary_dev_idx;
        let mut member = BchSbMember::new(dev_idx, format!("dev-{dev_idx}"));
        let mut uuid = self.uuid;
        if uuid == [0; 16] {
            uuid = [1; 16];
        }
        member.mark_alive(uuid);
        member.nbuckets = self.capacity / crate::alloc::DEFAULT_BUCKET_SIZE;
        member.first_bucket = 0;
        member.bucket_size =
            (crate::alloc::BLOCKS_PER_BUCKET * crate::alloc::SECTORS_PER_BLOCK) as u16;
        self.members.push(member);
    }

    /// 创建新的超块
    pub fn new() -> Self {
        Self {
            magic: SUPERBLOCK_MAGIC,
            version: SUPERBLOCK_VERSION,
            crc: 0,
            vol_name: String::new(),
            block_size: 4096,
            capacity: 0,
            vol_id: 1,
            pool_name: String::new(),
            backend_type: BackendType::Nfs,
            created_at: String::new(),
            last_mount_at: None,
            journal_seq: 0,
            clean_shutdown: false,
            journal_buckets: Vec::new(),
            journal_last_seq: 0,
            journal_last_bucket: 0,
            root_addrs: Vec::new(),
            root_levels: Vec::new(),
            journal_discard_idx: 0,
            journal_dirty_idx: 0,
            journal_dirty_idx_ondisk: 0,
            journal_bucket_seq: Vec::new(),
            journal_seq_blacklist: Vec::new(),
            replayed_seqs: Vec::new(),
            pass_done: 0,
            recovery_passes_required: 0,
            gc_pos: GcPos {
                phase: crate::btree::gc::GcPhase::NotRunning,
                btree: 0,
                level: 0,
                pos: 0,
                journal_seq: 0,
            },
            gc_pos_valid: false,
            // ─── P2: UUID ───
            uuid: [0u8; 16],
            user_uuid: [0u8; 16],
            // ─── P2: Feature flags ───
            features: [0u64; 2],
            compat: [0u64; 2],
            // ─── P2: Device membership metadata ───
            primary_dev_idx: 0,
            members: Vec::new(),
            // ─── P2: Backup superblock layout ───
            layout: BackupSbLayout::default(),
            // ─── P6: Quota 配置 ───
            sb_quota_type: Vec::new(),
            // ─── StorageConfig ───
            storage_config: None,
        }
    }

    /// 构造新的卷 superblock 头部信息
    pub fn with_volume_info(
        vol_name: String,
        vol_id: VolumeId,
        pool_name: String,
        block_size: u32,
        capacity: u64,
        backend_type: BackendType,
    ) -> Self {
        let created_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs().to_string())
            .unwrap_or_else(|_| "0".to_string());
        let mut member = BchSbMember::new(0, "dev-0");
        member.mark_alive([1; 16]);
        member.nbuckets = capacity / crate::alloc::DEFAULT_BUCKET_SIZE;
        member.first_bucket = 0;
        member.bucket_size =
            (crate::alloc::BLOCKS_PER_BUCKET * crate::alloc::SECTORS_PER_BLOCK) as u16;
        member.flags |= ((1 << crate::alloc::BchDataType::Journal as u8)
            | (1 << crate::alloc::BchDataType::Btree as u8)
            | (1 << crate::alloc::BchDataType::User as u8))
            << member_bits::DATA_ALLOWED_SHIFT;
        member.device_model = "default".to_string();
        Self {
            vol_name,
            block_size,
            capacity,
            vol_id,
            pool_name,
            backend_type,
            created_at,
            last_mount_at: None,
            layout: BackupSbLayout::default(),
            primary_dev_idx: 0,
            members: vec![member],
            ..Self::new()
        }
    }

    pub fn member(&self, dev_idx: u8) -> Option<&BchSbMember> {
        self.members.iter().find(|m| m.dev_idx == dev_idx)
    }

    pub fn member_mut(&mut self, dev_idx: u8) -> Option<&mut BchSbMember> {
        self.members.iter_mut().find(|m| m.dev_idx == dev_idx)
    }

    pub fn member_exists(&self, dev_idx: u8) -> bool {
        self.member(dev_idx).is_some_and(BchSbMember::is_alive)
    }

    pub fn member_state(&self, dev_idx: u8) -> Option<BchMemberState> {
        self.member(dev_idx).map(BchSbMember::state)
    }

    pub fn set_member_state(
        &mut self,
        dev_idx: u8,
        state: BchMemberState,
    ) -> Result<(), StorageError> {
        let member = self
            .member_mut(dev_idx)
            .ok_or_else(|| StorageError::NotFound(format!("member {} not found", dev_idx)))?;
        member.set_state(state);
        Ok(())
    }

    /// 序列化超块到字节（填充到 SUPERBLOCK_SIZE 以便直接写入 block）
    ///
    /// 序列化时计算 CRC32（将 `crc` 字段清零后对整个数据计算 CRC）。
    pub fn serialize(&self) -> Result<Vec<u8>, StorageError> {
        // 第一遍：crc=0 序列化，计算 CRC
        let mut crc_zero = self.clone();
        crc_zero.crc = 0;
        let zeroed_bytes = bincode::serialize(&crc_zero)?;
        if zeroed_bytes.len() > SUPERBLOCK_SIZE {
            return Err(StorageError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "superblock too large: {} > {}",
                    zeroed_bytes.len(),
                    SUPERBLOCK_SIZE
                ),
            )));
        }
        let crc = Crc32CHasher::hash(&zeroed_bytes);

        // 第二遍：填入 CRC 后序列化
        let mut sb_with_crc = self.clone();
        sb_with_crc.crc = crc;
        let mut data = bincode::serialize(&sb_with_crc)?;
        debug_assert_eq!(
            data.len(),
            zeroed_bytes.len(),
            "CRC field must not change serialized size"
        );
        debug_assert!(crc != 0, "CRC32 of non-empty superblock should never be 0");

        // 填充到固定大小
        data.resize(SUPERBLOCK_SIZE, 0);
        Ok(data)
    }

    /// 从字节反序列化超块
    ///
    /// 如果 `crc != 0` 则验证 CRC32 校验和（crc==0 向后兼容旧版本）。
    pub fn deserialize(data: &[u8]) -> Result<Self, StorageError> {
        if data.len() < 16 {
            return Err(StorageError::NotFound("superblock data too short".into()));
        }
        let sb: BchSb = bincode::deserialize(data)?;
        if sb.magic != SUPERBLOCK_MAGIC {
            return Err(StorageError::NotFound(format!(
                "invalid superblock magic: {:?}",
                &sb.magic
            )));
        }
        if sb.version != SUPERBLOCK_VERSION && sb.version != 1 {
            return Err(StorageError::NotFound(format!(
                "unsupported superblock version {}, expected {} or 1",
                sb.version, SUPERBLOCK_VERSION
            )));
        }
        if sb.layout.magic != BCHFS_MAGIC {
            return Err(StorageError::InvalidData(
                "not a bcachefs superblock layout".into(),
            ));
        }
        if sb.layout.layout_type != 0 {
            return Err(StorageError::InvalidData(format!(
                "invalid superblock layout type {}",
                sb.layout.layout_type
            )));
        }
        if sb.layout.nr_superblocks == 0 || sb.layout.nr_superblocks > 61 {
            return Err(StorageError::InvalidData(format!(
                "invalid superblock count {}",
                sb.layout.nr_superblocks
            )));
        }
        if sb.layout.sb_max_size_bits > 16 {
            return Err(StorageError::InvalidData(format!(
                "invalid superblock max size bits {}",
                sb.layout.sb_max_size_bits
            )));
        }
        let max_sectors = 1u64 << sb.layout.sb_max_size_bits;
        let offsets = &sb.layout.sb_offset[..sb.layout.nr_superblocks as usize];
        let mut previous = offsets[0];
        for &offset in &offsets[1..] {
            if offset < previous + max_sectors {
                return Err(StorageError::InvalidData(format!(
                    "overlapping superblocks ending at {} and starting at {}",
                    previous + max_sectors,
                    offset
                )));
            }
            previous = offset;
        }
        // CRC 校验：crc != 0 时验证，=0 时跳过（旧版兼容）
        if sb.crc != 0 {
            let mut check = sb.clone();
            check.crc = 0;
            let zeroed_bytes = bincode::serialize(&check)?;
            let computed = Crc32CHasher::hash(&zeroed_bytes);
            if computed != sb.crc {
                return Err(StorageError::ChecksumMismatch {
                    expected: sb.crc,
                    actual: computed,
                });
            }
        }
        let mut sb = sb;
        sb.normalize_members();
        Ok(sb)
    }

    /// 生成随机 UUID（填充 uuid 和 user_uuid 字段）
    ///
    /// 使用 `rand::thread_rng` 生成 16 字节随机值。
    pub fn generate_uuids(&mut self) {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        rng.fill(&mut self.uuid);
        rng.fill(&mut self.user_uuid);
    }

    // ─── Feature flag helpers ───

    /// 检查指定 feature bit 是否已设置
    ///
    /// bit 范围：0..127（features[0] 覆盖 0-63, features[1] 覆盖 64-127）。
    /// 超出范围的 bit 返回 false。
    pub fn feature_test(&self, bit: u32) -> bool {
        if bit < 64 {
            (self.features[0] & (1u64 << bit)) != 0
        } else if bit < 128 {
            (self.features[1] & (1u64 << (bit - 64))) != 0
        } else {
            false
        }
    }

    /// 设置指定 feature bit
    pub fn feature_set(&mut self, bit: u32) {
        if bit < 64 {
            self.features[0] |= 1u64 << bit;
        } else if bit < 128 {
            self.features[1] |= 1u64 << (bit - 64);
        }
    }

    /// 检查指定 compat bit 是否已设置
    pub fn compat_test(&self, bit: u32) -> bool {
        if bit < 64 {
            (self.compat[0] & (1u64 << bit)) != 0
        } else if bit < 128 {
            (self.compat[1] & (1u64 << (bit - 64))) != 0
        } else {
            false
        }
    }

    /// 设置指定 compat bit
    pub fn compat_set(&mut self, bit: u32) {
        if bit < 64 {
            self.compat[0] |= 1u64 << bit;
        } else if bit < 128 {
            self.compat[1] |= 1u64 << (bit - 64);
        }
    }

    /// 清除指定 compat bit
    pub fn compat_clear(&mut self, bit: u32) {
        if bit < 64 {
            self.compat[0] &= !(1u64 << bit);
        } else if bit < 128 {
            self.compat[1] &= !(1u64 << (bit - 64));
        }
    }

    /// 返回此超块需要写入的所有 BlockAddr 列表
    ///
    /// layout offsets 使用 512-byte sectors，BlockDevice 使用 4K BlockAddr。
    fn target_addrs(&self) -> Vec<u64> {
        self.layout.sb_offset[..self.layout.nr_superblocks as usize]
            .iter()
            .map(|offset| offset / crate::alloc::SECTORS_PER_BLOCK)
            .collect()
    }

    /// 将超块写入后端（所有副本）。
    ///
    /// 对应本地 bcachefs `__bch2_write_super()`（`fs/sb/io.c:1390-1430`）：
    /// 每个副本都必须尝试，即使某个 slot 写失败也不能提前退出，否则
    /// 后续 backup 会被跳过。返回按布局顺序遇到的第一个错误。
    async fn write_to_backend(&self, backend: &dyn BlockDevice) -> Result<(), StorageError> {
        let data = self.serialize()?;
        let addrs = self.target_addrs();
        let mut first_err = None;
        for addr in &addrs {
            if let Err(err) = backend.write_block(BlockAddr::new(*addr), &data).await {
                if first_err.is_none() {
                    first_err = Some(err);
                }
            }
        }
        first_err.map_or(Ok(()), Err)
    }

    /// 将超块写入指定设备。
    pub async fn write_to_device(&self, dev: &BchDev) -> Result<(), StorageError> {
        self.write_to_backend(dev.bdev().as_ref()).await
    }

    /// 从后端读取超块并选择最新有效副本。
    ///
    /// 对应本地 bcachefs `read_backup_supers()`（`fs/sb/io.c:917-967`）：
    /// 所有布局副本都必须读取，不能在 primary 或第一个有效 backup 处
    /// 提前返回；主副本可能是旧的但校验仍然有效，必须按持久化序列选择
    /// 最新状态。当前 subvol 格式没有单独的顶层 sb seq，使用 member
    /// seq 作为主序列，并以 journal_last_seq/journal_seq 作为旧格式回退。
    async fn read_from_backend(backend: &dyn BlockDevice) -> Result<Self, StorageError> {
        let mut last_err = StorageError::NotFound("no superblock found".into());
        let mut valid = Vec::new();

        // Keep the local layout order: primary first, then the two fixed
        // compatibility backup positions used by the current format.
        for addr in [SUPERBLOCK_ADDR, 4u64, 8u64] {
            match Self::read_from_addr(backend, addr).await {
                Ok(sb) => valid.push(sb),
                Err(e) => last_err = e,
            }
        }

        valid
            .into_iter()
            .max_by_key(|sb| {
                let member_seq = sb
                    .members
                    .iter()
                    .find(|member| member.dev_idx == sb.primary_dev_idx)
                    .map_or(0, |member| member.seq);
                (member_seq, sb.journal_last_seq, sb.journal_seq)
            })
            .ok_or(last_err)
    }

    /// 从指定设备读取超块。
    pub async fn read_from_device(dev: &BchDev) -> Result<Self, StorageError> {
        Self::read_from_backend(dev.bdev().as_ref()).await
    }

    /// 从后端读取超块（显式回退模式 — 跳过 primary，直接尝试副本）
    ///
    /// 当 primary 已知损坏且希望强制使用备份副本时使用。
    /// 按顺序尝试每个副本，返回第一个有效的超块。
    async fn read_from_backend_with_fallback(
        backend: &dyn BlockDevice,
    ) -> Result<Self, StorageError> {
        let mut last_err = StorageError::NotFound("no superblock found".into());

        // 尝试默认副本位置：BlockAddr 4, 8
        for addr in &[4u64, 8u64] {
            match Self::read_from_addr(backend, *addr).await {
                Ok(sb) => return Ok(sb),
                Err(e) => last_err = e,
            }
        }

        Err(last_err)
    }

    /// 从指定 BlockAddr 读取并反序列化超块
    async fn read_from_addr(backend: &dyn BlockDevice, addr: u64) -> Result<Self, StorageError> {
        let mut buf = vec![0u8; SUPERBLOCK_SIZE];
        backend.read_block(BlockAddr::new(addr), &mut buf).await?;
        Self::deserialize(&buf)
    }

    /// 检查设备是否缺少 alloc 信息
    ///
    /// 对应 bcachefs `c->sb.features & BIT_ULL(BCH_FEATURE_no_alloc_info)`。
    /// 当 `has_no_alloc_info()` 返回 true 时，PASS_ALLOC 标志的 pass 会被跳过。
    ///
    /// 语义与 bcachefs 一致——否定式：
    /// - bit=1 (NO_ALLOC_INFO set) → alloc 信息不存在
    /// - bit=0 (NO_ALLOC_INFO clear) → alloc 信息存在
    pub fn has_no_alloc_info(&self) -> bool {
        self.feature_test(feature_bits::NO_ALLOC_INFO)
    }

    // ─── Quota 配置 ───

    /// 获取指定配额类型的 superblock 配置
    ///
    /// 返回 `None` 如果该类型的配置尚未初始化（`sb_quota_type` 为空或索引越界）。
    pub fn quota_config(&self, qtype: BchQuotaType) -> Option<&BchSbQuotaType> {
        self.sb_quota_type.get(qtype as usize)
    }

    /// 获取指定配额类型的 superblock 配置（可变引用）
    ///
    /// 如果 `sb_quota_type` 长度不足，自动扩展到 `BchQuotaType::NR` 并用默认值填充。
    pub fn quota_config_mut(&mut self, qtype: BchQuotaType) -> &mut BchSbQuotaType {
        let idx = qtype as usize;
        if self.sb_quota_type.len() <= idx {
            self.sb_quota_type
                .resize(BchQuotaType::NR, BchSbQuotaType::default());
        }
        &mut self.sb_quota_type[idx]
    }

    /// 确保配额配置数组已初始化（长度为 `BchQuotaType::NR`）
    ///
    /// 在 volume 创建时调用，设置 QUOTAS feature bit 并填充默认配置。
    pub fn init_quota_config(&mut self) {
        if self.sb_quota_type.len() < BchQuotaType::NR {
            self.sb_quota_type
                .resize(BchQuotaType::NR, BchSbQuotaType::default());
        }
        self.feature_set(feature_bits::QUOTAS);
    }

    /// 检查配额功能是否已启用
    pub fn has_quotas(&self) -> bool {
        self.feature_test(feature_bits::QUOTAS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::BackendType;
    use std::sync::Arc;

    fn test_sb() -> BchSb {
        BchSb::with_volume_info(
            "test-vol".into(),
            1,
            "pool".into(),
            4096,
            1024 * 1024 * 1024,
            BackendType::Nfs,
        )
    }

    #[test]
    fn test_superblock_roundtrip() {
        let sb = test_sb();
        assert_eq!(sb.members[0].nbuckets, 1024);
        assert_eq!(sb.members[0].bucket_size, 2048);
        assert_eq!(
            (sb.members[0].flags >> member_bits::DATA_ALLOWED_SHIFT) & 0x1f,
            (1 << crate::alloc::BchDataType::Journal as u8)
                | (1 << crate::alloc::BchDataType::Btree as u8)
                | (1 << crate::alloc::BchDataType::User as u8)
        );
        let data = sb.serialize().unwrap();
        assert_eq!(data.len(), SUPERBLOCK_SIZE);

        let restored = BchSb::deserialize(&data).unwrap();
        assert_eq!(restored.vol_name, sb.vol_name);
        assert_eq!(restored.version, SUPERBLOCK_VERSION);
        assert_eq!(restored.magic, SUPERBLOCK_MAGIC);
        assert!(!restored.clean_shutdown);
        assert_eq!(restored.layout.layout_type, 0);
        assert_eq!(restored.layout.sb_max_size_bits, 3);
        assert_eq!(restored.layout.nr_superblocks, 3);
        assert_eq!(&restored.layout.sb_offset[..3], &[8, 32, 64]);
    }

    #[test]
    fn test_superblock_preserves_all_fields() {
        let mut sb = test_sb();
        sb.journal_seq = 42;
        sb.clean_shutdown = true;
        sb.primary_dev_idx = 0;
        let mut member = BchSbMember::new(0, "dev-0");
        member.mark_alive([2; 16]);
        member.nbuckets = 1024;
        member.bucket_size = 2048;
        member.set_state(BchMemberState::Ro);
        sb.members = vec![member];

        let data = sb.serialize().unwrap();
        let restored = BchSb::deserialize(&data).unwrap();
        assert_eq!(restored.journal_seq, 42);
        assert!(restored.clean_shutdown);
        assert_eq!(restored.primary_dev_idx, 0);
        assert_eq!(restored.members.len(), 1);
        assert_eq!(restored.members[0].dev_idx, 0);
        assert_eq!(restored.members[0].device_name, "dev-0");
        assert_eq!(restored.members[0].state(), BchMemberState::Ro);
        assert!(restored.members[0].is_alive());
    }

    #[test]
    fn test_superblock_invalid_magic() {
        let mut data = vec![0u8; SUPERBLOCK_SIZE];
        data[..8].copy_from_slice(b"BADMAGIC");
        let result = BchSb::deserialize(&data);
        assert!(result.is_err());
    }

    #[test]
    fn test_superblock_too_short() {
        let result = BchSb::deserialize(&[0u8; 8]);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_superblock_write_read_backend() {
        let backend = crate::block_device::MockBlockDevice::new();
        let dev = BchDev::new(Arc::new(backend.clone()), 0);
        let sb = test_sb();

        sb.write_to_device(&dev).await.unwrap();

        let restored = BchSb::read_from_device(&dev).await.unwrap();
        assert_eq!(restored.vol_name, sb.vol_name);
    }

    #[tokio::test]
    async fn test_superblock_read_chooses_newest_valid_backup() {
        let backend = crate::block_device::MockBlockDevice::new();
        let dev = BchDev::new(Arc::new(backend.clone()), 0);

        let mut old = test_sb();
        old.journal_last_seq = 7;
        old.members[0].seq = 7;
        let mut newest = old.clone();
        newest.journal_last_seq = 9;
        newest.members[0].seq = 9;
        newest.clean_shutdown = true;

        backend
            .write_block(BlockAddr::new(SUPERBLOCK_ADDR), &old.serialize().unwrap())
            .await
            .unwrap();
        backend
            .write_block(BlockAddr::new(4), &newest.serialize().unwrap())
            .await
            .unwrap();
        backend
            .write_block(BlockAddr::new(8), &old.serialize().unwrap())
            .await
            .unwrap();

        let restored = BchSb::read_from_device(&dev).await.unwrap();
        assert_eq!(restored.journal_last_seq, 9);
        assert!(restored.clean_shutdown);
    }

    #[test]
    fn test_superblock_quota_config_default() {
        let sb = test_sb();
        assert!(!sb.has_quotas());
        assert!(sb.quota_config(BchQuotaType::Usr).is_none());
        assert!(sb.quota_config(BchQuotaType::Grp).is_none());
        assert!(sb.quota_config(BchQuotaType::Prj).is_none());
    }

    #[test]
    fn test_superblock_init_quota_config() {
        let mut sb = test_sb();
        sb.init_quota_config();
        assert!(sb.has_quotas());

        for qtype in &[BchQuotaType::Usr, BchQuotaType::Grp, BchQuotaType::Prj] {
            let cfg = sb.quota_config(*qtype).unwrap();
            assert_eq!(cfg.flags, 0);
            assert_eq!(cfg.c.len(), 2);
            assert_eq!(cfg.c[0].timelimit, 86400);
            assert_eq!(cfg.c[0].warnlimit, 0);
            assert_eq!(cfg.c[1].timelimit, 86400);
            assert_eq!(cfg.c[1].warnlimit, 0);
        }
    }

    #[test]
    fn test_superblock_quota_config_mut() {
        let mut sb = test_sb();
        let cfg = sb.quota_config_mut(BchQuotaType::Prj);
        cfg.flags = 42;
        cfg.c[0].timelimit = 3600;
        cfg.c[1].warnlimit = 3;

        let cfg = sb.quota_config(BchQuotaType::Prj).unwrap();
        assert_eq!(cfg.flags, 42);
        assert_eq!(cfg.c[0].timelimit, 3600);
        assert_eq!(cfg.c[1].warnlimit, 3);

        // 其他类型仍为默认
        let cfg_usr = sb.quota_config(BchQuotaType::Usr).unwrap();
        assert_eq!(cfg_usr.flags, 0);
    }

    #[test]
    fn test_superblock_quota_persist() {
        let mut sb = test_sb();
        sb.init_quota_config();

        // 修改 Prj 配额配置
        {
            let cfg = sb.quota_config_mut(BchQuotaType::Prj);
            cfg.flags = 7;
            cfg.c[0].timelimit = 7200;
        }

        // roundtrip
        let data = sb.serialize().unwrap();
        let restored = BchSb::deserialize(&data).unwrap();

        assert!(restored.has_quotas());
        let cfg = restored.quota_config(BchQuotaType::Prj).unwrap();
        assert_eq!(cfg.flags, 7);
        assert_eq!(cfg.c[0].timelimit, 7200);

        // Usr 仍为默认
        let cfg_usr = restored.quota_config(BchQuotaType::Usr).unwrap();
        assert_eq!(cfg_usr.flags, 0);
    }

    #[tokio::test]
    async fn test_superblock_writes_backup_replicas_for_new_volume() {
        let backend = crate::block_device::MockBlockDevice::new();
        let dev = BchDev::new(Arc::new(backend.clone()), 0);
        let sb = test_sb();

        sb.write_to_device(&dev).await.unwrap();

        let mut buf = vec![0u8; SUPERBLOCK_SIZE];
        backend
            .read_block(BlockAddr::new(SUPERBLOCK_ADDR), &mut buf)
            .await
            .unwrap();
        let primary = BchSb::deserialize(&buf).unwrap();
        assert_eq!(primary.vol_name, sb.vol_name);

        backend
            .read_block(BlockAddr::new(4), &mut buf)
            .await
            .unwrap();
        let replica1 = BchSb::deserialize(&buf).unwrap();
        assert_eq!(replica1.vol_name, sb.vol_name);

        backend
            .read_block(BlockAddr::new(8), &mut buf)
            .await
            .unwrap();
        let replica2 = BchSb::deserialize(&buf).unwrap();
        assert_eq!(replica2.vol_name, sb.vol_name);
    }

    #[tokio::test]
    async fn test_superblock_write_continues_after_one_backup_failure() {
        let backend = crate::block_device::MockBlockDevice::new();
        backend.set_write_error_addr(Some(BlockAddr::new(4)));
        let dev = BchDev::new(Arc::new(backend.clone()), 0);
        let sb = test_sb();

        assert!(sb.write_to_device(&dev).await.is_err());

        let mut buf = vec![0u8; SUPERBLOCK_SIZE];
        backend
            .read_block(BlockAddr::new(8), &mut buf)
            .await
            .unwrap();
        assert_eq!(BchSb::deserialize(&buf).unwrap().vol_name, sb.vol_name);
    }

    #[test]
    fn test_superblock_member_roundtrip() {
        let mut sb = test_sb();
        sb.primary_dev_idx = 2;
        let mut member0 = BchSbMember::new(2, "data-2");
        member0.mark_alive([3; 16]);
        member0.nbuckets = 2048;
        member0.bucket_size = 2048;
        let mut member1 = BchSbMember::new(5, "journal-5");
        member1.mark_alive([4; 16]);
        member1.set_state(BchMemberState::Ro);
        sb.members = vec![member0, member1];

        let data = sb.serialize().unwrap();
        let restored = BchSb::deserialize(&data).unwrap();

        assert_eq!(restored.primary_dev_idx, 2);
        assert_eq!(restored.members.len(), 2);
        assert_eq!(restored.members[0].dev_idx, 2);
        assert_eq!(restored.members[0].device_name, "data-2");
        assert_eq!(restored.members[1].dev_idx, 5);
        assert_eq!(restored.members[1].state(), BchMemberState::Ro);
        assert!(restored.members[0].is_alive());
        assert!(restored.members[1].is_alive());
    }

    #[test]
    fn test_superblock_member_state_update() {
        let mut sb = test_sb();
        assert_eq!(sb.member_state(0), Some(BchMemberState::Rw));

        sb.set_member_state(0, BchMemberState::Evacuating).unwrap();
        assert_eq!(sb.member_state(0), Some(BchMemberState::Evacuating));

        let data = sb.serialize().unwrap();
        let restored = BchSb::deserialize(&data).unwrap();
        assert_eq!(restored.member_state(0), Some(BchMemberState::Evacuating));
    }

    #[test]
    fn test_superblock_normalizes_members() {
        let mut sb = BchSb::new();
        sb.vol_name = "legacy".into();
        sb.block_size = 4096;
        sb.capacity = 4096 * 16;
        sb.primary_dev_idx = 3;

        let data = sb.serialize().unwrap();
        let restored = BchSb::deserialize(&data).unwrap();

        assert_eq!(restored.primary_dev_idx, 3);
        assert_eq!(restored.members.len(), 1);
        assert_eq!(restored.members[0].dev_idx, 3);
        assert_eq!(restored.members[0].device_name, "dev-3");
        assert!(restored.members[0].is_alive());
        assert_eq!(restored.members[0].nbuckets, 0);
        assert_eq!(restored.members[0].bucket_size, 2048);
    }
}
