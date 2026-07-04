//! Recovery Passes — bcachefs 对齐的崩溃恢复框架
//!
//! 对应 bcachefs `fs/init/passes.c` 和 `fs/init/passes_format.h`。
//!
//! # 对齐设计
//!
//! - `BchRecoveryPass` 枚举（对应 `enum bch_recovery_pass`）+ `RecoveryPassFlags`（PASS_ALWAYS / PASS_UNCLEAN / PASS_SILENT）
//! - `RecoveryPass`（对应 `struct recovery_pass`）
//! - `RecoveryState`（对应 `struct bch_fs_recovery`）：含 `passes_complete`（位掩码）、`pass_done`（标量 max）
//! - `bch2_run_recovery_passes()`：用 `trailing_zeros()`（对应 `__ffs64`）迭代 pass 位掩码
//!
//! # Pass 依赖关系
//!
//! ```text
//! JournalRead ──→ BtreeRoots ──→ AllocRead ──→ SetMayGoRw ──→ JournalReplay
//!      │               │                           │
//!      └── 读所有       └── 从 journal 提取          └── journal replay
//!      journal          root 信息 + 加载
//!      entries
//! ```

pub mod passes;

use std::ops::BitOr;

use crate::btree::gc::BtreeGc;

use crate::btree::{BtreeId, BtreeTrans};
use crate::journal::Journal;
use crate::journal::Jset;
use crate::storage::superblock::BchSb;
use crate::subvol::bch2_initialize_subvolumes;
use crate::types::StorageError;
use crate::BchVol;

// ---------------------------------------------------------------------------
// Pass system types — bcachefs 对齐
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// 稳定 Pass ID — bcachefs 对齐（superblock 持久化用）
// ---------------------------------------------------------------------------

/// bcachefs 对齐的稳定 pass ID（superblock 持久化用）
///
/// 对应 bcachefs `enum bch_recovery_pass_stable`。
/// 值 0-48 与 bcachefs 完全一致。
///
/// 此枚举**不应添加或删除**任何变体——仅当 bcachefs 上游添加新 pass 时追加。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BchRecoveryPassStable {
    AllocRead = 0,
    StripesRead = 1,
    InitializeSubvolumes = 2,
    SnapshotsRead = 3,
    CheckTopology = 4,
    CheckAllocations = 5,
    TransMarkDevSbs = 6,
    FsJournalAlloc = 7,
    SetMayGoRw = 8,
    JournalReplay = 9,
    CheckAllocInfo = 10,
    CheckLrus = 11,
    CheckBtreeBackpointers = 12,
    CheckBackpointersToExtents = 13,
    CheckExtentsToBackpointers = 14,
    CheckAllocToLruRefs = 15,
    FsFreespaceInit = 16,
    BucketGensInit = 17,
    CheckSnapshotTrees = 18,
    CheckSnapshots = 19,
    CheckSubvols = 20,
    DeleteDeadSnapshots = 21,
    FsUpgradeForSubvolumes = 22,
    ResumeLoggedOps = 23,
    CheckInodes = 24,
    CheckExtents = 25,
    CheckIndirectExtents = 26,
    CheckDirents = 27,
    CheckXattrs = 28,
    CheckRoot = 29,
    CheckDirectoryStructure = 30,
    CheckNlinks = 31,
    DeleteDeadInodes = 32,
    FixReflinkP = 33,
    SetFsNeedsReconcile = 34,
    CheckSubvolChildren = 35,
    CheckSubvolumeStructure = 36,
    ScanForBtreeNodes = 37,
    ReconstructSnapshots = 38,
    AccountingRead = 39,
    CheckUnreachableInodes = 40,
    RecoveryPassEmpty = 41,
    LookupRootInode = 42,
    CheckReconcileWork = 43,
    DeleteDeadInteriorSnapshots = 44,
    MergeBtreeNodes = 45,
    BtreeBitmapGc = 46,
    KillIGenerationKeys = 47,
    PresplitShardBoundaries = 48,
}

// ---------------------------------------------------------------------------
// 运行时 Pass 枚举 — bcachefs enum 顺序
// ---------------------------------------------------------------------------

/// bcachefs 对齐的 recovery pass 枚举（位值 = 数组索引）
///
/// 顺序严格匹配 bcachefs `enum bch_recovery_pass` 展开顺序，
/// **不是** `BchRecoveryPassStable` 的顺序，也不是便利顺序。
///
/// | idx | pass | bcachefs enum idx | stable ID |
/// |-----|------|------------------|-----------|
/// | 0 | check_topology | 2 | 4 |
/// | 1 | AccountingRead | 3 | 39 |
/// | 2 | AllocRead | 4 | 0 |
/// | 3 | SnapshotsRead | 7 | 3 |
/// | 4 | check_allocations | 8 | 5 |
/// | 5 | TransMarkDevSbs | 9 | 6 |
/// | 6 | FsJournalAlloc | 10 | 7 |
/// | 7 | SetMayGoRw | 11 | 8 |
/// | 8 | JournalReplay | 12 | 9 |
/// | 9 | PresplitShardBoundaries | 14 | 48 |
/// | 10 | CheckAllocInfo | 10 | 10 |
/// | 11 | fs_freespace_init | 21 | 16 |
/// | 12 | bucket_gens_init | 17 | 17 |
/// | 13 | check_snapshots | 26 | 19 |
/// | 14 | LookupRootInode | 48 | 42 |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BchRecoveryPass {
    CheckTopology = 0,
    AccountingRead = 1,
    AllocRead = 2,
    SnapshotsRead = 3,
    CheckAllocations = 4,
    TransMarkDevSbs = 5,
    FsJournalAlloc = 6,
    SetMayGoRw = 7,
    JournalReplay = 8,
    PresplitShardBoundaries = 9,
    CheckAllocInfo = 10,
    FsFreespaceInit = 11,
    BucketGensInit = 12,
    CheckSnapshots = 13,
    LookupRootInode = 14,
}

/// 每个 pass 对应的位掩码常量（15 pass，indices 0-14）
pub const RECOVERY_PASS_BITS: [u64; 15] = [
    1 << 0,
    1 << 1,
    1 << 2,
    1 << 3,
    1 << 4,
    1 << 5,
    1 << 6,
    1 << 7,
    1 << 8,
    1 << 9,
    1 << 10,
    1 << 11,
    1 << 12,
    1 << 13,
    1 << 14,
];

// ---------------------------------------------------------------------------
// 运行时 ↔ 稳定 ID 映射（P0-1: stable identity）
// ---------------------------------------------------------------------------

/// 将运行时 pass 映射到稳定 ID（superblock 持久化用）
///
/// 对应 bcachefs `bch2_recovery_pass_to_stable()`。
pub(crate) fn bch2_recovery_pass_to_stable(pass: BchRecoveryPass) -> BchRecoveryPassStable {
    match pass {
        BchRecoveryPass::CheckTopology => BchRecoveryPassStable::CheckTopology,
        BchRecoveryPass::AccountingRead => BchRecoveryPassStable::AccountingRead,
        BchRecoveryPass::AllocRead => BchRecoveryPassStable::AllocRead,
        BchRecoveryPass::SnapshotsRead => BchRecoveryPassStable::SnapshotsRead,
        BchRecoveryPass::CheckAllocations => BchRecoveryPassStable::CheckAllocations,
        BchRecoveryPass::TransMarkDevSbs => BchRecoveryPassStable::TransMarkDevSbs,
        BchRecoveryPass::FsJournalAlloc => BchRecoveryPassStable::FsJournalAlloc,
        BchRecoveryPass::SetMayGoRw => BchRecoveryPassStable::SetMayGoRw,
        BchRecoveryPass::JournalReplay => BchRecoveryPassStable::JournalReplay,
        BchRecoveryPass::PresplitShardBoundaries => BchRecoveryPassStable::PresplitShardBoundaries,
        BchRecoveryPass::CheckAllocInfo => BchRecoveryPassStable::CheckAllocInfo,
        BchRecoveryPass::FsFreespaceInit => BchRecoveryPassStable::FsFreespaceInit,
        BchRecoveryPass::BucketGensInit => BchRecoveryPassStable::BucketGensInit,
        BchRecoveryPass::CheckSnapshots => BchRecoveryPassStable::CheckSnapshots,
        BchRecoveryPass::LookupRootInode => BchRecoveryPassStable::LookupRootInode,
    }
}

/// 将稳定 ID 映射到运行时 pass
///
/// 对应 bcachefs `bch2_recovery_pass_from_stable()`。
/// 返回 `None` 表示此稳定 ID 不对应任何已注册的运行时 pass。
pub(crate) fn bch2_recovery_pass_from_stable(
    stable: BchRecoveryPassStable,
) -> Option<BchRecoveryPass> {
    Some(match stable {
        BchRecoveryPassStable::CheckTopology => BchRecoveryPass::CheckTopology,
        BchRecoveryPassStable::AccountingRead => BchRecoveryPass::AccountingRead,
        BchRecoveryPassStable::AllocRead => BchRecoveryPass::AllocRead,
        BchRecoveryPassStable::SnapshotsRead => BchRecoveryPass::SnapshotsRead,
        BchRecoveryPassStable::CheckAllocations => BchRecoveryPass::CheckAllocations,
        BchRecoveryPassStable::TransMarkDevSbs => BchRecoveryPass::TransMarkDevSbs,
        BchRecoveryPassStable::FsJournalAlloc => BchRecoveryPass::FsJournalAlloc,
        BchRecoveryPassStable::SetMayGoRw => BchRecoveryPass::SetMayGoRw,
        BchRecoveryPassStable::JournalReplay => BchRecoveryPass::JournalReplay,
        BchRecoveryPassStable::PresplitShardBoundaries => BchRecoveryPass::PresplitShardBoundaries,
        BchRecoveryPassStable::CheckAllocInfo => BchRecoveryPass::CheckAllocInfo,
        BchRecoveryPassStable::FsFreespaceInit => BchRecoveryPass::FsFreespaceInit,
        BchRecoveryPassStable::BucketGensInit => BchRecoveryPass::BucketGensInit,
        BchRecoveryPassStable::CheckSnapshots => BchRecoveryPass::CheckSnapshots,
        BchRecoveryPassStable::LookupRootInode => BchRecoveryPass::LookupRootInode,
        // 所有其他 stable ID → 未注册的 bcachefs pass，subvol 不支持
        _ => return None,
    })
}

// 手动 flags 而非 bitflags crate（避免新增依赖）
// 对应 bcachefs `PASS_ALWAYS`, `PASS_UNCLEAN`, `PASS_SILENT`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryPassFlags(u64);

impl RecoveryPassFlags {
    pub const ALWAYS: Self = RecoveryPassFlags(1 << 0);
    pub const UNCLEAN: Self = RecoveryPassFlags(1 << 1);
    pub const SILENT: Self = RecoveryPassFlags(1 << 2);
    /// fsck 模式下运行（对应 bcachefs PASS_FSCK）
    pub const FSCK: Self = RecoveryPassFlags(1 << 3);
    /// 可在在线模式下运行（对应 bcachefs PASS_ONLINE）
    pub const ONLINE: Self = RecoveryPassFlags(1 << 4);
    /// 不允许延迟到后台（对应 bcachefs PASS_NODEFER）
    pub const NODEFER: Self = RecoveryPassFlags(1 << 5);
    /// 需要 alloc 信息可用（对应 bcachefs PASS_ALLOC）
    ///
    /// 若设备无 alloc 信息（新格式化），此标志的 pass 会被跳过。
    pub const ALLOC: Self = RecoveryPassFlags(1 << 6);

    pub const fn from_bits(bits: u64) -> Self {
        RecoveryPassFlags(bits)
    }

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

impl BitOr for RecoveryPassFlags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        RecoveryPassFlags(self.0 | rhs.0)
    }
}

/// bcachefs 对齐的 pass 描述符（对应 `struct recovery_pass`）
pub struct RecoveryPass {
    pub pass: BchRecoveryPass,
    pub flags: RecoveryPassFlags,
    /// 依赖的 pass 位掩码（对应 bcachefs `depends` 字段）
    pub deps: u64,
    pub name: &'static str,
}

/// 所有 pass 的定义（对应 bcachefs `recovery_passes[]` 数组）
///
/// 顺序严格匹配 `BchRecoveryPass` 枚举顺序（bcachefs `enum bch_recovery_pass` 展开顺序）。
const ALL_RECOVERY_PASSES: &[RecoveryPass] = &[
    // (0) check_topology — bcachefs enum idx 2, stable=4, flags=0
    RecoveryPass {
        pass: BchRecoveryPass::CheckTopology,
        flags: RecoveryPassFlags::from_bits(0),
        deps: 0,
        name: "check_topology",
    },
    // (1) AccountingRead — bcachefs enum idx 3, stable=39, deps: check_topology
    RecoveryPass {
        pass: BchRecoveryPass::AccountingRead,
        flags: RecoveryPassFlags::ALWAYS,
        deps: RECOVERY_PASS_BITS[0], // depends on check_topology
        name: "accounting_read",
    },
    // (2) AllocRead — bcachefs enum idx 4, stable=0
    RecoveryPass {
        pass: BchRecoveryPass::AllocRead,
        flags: RecoveryPassFlags::ALWAYS,
        deps: 0,
        name: "alloc_read",
    },
    // (3) SnapshotsRead — bcachefs enum idx 7, stable=3
    RecoveryPass {
        pass: BchRecoveryPass::SnapshotsRead,
        flags: RecoveryPassFlags::ALWAYS,
        deps: 0,
        name: "snapshots_read",
    },
    // (4) check_allocations — bcachefs enum idx 8, stable=5, FSCK|ALLOC
    RecoveryPass {
        pass: BchRecoveryPass::CheckAllocations,
        flags: RecoveryPassFlags::from_bits((1 << 3) | (1 << 6)), // FSCK | ALLOC
        deps: RECOVERY_PASS_BITS[0],                              // depends on check_topology
        name: "check_allocations",
    },
    // (5) TransMarkDevSbs — bcachefs enum idx 9, stable=6, ALWAYS|SILENT|ALLOC
    RecoveryPass {
        pass: BchRecoveryPass::TransMarkDevSbs,
        flags: RecoveryPassFlags::from_bits((1 << 0) | (1 << 2) | (1 << 6)), // ALWAYS|SILENT|ALLOC
        deps: 0,
        name: "trans_mark_dev_sbs",
    },
    // (6) FsJournalAlloc — bcachefs enum idx 10, stable=7, ALWAYS|SILENT|ALLOC
    RecoveryPass {
        pass: BchRecoveryPass::FsJournalAlloc,
        flags: RecoveryPassFlags::from_bits((1 << 0) | (1 << 2) | (1 << 6)), // ALWAYS|SILENT|ALLOC
        deps: 0,
        name: "fs_journal_alloc",
    },
    // (7) SetMayGoRw — bcachefs enum idx 11, stable=8, ALWAYS|SILENT
    //     依赖：check_allocations（bcachefs passes_format.h:69-72 BIT_ULL(check_allocations)）
    RecoveryPass {
        pass: BchRecoveryPass::SetMayGoRw,
        flags: RecoveryPassFlags::from_bits((1 << 0) | (1 << 2)), // ALWAYS | SILENT
        deps: RECOVERY_PASS_BITS[4],                              // depends on check_allocations
        name: "set_may_go_rw",
    },
    // (8) JournalReplay — bcachefs enum idx 12, stable=9, deps: SetMayGoRw
    RecoveryPass {
        pass: BchRecoveryPass::JournalReplay,
        flags: RecoveryPassFlags::ALWAYS,
        deps: RECOVERY_PASS_BITS[7], // depends on SetMayGoRw
        name: "journal_replay",
    },
    // (9) PresplitShardBoundaries — bcachefs enum idx 14, stable=48
    RecoveryPass {
        pass: BchRecoveryPass::PresplitShardBoundaries,
        flags: RecoveryPassFlags::ALWAYS,
        deps: RECOVERY_PASS_BITS[8], // depends on JournalReplay
        name: "presplit_shard_boundaries",
    },
    // (10) check_alloc_info — bcachefs enum idx 10, stable=10, ONLINE|FSCK|ALLOC
    RecoveryPass {
        pass: BchRecoveryPass::CheckAllocInfo,
        flags: RecoveryPassFlags::from_bits((1 << 3) | (1 << 4) | (1 << 6)),
        deps: RECOVERY_PASS_BITS[4], // depends on check_allocations
        name: "check_alloc_info",
    },
    // (11) fs_freespace_init — bcachefs enum idx 21, stable=16, ALWAYS|SILENT
    RecoveryPass {
        pass: BchRecoveryPass::FsFreespaceInit,
        flags: RecoveryPassFlags::from_bits((1 << 0) | (1 << 2)), // ALWAYS | SILENT
        deps: 0,
        name: "fs_freespace_init",
    },
    // (12) bucket_gens_init — bcachefs enum idx 17, stable=17
    RecoveryPass {
        pass: BchRecoveryPass::BucketGensInit,
        flags: RecoveryPassFlags::from_bits(0),
        deps: 0,
        name: "bucket_gens_init",
    },
    // (13) check_snapshots — bcachefs enum idx 26, stable=19, ONLINE|FSCK
    RecoveryPass {
        pass: BchRecoveryPass::CheckSnapshots,
        flags: RecoveryPassFlags::from_bits((1 << 3) | (1 << 4)), // FSCK | ONLINE
        deps: 0,
        name: "check_snapshots",
    },
    // (14) LookupRootInode — bcachefs enum idx 48, stable=42, ALWAYS|SILENT
    RecoveryPass {
        pass: BchRecoveryPass::LookupRootInode,
        flags: RecoveryPassFlags::from_bits((1 << 0) | (1 << 2)), // ALWAYS | SILENT
        deps: 0,
        name: "lookup_root_inode",
    },
];

/// bcachefs 对齐的 recovery 状态（对应 `struct bch_fs_recovery`）
pub struct RecoveryState {
    pub vol: Box<BchVol>,
    pub journal: Journal,
    pub superblock: BchSb,

    // === bcachefs 对齐的 passes 跟踪 ===
    /// 当前迭代中待运行的 pass 位掩码（对应 `current_passes`）
    pub current_passes: u64,
    /// 当前正在运行的 pass 序号（对应 `current_pass`）
    pub current_pass: usize,
    /// 已完成 pass 的位掩码（对应 `passes_complete`）
    pub passes_complete: u64,
    /// 已完成的最高 pass 序号（对应 `pass_done`，标量 max）
    pub pass_done: usize,
    /// 失败 pass 的位掩码（对应 `passes_failing`）
    ///
    /// 失败的 pass 在当前迭代轮次跳过，后续轮次重试。
    /// 若其他 pass 成功（passes_complete 前进）则清空此掩码。
    pub passes_failing: u64,

    // === RW 过渡保险（P0-3: set_may_go_rw insurance） ===
    /// 是否已通过 set_may_go_rw 过渡到 RW 模式
    /// 对应 bcachefs `BCH_FS_may_go_rw` flag
    pub may_go_rw: bool,

    // === Accounting replay 状态（P0-2: accounting key replay） ===
    /// accounting keys（Alloc btree）是否已完成重放
    /// 对应 bcachefs `BCH_FS_accounting_replay_done` flag
    pub accounting_replay_done: bool,

    // === pass 间共享数据 ===
    /// Journal entries（journal_read pass 填充，供后续 pass 使用）
    pub jsets: Vec<Jset>,
    /// 从 journal 恢复的 btree roots (btree_id, addr, level)
    pub recovered_roots: Vec<(BtreeId, u64, u8)>,
    /// 已回放的 journal entry 数量
    pub applied_count: u64,
    /// 已回放的 seq 列表（持久化到 superblock 以跳过已重放 entries）
    pub replayed_seqs: Vec<u64>,

    // === GC 状态 ===
    /// BtreeGc 实例（GcScan pass 使用）
    pub gc: BtreeGc,

    // === Rewind 状态 ===
    /// Rewind 来源 pass（调试用途，记录哪个 pass 触发了回退）
    /// 当 bch2_rewind_recovery() 被调用时设置。
    pub rewound_from: Option<usize>,
    /// Rewind 目标 pass（外层 loop 检测后清空）
    /// 表示需要回退到哪个 pass 重新执行。
    pub rewound_to: Option<usize>,
    /// 可在在线模式（post-RW）运行的 pass 位掩码
    pub passes_online: u64,
}

impl RecoveryState {
    /// 创建新的 recovery 状态
    pub fn new(vol: Box<BchVol>, journal: Journal, sb: BchSb) -> Self {
        if let Some(ca) = vol.primary_device_rcu_noerror() {
            *ca.disk_sb.lock().unwrap() = sb.clone();
            let journal_state = journal.to_superblock_state();
            let bucket_blocks = vol
                .superblock()
                .member(ca.dev_idx)
                .map(|member| u64::from(member.bucket_size) / crate::alloc::SECTORS_PER_BLOCK)
                .expect("primary device has no superblock member");
            let mut ja = ca.journal.lock().unwrap();
            ja.bucket_seq = if sb.journal_bucket_seq.len() == journal_state.bucket_addrs.len() {
                sb.journal_bucket_seq.clone()
            } else {
                vec![0; journal_state.bucket_addrs.len()]
            };
            ja.discard_idx = journal_state.discard_idx;
            ja.dirty_idx_ondisk = journal_state.dirty_idx_ondisk;
            ja.dirty_idx = journal_state.dirty_idx;
            ja.cur_idx = journal_state.last_bucket;
            ja.nr = journal_state.bucket_addrs.len() as u32;
            ja.buckets = journal_state
                .bucket_addrs
                .into_iter()
                .map(|addr| addr / bucket_blocks)
                .collect();
        }
        Self {
            vol,
            journal,
            superblock: sb,
            current_passes: 0,
            current_pass: 0,
            passes_complete: 0,
            pass_done: 0,
            passes_failing: 0,
            may_go_rw: false,
            accounting_replay_done: false,
            jsets: vec![],
            recovered_roots: vec![],
            applied_count: 0,
            replayed_seqs: vec![],
            gc: BtreeGc::new(),
            rewound_from: None,
            rewound_to: None,
            passes_online: 0,
        }
    }

    /// 从 superblock 恢复 recovery 进度（crash resume 支持，P0-4）
    ///
    /// 读取持久化的 pass_done（稳定 ID），将稳定 ID ≤ 该值的 pass 标记为已完成。
    /// 调用后已完成的 pass 会被运行循环跳过。
    ///
    /// 同时恢复 `may_go_rw` 标志：SetMayGoRw (stable=8) 已在之前 recovery 中
    /// 完成，说明 RW 过渡已通过，`may_go_rw` 内存标志应同步恢复。
    /// 防止 `bch2_run_recovery_passes` 中因 all_pass_mask 完全覆盖但 may_go_rw
    /// 未设置而误报 "recovery completed without set_may_go_rw"。
    pub fn restore_progress(&mut self) {
        let done_stable = self.superblock.pass_done;
        let old_done = self.pass_done;
        let mut max_enum_idx = 0usize;

        // 遍历所有注册的 pass，将稳定 ID ≤ done_stable 的标记为已完成
        for pd in ALL_RECOVERY_PASSES {
            let idx = pd.pass as usize;
            let stable = bch2_recovery_pass_to_stable(pd.pass) as u64;
            // 当 done_stable == 0 时，表示"未执行任何 pass"，而非表示
            // "stable=0 的 AllocRead 已完成"，因此需要排除 done_stable == 0
            // 的情况以保持与 AllocRead (stable=0) 的区分。
            if done_stable > 0 && stable <= done_stable {
                self.passes_complete |= 1 << idx;
                max_enum_idx = max_enum_idx.max(idx);
            }
            // SetMayGoRw (stable=8) 已由之前 recovery 完成，恢复内存标志
            if done_stable > 0 && stable <= done_stable
                && pd.pass == BchRecoveryPass::SetMayGoRw
            {
                self.may_go_rw = true;
            }
        }

        self.pass_done = self.pass_done.max(max_enum_idx);

        // 仅在进度前进时保持 superblock 中的稳定 pass 号不回退。
        // 这里必须保留原始 stable ID，而不是回写 runtime index 对应的 stable，
        // 因为 bcachefs 的 stable ID 顺序并不等同于 runtime 顺序。
        if self.pass_done > old_done {
            self.superblock.pass_done = self.superblock.pass_done.max(done_stable);
        }

        // 计算可在在线模式运行的 pass 集合
        self.passes_online = compute_passes_with_flag(RecoveryPassFlags::ONLINE);
    }

    /// 当前 pass 成功后持久化进度（crash 后能从最近完成点恢复）
    ///
    /// 写入 stable ID 而非 enum index，确保未来 pass 顺序变化时 superblock 兼容。
    pub async fn persist_progress(&mut self) -> Result<(), StorageError> {
        let stable = ALL_RECOVERY_PASSES
            .get(self.pass_done)
            .map(|pd| bch2_recovery_pass_to_stable(pd.pass) as u64)
            .unwrap_or(self.superblock.pass_done);
        self.superblock.pass_done = self.superblock.pass_done.max(stable);
        let mut first_error = None;
        for dev_idx in self.vol.device_registry.dev_indices() {
            let Some(dev) = self.vol.device_registry.resolve_bch_dev(dev_idx) else {
                continue;
            };
            if !dev.is_online() {
                continue;
            }
            if let Err(error) = self.superblock.write_to_device(dev.as_ref()).await {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    /// Recovery 完成后将 pass_done + replayed_seqs 同步回 superblock 并持久化
    pub async fn sync_to_superblock(&mut self) -> Result<(), StorageError> {
        let stable = ALL_RECOVERY_PASSES
            .get(self.pass_done)
            .map(|pd| bch2_recovery_pass_to_stable(pd.pass) as u64)
            .unwrap_or(self.superblock.pass_done);
        self.superblock.pass_done = self.superblock.pass_done.max(stable);
        self.superblock.replayed_seqs = self.replayed_seqs.clone();
        let mut first_error = None;
        for dev_idx in self.vol.device_registry.dev_indices() {
            let Some(dev) = self.vol.device_registry.resolve_bch_dev(dev_idx) else {
                continue;
            };
            if !dev.is_online() {
                continue;
            }
            if let Err(error) = self.superblock.write_to_device(dev.as_ref()).await {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

// ---------------------------------------------------------------------------
// Pass 调度器 — bcachefs 对齐
// ---------------------------------------------------------------------------

/// bcachefs 对齐的 recovery pass 限制，连续失败达此阈值报错。
pub const MAX_PASS_FAILURES: usize = 3;

/// Recovery 无法继续时返回的错误消息
pub const ERR_PASSES_FAILING: &str = "recovery passes still failing after max retries";

/// 回退到指定 pass——清除目标 pass 及之后的所有已完成状态。
///
/// 对应 bcachefs `bch2_rewind_recovery()`。
/// 由需要回退的 pass 自行调用，返回 RewindRecovery 错误码通知调度器。
/// 调度器外层循环检测到 rewound_to 后重新计算 passes_to_run。
///
/// # Arguments
///
/// * `state` - Recovery 状态
/// * `target_pass` - 回退目标 pass index（该 pass 及之后的所有 pass 将被重新执行）
///
/// # Returns
///
/// 总是返回 `StorageError::RewindRecovery(target_pass)`。
pub(crate) fn rewind_recovery(state: &mut RecoveryState, target_pass: usize) -> StorageError {
    debug_assert!(
        target_pass < state.current_pass,
        "bch2_rewind_recovery: target_pass {} >= current_pass {}",
        target_pass,
        state.current_pass
    );

    // 清除 target_pass 及之后的所有已完成 pass
    for idx in target_pass..=state.pass_done {
        state.passes_complete &= !(1 << idx);
    }

    // 清除失败掩码（从 target 开始重新执行）
    state.passes_failing = 0;

    // 更新 pass_done 为 target_pass 之前的一个
    state.pass_done = target_pass.saturating_sub(1);

    // 记录 rewind 信息（调试用途）
    state.rewound_from = Some(state.current_pass);
    state.rewound_to = Some(target_pass);

    StorageError::RewindRecovery(target_pass)
}

/// 运行单个 recovery pass（对应 bcachefs `bch2_run_recovery_pass()`）
///
/// 不会修改 current_passes 位掩码；由 `bch2_run_recovery_passes()` 管理迭代状态。
///
/// # 保险检查
///
/// 每次成功后持久化 pass_done 到 superblock（P0-4: crash resume）。
/// set_may_go_rw pass 设置 may_go_rw 标志（P0-3: RW 过渡保险）。
async fn bch2_run_recovery_pass(
    state: &mut RecoveryState,
    pass_idx: usize,
) -> Result<(), StorageError> {
    state.current_pass = pass_idx;

    // Dispatch via match（避免 async fn pointer 生命周期问题）
    let result = match pass_idx {
        0 => passes::check_topology::run(state).await,
        1 => passes::accounting_read::run(state).await,
        2 => passes::alloc_read::run(state).await,
        3 => passes::snapshots_read::run(state).await,
        4 => passes::check_allocations::run(state).await,
        5 => passes::trans_mark_dev_sbs::run(state).await,
        6 => passes::fs_journal_alloc::run(state).await,
        7 => passes::set_may_go_rw::run(state).await,
        8 => passes::journal_replay::run(state).await,
        9 => passes::presplit_shard_boundaries::run(state).await,
        10 => passes::check_alloc_info::run(state).await,
        11 => passes::fs_freespace_init::run(state).await,
        12 => passes::bucket_gens_init::run(state).await,
        13 => passes::check_snapshots::run(state).await,
        14 => passes::lookup_root_inode::run(state).await,
        _ => unreachable!(),
    };

    match &result {
        Ok(()) => {
            // bcachefs 对齐：设置 passes_complete + pass_done
            state.passes_complete |= 1 << pass_idx;
            state.pass_done = state.pass_done.max(pass_idx);
            state.superblock.pass_done = state.pass_done as u64;
            // 任一个 pass 成功后清除 passes_failing（对应 bcachefs passes.c:524）
            state.passes_failing = 0;

            // P0-4: crash resume — 每个 pass 成功后持久化进度
            // 可选的 sync；若失败不影响 recovery 流程
            let _ = state.persist_progress().await;
        }
        Err(StorageError::RewindRecovery(_)) => {
            // RewindRecovery 不标记 passes_failing——这是预期的调度信号。
            // bch2_rewind_recovery() 已经处理了 state 变更（清除 passes_complete 等）。
            // 此错误由 bch2_run_recovery_passes() 的内层循环检测并 break。
        }
        Err(_e) => {
            // 失败 pass 标记位，供后续轮次重试
            state.passes_failing |= 1 << pass_idx;
        }
    }

    result
}

/// 运行所有 recovery pass，支持 fail-retry 循环
///
/// 对应 bcachefs `bch2_run_recovery_passes()`：
/// 1. 按 flags 组装 `current_passes`（ALWAYS + UNCLEAN if !clean）
/// 2. 排除 `passes_failing` 中的 pass（当前轮次跳过，后续重试）
/// 3. 用 `trailing_zeros()`（对应 `__ffs64`）逐个运行需要执行的 pass
/// 4. 失败 → 在 `passes_failing` 中标记位，不立即报错
/// 5. 成功 → 清除 `passes_failing`，设置 `passes_complete` + `pass_done`
/// 6. 若所有剩余 pass 均失败 → 重试；超阈值则返回错误
///
/// 连续失败后检查 `passes_failing` 位，达 `MAX_PASS_FAILURES` 阈值报错。
pub async fn bch2_run_recovery_passes(state: &mut RecoveryState) -> Result<(), StorageError> {
    // P0-4: crash resume — 从 superblock 恢复之前已完成的 pass
    state.restore_progress();

    // 计算本次应该运行的 pass 集合
    // 此掩码已排除 FSCK 标志的 pass 和 PASS_ALLOC 跳过的 pass
    state.current_passes = compute_passes_to_run(state);

    if state.current_passes == 0 {
        // P0-3: set_may_go_rw 保险 — 若所有 pass 已跳过但 may_go_rw 尚未设置，强制运行
        if !state.may_go_rw {
            state.current_passes |= RECOVERY_PASS_BITS[BchRecoveryPass::SetMayGoRw as usize];
        }
        // 不洁关闭时始终需要 journal replay，即使 may_go_rw 已恢复
        // （journal 中可能有 subvol create/delete 等未重放到 btree 的条目）
        if !state.superblock.clean_shutdown {
            state.current_passes |= RECOVERY_PASS_BITS[BchRecoveryPass::JournalReplay as usize];
        }
        if state.current_passes == 0 {
            return Ok(());
        }
    }

    // 完成检查所用掩码 = current_passes（仅包含实际运行的 pass）
    // 注意：不能使用 ALL_RECOVERY_PASSES 的全集，因为 FSCK/ALLOC 跳过的 pass 不会运行。
    // 若触发 set_may_go_rw / journal_replay 兜底路径，也必须把强制运行的 pass 计入完成掩码。
    let all_pass_mask: u64 = state.current_passes;

    let mut retry_count = 0;

    loop {
        let mut passes_to_run = state.current_passes;

        // 排除当前轮次失败的 pass（重试前等待其他 pass 修复问题）
        passes_to_run &= !state.passes_failing;
        passes_to_run &= !state.passes_complete;

        if passes_to_run == 0 {
            // 所有 pass 已完成，或仅剩失败的 pass
            if state.passes_complete & all_pass_mask == all_pass_mask {
                // P0-3: set_may_go_rw 保险 — 验证 RW 过渡已完成
                if !state.may_go_rw {
                    return Err(StorageError::Recovery(
                        "recovery completed without set_may_go_rw (may_go_rw not set)",
                    ));
                }
                return Ok(());
            }
            // 仅剩失败的 pass：重试
            retry_count += 1;
            if retry_count > MAX_PASS_FAILURES {
                return Err(StorageError::Recovery(ERR_PASSES_FAILING));
            }
            state.passes_failing = 0;
            continue;
        }

        let mut any_succeeded = false;

        // 迭代执行（对应 bcachefs `while(r->current_passes) { __ffs64 + run }`）
        while passes_to_run != 0 {
            let pass_idx = passes_to_run.trailing_zeros() as usize;
            passes_to_run &= !(1 << pass_idx);

            // 跳过已完成的 pass
            if state.passes_complete & (1 << pass_idx) != 0 {
                continue;
            }

            // 跳过此轮失败的 pass
            if state.passes_failing & (1 << pass_idx) != 0 {
                continue;
            }

            // deps 依赖靠 ALL_RECOVERY_PASSES 声明顺序自然满足（对齐 bcachefs passes.c:532-597）
            // bcachefs bch2_run_recovery_passes() 用 __ffs64 遍历位掩码，无运行时 deps 检查

            let result = bch2_run_recovery_pass(state, pass_idx).await;

            match result {
                Ok(()) => {
                    any_succeeded = true;
                }
                Err(StorageError::RewindRecovery(_)) => {
                    // Rewind 请求——bch2_rewind_recovery() 已清除 passes_complete 等状态。
                    // break 内层循环回到外层，外层重新计算 passes_to_run。
                    break;
                }
                Err(_e) => {
                    // 已在 bch2_run_recovery_pass 中标记 passes_failing
                }
            }
        }

        // 检查 rewind 请求：若 rewind 触发，回到循环顶部重新计算 passes_to_run
        if state.rewound_to.is_some() {
            state.rewound_to = None; // 重置以持续循环
            continue;
        }

        // 检查是否全部完成
        if state.passes_complete & all_pass_mask == all_pass_mask {
            // P0-3: set_may_go_rw 保险 — 验证 RW 过渡已完成
            if !state.may_go_rw {
                return Err(StorageError::Recovery(
                    "recovery completed without set_may_go_rw (may_go_rw not set)",
                ));
            }
            // 所有 pass 完成后，计算可在在线模式运行的 pass 位掩码
            state.passes_online = compute_passes_with_flag(RecoveryPassFlags::ONLINE);
            return Ok(());
        }

        // 若本轮没有任何 pass 成功触发 clear，检查是否需要继续重试
        if !any_succeeded {
            retry_count += 1;
            if retry_count > MAX_PASS_FAILURES {
                return Err(StorageError::Recovery(ERR_PASSES_FAILING));
            }
        }
    }
}

/// bch2_run_recovery_passes 的启动包装（对应 bcachefs `bch2_run_recovery_passes_startup()`）
///
/// 计算应运行的 pass 集合并启动恢复调度。
pub async fn bch2_run_recovery_passes_startup(
    state: &mut RecoveryState,
) -> Result<(), StorageError> {
    bch2_run_recovery_passes(state).await
}

/// 按 flags 组装 pass 运行集合
fn compute_passes_to_run(state: &RecoveryState) -> u64 {
    let mut mask = 0u64;
    for pd in ALL_RECOVERY_PASSES {
        let idx = pd.pass as usize;
        let should_run = if pd.flags.contains(RecoveryPassFlags::ALWAYS) {
            true
        } else if pd.flags.contains(RecoveryPassFlags::UNCLEAN) {
            !state.superblock.clean_shutdown
        } else if pd.flags.contains(RecoveryPassFlags::FSCK) {
            // FSCK 标记的 pass 默认不运行（subvol 无 fsck 模式）
            false
        } else {
            false
        };
        if should_run {
            mask |= RECOVERY_PASS_BITS[idx];
        }
    }

    // PASS_ALLOC 跳过逻辑：若设备无 alloc 信息，移除带 ALLOC 标志的 pass
    // 对应 bcachefs `BCH_FS_HAVE_ALLOC_INFO` 检查
    if state.superblock.has_no_alloc_info() {
        mask &= !compute_passes_with_flag(RecoveryPassFlags::ALLOC);
    }

    // recovery_passes_required：对应 bcachefs `c->sb.recovery_passes_required`
    // 遍历所有运行时 pass，检查其稳定 ID 是否在必需掩码中
    let required = state.superblock.recovery_passes_required;
    if required != 0 {
        for pd in ALL_RECOVERY_PASSES {
            let stable_id = bch2_recovery_pass_to_stable(pd.pass) as u64;
            if required & (1u64 << stable_id) != 0 {
                mask |= RECOVERY_PASS_BITS[pd.pass as usize];
            }
        }
    }

    mask
}

/// 计算具有指定标志的所有 pass 的位掩码
fn compute_passes_with_flag(flag: RecoveryPassFlags) -> u64 {
    let mut mask = 0u64;
    for pd in ALL_RECOVERY_PASSES {
        if pd.flags.contains(flag) {
            mask |= RECOVERY_PASS_BITS[pd.pass as usize];
        }
    }
    mask
}

// ---------------------------------------------------------------------------
// Top-level bcachefs 对齐的恢复入口
// ---------------------------------------------------------------------------

/// 启动文件系统恢复流程（对应 bcachefs `bch2_fs_recovery()`）
///
/// 1. 先读取 journal entries + 加载 btree roots（bcachefs 对齐：在 pass 调度之前）
/// 2. 再运行 recovery passes（按 flags: ALWAYS + UNCLEAN + FSCK 等决定运行集合）
///
/// 不持久化——caller（run_recovery / daemon）负责同步到 superblock。
pub async fn bch2_fs_recovery(state: &mut RecoveryState) -> Result<(), StorageError> {
    // Step 1: 读取 journal + 加载 btree roots（bcachefs 对齐：非 pass）
    passes::journal_read::run(state).await?;
    // Step 2: 运行 recovery passes
    bch2_run_recovery_passes_startup(state).await?;
    // Step 3: 标记 journal 恢复完成（恢复→正常运行模式过渡）
    // 对应 bcachefs `bch2_journal_set_replay_done()` (init.c:619)
    state.journal.bch2_journal_set_replay_done();
    Ok(())
}

/// 初始化新文件系统（对应 bcachefs `bch2_fs_initialize()`）
///
/// 在 subvol 中，新文件系统初始化按以下顺序进行：
/// 1. 创建根快照/子卷结构（initialize_subvolumes，在 passes 之前）
/// 2. 运行 recovery passes（创建初始 btree 结构）
/// 3. 同步 recovery 状态到 superblock
pub async fn bch2_fs_initialize(state: &mut RecoveryState) -> Result<(), StorageError> {
    // 先创建根快照/子卷结构，确保 SnapshotsRead 等 pass 能看到它们
    {
        let mut t = BtreeTrans::new(&state.vol);
        bch2_initialize_subvolumes(&mut t)?;
        t.bch2_trans_commit()?;
    }
    // 再运行 recovery passes
    let result = bch2_run_recovery_passes_startup(state).await;
    if result.is_ok() {
        // 新格式化的 FS 始终有 alloc 信息：默认 features=[0,0] 即 NO_ALLOC_INFO=0
        // 对应 bcachefs `bch2_fs_initialize()` ——新 FS 不设置 BCH_FEATURE_no_alloc_info
    }
    result
}

/// 重建 alloc 信息 — 对应 bcachefs `bch2_reconstruct_alloc()` (recovery.c:138)
///
/// 在 bcachefs 中，此函数在 recovery 期间 alloc info 损坏时被调用，
/// 设置 `recovery_passes_required` 中的 `check_allocations` 位，
/// 确保下次 mounts 时 check_allocations pass 运行以重建 alloc 信息。
///
/// Volmount 版语义相同：写入 `state.superblock.recovery_passes_required` 位，
/// 使下次 `compute_passes_to_run` 调用时包含 CheckAllocations pass。
pub(crate) fn bch2_reconstruct_alloc(state: &mut RecoveryState) {
    let stable_id = BchRecoveryPassStable::CheckAllocations as u64;
    state.superblock.recovery_passes_required |= 1u64 << stable_id;
}

/// 简洁的恢复入口函数 — 供 daemon 层在 Volume::new() 前调用
///
/// 创建 RecoveryState，从 superblock 恢复进度（P0-4: crash resume），
/// 运行所有 recovery passes，同步恢复状态到 superblock，
/// 返回完成恢复的 BchVol；filesystem-global allocator 始终由该 BchVol 持有。
pub async fn run_recovery(
    vol: BchVol,
    journal: Journal,
    sb: BchSb,
) -> Result<BchVol, StorageError> {
    let mut state = RecoveryState::new(Box::new(vol), journal, sb);
    bch2_fs_recovery(&mut state).await?;
    // P0-4: crash resume — 恢复完成后持久化进度到 superblock
    state.sync_to_superblock().await?;
    Ok(*state.vol)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_device::{BchDev, MockBlockDevice};
    use crate::journal::Journal;
    use crate::storage::superblock::feature_bits;
    use crate::storage::superblock::BchSb;
    use crate::types::BackendType;
    use std::sync::Arc;

    #[tokio::test]
    async fn test_run_recovery_clean_backend() {
        let backend = Arc::new(MockBlockDevice::new());
        let journal = Journal::new(vec![256, 512]);
        journal.set_test_device(Arc::new(BchDev::new(backend.clone(), 0)));
        let vol = crate::BchVol::test_trees();
        let ca = vol.primary_device_rcu_noerror().unwrap();
        vol.superblock_mut().member_mut(0).unwrap().nbuckets = 3;
        crate::alloc::bch2_dev_buckets_resize(&vol, &ca, 3).unwrap();
        let sb = BchSb::with_volume_info(
            "test".into(),
            1,
            "default".into(),
            4096,
            1024 * 1024,
            BackendType::Nfs,
        );
        // 模拟新文件系统：NO_ALLOC_INFO=0（默认 features=[0,0] 即 alloc 信息存在）
        let result = run_recovery(vol, journal, sb).await;
        assert!(
            result.is_ok(),
            "recovery should succeed on clean backend: {:?}",
            result.as_ref().err()
        );

        let vol = result.unwrap();
        // After recovery on a clean system, only Alloc btree has metadata
        // (trans_mark_dev_sbs marks Sb + journal buckets).
        // Alloc: 3 entries — bucket 0 as Sb, then buckets for
        // journal block addresses 256 and 512 map to buckets 1 and 2.
        let expected = vol.btree(crate::btree::BtreeId::Alloc).root().node.packed_keys + vol.btree(crate::btree::BtreeId::Alloc).root().node.unpacked_keys;
        assert_eq!(
            expected, 3,
            "Alloc should have 3 entries after recovery (Sb + 2 journal marks)"
        );
        vol.btree(crate::btree::BtreeId::Alloc)
            .for_each_btree_key_entry(|entry| {
                let crate::btree::key::KeyValue::Raw(bytes) = &entry.value else {
                    panic!("trans_mark_dev_sbs must write raw alloc_v4 values");
                };
                crate::alloc::btree::deserialize_alloc_entry(&bytes)
                    .expect("trans_mark_dev_sbs must write valid alloc_v4 values");
            });

        for ty in crate::btree::BTREE_ID_NR {
            if ty == crate::btree::BtreeId::Alloc || ty == crate::btree::BtreeId::Freespace {
                continue;
            }
            assert_eq!(
                vol.btree(ty).root().node.packed_keys + vol.btree(ty).root().node.unpacked_keys,
                0,
                "btree {:?} should be empty after clean recovery",
                ty
            );
        }
    }

    #[tokio::test]
    async fn test_fs_initialize_new() {
        let journal = Journal::new(vec![256, 512]);
        let sb = BchSb::with_volume_info(
            "test".into(),
            1,
            "default".into(),
            4096,
            1024 * 1024,
            BackendType::Nfs,
        );
        // bch2_fs_initialize 模拟新文件系统初始化
        let vol = crate::BchVol::test_trees();
        let mut state = RecoveryState::new(Box::new(vol), journal, sb);
        let result = bch2_fs_initialize(&mut state).await;
        assert!(result.is_ok(), "fs_initialize should succeed");
    }

    fn test_state(clean_shutdown: bool, no_alloc_info: bool) -> RecoveryState {
        let journal = Journal::new(vec![256]);
        let mut sb = BchSb::with_volume_info(
            "test".into(),
            1,
            "default".into(),
            4096,
            1024 * 1024,
            BackendType::Nfs,
        );
        sb.clean_shutdown = clean_shutdown;
        if no_alloc_info {
            sb.features[0] |= 1u64 << feature_bits::NO_ALLOC_INFO;
        }
        RecoveryState::new(Box::new(crate::BchVol::test_trees()), journal, sb)
    }

    #[test]
    fn test_compute_passes_to_run_skips_unclean_on_clean_shutdown() {
        let state = test_state(true, false);
        let mask = compute_passes_to_run(&state);

        assert_eq!(
            mask & RECOVERY_PASS_BITS[BchRecoveryPass::CheckTopology as usize],
            0
        );
        assert_ne!(
            mask & RECOVERY_PASS_BITS[BchRecoveryPass::AllocRead as usize],
            0
        );
        assert_ne!(
            mask & RECOVERY_PASS_BITS[BchRecoveryPass::FsFreespaceInit as usize],
            0
        );
    }

    #[test]
    fn test_compute_passes_to_run_skips_alloc_without_alloc_info() {
        let state = test_state(false, true);
        let mask = compute_passes_to_run(&state);

        assert_eq!(
            mask & RECOVERY_PASS_BITS[BchRecoveryPass::TransMarkDevSbs as usize],
            0
        );
        assert_eq!(
            mask & RECOVERY_PASS_BITS[BchRecoveryPass::FsJournalAlloc as usize],
            0
        );
        // check_topology flags=0 → 不在自动运行掩码中
        assert_eq!(
            mask & RECOVERY_PASS_BITS[BchRecoveryPass::CheckTopology as usize],
            0
        );
    }

    #[test]
    fn test_restore_progress_marks_completed_passes_from_stable_id() {
        let mut state = test_state(false, false);
        state.superblock.pass_done =
            bch2_recovery_pass_to_stable(BchRecoveryPass::AccountingRead) as u64;

        state.restore_progress();

        assert_eq!(state.pass_done, BchRecoveryPass::CheckSnapshots as usize);
        assert_ne!(
            state.passes_complete & RECOVERY_PASS_BITS[BchRecoveryPass::CheckTopology as usize],
            0
        );
        assert_ne!(
            state.passes_complete & RECOVERY_PASS_BITS[BchRecoveryPass::AccountingRead as usize],
            0
        );
        assert_ne!(
            state.passes_complete & RECOVERY_PASS_BITS[BchRecoveryPass::JournalReplay as usize],
            0
        );
        assert_eq!(
            state.passes_complete & RECOVERY_PASS_BITS[BchRecoveryPass::LookupRootInode as usize],
            0
        );
        assert_eq!(
            state.superblock.pass_done,
            bch2_recovery_pass_to_stable(BchRecoveryPass::AccountingRead) as u64
        );
        assert_ne!(
            state.passes_online & RECOVERY_PASS_BITS[BchRecoveryPass::CheckSnapshots as usize],
            0
        );
    }
}
