use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BtreeId(pub u8);

impl BtreeId {
    pub const fn from_u8(v: u8) -> Self {
        BtreeId(v)
    }
}

impl fmt::Display for BtreeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "btree_{}", self.0)
    }
}

// ═══════════════════════════════════════════════════════════════
// Btree ID 常量 — 三种核心 btree
// ═══════════════════════════════════════════════════════════════

/// 空闲空间 btree — 记录空闲块区间
/// key: block offset, value: extent size (u64 LE)
pub const BTREE_ID_FREESPACE: BtreeId = BtreeId(0);

/// 分配 btree — 记录每个块的分配状态
/// key: block offset, value: serialized AllocEntry
pub const BTREE_ID_ALLOC: BtreeId = BtreeId(1);

/// 数据索引 btree — 逻辑地址 → 物理位置映射
/// key: (inode, offset), value: serialized ExtentEntry
pub const BTREE_ID_DATA_INDEX: BtreeId = BtreeId(2);

/// btree node 大小（对应 bcachefs btree_node_size，默认 256KB）
pub const NODE_SIZE: u64 = 256 * 1024;
/// Btree 最大深度（对应 bcachefs BTREE_MAX_DEPTH）
pub const BTREE_MAX_DEPTH: usize = 8;

pub const BTREE_ID_NR: [BtreeId; 28] = {
    let mut ids = [BtreeId(0); 28];
    let mut i: usize = 0;
    while i < 28 {
        ids[i] = BtreeId(i as u8);
        i += 1;
    }
    ids
};
