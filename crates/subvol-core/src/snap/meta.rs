use serde::{Deserialize, Serialize};

// 备注：快照 ID 状态枚举 — 对齐 bcachefs enum snapshot_id_state
// 备注：
// 备注：- Empty: 未使用（已删除或未分配）
// 备注：- Live: 活跃快照，可访问
// 备注：- Deleted: 标记删除，等待清理
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SnapshotIdState {
    Empty,
    Live,
    Deleted,
}

impl Default for SnapshotIdState {
    fn default() -> Self {
        Self::Live
    }
}

/// 快照元数据（用于 Volume API 返回）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotMeta {
    /// 快照 ID
    pub id: u32,
    /// 父快照 ID
    pub parent: u32,
    /// 所属子卷
    pub subvol: u32,
    /// 树深度
    pub depth: u32,
    /// 创建时间戳（Unix 秒）
    pub created_at: i64,
    /// 是否标记为待删除（对齐 bcachefs 的 `WILL_DELETE` 语义）
    pub deleted: bool,
}

impl SnapshotMeta {}

/// 快照标志位（对齐 bcachefs BCH_SNAPSHOT_* bitmask）
///
/// Batch B 起从 1<<0..1<<3 迁移到 1<<4..1<<7，对齐 bcachefs 原始位布局：
/// - bits 0-3: 内部使用（count, deleted, etc.）
/// - bits 4-7: 用户可见标志（SUBVOL, NO_KEYS, WILL_DELETE, DELETED）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BchSnapshotFlags(u8);

impl BchSnapshotFlags {
    /// 持有子卷（leaf 节点，关联 subvol）。对齐 BCH_SNAPSHOT_SUBVOL。
    pub const SUBVOL: Self = Self(1 << 4);
    /// interior，key 已转移到子节点。对齐 BCH_SNAPSHOT_NO_KEYS。
    pub const NO_KEYS: Self = Self(1 << 5);
    /// 删除标记，等待下一轮清理。对齐 BCH_SNAPSHOT_WILL_DELETE。
    pub const WILL_DELETE: Self = Self(1 << 6);
    /// 已删除标记。对齐 BCH_SNAPSHOT_DELETED。
    pub const DELETED: Self = Self(1 << 7);

    /// 无标志
    pub const fn empty() -> Self {
        Self(0)
    }

    /// 是否包含指定标志
    pub const fn contains(self, flag: Self) -> bool {
        self.0 & flag.0 != 0
    }

    /// 添加标志
    pub fn insert(&mut self, flag: Self) {
        self.0 |= flag.0;
    }

    /// 移除标志
    pub fn remove(&mut self, flag: Self) {
        self.0 &= !flag.0;
    }
}

impl From<BchSnapshotFlags> for u8 {
    fn from(f: BchSnapshotFlags) -> Self {
        f.0
    }
}

/// Snapshot node 值，存储在 Snapshots btree 中。
///
/// 每个快照是一个节点，通过 parent + children 形成二叉树结构。
/// skip[] + is_ancestor 实现三层祖先查询（见 `is_ancestor`）。
///
/// # 字段对齐 bcachefs
///
/// | 字段 | bcachefs 对应 | 说明 |
/// |------|---------------|------|
/// | `state` | `snapshot_t.state` | 快照生命周期状态 |
/// | `parent` | `snapshot_t.parent` | 父快照 ID |
/// | `skip` | `snapshot_t.skip[3]` | 跳表索引，3 个祖先 |
/// | `children` | `snapshot_t.children[2]` | 二叉树子节点 |
/// | `subvol` | `snapshot_t.subvol` | 关联子卷 ID |
/// | `tree` | `snapshot_t.tree` | 所属快照树 ID |
/// | `is_ancestor` | `snapshot_t.is_ancestor[]` | 128 位祖先位图 |
/// | `depth` | `snapshot_t.depth` | 树深度 |
/// | `btime` | `bch_snapshot.btime` | 创建时间 |
/// | `deleted` | pending-delete 标记 | 对应 bcachefs `WILL_DELETE` 语义 |
/// | `flags` | `bch_snapshot.flags` | BCH_SNAPSHOT_* 标志（Batch B+ 使用位 4-7） |
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotT {
    /// 快照生命周期状态（对齐 bcachefs snapshot_t.state）
    pub state: SnapshotIdState,
    /// 父快照 ID（0 = root）
    pub parent: u32,
    /// 二叉树子节点 [left, right]（0 = 无子节点）
    pub children: [u32; 2],
    /// 所属子卷 ID（interior 节点 = 0）
    pub subvol: u32,
    /// 所属的 snapshot tree ID
    pub tree: u32,
    /// 跳跃表，3 个祖先（升序），用于 skip list 跳跃。对齐 bcachefs skip[3]。
    pub skip: [u32; 3],
    /// 128 位祖先位图：bit N 表示 (id - N - 1) 是 id 的祖先。对齐 bcachefs is_ancestor[]。
    pub is_ancestor: u128,
    /// 树深度（根 depth=1）
    pub depth: u32,
    /// 创建时间戳（Unix 秒）。对齐 bcachefs btime。
    pub btime: i64,
    /// 是否已删除（软删除）
    pub deleted: bool,
    /// 快照标志
    pub flags: BchSnapshotFlags,
}

impl SnapshotT {
    /// 创建新的 snapshot leaf node（子卷）
    pub fn new_leaf(parent: u32, subvol: u32, tree: u32, depth: u32, btime: i64) -> Self {
        Self {
            state: SnapshotIdState::Live,
            parent,
            children: [0, 0],
            subvol,
            tree,
            skip: [0, 0, 0],
            is_ancestor: 0,
            depth,
            btime,
            deleted: false,
            flags: BchSnapshotFlags::SUBVOL,
        }
    }

    /// 创建新的 snapshot interior node（不持有子卷）
    pub fn new_interior(
        parent: u32,
        children: [u32; 2],
        tree: u32,
        depth: u32,
        btime: i64,
    ) -> Self {
        Self {
            state: SnapshotIdState::Live,
            parent,
            children,
            subvol: 0,
            tree,
            skip: [0, 0, 0],
            is_ancestor: 0,
            depth,
            btime,
            deleted: false,
            flags: BchSnapshotFlags::empty(),
        }
    }

    /// 标记为待删除
    ///
    /// 对齐 bcachefs `bch2_snapshot_node_set_deleted()`：
    /// 仅标记 `WILL_DELETE`，保留 parent / skip / is_ancestor，以便后续删除和修复还能沿该节点遍历。
    pub fn mark_deleted(&mut self) {
        self.deleted = true;
    }

    /// 是否为 leaf 节点（无子节点）
    pub fn is_leaf(&self) -> bool {
        self.children == [0, 0]
    }

    /// 是否为 interior 节点
    pub fn is_interior(&self) -> bool {
        !self.is_leaf()
    }

    /// 是否持有 SUBVOL 标志
    pub fn has_subvol(&self) -> bool {
        self.flags.contains(BchSnapshotFlags::SUBVOL)
    }

    /// 创建空槽占位条目（用于填充表 Vec 的空位）。
    ///
    /// `state=Empty` 确保 `get()` 和 `parent()` 等方法对占位返回 `None`。
    pub fn deleted_placeholder() -> Self {
        Self {
            state: SnapshotIdState::Empty,
            parent: 0,
            children: [0, 0],
            subvol: 0,
            tree: 0,
            skip: [0, 0, 0],
            is_ancestor: 0,
            depth: 0,
            btime: 0,
            deleted: true,
            flags: BchSnapshotFlags::empty(),
        }
    }
}

/// Snapshot tree 元信息，存储在 SnapshotTrees btree 中。
///
/// 每个子卷对应一棵快照树，由这个值描述其根节点和统计信息。
///
/// 对齐 bcachefs `struct bch_snapshot_tree`（fields: master_subvol, root_snapshot）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotTreeT {
    /// 所属主（master）子卷 ID。对齐 bcachefs master_subvol。
    pub master_subvol: u32,
    /// 根快照 ID（对齐 bcachefs root_snapshot）
    pub root_snapshot: u32,
}

impl SnapshotTreeT {
    /// 创建新的快照树元信息
    pub fn new(master_subvol: u32, root_snapshot: u32) -> Self {
        Self {
            master_subvol,
            root_snapshot,
        }
    }
}

impl Default for SnapshotTreeT {
    fn default() -> Self {
        Self {
            master_subvol: 0,
            root_snapshot: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_snapshot_value_new_leaf() {
        let sv = SnapshotT::new_leaf(10, 1, 0, 2, 1000);
        assert_eq!(sv.parent, 10);
        assert_eq!(sv.subvol, 1);
        assert_eq!(sv.btime, 1000);
        assert!(!sv.deleted);
        assert_eq!(sv.state, SnapshotIdState::Live);
        assert_eq!(sv.is_ancestor, 0);
        assert_eq!(sv.skip, [0, 0, 0]);
        assert_eq!(sv.depth, 2);
        assert!(sv.is_leaf());
        assert!(sv.has_subvol());
    }

    #[test]
    fn test_snapshot_value_new_interior() {
        let sv = SnapshotT::new_interior(10, [5, 3], 0, 3, 2000);
        assert_eq!(sv.parent, 10);
        assert_eq!(sv.children, [5, 3]);
        assert_eq!(sv.subvol, 0);
        assert_eq!(sv.depth, 3);
        assert!(sv.is_interior());
        assert!(!sv.has_subvol());
    }

    #[test]
    fn test_snapshot_value_mark_deleted() {
        let mut sv = SnapshotT::new_leaf(5, 1, 0, 1, 2000);
        // 模拟 skip + is_ancestor 数据
        sv.is_ancestor = 0xFFFF;
        sv.skip = [10, 20, 30];
        let original_skip = sv.skip;
        sv.mark_deleted();
        assert!(sv.deleted);
        assert_eq!(sv.state, SnapshotIdState::Live);
        assert_eq!(
            sv.is_ancestor, 0xFFFF,
            "mark_deleted 应保留 is_ancestor（对齐 bcachefs：WILL_DELETE 节点仍需要 bitmap）"
        );
        assert_eq!(sv.skip, original_skip);
    }

    #[test]
    fn test_snapshot_tree_value_new() {
        let stv = SnapshotTreeT::new(42, 100);
        assert_eq!(stv.master_subvol, 42);
        assert_eq!(stv.root_snapshot, 100);
    }

    #[test]
    fn test_snapshot_value_serde_roundtrip() {
        let sv = SnapshotT::new_leaf(3, 2, 1, 4, 5000);
        let data = bincode::serialize(&sv).unwrap();
        let restored: SnapshotT = bincode::deserialize(&data).unwrap();
        assert_eq!(restored.parent, sv.parent);
        assert_eq!(restored.subvol, sv.subvol);
        assert_eq!(restored.btime, sv.btime);
        assert_eq!(restored.tree, 1);
        assert_eq!(restored.depth, 4);
        assert!(restored.has_subvol());
        assert_eq!(restored.state, SnapshotIdState::Live);
    }

    #[test]
    fn test_snapshot_tree_value_serde_roundtrip() {
        let stv = SnapshotTreeT::new(7, 42);
        let data = bincode::serialize(&stv).unwrap();
        let restored: SnapshotTreeT = bincode::deserialize(&data).unwrap();
        assert_eq!(restored.master_subvol, stv.master_subvol);
        assert_eq!(restored.root_snapshot, stv.root_snapshot);
    }

    #[test]
    fn test_snapshot_id_state_transition() {
        let mut sv = SnapshotT::new_leaf(5, 1, 0, 1, 2000);
        assert_eq!(sv.state, SnapshotIdState::Live);
        assert!(!sv.deleted);

        let original_skip = sv.skip;
        sv.mark_deleted();
        assert_eq!(sv.state, SnapshotIdState::Live);
        assert!(sv.deleted);
        assert_eq!(sv.skip, original_skip);
    }

    #[test]
    fn test_deleted_placeholder_state() {
        let placeholder = SnapshotT::deleted_placeholder();
        assert_eq!(placeholder.state, SnapshotIdState::Empty);
        assert!(placeholder.deleted);
    }

    #[test]
    fn test_flags_delayed_set() {
        let mut flags = BchSnapshotFlags::empty();
        assert!(!flags.contains(BchSnapshotFlags::DELETED));
        flags.insert(BchSnapshotFlags::DELETED);
        assert!(flags.contains(BchSnapshotFlags::DELETED));
        assert!(!flags.contains(BchSnapshotFlags::WILL_DELETE));
        assert!(!flags.contains(BchSnapshotFlags::SUBVOL));
        assert!(!flags.contains(BchSnapshotFlags::NO_KEYS));
    }
}
