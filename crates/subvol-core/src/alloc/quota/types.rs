use serde::{Deserialize, Serialize};

/// 配额类型 — 对应 bcachefs `enum bch_quota_type`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum BchQuotaType {
    Usr = 0,
    Grp = 1,
    Prj = 2,
}

impl BchQuotaType {
    pub const NR: usize = 3;
}

/// 配额计数器类型 — 对应 bcachefs `enum bch_quota_counters`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum BchQuotaCounters {
    Spc = 0,
    Ino = 1,
}

impl BchQuotaCounters {
    pub const NR: usize = 2;

    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Spc),
            1 => Some(Self::Ino),
            _ => None,
        }
    }
}

/// 配额计数器（硬限制 + 软限制）
///
/// 对应 bcachefs `struct bch_quota_counter`
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[repr(C)]
pub struct BchQuotaCounter {
    pub hardlimit: u64,
    pub softlimit: u64,
}

impl Default for BchQuotaCounter {
    fn default() -> Self {
        Self {
            hardlimit: 0,
            softlimit: 0,
        }
    }
}

/// 配额条目（空间 + inode 双计数器）
///
/// 对应 bcachefs `struct bch_quota`，存储在 BTREE_ID_quotas 中。
/// key: Bpos { inode: qid_encode(type, id), offset: BchQuotaCounters, snapshot: 0 }
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[repr(C)]
pub struct BchQuota {
    /// 计数器数组 [spc, ino]
    pub c: [BchQuotaCounter; BchQuotaCounters::NR],
}

impl Default for BchQuota {
    fn default() -> Self {
        Self {
            c: [BchQuotaCounter::default(), BchQuotaCounter::default()],
        }
    }
}

/// Superblock 配额计数器配置 — 对应 bcachefs `struct bch_sb_quota_counter`
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[repr(C)]
pub struct BchSbQuotaCounter {
    /// 宽限时间（秒），软限制超限后开始计时
    pub timelimit: u32,
    /// 警告消息发出次数上限
    pub warnlimit: u32,
}

impl Default for BchSbQuotaCounter {
    fn default() -> Self {
        Self {
            timelimit: 86400,
            warnlimit: 0,
        }
    }
}

/// Superblock per-type 配额配置 — 对应 bcachefs `struct bch_sb_quota_type`
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[repr(C)]
pub struct BchSbQuotaType {
    pub flags: u64,
    pub c: [BchSbQuotaCounter; BchQuotaCounters::NR],
}

impl Default for BchSbQuotaType {
    fn default() -> Self {
        Self {
            flags: 0,
            c: [BchSbQuotaCounter::default(), BchSbQuotaCounter::default()],
        }
    }
}

/// 编码 qid 为 bpos.inode
///
/// bcachefs 对齐：高 8 位存储类型，低 56 位存储 id
pub fn qid_encode(qtype: BchQuotaType, id: u64) -> u64 {
    ((qtype as u64) << 56) | (id & 0x00FF_FFFF_FFFF_FFFF)
}

/// 从 bpos.inode 解码 qid
pub fn qid_decode(raw: u64) -> (BchQuotaType, u64) {
    let type_val = (raw >> 56) as u8;
    let id = raw & 0x00FF_FFFF_FFFF_FFFF;
    let qtype = match type_val {
        0 => BchQuotaType::Usr,
        1 => BchQuotaType::Grp,
        _ => BchQuotaType::Prj,
    };
    (qtype, id)
}

/// 配额 quota 值序列化大小（u64 数）：2 counters × 2 u64s = 4 u64s = 32 bytes
pub const QUOTA_VALUE_U64S: u8 = 4;
