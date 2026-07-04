//! BtreeTrans — B-tree 事务
//!
//! 对应 bcachefs `struct btree_trans` (btree/types.h:792)
//! 事务管理一组原子 btree 操作，通过 Journal 保证持久化。

use std::sync::Arc;

use crate::bch_vol::BchVol;
use crate::btree::key::{Bpos, BtreeEntry};
use crate::btree::tree::{Btree, BtreeIter, BtreeIterPath};
use crate::btree::types::BtreeId;
use crate::engine::Allocator;
use crate::journal::JournalRes;
use crate::types::{StorageError, Watermark};

/// 编码 journal BtreeKeys 载荷：前 20 字节为 Bpos，后跟 entry payload
fn encode_journal_payload(pos: &Bpos, entry_type: u8, entry_payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(21 + entry_payload.len());
    buf.extend_from_slice(&pos.inode.to_le_bytes());
    buf.extend_from_slice(&pos.offset.to_le_bytes());
    buf.extend_from_slice(&pos.snapshot.to_le_bytes());
    buf.push(entry_type);
    buf.extend_from_slice(entry_payload);
    buf
}

// ═══════════════════════════════════════════════════════════════
// BtreeProvider — Btree 解析 trait
// ═══════════════════════════════════════════════════════════════

/// BtreeProvider — 按 btree_id 提供 &mut Btree 的 trait
///
/// 对应 bcachefs `bch2_btree_id_root` / `c->btree_roots[btree_id].b`
pub trait BtreeProvider: Send {
    fn get_btree(&mut self, id: BtreeId) -> &mut Btree;
}

// ═══════════════════════════════════════════════════════════════
// BtreeTransEntry — 事务中的单条更新
// ═══════════════════════════════════════════════════════════════

/// BtreeTransEntry — 事务中的单条待处理更新
///
/// 对应 bcachefs `struct btree_insert_entry` (btree/types.h:684)
/// 通过 `path_index` 引用 `BtreeTrans.paths[]` 中已遍历好的路径。
#[derive(Debug, Clone)]
pub struct BtreeTransEntry {
    /// 操作类型: 0=insert, 1=delete（btree_insert_entry.flags 中编码）
    pub entry_type: u8,

    /// 负载数据（对应 btree_insert_entry.k 的 payload）
    pub payload: Vec<u8>,

    /// 引用的事务路径索引（对应 btree_insert_entry.path_index）
    pub path_index: usize,
}

// ═══════════════════════════════════════════════════════════════
// BtreeTrans — 事务核心
// ═══════════════════════════════════════════════════════════════

/// BtreeTrans — B-tree 事务
///
/// 对应 bcachefs `struct btree_trans` (btree/types.h:792)
///
/// 字段对应:
/// - `vol` → `struct bch_fs *c` (types.h:793) — volume 引用
/// - `paths` → `struct btree_path *paths` (types.h:795) — 已遍历路径
/// - `updates` → `struct btree_insert_entry *updates` (types.h:796) — 更新列表
pub struct BtreeTrans {
    /// Volume 引用
    vol: Arc<BchVol>,

    /// 已遍历路径（对应 bcachefs trans->paths）
    pub(crate) paths: Vec<BtreeIterPath>,

    /// 待处理更新（对应 bcachefs trans->updates）
    updates: Vec<BtreeTransEntry>,

    /// 提交后的 journal 序列号
    journal_seq: Option<u64>,

    /// journal reservation
    journal_res: Option<JournalRes>,

    /// 是否已提交
    committed: bool,

    /// journal replay 提交不再次写入 journal
    journal_replay: bool,

    /// 本次提交中发生了 split_root 的 btree id 列表
    root_changed: Vec<BtreeId>,
}

// ═══════════════════════════════════════════════════════════════
// 构造函数
// ═══════════════════════════════════════════════════════════════

impl BtreeTrans {
    pub fn new(vol: &Arc<BchVol>) -> Self {
        Self {
            vol: vol.clone(),
            paths: Vec::new(),
            updates: Vec::new(),
            journal_seq: None,
            journal_res: None,
            committed: false,
            journal_replay: false,
            root_changed: Vec::new(),
        }
    }

    pub fn with_capacity(vol: &Arc<BchVol>, capacity: usize) -> Self {
        Self {
            vol: vol.clone(),
            paths: Vec::new(),
            updates: Vec::with_capacity(capacity),
            journal_seq: None,
            journal_res: None,
            committed: false,
            journal_replay: false,
            root_changed: Vec::new(),
        }
    }

    pub fn new_replay(vol: &Arc<BchVol>) -> Self {
        let mut trans = Self::new(vol);
        trans.journal_replay = true;
        trans
    }
}

// ═══════════════════════════════════════════════════════════════
// 迭代器与路径创建
// ═══════════════════════════════════════════════════════════════

impl BtreeTrans {
    /// 在事务中创建 btree 迭代器
    ///
    /// 对应 bcachefs `bch2_trans_iter_init()` (iter.h:806)
    /// - `want_intent=true`：持意向锁，路径存入 `self.paths`，用于后续写操作
    /// - `want_intent=false`：持读锁（standalone），仅用于只读查找
    pub fn iter(
        &mut self,
        alloc: &Allocator,
        btree_id: BtreeId,
        pos: Bpos,
        want_intent: bool,
    ) -> BtreeIter {
        let tree: &Btree = alloc.get_btree_ref(btree_id);
        if want_intent {
            let path_idx = self.paths.len();
            self.paths
                .push(BtreeIterPath::traverse(tree, btree_id, &pos, true));
            let path_ptr = &mut self.paths[path_idx] as *mut BtreeIterPath;
            BtreeIter::from_trans(tree as *const Btree, pos, path_idx, path_ptr)
        } else {
            BtreeIter::new(tree, pos)
        }
    }

    /// 返回路径数量
    pub fn path_count(&self) -> usize {
        self.paths.len()
    }
}

// ═══════════════════════════════════════════════════════════════
// 更新操作
// ═══════════════════════════════════════════════════════════════

impl BtreeTrans {
    /// 创建路径并持意图锁（对应 bcachefs `bch2_trans_iter_init` + traverse）
    ///
    /// traverses root→leaf，全路径持 intent/read 锁。
    /// 返回 path_index 供后续 `update()` 注册更新条目。
    pub fn prepare(
        &mut self,
        alloc: &Allocator,
        btree_id: BtreeId,
        pos: Bpos,
        want_intent: bool,
    ) -> usize {
        let tree = alloc.get_btree_ref(btree_id);
        let path = BtreeIterPath::traverse(tree, btree_id, &pos, want_intent);
        let path_idx = self.paths.len();
        self.paths.push(path);
        path_idx
    }

    /// 注册更新条目到已有路径（对应 bcachefs `bch2_trans_update`）
    ///
    /// path 必须已通过 `prepare()` 创建并持 intent 锁。
    /// 纯注册操作，不自建路径。
    pub fn update(&mut self, entry_type: u8, payload: Vec<u8>, path_index: usize) {
        assert!(path_index < self.paths.len(), "btree transaction path index out of range");
        assert_ne!(
            self.paths[path_index].nodes_locked,
            0,
            "btree transaction update requires a locked iterator path"
        );
        self.updates.push(BtreeTransEntry {
            entry_type,
            payload,
            path_index,
        });
    }

    /// 从迭代器添加更新（对应 bcachefs `bch2_trans_update`）
    ///
    /// 通过 `iter.path_index()` 引用已存在的路径。
    /// 注意：iter 生命周期与 trans 的 `&mut self` 冲突时无法调用。
    ///       推荐使用 `prepare()` + `update()` API。
    pub fn update_from_iter(&mut self, iter: &BtreeIter, entry_type: u8, payload: Vec<u8>) {
        assert!(!iter.transaction_path_ptr().is_null(), "btree update requires a transaction iterator");
        let path_index = iter.path_index();
        assert!(path_index < self.paths.len(), "btree iterator path index out of range");
        let expected = unsafe { self.paths.as_ptr().add(path_index) as *mut BtreeIterPath };
        assert_eq!(
            iter.transaction_path_ptr(),
            expected,
            "btree iterator belongs to a different transaction"
        );
        assert_ne!(
            self.paths[path_index].nodes_locked,
            0,
            "btree update requires a locked iterator path"
        );
        self.updates.push(BtreeTransEntry {
            entry_type,
            payload,
            path_index,
        });
    }

    pub fn pending_count(&self) -> usize {
        self.updates.len()
    }

    pub fn is_empty(&self) -> bool {
        self.updates.is_empty()
    }

    pub fn clear_updates(&mut self) {
        self.updates.clear();
    }
}

// ═══════════════════════════════════════════════════════════════
// LockedLeaf — 写锁持有的叶子节点（纯分组，不管理锁生命周期）
// ═══════════════════════════════════════════════════════════════

struct LockedLeaf {
    node: *mut crate::btree::node::BtreeNode,
    node_idx: usize,
    indices: Vec<usize>,
    path_index: usize,
    btree_id: BtreeId,
}

/// Safety: node 指向 BtreeNode，由 Arc<BchVol> 保证存活
unsafe impl Send for LockedLeaf {}

impl LockedLeaf {
    fn node(&self) -> &mut crate::btree::node::BtreeNode {
        unsafe { &mut *self.node }
    }
}

// ═══════════════════════════════════════════════════════════════
// 事务提交
// ═══════════════════════════════════════════════════════════════

impl BtreeTrans {
    fn calc_journal_bytes(&self) -> u32 {
        let hdr_size = std::mem::size_of::<crate::journal::JsetEntryHeader>() as u32;
        let pos_extra = 21u32;
        let root_entry_size = hdr_size + 10;
        self.updates
            .iter()
            .map(|u| hdr_size + pos_extra + u.payload.len() as u32)
            .sum::<u32>()
            + root_entry_size * self.root_changed.len() as u32
    }

    /// 从路径获取叶子节点的可变指针
    fn leaf_ptr(
        path: &BtreeIterPath,
        tree: &mut Btree,
    ) -> (usize, *mut crate::btree::node::BtreeNode) {
        let lvl = path.l[0].as_ref().expect("path has leaf");
        match lvl.node_idx {
            usize::MAX => (usize::MAX, &mut tree.root as *mut _),
            idx => (idx, &mut tree.child_nodes[idx] as *mut _),
        }
    }

    /// 释放事务中所有路径持有的锁（用于 split 重试前或提交结束）
    ///
    /// `unlock_all` 通过 `node_ptr` 解引用，无需 Btree 引用。
    /// 同时重置 `nodes_locked` 为 0，使 Drop 下安全（不会重复解锁）。
    fn unlock_all_paths(&mut self) {
        for p in &mut self.paths {
            p.unlock_all();
        }
    }

    fn rebuild_paths_after_split(&mut self, alloc: &Allocator) {
        let specs: Vec<(BtreeId, Bpos)> = self
            .updates
            .iter()
            .map(|update| {
                let path = &self.paths[update.path_index];
                (path.btree_id, path.pos)
            })
            .collect();
        self.paths.clear();
        self.paths.reserve(specs.len());
        for (index, (btree_id, pos)) in specs.into_iter().enumerate() {
            let tree = alloc.get_btree_ref(btree_id);
            self.paths
                .push(BtreeIterPath::traverse(tree, btree_id, &pos, true));
            self.updates[index].path_index = index;
        }
    }

    /// 提交事务 — bcachefs 风格锁升级流程
    ///
    /// 对应 bcachefs `__bch2_trans_commit()` (commit.c:1381-1523)
    ///
    /// 前置条件：所有路径已在 `iter()`/`update()` 时持有了 intent 锁。
    /// commit 只需将 leaf intent→write，完成后 downgrade write→intent 并释放。
    ///
    /// 流程:
    ///   0) 收集所有涉及的路径，升级 leaf intent→write
    ///   1) 按叶子节点分组
    ///   2) 校验空间（不足则 split 重试）
    ///   3) 预分配 journal + 写入
    ///   4) 在写锁下插入 key
    ///   5) 释放写锁（downgrade write→intent）
    ///   6) flush 脏节点
    ///   7) 释放所有路径锁
    pub async fn commit(&mut self, alloc: &mut Allocator) -> Result<(), StorageError> {
        if self.updates.is_empty() {
            return Ok(());
        }

        const MAX_BATCH_UPDATES: usize = 512;
        if !self.journal_replay && self.updates.len() > MAX_BATCH_UPDATES {
            let pending: Vec<(BtreeId, Bpos, u8, Vec<u8>)> = self
                .updates
                .iter()
                .map(|update| {
                    let path = &self.paths[update.path_index];
                    (path.btree_id, path.pos, update.entry_type, update.payload.clone())
                })
                .collect();
            let max_journal_bytes = crate::journal::BUF_SIZE
                .saturating_sub(std::mem::size_of::<crate::journal::JsetHeader>() + 1024);
            let mut batches: Vec<Vec<(BtreeId, Bpos, u8, Vec<u8>)>> = Vec::new();
            let mut batch = Vec::new();
            let mut batch_bytes = 0usize;
            for update in pending {
                let journal_payload_bytes = 21usize.saturating_add(update.3.len());
                if journal_payload_bytes > u16::MAX as usize {
                    return Err(StorageError::Internal(format!(
                        "single transaction update payload exceeds journal entry limit: {} bytes",
                        journal_payload_bytes
                    )));
                }
                let encoded_bytes = std::mem::size_of::<crate::journal::JsetEntryHeader>()
                    .saturating_add(journal_payload_bytes);
                if encoded_bytes > max_journal_bytes {
                    return Err(StorageError::Internal(format!(
                        "single transaction update exceeds journal capacity: {} bytes",
                        encoded_bytes
                    )));
                }
                if !batch.is_empty()
                    && (batch.len() >= MAX_BATCH_UPDATES
                        || batch_bytes.saturating_add(encoded_bytes) > max_journal_bytes)
                {
                    batches.push(std::mem::take(&mut batch));
                    batch_bytes = 0;
                }
                batch_bytes = batch_bytes.saturating_add(encoded_bytes);
                batch.push(update);
            }
            if !batch.is_empty() {
                batches.push(batch);
            }
            self.unlock_all_paths();
            self.paths.clear();
            self.updates.clear();
            for chunk in batches {
                let mut sub = BtreeTrans::new(&self.vol);
                for (bt, pos, entry_type, payload) in chunk {
                    let iter = sub.iter(alloc, bt, pos, true);
                    sub.update_from_iter(&iter, entry_type, payload);
                }
                sub.commit_once(alloc).await?;
            }
            self.committed = true;
            return Ok(());
        }

        self.commit_once(alloc).await
    }

    async fn commit_once(&mut self, alloc: &mut Allocator) -> Result<(), StorageError> {
        if self.updates.is_empty() {
            return Ok(());
        }

        self.root_changed.clear();

        loop {
            // ── Collect unique paths ──
            let mut write_locked_paths: Vec<usize> = Vec::new();
            let mut write_locked_nodes: Vec<usize> = Vec::new();
            for u in &self.updates {
                let node_ptr = self.paths[u.path_index]
                    .l[0]
                    .as_ref()
                    .map(|level| level.node_ptr)
                    .expect("transaction path has no leaf");
                let node_addr = node_ptr as usize;
                if !write_locked_nodes.contains(&node_addr) {
                    write_locked_nodes.push(node_addr);
                    write_locked_paths.push(u.path_index);
                }
            }

            // ── Phase 0: 升级 leaf intent → write ──
            // bcachefs: bch2_btree_node_lock_write (locking.h:538)
            // 路径已在 iter()/update() 时持有 intent 锁，只需升级 leaf
            for &pi in &write_locked_paths {
                let bt = self.paths[pi as usize].btree_id;
                let tree = alloc.get_btree(bt);
                self.paths[pi as usize].upgrade_leaf_to_write(tree);
            }

            // ── Phase 1: 按叶子节点分组 ──
            let mut locked: Vec<LockedLeaf> = Vec::new();
            for (i, u) in self.updates.iter().enumerate() {
                let bt = self.paths[u.path_index].btree_id;
                let tree = alloc.get_btree(bt);
                let (node_idx, node_ptr) = Self::leaf_ptr(&self.paths[u.path_index], tree);
                let existing = locked.iter_mut().find(|l| std::ptr::eq(l.node, node_ptr));
                if let Some(existing) = existing {
                    existing.indices.push(i);
                } else {
                    locked.push(LockedLeaf {
                        node: node_ptr,
                        node_idx,
                        indices: vec![i],
                        path_index: u.path_index,
                        btree_id: bt,
                    });
                }
            }

            // ── Phase 2: 检查空间 ──
            let mut split_needed: Option<(BtreeId, usize, Bpos)> = None;

            'check_loop: for ll in &locked {
                let leaf = ll.node();
                let pending: Vec<BtreeEntry> = ll
                    .indices
                    .iter()
                    .map(|&idx| {
                        let u = &self.updates[idx];
                        BtreeEntry {
                            btree_type: ll.btree_id.0,
                            level: 0,
                            entry_type: u.entry_type,
                            pos: self.paths[u.path_index].pos,
                            payload: u.payload.clone(),
                        }
                    })
                    .collect();
                if !leaf.would_fit_entries(&pending) {
                    let pos = pending[0].pos;
                    split_needed = Some((ll.btree_id, ll.node_idx, pos));
                    break 'check_loop;
                }
                for &idx in &ll.indices {
                    let u = &self.updates[idx];
                    if u.entry_type == 1 && u.payload.is_empty() {
                        continue;
                    }
                    let pos = self.paths[u.path_index].pos;
                    let entry_size = u.payload.len() + std::mem::size_of::<Bpos>();
                    if leaf.last_bset_is_full() {
                        if leaf.try_rotate_bset().is_err() {
                            split_needed = Some((ll.btree_id, ll.node_idx, pos));
                            break 'check_loop;
                        }
                    }
                    if leaf.prep_for_insert(entry_size).is_err() {
                        split_needed = Some((ll.btree_id, ll.node_idx, pos));
                        break 'check_loop;
                    }
                }
            }

            // ── Phase 2.5: Split 重试 ──
            if let Some((bt, node_idx, split_pos)) = split_needed {
                for &pi in &write_locked_paths {
                    let tree = alloc.get_btree(self.paths[pi as usize].btree_id);
                    self.paths[pi as usize].downgrade_leaf_from_write(tree);
                }
                self.unlock_all_paths();

                let tree = alloc.get_btree(bt);
                let old_n = tree.child_nodes.len();

                if node_idx == usize::MAX {
                    if tree.root.total_key_count() == 0 {
                        return Err(StorageError::BtreeNodeFull);
                    }
                    tree.split_root().map_err(|err| {
                        crate::log_error!("transaction split_root failed: {:?}", err);
                        err
                    })?;
                } else {
                    // Split the leaf first, then raise the root if the new
                    // pointer set no longer fits. If the root was already
                    // full, raise it before locating the leaf so the old
                    // one-level path is not mistaken for an internal node.
                    if tree.root.is_full() {
                        tree.split_root().map_err(|err| {
                            crate::log_error!("transaction pre-split_root failed: {:?}", err);
                            err
                        })?;
                        let leaf_idx = tree.find_leaf_idx(&split_pos)?;
                        tree.split_leaf(leaf_idx, &split_pos).map_err(|err| {
                            crate::log_error!("transaction split_leaf after root failed: {:?}", err);
                            err
                        })?;
                    } else {
                        tree.split_leaf(node_idx, &split_pos).map_err(|err| {
                            crate::log_error!("transaction split_leaf failed: {:?}", err);
                            err
                        })?;
                        if tree.root.is_full() {
                            tree.split_root().map_err(|err| {
                                crate::log_error!("transaction post-split_root failed: {:?}", err);
                                err
                            })?;
                        }
                    }
                }

                let mut need_alloc: Vec<(usize, u64, u32)> = Vec::new();
                for ci in old_n..tree.child_nodes.len() {
                    need_alloc.push((
                        ci,
                        tree.child_nodes[ci].disk_offset,
                        tree.child_nodes[ci].disk_size,
                    ));
                }
                let root_changed = tree.take_root_changed();
                let root_needs_alloc = root_changed && tree.root.disk_offset == 0;

                let mut assign: Vec<(usize, u64, u32)> = Vec::new();
                if alloc.freespace_tree.total_key_count() != 0 {
                    for (ci, _, _) in &need_alloc {
                        let (off, sz) = alloc
                            .allocate_in_trans(self, crate::btree::types::NODE_SIZE)
                            .map_err(|err| {
                                crate::log_error!("transaction child allocation failed: {:?}", err);
                                err
                            })?;
                        assign.push((*ci, off, sz as u32));
                    }
                }
                let mut root_assign: Option<(u64, u32)> = None;
                if root_needs_alloc && alloc.freespace_tree.total_key_count() != 0 {
                    let (off, sz) = alloc
                        .allocate_in_trans(self, crate::btree::types::NODE_SIZE)
                        .map_err(|err| {
                            crate::log_error!("transaction root allocation failed: {:?}", err);
                            err
                        })?;
                    root_assign = Some((off, sz as u32));
                }

                if root_changed && !self.root_changed.contains(&bt) {
                    self.root_changed.push(bt);
                }
                if let Some((off, sz)) = root_assign {
                    let tree = alloc.get_btree(bt);
                    tree.root.disk_offset = off;
                    tree.root.disk_size = sz;
                }
                for (ci, off, sz) in &assign {
                    let tree = alloc.get_btree(bt);
                    tree.child_nodes[*ci].disk_offset = *off;
                    tree.child_nodes[*ci].disk_size = *sz;
                }

                self.unlock_all_paths();
                self.rebuild_paths_after_split(alloc);
                continue;
            }

            let journal_res = None;
            if !self.journal_replay {
                // ── Phase 3: 预分配 journal ──
                let total_bytes = self.calc_journal_bytes();
                let journal = self.vol.journal_ref();
                let mut res = match journal.bch2_journal_res_get(Watermark::Low, total_bytes) {
                    Ok(res) => res,
                    Err(e) => {
                        for &pi in &write_locked_paths {
                            let tree = alloc.get_btree(self.paths[pi].btree_id);
                            self.paths[pi].downgrade_leaf_from_write(tree);
                        }
                        self.unlock_all_paths();
                        return Err(StorageError::Internal(format!("journal res_get: {}", e)));
                    }
                };

                // ── Phase 4: 写入 journal ──
                for u in &self.updates {
                    let path = &self.paths[u.path_index];
                    let jp = encode_journal_payload(&path.pos, u.entry_type, &u.payload);
                    if let Err(e) = journal.bch2_journal_add_entry(
                        &mut res,
                        crate::journal::JsetEntryType::BtreeKeys as u8,
                        path.btree_id.0,
                        0,
                        &jp,
                    ) {
                        journal.bch2_journal_res_put(&res);
                        for &pi in &write_locked_paths {
                            let tree = alloc.get_btree(self.paths[pi].btree_id);
                            self.paths[pi].downgrade_leaf_from_write(tree);
                        }
                        self.unlock_all_paths();
                        return Err(StorageError::Internal(format!("journal add_entry: {}", e)));
                    }
                }
                for bt in &self.root_changed {
                    let tree = alloc.get_btree(*bt);
                    let root_off = tree.root.disk_offset;
                    let root_lvl = tree.root.level;
                    if let Err(e) = journal.bch2_journal_add_entry(
                        &mut res,
                        crate::journal::JsetEntryType::BtreeRoot as u8,
                        bt.0,
                        root_lvl,
                        &[
                            bt.0,
                            root_lvl,
                            root_off as u8,
                            (root_off >> 8) as u8,
                            (root_off >> 16) as u8,
                            (root_off >> 24) as u8,
                            (root_off >> 32) as u8,
                            (root_off >> 40) as u8,
                            (root_off >> 48) as u8,
                            (root_off >> 56) as u8,
                        ],
                    ) {
                        journal.bch2_journal_res_put(&res);
                        for &pi in &write_locked_paths {
                            let tree = alloc.get_btree(self.paths[pi].btree_id);
                            self.paths[pi].downgrade_leaf_from_write(tree);
                        }
                        self.unlock_all_paths();
                        return Err(StorageError::Internal(format!(
                            "journal add_entry root: {}",
                            e
                        )));
                    }
                }

                // Close the reservation so the Jset header (entry count and
                // checksum) is finalized before it is flushed.
                journal.bch2_journal_res_put(&res);

                // A root split publishes new device offsets in the journal.
                // Materialize those nodes before the root record becomes
                // durable, so recovery never observes a journaled root that
                // has not been written to the device yet.
                for bt in &self.root_changed {
                    if let Err(err) = alloc.get_btree(*bt).flush_pending_writes().await {
                        for &pi in &write_locked_paths {
                            let tree = alloc.get_btree(self.paths[pi].btree_id);
                            self.paths[pi].downgrade_leaf_from_write(tree);
                        }
                        self.unlock_all_paths();
                        return Err(err);
                    }
                }

                // WAL ordering: the journal entry must reach the device
                // before any btree node is written.  A volume without
                // configured journal buckets is the in-memory test path and
                // intentionally remains volatile.
                if !journal.to_superblock_state().bucket_addrs.is_empty() {
                    if let Err(e) = journal.bch2_journal_flush().await {
                        for &pi in &write_locked_paths {
                            let tree = alloc.get_btree(self.paths[pi].btree_id);
                            self.paths[pi].downgrade_leaf_from_write(tree);
                        }
                        self.unlock_all_paths();
                        return Err(StorageError::Internal(format!(
                            "journal flush before btree write: {}",
                            e
                        )));
                    }
                }

                self.journal_seq = Some(res.seq);
            }

            // ── Phase 5: 在写锁下插入 key ──
            for ll in &locked {
                let leaf = ll.node();
                for &idx in &ll.indices {
                    let u = &self.updates[idx];
                    let path = &self.paths[u.path_index];
                    if u.entry_type == 1 && u.payload.is_empty() {
                        match leaf.remove_key(&path.pos) {
                            Ok(()) | Err(StorageError::NotFound) => {}
                            Err(err) => {
                                for &pi in &write_locked_paths {
                                    let tree = alloc.get_btree(self.paths[pi].btree_id);
                                    self.paths[pi].downgrade_leaf_from_write(tree);
                                }
                                self.unlock_all_paths();
                                return Err(err);
                            }
                        }
                    } else {
                        if let Err(err) = leaf.insert_key(BtreeEntry {
                            btree_type: path.btree_id.0,
                            level: 0,
                            entry_type: u.entry_type,
                            pos: path.pos,
                            payload: u.payload.clone(),
                        }) {
                            crate::log_error!(
                                "transaction leaf insert failed: bt={} pos={:?} leaf_level={} keys={} remaining={} err={:?}",
                                path.btree_id.0,
                                path.pos,
                                leaf.level,
                                leaf.total_key_count(),
                                leaf.keys_u64s_remaining(),
                                err
                            );
                            if let Some(res) = journal_res.as_ref() {
                                self.vol.journal_ref().bch2_journal_res_put(res);
                            }
                            for &pi in &write_locked_paths {
                                let tree = alloc.get_btree(self.paths[pi].btree_id);
                                self.paths[pi].downgrade_leaf_from_write(tree);
                            }
                            self.unlock_all_paths();
                            return Err(err);
                        }
                    }
                }
            }

            // ── Phase 6: 释放写锁（downgrade write → intent） ──
            // bcachefs: bch2_btree_node_unlock_write_inlined
            for &pi in &write_locked_paths {
                let tree = alloc.get_btree(self.paths[pi as usize].btree_id);
                self.paths[pi as usize].downgrade_leaf_from_write(tree);
            }

            // ── Phase 7: flush ──
            let mut affected: Vec<BtreeId> = self.paths.iter().map(|p| p.btree_id).collect();
            affected.extend(self.root_changed.iter().copied());
            affected.sort_by_key(|id| id.0);
            affected.dedup();
            for bt in &affected {
                if let Err(err) = alloc.get_btree(*bt).flush_pending_writes().await {
                    if let Some(res) = journal_res.as_ref() {
                        self.vol.journal_ref().bch2_journal_res_put(res);
                    }
                    self.unlock_all_paths();
                    return Err(err);
                }
            }

            if let Some(res) = journal_res.as_ref() {
                self.vol.journal_ref().bch2_journal_res_put(res);
            }

            // ── Phase 8: 释放所有路径的 intent 锁 ──
            self.unlock_all_paths();

            break;
        }

        self.updates.clear();
        self.committed = true;
        Ok(())
    }

    pub fn root_changed_trees(&self) -> &[BtreeId] {
        &self.root_changed
    }

    pub fn journal_seq(&self) -> Option<u64> {
        self.journal_seq
    }

    pub fn is_committed(&self) -> bool {
        self.committed
    }
}

/// Drop：释放所有路径残留的锁
///
/// 对应 bcachefs `bch2_trans_put` (btree_iter.c:3398)
/// `unlock_all_paths` 内部重置了 `nodes_locked`，因此：
/// - 已提交: paths 的 nodes_locked=0 → no-op
/// - split retry 后失败: unlock_all_paths 已调用 → no-op
/// - 提交中途失败（Phase 0-2）: node_ptrs 仍有效 → 正确释放
impl Drop for BtreeTrans {
    fn drop(&mut self) {
        self.unlock_all_paths();
    }
}

impl std::fmt::Debug for BtreeTrans {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BtreeTrans")
            .field("paths", &self.paths.len())
            .field("pending", &self.updates.len())
            .field("committed", &self.committed)
            .field("journal_seq", &self.journal_seq)
            .finish()
    }
}
