//! B-tree 内部节点操作（split/merge/rewrite/set_root）— bcachefs 对齐
//!
//! 对应 bcachefs `interior.h` + `interior.c`，提供节点级拓扑变更操作。
//!
//! ## API 概述
//!
//! | 函数 | bcachefs 对应 | 说明 |
//! |------|--------------|------|
//! | `bch2_btree_split_leaf` | `bch2_btree_split_leaf` | ⚠️ 叶节点分裂（架构差异：subvol 委托 Btree::insert 内联分裂） |
//! | `btree_split` | `btree_split` | ⚠️ test-only wrapper，非 bcachefs 完整语义 |
//! | `btree_increase_depth` | `bch2_btree_increase_depth` / `__btree_increase_depth` | ✅ 创建新根，增加树深度 |
//! | `bch2_btree_set_root_inmem` | `bch2_btree_set_root_inmem` | ✅ 更新根节点指针 |
//! | `bch2_btree_set_root_for_read` | `bch2_btree_set_root_for_read` | ✅ 读路径设置根节点 |
//!
//! ## 设计说明
//!
//! subvol 的节点分裂/合并已经在 `Btree` 上实现（`split_root`、内联 insert 分裂、
//! routing entry insertion、`bch2_foreground_maybe_merge`）。
//! 本模块只保留与 bcachefs 语义直接对应的公开 API，并补充
//! 缺少的操作（`increase_depth`、`rewrite`、`set_root`、`root_alloc_fake`）。
//!
//! 生命周期（bcachefs `btree_update` 状态机）：
//! ```text
//! Init → NodesAllocated → UpdateParent → Done
//! ```

use std::sync::Arc;

use crate::bch_vol::BchVol;
use crate::btree::key::{BchVal, Bpos, BtreeKey};
use crate::btree::node::{BsetTree, BtreeNode};
use crate::btree::types::BTREE_MAX_DEPTH;
use crate::btree::writer::BtreeNodeWriter;
use crate::btree::Btree;
use crate::StorageError;

// ---------------------------------------------------------------------------
// 更新模式 & 重写原因 — bcachefs 对齐
// ---------------------------------------------------------------------------

/// Btree 节点重写原因 — 对应 bcachefs `enum btree_node_rewrite_reason`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum BtreeNodeRewriteReason {
    /// 非重写（默认）
    None = 0,
    /// 格式迁移：节点 key format 已变更需重写
    Format = 1,
    /// 节点损伤：CRC 校验失败等需要重写恢复
    Corrupt = 2,
    /// 内部节点需重新计算格式
    InternalFormat = 3,
    /// 预分裂后需重写节点（shard 对齐等）
    PreSplit = 4,
    /// 快照清理后需重写节点
    SnapshotCleanup = 5,
}

// ---------------------------------------------------------------------------
// 预留计算辅助函数
// ---------------------------------------------------------------------------

/// bcachefs 对齐: btree_update_reserve_required（interior.h:251-265）
///
/// 计算最坏情况下节点分裂所需的预留节点数。
///
/// 对应本地 `btree_update_reserve_required()`（`interior.h:251-265`）：
/// 从当前 btree root 读取深度，一直分裂到根节点，然后分配一个新根，除非已达最大深度。
pub(crate) fn btree_update_reserve_required(btree: &Btree, b: &BtreeNode) -> usize {
    let depth = usize::from(btree.root().node.level) + 1;
    let node_level = usize::from(b.level);
    if depth < BTREE_MAX_DEPTH {
        (depth - node_level) * 2 + 1
    } else {
        (depth - node_level) * 2 - 1
    }
}

/// 本地 bcachefs-tools 的 static-key 兼容层固定保持该 debug 参数为 false。
const BCH2_BTREE_NODE_MERGING_DISABLED: bool = false;

/// 检查节点是否需要合并。
///
/// 对应本地 `btree_node_needs_merge()`（`interior.h:194-201`）：先检查全局
/// merging-disabled static key，再将 sibling 估计和 delta 与文件系统级阈值比较。
pub(crate) fn btree_node_needs_merge(c: &BchVol, b: &BtreeNode, d: i32) -> bool {
    if BCH2_BTREE_NODE_MERGING_DISABLED {
        return false;
    }

    i32::from(b.sib_u64s[0].min(b.sib_u64s[1])) + d <= i32::from(c.btree_foreground_merge_threshold)
}

/// 重置节点的 sibling u64s 估计值。
///
/// 对应本地 `btree_node_reset_sib_u64s()`（`interior.h:267-271`）。
pub(crate) fn btree_node_reset_sib_u64s(b: &mut BtreeNode) {
    let live_u64s = b.live_data_bytes() / 8;
    let live_u64s = live_u64s.min(u16::MAX as u32) as u16;
    b.sib_u64s[0] = if b.min_key != Bpos::MIN {
        live_u64s
    } else {
        u16::MAX
    };
    b.sib_u64s[1] = if b.max_key != Bpos::MAX {
        live_u64s
    } else {
        u16::MAX
    };
}

// ---------------------------------------------------------------------------
// BtreeInteriorUpdate — 内部更新的 Rust 封装（引用 update.rs 的现有类型）
// ---------------------------------------------------------------------------

/// 重新导出 `BtreeInteriorUpdate` 类型别名
pub use crate::btree::update::{
    BtreeInteriorUpdate, BtreeUpdateMode, InteriorUpdateState, InteriorUpdateType,
};

// ---------------------------------------------------------------------------
// 分裂操作（Split）
// ---------------------------------------------------------------------------

/// 叶节点分裂 — 对应 bcachefs `bch2_btree_split_leaf`（`interior.c:2281-2320`）
///
/// 当叶节点满时触发分裂。如果分裂向上传播至根节点，触发根节点分裂。
///
/// 对应 bcachefs `interior.c:2281-2320` 的状态机：
/// ```text
/// bch2_btree_update_start → lock_write → btree_split(keys=NULL) → update_done → maybe_merge 链
/// ```
///
/// ⚠️ **架构差异**：bcachefs 显式管理 `struct btree_update` 生命周期，
/// 包含 `btree_split`（分配节点→pack→insert keys→update parent→free old→write new）。
/// subvol 委托 `Btree::insert` 实现内联分裂，不暴露显式 split 状态机。
///
/// # 错误恢复
///
/// 对应 bcachefs `interior.c:2317-2318`: `bch2_btree_update_free(as, trans, true)`
///
/// # 参数
///
/// * `btree` — 目标 btree
/// * `writer` — 节点写入器
/// * `target` — 触发分裂的 key
/// * `value` — 触发分裂的 value
///
/// # 返回值
///
/// `Ok(true)` 表示分裂成功，`Ok(false)` 表示需重试（write_blocked 冲突），
/// `Err(...)` 表示不可恢复错误。
pub async fn bch2_btree_split_leaf<W: BtreeNodeWriter>(
    btree: &Btree,
    writer: &W,
    target: &BtreeKey,
    value: &BchVal,
) -> Result<bool, StorageError> {
    // bcachefs `bch2_btree_split_leaf()` creates exactly one
    // `btree_update` and then calls `btree_split()`.  The insertion path owns
    // that lifecycle here; do not pre-acquire the update guard and make the
    // nested split see its own update as a conflict.
    btree
        .bch2_btree_insert_trans(writer, *target, *value, None, 0)
        .await
}

/// 通用节点分裂（test-only）— 对应 bcachefs `btree_split`（`interior.c:1962-2174`）
///
/// bcachefs `btree_split`（static，line 1962-2174）核心逻辑：
/// 1. 拓扑检查 + compact-vs-split 决策（`new_key_u64s` + `compact_fits`）
/// 2. `find_balanced_split` — 计算平衡分裂点
/// 3. `bch2_btree_node_alloc` — 分配 dst 节点
/// 4. `btree_pack_into_dsts` — 将 src keys 打包到 dst
/// 5. `btree_split_insert_keys` — 插入新 key 到 dst
/// 6. `bch2_btree_build_aux_trees` — 构建辅助搜索树
/// 7. `bch2_btree_update_emit_new_node_key` + `bch2_keylist_add` — 发射新节点 key
/// 8. 有 parent → `bch2_btree_insert_node`；无 parent+n3 → `bch2_btree_set_root`；否则替换 root
/// 9. `bch2_btree_interior_update_will_free_node` — 转移 journal pin
/// 10. `bch2_btree_update_write_new_node` — 写盘
/// 11. `bch2_btree_node_free_inmem` — 释放旧节点
/// 12. `bch2_trans_node_add` — 加入事务
///
/// ⚠️ **架构差异**：subvol 版本仅为 test-only wrapper，直接委托 `Btree::insert`。
/// 不包含 dst allocation、pack、parent update、journal pin 转移等完整逻辑。
#[cfg(test)]
async fn btree_split<W: BtreeNodeWriter>(
    btree: &Btree,
    writer: &W,
    _node_addr: u64,
    trigger_key: &BtreeKey,
    trigger_val: &BchVal,
) -> Result<bool, StorageError> {
    // 检查是否为根节点
    if btree.depth() == 0 {
        // depth=0: 根节点就是唯一的 leaf
        return btree
            .bch2_btree_insert(writer, *trigger_key, *trigger_val, 0)
            .await;
    }

    // 构建从 root 到目标节点的路径
    let mut path = Vec::new();
    let leaf_addr = btree.bch2_btree_path_traverse_one(trigger_key, &mut path);

    if Some(_node_addr) == leaf_addr {
        // 到达叶节点：使用 insert（内含分裂逻辑）
        btree
            .bch2_btree_insert(writer, *trigger_key, *trigger_val, 0)
            .await
    } else {
        // 非叶节点分裂：回退到正常插入路径，让 Btree 自己沿路径递归分裂。
        // 这比静默返回 false 更接近 bcachefs 的\u201c分裂向上传播\u201d语义，
        // 同时避免把可继续推进的更新误判成失败。
        btree
            .bch2_btree_insert(writer, *trigger_key, *trigger_val, 0)
            .await
    }
}

/// 内存中设置根节点 — 对应 bcachefs `bch2_btree_set_root_inmem`（`interior.c:1606-1626`）
///
/// 对应 bcachefs `interior.c:1606-1626` static 函数，将节点注册为 btree 的新根。
///
/// bcachefs 实现：
/// 1. `set_btree_node_permanent(b)` — 标记根节点不可回收（subvol 无此概念）
/// 2. `bch2_btree_id_root(c, id)->b = b` — 设置根指针
/// 3. `roots_b[btree_id] = bch2_btree_root_pack(b)` — 更新 roots 数组
/// 4. `bch2_recalc_btree_reserve(c)` — 重新计算预留（subvol 无此概念）
///
/// # 参数
///
/// * `btree` — 目标 btree
/// * `new_root` — 新的根节点（level 必须 ≥ 原根 level）
/// * `root_addr` — 根节点的磁盘地址（用于 PendingRootJournal + current_root_disk）
///
/// # 说明
///
/// 函数内部持有 `root_lock`，统一保护内存指针和 journal 元数据发布；
/// 对应 bcachefs `interior.c:1628-1645` 的 `bch2_btree_set_root`（static）则额外通过
/// `btree_update_updated_root` 触发 journal entry 写入。
pub(crate) fn bch2_btree_set_root_inmem(btree: &Btree, new_root: Arc<BtreeNode>, root_addr: u64) {
    // bcachefs interior.c:1606-1626 serializes root publication under the
    // cache/root lock pair. Keep the lock boundary inside this API so every
    // caller, including recovery and tests, gets the same publication order.
    let _lock = btree.root_lock.lock().unwrap();
    let new_level = new_root.level;
    let root = unsafe { &mut *btree.root.get() };
    root.node = new_root;
    root.depth = new_level;
    unsafe {
        *btree.pending_root_journal.get() = Some(crate::btree::types::PendingRootJournal {
            root_addr,
            level: new_level,
        });
        *btree.current_root_disk.get() = Some((root_addr, new_level));
    }
}

/// 读路径设置根节点 — 对应 bcachefs `bch2_btree_set_root_for_read`（`interior.c:3633-3638`）
///
/// 对应 bcachefs `interior.c:3633-3638`：
/// ```c
/// BUG_ON(btree_node_root(c, b));  // 节点不能已是当前 root
/// bch2_btree_set_root_inmem(c, b);
/// ```
///
/// 从后端读出根节点后调用，直接将节点设置为根。
/// 与 `bch2_btree_set_root_inmem` 不同，此函数假设节点已经是当前树的根，
/// 不验证 level，但必须不是已经挂在树上的当前 root。
///
/// bcachefs `btree_node_root(c, b)` 检查 b 是否等于 `c->btree.roots_b[id]`；
/// subvol 用 `Arc::ptr_eq` 等价检查。
pub fn bch2_btree_set_root_for_read(btree: &mut Btree, node: Arc<BtreeNode>) {
    assert!(
        !Arc::ptr_eq(&btree.root().node, &node),
        "bch2_btree_set_root_for_read must not be called with the current root"
    );
    btree.set_root_internal(node);
}

// ---------------------------------------------------------------------------
// 深度增长（Increase Depth）
// ---------------------------------------------------------------------------

/// 增加 btree 深度 — 对应 bcachefs `bch2_btree_increase_depth`（`interior.c:2369-2391`）
/// 核心实现对应 `__btree_increase_depth`（`interior.c:2322-2367`）
///
/// bcachefs `bch2_btree_increase_depth`（line 2369-2391）：
/// 1. 如果是 fake 根 → 委托 `bch2_btree_split_leaf`
/// 2. 否则：`bch2_btree_update_start` → `__btree_increase_depth` → `update_done`
///
/// bcachefs `__btree_increase_depth`（line 2322-2367）：
/// 1. `bch2_btree_node_lock_write` 锁定旧根
/// 2. `__btree_root_alloc(as, trans, level+1)` 分配新根节点
/// 3. `path->locks_want++; btree_path_take_new_node` 将新节点绑定到路径
/// 4. `n->sib_u64s[0] = U16_MAX; n->sib_u64s[1] = U16_MAX;` — 新根是边界节点
/// 5. `bch2_keylist_add(&as->parent_keys, &b->key)` — 加入旧根的 btree ptr key
/// 6. `btree_split_insert_keys` 将旧根 key 插入新根节点
/// 7. `bch2_btree_update_emit_new_node_key` + `bch2_btree_set_root` 注册
/// 8. `bch2_btree_update_write_new_node` 标记写盘
/// 9. `bch2_trans_node_add` 添加到事务节点列表
/// 10. `bch2_btree_node_unlock_write` 解锁旧根
/// 11. `clear_btree_node_permanent(b)` — 旧根不再受 permanent 保护
///
/// subvol 差异：
/// - 新根节点在构造函数中已设置 `sib_u64s = [u16::MAX; 2]`（对应 line 2339-2340）
/// - 不使用 `btree_path` 路径抽象
/// - 不使用 `btree_update` 状态机
/// - 无 permanent 标志（subvol 使用不同驱逐策略）
///
/// # 返回值
///
/// 返回新根节点的 Arc<BtreeNode>。
pub async fn bch2_btree_increase_depth<W: crate::btree::writer::BtreeNodeWriter>(
    btree: &Btree,
    child_addr: u64,
    writer: &W,
) -> Result<Arc<BtreeNode>, StorageError> {
    let old_root = unsafe { &*btree.root.get() };
    let old_depth = old_root.depth;

    if old_depth as usize >= crate::btree::types::BTREE_MAX_DEPTH {
        return Err(StorageError::InvalidArgument(
            "btree max depth exceeded".into(),
        ));
    }

    let new_level = old_depth + 1;
    let mut new_root = crate::btree::node::BtreeNode::new_internal();
    new_root.level = new_level;
    new_root.node_size = old_root.node.node_size;
    if let Some(vol) = btree.vol_arc() {
        new_root.set_vol_ref(&vol);
    }

    let mut cur = u32::from(crate::btree::node::BSET_HEADER_U64S) * 8;
    cur += new_root.write_entry(
        cur,
        &crate::btree::key::BtreeKey::MIN_KEY,
        &crate::btree::key::BchVal::new(child_addr, 0),
        0,
    );
    new_root.sets[0] = BsetTree {
        size: 0,
        extra: crate::btree::node::BSET_NO_AUX_TREE_VAL,
        data_offset: 0,
        aux_data_offset: u16::MAX,
        end_offset: (cur / 8) as u16,
    };
    new_root.packed_keys = 1;
    new_root.unpacked_keys = 0;
    new_root.min_key = old_root.node.min_key;
    new_root.max_key = old_root.node.max_key;

    let root_arc = Arc::new(new_root);
    // bcachefs 对齐：写盘前设置 will_make_reachable，IO 回调中清理
    root_arc.set_will_make_reachable();
    let root_addr = writer
        .write_btree_node(root_arc.clone(), crate::types::Watermark::Btree)
        .await?;

    // bcachefs 对齐：root op lock 保护根指针切换
    let _lock = btree.root_lock.lock().unwrap();
    unsafe { &mut *btree.root.get() }.node = root_arc.clone();
    unsafe { &mut *btree.root.get() }.depth += 1;

    // 存储 PendingRootJournal + current_root_disk
    unsafe {
        *btree.pending_root_journal.get() = Some(crate::btree::types::PendingRootJournal {
            root_addr,
            level: root_arc.level,
        });
        *btree.current_root_disk.get() = Some((root_addr, root_arc.level));
    }
    Ok(root_arc)
}

// ---------------------------------------------------------------------------
// 合并操作（Merge）
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btree::key::{BtreeKey, ExtentValue, KeyType};
    use crate::btree::node::BtreeNode;
    use crate::btree::writer::NoopWriter;

    #[test]
    fn test_btree_update_reserve_required_leaf() {
        // depth=3, node_level=0（leaf）→ (3-0)*2+1 = 7
        let btree = Btree::new();
        btree.root_node_mut_internal().level = 2;
        let node = BtreeNode::new_leaf();
        let r = btree_update_reserve_required(&btree, &node);
        assert_eq!(r, 7);
    }

    #[test]
    fn test_btree_update_reserve_required_max_depth() {
        // depth = BTREE_MAX_DEPTH, node_level = 0 → (BTREE_MAX_DEPTH-0)*2-1
        let btree = Btree::new();
        btree.root_node_mut_internal().level = BTREE_MAX_DEPTH as u8 - 1;
        let node = BtreeNode::new_leaf();
        let r = btree_update_reserve_required(&btree, &node);
        assert_eq!(r, BTREE_MAX_DEPTH * 2 - 1);
    }

    #[test]
    fn test_btree_node_needs_merge_empty_leaf() {
        let vol = BchVol::test_trees();
        let mut node = BtreeNode::new_leaf();
        // 空节点 data_bytes = 0 < node_size/3 → 如果 sib_u64s 允许则触发
        // 默认 sib_u64s = [u16::MAX; 2]（边界节点，无兄弟）→ false
        assert!(!btree_node_needs_merge(&vol, &node, 0));
        // 设置模拟的兄弟估计值 → 应触发
        node.sib_u64s = [100, 100];
        assert!(btree_node_needs_merge(&vol, &node, 0));
        // delta 促进：负 delta 使空节点更容易触发
        assert!(btree_node_needs_merge(&vol, &node, -50));
        // delta 抑制：正 delta 可阻止触发
        let threshold = i32::from(vol.btree_foreground_merge_threshold);
        assert!(!btree_node_needs_merge(&vol, &node, threshold));

        // U16_MAX 不是独立的早退条件；本地表达式仍先加 delta 再比较。
        node.sib_u64s = [u16::MAX; 2];
        assert!(btree_node_needs_merge(&vol, &node, -(u16::MAX as i32)));
    }

    #[test]
    fn test_btree_node_needs_merge_full_leaf() {
        let vol = BchVol::test_trees();
        let mut node = BtreeNode::new_leaf();
        for i in 0..6000u64 {
            if !node.insert(BtreeKey::new(i, 1, KeyType::Normal), BchVal::new(i * 10, 0)) {
                break;
            }
        }
        assert!(
            node.total_data_bytes() > node.node_size / 3,
            "node {} bytes > 1/3 of {} after 6000 entries",
            node.total_data_bytes(),
            node.node_size
        );
        // sib_u64s 为 u16::MAX（边界节点）→ 不应触发
        assert!(!btree_node_needs_merge(&vol, &node, 0));
        // 设小 sib_u64s → 应触发（兄弟节点数据很少，值得合并）
        node.sib_u64s = [100, 100];
        // min_sib(100) + 0 = 100 << threshold(≈10922) → needs merge
        assert!(btree_node_needs_merge(&vol, &node, 0));
    }

    #[test]
    fn test_btree_node_reset_sib_u64s() {
        let mut node = BtreeNode::new_leaf();
        for i in 0..100u64 {
            node.insert(BtreeKey::new(i, 1, KeyType::Normal), BchVal::new(i * 10, 0));
        }
        btree_node_reset_sib_u64s(&mut node);
        // 非边界节点应该有合理的 sib_u64s
        let live_u64s = (node.live_data_bytes() / 8) as u16;
        assert_eq!(node.sib_u64s, [live_u64s; 2]);
        // 边界节点检查
        node.min_key = Bpos::MIN;
        btree_node_reset_sib_u64s(&mut node);
        assert_eq!(node.sib_u64s[0], u16::MAX);
    }

    #[test]
    fn test_btree_node_reset_sib_u64s_uses_live_keys() {
        let mut node = BtreeNode::new_leaf();
        let key = BtreeKey::new(1, 1, KeyType::Normal);
        assert!(node.insert(key, BchVal::new(10, 0)));
        node.compact();
        // bcachefs: write + prep_for_write 流程确保 delete 有增量 bset 接收
        // Deleted 条目，compact 状态下 delete 追加到压缩 set 末尾
        assert!(node.delete_key(&key));
        // bcachefs: delete_key 在 writable bset 中物理删除条目（bset_delete），
        // 不会留下 Deleted 墓碑。因此 total_data == live_data == key_count == 0。
        assert_eq!(node.packed_keys + node.unpacked_keys, 0);
        assert_eq!(node.total_data_bytes(), 0);
        assert_eq!(node.live_data_bytes(), 0);

        node.min_key = Bpos::new(1, 0, 0);
        node.max_key = Bpos::new(1, 2, 0);
        btree_node_reset_sib_u64s(&mut node);
        assert_eq!(node.sib_u64s, [0, 0]);
    }

    #[test]
    fn test_btree_set_root_basic() {
        let btree = Btree::new();
        let old_depth = btree.depth();

        let new_node = Arc::new(BtreeNode::new_internal());
        assert_eq!(new_node.level, 1);

        bch2_btree_set_root_inmem(&btree, new_node, 1);
        assert_eq!(btree.depth(), old_depth + 1);
        // 验证 current_root_disk 已设置
        assert_eq!(
            btree.current_root_disk_info(),
            Some((1, 1)),
            "set_root should set current_root_disk"
        );
    }

    #[test]
    fn test_bch2_btree_set_root_for_read() {
        let mut btree = Btree::new();
        let node = Arc::new(BtreeNode::new_leaf());
        bch2_btree_set_root_for_read(&mut btree, node);
        assert_eq!(btree.depth(), 0);
    }

    #[test]
    #[should_panic(
        expected = "bch2_btree_set_root_for_read must not be called with the current root"
    )]
    fn test_bch2_btree_set_root_for_read_rejects_current_root() {
        let mut btree = Btree::new();
        let node = btree.root().node.clone();
        bch2_btree_set_root_for_read(&mut btree, node);
    }

    #[tokio::test]
    async fn test_bch2_btree_split_leaf_basic() {
        let btree = Btree::new();
        let writer = NoopWriter;
        // 小节点加速分裂
        let root = btree.root_node_mut_internal();
        root.node_size = 256;

        // 插入 key 直到触发分裂
        for i in 0..50u64 {
            assert!(btree
                .bch2_btree_insert(
                    &writer,
                    BtreeKey::new(i, 1, KeyType::Normal),
                    BchVal::new(i * 10, 0),
                    0,
                )
                .await
                .unwrap());
        }
        // 分裂后 key 应该都在
        for i in 0..50u64 {
            let found = btree.bch2_btree_iter_peek(&BtreeKey::new(i, 1, KeyType::Normal));
            assert!(found.is_some(), "key {} should exist after splits", i);
        }
    }

    #[tokio::test]
    async fn test_btree_split_falls_back_to_insert_when_node_addr_mismatch() {
        let btree = Btree::new();
        let writer = NoopWriter;
        btree.root_node_mut_internal().node_size = 256;

        for i in 0..32u64 {
            assert!(btree
                .bch2_btree_insert(
                    &writer,
                    BtreeKey::new(i, 1, KeyType::Normal),
                    BchVal::new(i * 10, 0),
                    0,
                )
                .await
                .unwrap());
        }

        let key = BtreeKey::new(100, 1, KeyType::Normal);
        let value = BchVal::new(1000, 0);

        assert!(btree_split(&btree, &writer, 0xDEAD_BEEF, &key, &value)
            .await
            .unwrap());
        assert_eq!(
            btree.bch2_btree_iter_peek(&key),
            Some((key, value)),
            "fallback insert should keep the new key reachable"
        );
    }

    #[tokio::test]
    async fn test_btree_increase_depth_small() {
        let btree = Btree::new();
        let writer = crate::btree::writer::NoopWriter;
        let child_addr = 42;
        let new_root = bch2_btree_increase_depth(&btree, child_addr, &writer)
            .await
            .unwrap();

        assert_eq!(btree.depth(), 1);
        assert_eq!(new_root.packed_keys + new_root.unpacked_keys, 1);

        let set = &new_root.sets[0];
        let (key, value) = new_root.read_entry(set, 1);
        assert_eq!(key, BtreeKey::MIN_KEY);
        assert_eq!(
            value,
            ExtentValue {
                paddr: child_addr,
                size: 1,
                ver: 0,
                crc32c: 0,
                crc_offset_blocks: 0,
                dev_idx: 0
            }
        );

        // 验证 current_root_disk 和 PendingRootJournal
        let (disk_addr, disk_level) = btree.current_root_disk_info().unwrap();
        assert!(
            disk_addr > 0,
            "current_root_disk should have a valid address"
        );
        assert_eq!(disk_level, 1, "new root level should be 1");
        let prj = btree.take_pending_root_journal().unwrap();
        assert_eq!(prj.root_addr, disk_addr);
        assert_eq!(prj.level, 1);
    }

    #[tokio::test]
    async fn test_btree_merge_after_delete() {
        let btree = Btree::new();
        let writer = NoopWriter;

        // 小节点强制分裂
        btree.root_node_mut_internal().node_size = 512;

        // 插入 30 个 key → 多个 leaf
        for i in 0..30u64 {
            assert!(btree
                .bch2_btree_insert(
                    &writer,
                    BtreeKey::new(i, 1, KeyType::Normal),
                    BchVal::new(i * 10, 0),
                    0,
                )
                .await
                .unwrap());
        }

        // 从左 leaf 删除大量 key，触发合并
        for i in 0..12u64 {
            assert!(
                btree
                    .bch2_btree_delete(&writer, &BtreeKey::new(i, 1, KeyType::Normal), 0)
                    .await
                    .unwrap(),
                "delete failed at i={}",
                i
            );
        }

        // 验证剩余 key 可达
        for i in 12..30u64 {
            let found = btree.bch2_btree_iter_peek(&BtreeKey::new(i, 1, KeyType::Normal));
            assert!(found.is_some(), "key {} should exist after merge", i);
        }
    }

    #[test]
    fn test_btree_reserve_required_constants() {
        // depth=2, node_level=1 → (2-1)*2+1 = 3
        let btree = Btree::new();
        btree.root_node_mut_internal().level = 1;
        let node = BtreeNode::new_internal();
        assert_eq!(btree_update_reserve_required(&btree, &node), 3);

        // 本地 BTREE_MAX_DEPTH=4；root level=3 时走 max-depth 分支。
        btree.root_node_mut_internal().level = BTREE_MAX_DEPTH as u8 - 1;
        let leaf = BtreeNode::new_leaf();
        assert_eq!(btree_update_reserve_required(&btree, &leaf), 7);
    }
}
