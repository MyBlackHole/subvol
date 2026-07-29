//! Btree — 完整 B+tree 实现
//!
//! level 0: 叶子节点，存储数据条目
//! level > 0: 内部节点，存储 BtreePtr 指向子节点

use std::collections::HashMap;
use std::sync::Arc;

use crate::block_device::BchDev;
use crate::btree::bset::BtreeNodeIter;
use crate::btree::key::{Bpos, BtreeEntry};
use crate::btree::node::BtreeNode;
use crate::btree::types::{BtreeId, BTREE_MAX_DEPTH, NODE_SIZE};
use crate::data::extents_format::{BtreePtr, ENTRY_TYPE_BTREE_PTR};
use crate::lock::six::SixLockType;
use crate::types::StorageError;

/// Btree — 一棵完整的 B+tree
pub struct Btree {
    pub btree_id: BtreeId,
    pub(crate) root: BtreeNode,
    _dev: Arc<BchDev>,
    pub(crate) child_nodes: Vec<BtreeNode>,
    pub(crate) root_changed: bool,
    /// 重播阶段的可选查询回调（用于 journal overlay 查询）
    /// 设置后 lookup/next_entry/prev_entry 命中此回调优先返回
    pub(crate) lookup_override: Option<Arc<dyn Fn(&Bpos) -> Option<BtreeEntry> + Send + Sync>>,
}

impl Btree {
    pub fn new(btree_id: BtreeId, dev: &Arc<BchDev>) -> Self {
        Btree {
            btree_id,
            root: BtreeNode::new_leaf(btree_id),
            _dev: dev.clone(),
            child_nodes: Vec::new(),
            root_changed: false,
            lookup_override: None,
        }
    }

    /// 设置重播阶段查询回调（journal overlay）
    /// 设置后 lookup/next_entry/prev_entry 回调优先于实际 btree 数据
    pub fn set_lookup_overlay(
        &mut self,
        cb: Option<Arc<dyn Fn(&Bpos) -> Option<BtreeEntry> + Send + Sync>>,
    ) {
        self.lookup_override = cb;
    }

    pub fn take_root_changed(&mut self) -> bool {
        let c = self.root_changed;
        self.root_changed = false;
        c
    }

    pub fn total_key_count(&self) -> u32 {
        self.root.total_key_count()
    }

    // ── 注意：Btree 不再提供直接读 API ──
    // 所有读取操作通过 BtreeIter（记录路径、持读锁）
    // BtreeTrans::iter(alloc, id, pos) → BtreeIter
    // BtreeIter::peek() / next() / prev() / seek()

    // ═══════════════════════════════════════════════════════════
    // 更新 — BtreeTrans::commit_once 通过 node::insert_key/remove_key 直接操作
    // 所有修改必须走 BtreeTrans 路径，不得直接调用 update/insert_key/remove_key
    // ═══════════════════════════════════════════════════════════

    /// 找到子节点在 child_nodes 中的索引
    pub(crate) fn find_child_idx(&self, pos: &Bpos) -> Result<usize, StorageError> {
        self.find_leaf_idx(pos)
    }

    /// Find the leaf child index by descending all internal levels.
    pub(crate) fn find_leaf_idx(&self, pos: &Bpos) -> Result<usize, StorageError> {
        if self.root.level == 0 {
            return Err(StorageError::Internal("root is already a leaf".into()));
        }
        let mut node = &self.root;
        loop {
            let ptr = find_child_ptr_val(node, pos)
                .ok_or_else(|| StorageError::Internal("no child ptr in btree path".into()))?;
            let idx = ptr.offset as usize;
            let child = self
                .child_nodes
                .get(idx)
                .ok_or_else(|| StorageError::Internal("btree child index out of range".into()))?;
            if child.level == 0 {
                return Ok(idx);
            }
            node = child;
        }
    }

    fn find_parent_idx(&self, child_idx: usize) -> Option<usize> {
        fn descend(node: &BtreeNode, children: &[BtreeNode], target: usize) -> Option<usize> {
            for i in 0..node.nsets as usize {
                for entry in &node.set[i].bset.entries {
                    if entry.entry_type != ENTRY_TYPE_BTREE_PTR {
                        continue;
                    }
                    let ptr = BtreePtr::from_bytes(&entry.payload)?;
                    let idx = ptr.offset as usize;
                    if idx == target {
                        return Some(target);
                    }
                    let child = children.get(idx)?;
                    if child.level > 0 && descend(child, children, target).is_some() {
                        return Some(idx);
                    }
                }
            }
            None
        }

        for i in 0..self.root.nsets as usize {
            for entry in &self.root.set[i].bset.entries {
                if entry.entry_type != ENTRY_TYPE_BTREE_PTR {
                    continue;
                }
                let ptr = BtreePtr::from_bytes(&entry.payload)?;
                let idx = ptr.offset as usize;
                if idx == child_idx {
                    return None;
                }
                if self.child_nodes.get(idx)?.level > 0
                    && descend(self.child_nodes.get(idx)?, &self.child_nodes, child_idx)
                        .is_some()
                {
                    return Some(idx);
                }
            }
        }
        None
    }

    fn refresh_ancestor_min(&mut self, child_idx: usize) -> Result<(), StorageError> {
        let Some(parent_idx) = self.find_parent_idx(child_idx) else {
            return Ok(());
        };
        let min = self.child_nodes[child_idx].min_key_or_pivot();
        let level = self.child_nodes[child_idx].level;
        let parent = &mut self.child_nodes[parent_idx];
        parent.lock_write_blocking();
        parent.remove_child_ptr_by_offset(child_idx as u64);
        parent.insert_key(BtreeEntry {
            btree_type: self.btree_id.0,
            level: parent.level,
            entry_type: ENTRY_TYPE_BTREE_PTR,
            pos: min,
            payload: BtreePtr {
                offset: child_idx as u64,
                child_level: level,
            }
            .to_bytes(),
        })?;
        parent.unlock_write();
        self.refresh_ancestor_min(parent_idx)
    }

    // ═══════════════════════════════════════════════════════════
    // 分裂
    // ═══════════════════════════════════════════════════════════

    /// 分裂根节点（必要时提升树深度）
    ///
    /// 对应 bcachefs `__btree_increase_depth` (interior.c:2322) +
    /// `btree_split` (interior.c:1962)
    pub(crate) fn split_root(&mut self) -> Result<(), StorageError> {
        if self.root.total_key_count() < 2 {
            return Err(StorageError::BtreeNodeFull);
        }

        // 对老 root 加写锁后再分裂
        self.root.lock_write_blocking();
        let children = self.root.split_for_root()?;
        self.root.unlock_write();
        crate::log_verbose!(
            "split_root: id={} old_level={} new_level={} children={}",
            self.btree_id.0,
            self.root.level,
            self.root.level + 1,
            children.len()
        );
        let level = self.root.level;

        let mut ptrs = Vec::with_capacity(children.len());
        for child in children {
            let idx = self.child_nodes.len();
            let min = child.min_key_or_pivot();
            self.child_nodes.push(child);
            ptrs.push((min, idx));
        }
        let mut new_root = BtreeNode::new(level + 1, self.btree_id);
        for (min, idx) in ptrs {
            new_root.insert_key(BtreeEntry {
                btree_type: self.btree_id.0,
                level: level + 1,
                entry_type: ENTRY_TYPE_BTREE_PTR,
                pos: min,
                payload: BtreePtr {
                    offset: idx as u64,
                    child_level: level,
                }
                .to_bytes(),
            }).map_err(|err| {
                crate::log_error!("split_root: root pointer insertion failed children={} level={}", new_root.total_key_count(), level);
                err
            })?;
        }

        self.root = new_root;
        self.root_changed = true;
        Ok(())
    }

    /// 分裂非 root 叶子节点
    ///
    /// 对应 bcachefs `bch2_btree_split_leaf` (interior.c:2281)
    /// 将 child_nodes[idx] 分裂为两个叶子，pos 所在的半边留在 idx，
    /// 另一半追加到 child_nodes 末尾，并更新 root 中的 BtreePtr。
    pub(crate) fn split_leaf(&mut self, idx: usize, pos: &Bpos) -> Result<(), StorageError> {
        crate::log_verbose!(
            "split_leaf: id={} idx={} pos=({},{},{})",
            self.btree_id.0,
            idx,
            pos.inode,
            pos.offset,
            pos.snapshot
        );
        let parent_idx = self.find_parent_idx(idx);
        if let Some(parent_idx) = parent_idx {
            if self.child_nodes[parent_idx].is_full() {
                return self.split_internal_child(parent_idx);
            }
        } else if self.root.is_full() {
            return self.split_root();
        }

        // Preview the split before replacing the child so the parent can be
        // checked for the two resulting pointers.  A parent may have space
        // for its current keys while still lacking room for the replacement
        // pointer plus the new sibling pointer.
        let (left, right) = self.child_nodes[idx].split().map_err(|err| {
            crate::log_error!("split_leaf child split failed: idx={} keys={} remaining={} err={:?}", idx, self.child_nodes[idx].total_key_count(), self.child_nodes[idx].keys_u64s_remaining(), err);
            err
        })?;
        let old_pos = {
            let parent = match parent_idx {
                Some(parent_idx) => &self.child_nodes[parent_idx],
                None => &self.root,
            };
            parent
                .set
                .iter()
                .take(parent.nsets as usize)
                .flat_map(|set| set.bset.entries.iter())
                .find_map(|entry| {
                    if entry.entry_type != ENTRY_TYPE_BTREE_PTR {
                        return None;
                    }
                    let ptr = BtreePtr::from_bytes(&entry.payload)?;
                    (ptr.offset as usize == idx).then_some(entry.pos)
                })
                .ok_or_else(|| StorageError::Internal("missing parent pointer for split".into()))?
        };
        let left_min = left.min_key_or_pivot();
        let right_min = right.min_key_or_pivot();
        let parent = match parent_idx {
            Some(parent_idx) => &self.child_nodes[parent_idx],
            None => &self.root,
        };
        let parent_updates = [
            BtreeEntry {
                btree_type: self.btree_id.0,
                level: parent.level,
                entry_type: 1,
                pos: old_pos,
                payload: Vec::new(),
            },
            BtreeEntry {
                btree_type: self.btree_id.0,
                level: parent.level,
                entry_type: ENTRY_TYPE_BTREE_PTR,
                pos: left_min,
                payload: BtreePtr {
                    offset: idx as u64,
                    child_level: left.level,
                }
                .to_bytes(),
            },
            BtreeEntry {
                btree_type: self.btree_id.0,
                level: parent.level,
                entry_type: ENTRY_TYPE_BTREE_PTR,
                pos: right_min,
                payload: BtreePtr {
                    offset: self.child_nodes.len() as u64,
                    child_level: right.level,
                }
                .to_bytes(),
            },
        ];
        if !parent.would_fit_entries(&parent_updates) {
            if let Some(parent_idx) = parent_idx {
                self.split_internal_child(parent_idx)?;
            } else {
                self.split_root()?;
            }
            return Ok(());
        }

        let new_idx = self.child_nodes.len();

        // Keep the existing child index on the left half.  This preserves
        // the ancestor's minimum-key pivot; replacing it with the right half
        // would require rewriting every ancestor pointer on the path.
        self.child_nodes[idx] = left;
        self.child_nodes.push(right);

        let child_at_idx = &self.child_nodes[idx];
        let child_at_new = &self.child_nodes[new_idx];
        let idx_min = child_at_idx.min_key_or_pivot();
        let new_min = child_at_new.min_key_or_pivot();

        let ptr_at_idx = BtreePtr {
            offset: idx as u64,
            child_level: child_at_idx.level,
        };
        let ptr_at_new = BtreePtr {
            offset: new_idx as u64,
            child_level: child_at_new.level,
        };

        let parent = match parent_idx {
            Some(parent_idx) => &mut self.child_nodes[parent_idx],
            None => &mut self.root,
        };
        parent.lock_write_blocking();
        parent.remove_child_ptr_by_offset(idx as u64);
        parent.insert_key(BtreeEntry {
            btree_type: self.btree_id.0,
            level: parent.level,
            entry_type: ENTRY_TYPE_BTREE_PTR,
            pos: idx_min,
            payload: ptr_at_idx.to_bytes(),
        }).map_err(|err| {
            crate::log_error!("split_leaf left parent insert failed: parent_keys={} remaining={} err={:?}", parent.total_key_count(), parent.keys_u64s_remaining(), err);
            err
        })?;
        parent.insert_key(BtreeEntry {
            btree_type: self.btree_id.0,
            level: parent.level,
            entry_type: ENTRY_TYPE_BTREE_PTR,
            pos: new_min,
            payload: ptr_at_new.to_bytes(),
        }).map_err(|err| {
            crate::log_error!("split_leaf right parent insert failed: parent_keys={} remaining={} err={:?}", parent.total_key_count(), parent.keys_u64s_remaining(), err);
            err
        })?;
        parent.unlock_write();

        if let Some(parent_idx) = parent_idx {
            if self.child_nodes[parent_idx].is_full() {
                self.split_internal_child(parent_idx)?;
            }
        } else if self.root.is_full() {
            self.split_root()?;
        }
        if let Some(parent_idx) = parent_idx {
            self.refresh_ancestor_min(parent_idx)?;
        }

        Ok(())
    }

    fn split_internal_child(&mut self, idx: usize) -> Result<(), StorageError> {
        let parent_idx = self.find_parent_idx(idx);
        let (left, right) = self.child_nodes[idx].split().map_err(|err| {
            crate::log_error!("split_internal child split failed: idx={} keys={} remaining={} err={:?}", idx, self.child_nodes[idx].total_key_count(), self.child_nodes[idx].keys_u64s_remaining(), err);
            err
        })?;
        let old_pos = {
            let parent = match parent_idx {
                Some(parent_idx) => &self.child_nodes[parent_idx],
                None => &self.root,
            };
            parent
                .set
                .iter()
                .take(parent.nsets as usize)
                .flat_map(|set| set.bset.entries.iter())
                .find_map(|entry| {
                    if entry.entry_type != ENTRY_TYPE_BTREE_PTR {
                        return None;
                    }
                    let ptr = BtreePtr::from_bytes(&entry.payload)?;
                    (ptr.offset as usize == idx).then_some(entry.pos)
                })
                .ok_or_else(|| StorageError::Internal("missing parent pointer for split".into()))?
        };
        let left_min = left.min_key_or_pivot();
        let right_min = right.min_key_or_pivot();
        let new_idx = self.child_nodes.len();

        let parent = match parent_idx {
            Some(parent_idx) => &self.child_nodes[parent_idx],
            None => &self.root,
        };
        let parent_updates = [
            BtreeEntry {
                btree_type: self.btree_id.0,
                level: parent.level,
                entry_type: 1,
                pos: old_pos,
                payload: Vec::new(),
            },
            BtreeEntry {
                btree_type: self.btree_id.0,
                level: parent.level,
                entry_type: ENTRY_TYPE_BTREE_PTR,
                pos: left_min,
                payload: BtreePtr {
                    offset: idx as u64,
                    child_level: left.level,
                }
                .to_bytes(),
            },
            BtreeEntry {
                btree_type: self.btree_id.0,
                level: parent.level,
                entry_type: ENTRY_TYPE_BTREE_PTR,
                pos: right_min,
                payload: BtreePtr {
                    offset: new_idx as u64,
                    child_level: right.level,
                }
                .to_bytes(),
            },
        ];
        if !parent.would_fit_entries(&parent_updates) {
            if let Some(parent_idx) = parent_idx {
                self.split_internal_child(parent_idx)?;
            } else {
                self.split_root()?;
            }
            return Ok(());
        }

        self.child_nodes[idx] = left;
        self.child_nodes.push(right);
        let left_min = self.child_nodes[idx].min_key_or_pivot();
        let left_level = self.child_nodes[idx].level;
        let right_level = self.child_nodes[new_idx].level;

        let parent = match parent_idx {
            Some(parent_idx) => &mut self.child_nodes[parent_idx],
            None => &mut self.root,
        };
        parent.lock_write_blocking();
        parent.remove_child_ptr_by_offset(idx as u64);
        let left_ptr = BtreePtr {
            offset: idx as u64,
            child_level: left_level,
        };
        let right_ptr = BtreePtr {
            offset: new_idx as u64,
            child_level: right_level,
        };
        parent.insert_key(BtreeEntry {
            btree_type: self.btree_id.0,
            level: parent.level,
            entry_type: ENTRY_TYPE_BTREE_PTR,
            pos: left_min,
            payload: left_ptr.to_bytes(),
        }).map_err(|err| {
            crate::log_error!("split_internal left parent insert failed: parent_keys={} remaining={} err={:?}", parent.total_key_count(), parent.keys_u64s_remaining(), err);
            err
        })?;
        parent.insert_key(BtreeEntry {
            btree_type: self.btree_id.0,
            level: parent.level,
            entry_type: ENTRY_TYPE_BTREE_PTR,
            pos: right_min,
            payload: right_ptr.to_bytes(),
        }).map_err(|err| {
            crate::log_error!("split_internal right parent insert failed: parent_keys={} remaining={} err={:?}", parent.total_key_count(), parent.keys_u64s_remaining(), err);
            err
        })?;
        parent.unlock_write();

        match parent_idx {
            Some(parent_idx) if self.child_nodes[parent_idx].is_full() => {
                self.split_internal_child(parent_idx)?;
            }
            None if self.root.is_full() => self.split_root()?,
            _ => {}
        }
        if let Some(parent_idx) = parent_idx {
            self.refresh_ancestor_min(parent_idx)?;
        }
        Ok(())
    }

    /// 遍历所有叶子节点，返回所有条目（不含 BtreePtr）

    /// 写入节点数据到设备
    ///
    /// 将当前节点序列化后写入设备上的 btree node 块。
    /// 若序列化数据超过 NODE_SIZE 且节点已有 disk_offset，返回 CowNeeded。
    ///
    /// 写盘前将根节点内 BtreePtr 的 child_idx 替换为子节点的 disk_offset，
    /// 写盘后立即恢复，对内存操作透明。
    pub async fn flush_pending_writes(&mut self) -> Result<(), StorageError> {
        // Step 1: 收集子节点 disk_offset 映射（只对非叶子树有意义）
        let child_offsets: Vec<u64> = self.child_nodes.iter().map(|c| c.disk_offset).collect();

        // Step 2: 根节点 BtreePtr 重写
        let has_root_offset = self.root.disk_offset != 0;
        let root_saved = if has_root_offset {
            self.root.rewrite_ptrs_for_write(&child_offsets)
        } else {
            Vec::new()
        };

        // Step 3: flush child nodes. Internal child pointers use the same
        // in-memory child index as the root, but must contain device offsets
        // on disk so the complete tree can be reconstructed after restart.
        // Children are written before the parent publishes pointers to them.
        for child in &mut self.child_nodes {
            if child.disk_offset == 0 {
                continue;
            }
            let child_saved = if child.level > 0 {
                child.rewrite_ptrs_for_write(&child_offsets)
            } else {
                Vec::new()
            };
            let child_data = child.to_bytes();
            if child_data.len() > NODE_SIZE as usize {
                child.restore_ptr_offsets(&child_saved);
                self.root.restore_ptr_offsets(&root_saved);
                return Err(StorageError::CowNeeded(format!(
                    "child node: {} bytes > NODE_SIZE {}",
                    child_data.len(),
                    NODE_SIZE
                )));
            }
            let result = self._dev.write_at(child.disk_offset, &child_data).await;
            child.restore_ptr_offsets(&child_saved);
            if let Err(err) = result {
                self.root.restore_ptr_offsets(&root_saved);
                return Err(err);
            }
        }

        // Step 4: write the root only after every child it references has
        // reached the device. This is the parent-after-child ordering used
        // by the btree write path and closes the root-pointer crash window.
        if has_root_offset {
            let root_data = self.root.to_bytes();
            if root_data.len() > NODE_SIZE as usize {
                self.root.restore_ptr_offsets(&root_saved);
                return Err(StorageError::CowNeeded(format!(
                    "root node: {} bytes > NODE_SIZE {}",
                    root_data.len(),
                    NODE_SIZE
                )));
            }
            if let Err(err) = self._dev.write_at(self.root.disk_offset, &root_data).await {
                self.root.restore_ptr_offsets(&root_saved);
                return Err(err);
            }
            self.root.restore_ptr_offsets(&root_saved);
        }
        self._dev.flush().await?;
        Ok(())
    }

    /// 序列化根节点
    /// 持久化根节点（不包含子节点数据）
    ///
    /// 遵循 bcachefs 设计：子节点内容由 journal replay 恢复，
    /// persist_roots 只记录根节点的结构骨架（含所有 BtreePtr 条目）。
    pub fn persist(&self) -> Vec<u8> {
        let root_bytes = self.root.to_bytes();
        let mut buf = Vec::with_capacity(4 + root_bytes.len());
        buf.extend_from_slice(&(root_bytes.len() as u32).to_le_bytes());
        buf.extend_from_slice(&root_bytes);
        buf
    }

    /// 从持久化数据重建整棵树
    ///
    /// 加载根节点后，从 root 的 BtreePtr 条目重建 child_nodes：
    /// - 所有内部节点的 BtreePtr 条目从 root 的条目按 level 分区重建
    /// - 叶子节点创建为空骨架，内容由 journal replay 回填
    pub fn from_persisted(data: &[u8], btree_id: BtreeId, dev: &Arc<BchDev>) -> Option<Self> {
        let mut off = 0usize;
        if off + 4 > data.len() {
            return None;
        }
        let root_len = u32::from_le_bytes(data[off..off + 4].try_into().ok()?) as usize;
        off += 4;
        if off + root_len > data.len() {
            return None;
        }
        let root = BtreeNode::from_bytes(&data[off..off + root_len])?;

        let child_nodes = rebuild_child_nodes_from_root(&root, btree_id);

        Some(Btree {
            btree_id,
            root,
            _dev: dev.clone(),
            child_nodes,
            root_changed: false,
            lookup_override: None,
        })
    }

    /// Load a persisted tree whose btree pointers contain device offsets.
    ///
    /// The synchronous `from_persisted` helper retains the in-memory test
    /// representation where pointers are child indexes. Device recovery uses
    /// this path so every internal and leaf node is read before the journal is
    /// discarded.
    pub async fn from_persisted_with_device(
        data: &[u8],
        btree_id: BtreeId,
        dev: &Arc<BchDev>,
        root_offset: u64,
    ) -> Result<Option<Self>, StorageError> {
        if data.len() < 4 {
            return Ok(None);
        }
        let root_len = u32::from_le_bytes(
            data[..4]
                .try_into()
                .map_err(|_| StorageError::Internal("invalid persisted root length".into()))?,
        ) as usize;
        if 4 + root_len > data.len() {
            return Ok(None);
        }
        let mut root = match BtreeNode::from_bytes(&data[4..4 + root_len]) {
            Some(root) => root,
            None => return Ok(None),
        };
        root.btree_id = btree_id;
        root.disk_offset = root_offset;
        root.disk_size = root_len as u32;

        let mut children: Vec<Option<BtreeNode>> = Vec::new();
        let mut indices = HashMap::<u64, usize>::new();
        let mut pending = Vec::<(u64, u8)>::new();

        let queue_child = |disk_offset: u64,
                               level: u8,
                               children: &mut Vec<Option<BtreeNode>>,
                               indices: &mut HashMap<u64, usize>,
                               pending: &mut Vec<(u64, u8)>|
         -> Result<usize, StorageError> {
            if disk_offset == 0 {
                return Err(StorageError::Internal(
                    "persisted btree pointer has zero device offset".into(),
                ));
            }
            if let Some(index) = indices.get(&disk_offset) {
                return Ok(*index);
            }
            let index = children.len();
            indices.insert(disk_offset, index);
            children.push(None);
            pending.push((disk_offset, level));
            Ok(index)
        };

        for set in root.set.iter_mut().take(root.nsets as usize) {
            for entry in &mut set.bset.entries {
                if entry.entry_type != ENTRY_TYPE_BTREE_PTR {
                    continue;
                }
                let ptr = BtreePtr::from_bytes(&entry.payload).ok_or_else(|| {
                    StorageError::Internal("invalid persisted root btree pointer".into())
                })?;
                let index = queue_child(
                    ptr.offset,
                    ptr.child_level,
                    &mut children,
                    &mut indices,
                    &mut pending,
                )?;
                entry.payload = BtreePtr {
                    offset: index as u64,
                    child_level: ptr.child_level,
                }
                .to_bytes();
            }
        }

        while let Some((disk_offset, level)) = pending.pop() {
            let bytes = dev.read_at(disk_offset, NODE_SIZE as usize).await?;
            let mut node = BtreeNode::from_bytes(&bytes).ok_or_else(|| {
                StorageError::Internal(format!(
                    "failed to deserialize btree child at offset {}",
                    disk_offset
                ))
            })?;
            node.level = level;
            node.btree_id = btree_id;
            node.disk_offset = disk_offset;
            node.disk_size = NODE_SIZE as u32;
            for set in node.set.iter_mut().take(node.nsets as usize) {
                for entry in &mut set.bset.entries {
                    if entry.entry_type != ENTRY_TYPE_BTREE_PTR {
                        continue;
                    }
                    let ptr = BtreePtr::from_bytes(&entry.payload).ok_or_else(|| {
                        StorageError::Internal("invalid persisted child btree pointer".into())
                    })?;
                    let index = queue_child(
                        ptr.offset,
                        ptr.child_level,
                        &mut children,
                        &mut indices,
                        &mut pending,
                    )?;
                    entry.payload = BtreePtr {
                        offset: index as u64,
                        child_level: ptr.child_level,
                    }
                    .to_bytes();
                }
            }
            let index = indices.get(&disk_offset).copied().ok_or_else(|| {
                StorageError::Internal("queued btree child index missing".into())
            })?;
            children[index] = Some(node);
        }

        Ok(Some(Btree {
            btree_id,
            root,
            _dev: dev.clone(),
            child_nodes: children
                .into_iter()
                .map(|node| {
                    node.ok_or_else(|| {
                        StorageError::Internal("queued btree child was not loaded".into())
                    })
                })
                .collect::<Result<Vec<_>, _>>()?,
            root_changed: false,
            lookup_override: None,
        }))
    }
}

/// 从根节点的 BtreePtr 条目重建完整 child_nodes
///
/// 对应 bcachefs 恢复路径：从根节点反序列化整棵树结构，
/// 叶子节点创建为空（journal replay 回填内容）。
pub fn rebuild_child_nodes_from_root(root: &BtreeNode, btree_id: BtreeId) -> Vec<BtreeNode> {
    let mut ptrs = Vec::new();
    for i in (0..root.nsets as usize).rev() {
        for entry in &root.set[i].bset.entries {
            if entry.entry_type != ENTRY_TYPE_BTREE_PTR {
                continue;
            }
            if let Some(ptr) = BtreePtr::from_bytes(&entry.payload) {
                ptrs.push((ptr.offset, ptr.child_level, entry.pos));
            }
        }
    }
    if ptrs.is_empty() {
        return Vec::new();
    }

    let max_off = ptrs.iter().map(|(o, _, _)| *o).max().unwrap() as usize;
    let mut children: Vec<Option<BtreeNode>> = (0..max_off + 1).map(|_| None).collect();

    for &(offset, level, _) in &ptrs {
        let idx = offset as usize;
        if children[idx].is_none() || children[idx].as_ref().map_or(true, |n| n.level != level) {
            children[idx] = Some(BtreeNode::new(level, btree_id));
        }
    }

    for parent_level in (1..root.level).rev() {
        let parent_offsets: Vec<u64> = ptrs
            .iter()
            .filter(|(_, lvl, _)| *lvl == parent_level)
            .map(|(o, _, _)| *o)
            .collect();
        if parent_offsets.is_empty() {
            continue;
        }

        for &parent_off in &parent_offsets {
            let parent_pos = ptrs
                .iter()
                .find(|(o, lvl, _)| *o == parent_off && *lvl == parent_level)
                .map(|(_, _, pos)| *pos);

            let next_parent_pos: Option<Bpos> = ptrs
                .iter()
                .filter(|(_, lvl, _)| *lvl == parent_level)
                .filter(|(o, _, _)| *o > parent_off)
                .map(|(_, _, pos)| *pos)
                .min_by(|a, b| a.cmp(b));

            if let Some(pmin) = parent_pos {
                let children_of_parent: Vec<(u64, u8, Bpos)> = ptrs
                    .iter()
                    .filter(|(_, lvl, _)| *lvl == parent_level.saturating_sub(1))
                    .filter(|(_, _, pos)| {
                        *pos >= pmin && next_parent_pos.map_or(true, |np| *pos < np)
                    })
                    .copied()
                    .collect();

                if let Some(ref mut node) = children[parent_off as usize] {
                    for (child_off, child_lvl, child_pos) in &children_of_parent {
                        let child_ptr = BtreePtr {
                            offset: *child_off,
                            child_level: *child_lvl,
                        };
                        let entry = BtreeEntry {
                            btree_type: btree_id.0,
                            level: parent_level,
                            entry_type: ENTRY_TYPE_BTREE_PTR,
                            pos: *child_pos,
                            payload: child_ptr.to_bytes(),
                        };
                        let _ = node.insert_key(entry);
                    }
                }
            }
        }
    }

    children
        .into_iter()
        .map(|c| c.unwrap_or_else(|| BtreeNode::new(0, btree_id)))
        .collect()
}

// ═══════════════════════════════════════════════════════════════
// 辅助函数
// ═══════════════════════════════════════════════════════════════

/// 在内部节点中查找包含 pos 的子节点指针（按值返回）
///
/// 对应 bcachefs `btree_node_child` (interior.c) 语义：
/// 每个 BtreePtr 的 pos 表示该子节点的最小 key，查找 pos 所属的 child 即找
/// 最右（最大 pos）的 BtreePtr 且 `entry.pos <= *pos`。
///
/// `entries` 按 pos 升序排列，故从右向左遍历。
pub fn find_child_ptr_val(node: &BtreeNode, pos: &Bpos) -> Option<BtreePtr> {
    let mut best: Option<(Bpos, BtreePtr)> = None;
    let nsets = node.nsets as usize;
    for i in 0..nsets {
        for entry in &node.set[i].bset.entries {
            if entry.entry_type != ENTRY_TYPE_BTREE_PTR {
                continue;
            }
            if entry.pos <= *pos {
                if let Some(ptr) = BtreePtr::from_bytes(&entry.payload) {
                    if best.as_ref().map_or(true, |(best_pos, _)| entry.pos > *best_pos) {
                        best = Some((entry.pos, ptr));
                    }
                }
            }
        }
    }
    best.map(|(_, ptr)| ptr)
}

impl BtreeNode {
    /// 获取节点最小 key
    pub fn min_key_or_pivot(&self) -> Bpos {
        let mut min = None;
        for i in 0..self.nsets as usize {
            if let Some(first) = self.set[i].bset.entries.first() {
                if min.map_or(true, |current: Bpos| first.pos < current) {
                    min = Some(first.pos);
                }
            }
        }
        min.unwrap_or(Bpos {
            inode: 0,
            offset: 0,
            snapshot: 0,
        })
    }
}

// ═══════════════════════════════════════════════════════════════
// BtreePathEntry — 路径条目
// ═══════════════════════════════════════════════════════════════

/// BtreePathLevel — 路径中某一层的节点 + 游标 + 锁状态
///
/// 对应 bcachefs `struct btree_path_level` (iter_path.h:38)
/// ```ignore
/// struct btree_path_level {
///     struct btree *b;
///     struct btree_node_iter iter;
///     u64 lock_seq;
/// };
/// ```
#[derive(Debug)]
pub struct BtreePathLevel {
    /// 节点索引（usize::MAX = root，否则 child_nodes 下标）
    pub node_idx: usize,
    /// 节点原始指针（Drop 释放锁时无需 Btree 引用）
    pub node_ptr: *const BtreeNode,
    /// 该层的 BtreeNodeIter（每 bset 游标）
    pub iter: BtreeNodeIter,
    /// 锁序列号（检测并发修改）
    pub lock_seq: u64,
}

/// Safety: node_ptr 指向 Arc<BchVol> 管理的 BtreeNode，BtreeTrans 持有期间 Volume 存活
unsafe impl Send for BtreePathLevel {}
unsafe impl Sync for BtreePathLevel {}

/// BtreeIterPath — 从 leaf 到 root 的完整遍历路径
///
/// 对应 bcachefs `struct btree_path` (iter_path.h:16)
/// ```ignore
/// struct btree_path {
///     struct bpos pos;
///     enum btree_id btree_id;
///     unsigned level;
///     unsigned nodes_locked;
///     unsigned locks_want;
///     struct btree_path_level l[BTREE_MAX_DEPTH];
/// };
/// ```
/// 约定：l[0] = leaf, l[depth-1] = root
///
/// nodes_locked 使用每层 2 位的锁类型编码（对应 bcachefs `btree_node_locked_type`）：
/// - 位 [level*2 : level*2+1] = 锁类型（0=UNLOCKED, 1=READ, 2=INTENT, 3=WRITE）
pub const BTREE_NODE_UNLOCKED: u8 = 0;
pub const BTREE_NODE_READ_LOCKED: u8 = 1;
pub const BTREE_NODE_INTENT_LOCKED: u8 = 2;
pub const BTREE_NODE_WRITE_LOCKED: u8 = 3;

#[derive(Debug)]
pub struct BtreeIterPath {
    /// l[0]=leaf, l[1]=level1, ..., l[depth-1]=root
    pub l: [Option<BtreePathLevel>; BTREE_MAX_DEPTH],
    /// 目标 btree id（对应 btree_path.btree_id）
    pub btree_id: BtreeId,
    /// 目标位置（对应 btree_path.pos）
    pub pos: Bpos,
    /// 路径有效深度（即有几层）
    pub depth: u8,
    /// 每层 2 位的锁类型编码（对应 bcachefs `btree_path.nodes_locked`）
    pub nodes_locked: u32,
    /// 需要加锁的 level 位图
    pub locks_want: u32,
}

impl BtreeIterPath {
    pub fn new(btree_id: BtreeId, pos: Bpos) -> Self {
        Self {
            l: core::array::from_fn(|_| None),
            btree_id,
            pos,
            depth: 0,
            nodes_locked: 0,
            locks_want: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.depth == 0
    }

    pub fn depth(&self) -> u8 {
        self.depth
    }

    /// 获取指定 level 的锁类型
    fn lock_type_at(&self, level: u8) -> u8 {
        ((self.nodes_locked >> (level as u32 * 2)) & 3) as u8
    }

    /// 设置指定 level 的锁类型
    fn set_lock_type_at(&mut self, level: u8, typ: u8) {
        let bit = level as u32 * 2;
        self.nodes_locked = (self.nodes_locked & !(3 << bit)) | ((typ as u32) << bit);
    }

    /// 获取 leaf 层（l[0]）
    pub fn leaf(&self) -> Option<&BtreePathLevel> {
        if self.depth > 0 {
            self.l[0].as_ref()
        } else {
            None
        }
    }

    /// 获取 leaf 层可变引用
    pub fn leaf_mut(&mut self) -> Option<&mut BtreePathLevel> {
        if self.depth > 0 {
            self.l[0].as_mut()
        } else {
            None
        }
    }

    /// 获取指定 level 的路径层
    pub fn at_level(&self, level: u8) -> Option<&BtreePathLevel> {
        let idx = level as usize;
        if idx < BTREE_MAX_DEPTH {
            self.l[idx].as_ref()
        } else {
            None
        }
    }

    /// 遍历 tree 从 root 到 pos 所在 leaf，记录路径并持有锁
    ///
    /// bcachefs: 路径中所有层级（root→internal→leaf）从遍历开始到解锁前都持有对应类型锁。
    /// `want_intent=true`：持意向锁（用于写路径），`false`：持读锁（用于只读路径）。
    /// 结果存为 l[0]=leaf, l[depth-1]=root
    pub fn traverse(tree: &Btree, btree_id: BtreeId, pos: &Bpos, want_intent: bool) -> Self {
        let lock_tag = if want_intent {
            BTREE_NODE_INTENT_LOCKED
        } else {
            BTREE_NODE_READ_LOCKED
        };
        let mut rev_entries: Vec<(&BtreeNode, usize)> = Vec::new();
        let mut node: &BtreeNode = &tree.root;
        if want_intent {
            node.lock_intent_blocking();
        } else {
            node.lock_read_blocking();
        }
        rev_entries.push((node, usize::MAX));

        while node.level > 0 {
            let ptr = find_child_ptr_val(node, pos);
            match ptr.and_then(|p| {
                let idx = p.offset as usize;
                if idx < tree.child_nodes.len() {
                    Some(idx)
                } else {
                    None
                }
            }) {
                Some(idx) => {
                    let child: &BtreeNode = &tree.child_nodes[idx];
                    if want_intent {
                        child.lock_intent_blocking();
                    } else {
                        child.lock_read_blocking();
                    }
                    rev_entries.push((child, idx));
                    node = child;
                }
                None => break,
            }
        }

        let depth = rev_entries.len() as u8;
        let mut path = Self::new(btree_id, *pos);
        path.depth = depth;
        for i in 0..depth as usize {
            if i >= BTREE_MAX_DEPTH {
                break;
            }
            path.set_lock_type_at(i as u8, lock_tag);
        }
        for (i, &(n, idx)) in rev_entries.iter().rev().enumerate() {
            if i >= BTREE_MAX_DEPTH {
                break;
            }
            let mut iter = BtreeNodeIter::new();
            iter.init(&n.set, n.nsets, pos);
            path.l[i] = Some(BtreePathLevel {
                node_idx: idx,
                node_ptr: n as *const BtreeNode,
                iter,
                lock_seq: n.lock.six_lock_seq(),
            });
        }
        path
    }

    /// 释放路径上所有锁并重置 nodes_locked
    ///
    /// 使用 `node_ptr` 直接解引用（无需 Btree 引用），并清零 nodes_locked 位。
    /// 重置 nodes_locked 防止 split retry 后 double-unlock，同时使 Drop 下安全。
    pub fn unlock_all(&mut self) {
        for i in 0..self.depth as usize {
            if i >= BTREE_MAX_DEPTH {
                break;
            }
            let typ = self.lock_type_at(i as u8);
            if typ == BTREE_NODE_UNLOCKED {
                continue;
            }
            if let Some(ref lvl) = self.l[i] {
                let node = unsafe { &*lvl.node_ptr };
                match typ {
                    BTREE_NODE_READ_LOCKED => node.unlock_read(),
                    BTREE_NODE_INTENT_LOCKED => node.unlock_intent(),
                    BTREE_NODE_WRITE_LOCKED => node.unlock_write(),
                    _ => {}
                }
            }
            self.set_lock_type_at(i as u8, BTREE_NODE_UNLOCKED);
        }
    }

    /// 根据 level 和 tree 获取节点引用，并刷新 `node_ptr`
    fn level_node_refresh(&mut self, tree: &Btree, level: u8) -> &BtreeNode {
        let node_ptr = {
            let lvl = self.l[level as usize].as_ref().unwrap();
            match lvl.node_idx {
                usize::MAX => &tree.root as *const BtreeNode,
                idx => &tree.child_nodes[idx] as *const BtreeNode,
            }
        };
        self.l[level as usize].as_mut().unwrap().node_ptr = node_ptr;
        unsafe { &*node_ptr }
    }

    /// 升级指定 level 从 read → intent（必须已持有 read 锁）
    ///
    /// bcachefs: `btree_node_lock_increment`(intent) + `unlock_read`
    pub fn upgrade_read_to_intent(&mut self, tree: &Btree, level: u8) {
        let typ = self.lock_type_at(level);
        if typ == BTREE_NODE_INTENT_LOCKED || typ == BTREE_NODE_WRITE_LOCKED {
            return;
        }
        debug_assert_eq!(typ, BTREE_NODE_READ_LOCKED);
        let node = self.level_node_refresh(tree, level);
        node.lock_increment(SixLockType::Intent);
        node.unlock_read();
        self.set_lock_type_at(level, BTREE_NODE_INTENT_LOCKED);
    }

    /// 升级 leaf（level 0）从 intent → write
    ///
    /// 对应 bcachefs `bch2_btree_node_lock_write` (locking.h:538)
    /// 处理四种锁状态：
    /// - WRITE: 已持写锁 → no-op
    /// - UNLOCKED: split retry 后 path 被完全解锁 → 先获取 intent 再升级写锁
    /// - READ: 先升级 intent 再升级写锁
    /// - INTENT: 直接升级写锁
    pub fn upgrade_leaf_to_write(&mut self, tree: &Btree) {
        if self.lock_type_at(0) == BTREE_NODE_WRITE_LOCKED {
            return;
        }

        // 确保持有 intent 锁后再升级 write
        match self.lock_type_at(0) {
            BTREE_NODE_UNLOCKED => {
                self.level_node_refresh(tree, 0).lock_intent_blocking();
                self.set_lock_type_at(0, BTREE_NODE_INTENT_LOCKED);
            }
            BTREE_NODE_READ_LOCKED => {
                self.upgrade_read_to_intent(tree, 0);
            }
            BTREE_NODE_INTENT_LOCKED => {
                self.level_node_refresh(tree, 0);
            }
            _ => unreachable!(),
        }

        debug_assert_eq!(self.lock_type_at(0), BTREE_NODE_INTENT_LOCKED);
        self.level_node_refresh(tree, 0).lock_write_blocking();
        self.set_lock_type_at(0, BTREE_NODE_WRITE_LOCKED);
    }

    /// 降级 leaf 从 write → intent（对应 bcachefs `bch2_btree_node_unlock_write_inlined`）
    ///
    /// 先释放写锁（six_unlock_write），再标记为 INTENT_LOCKED。
    /// intent_lock_recurse 计数不变，实际仍持有 intent 锁。
    pub fn downgrade_leaf_from_write(&mut self, tree: &Btree) {
        debug_assert_eq!(self.lock_type_at(0), BTREE_NODE_WRITE_LOCKED);
        let node = self.level_node_refresh(tree, 0);
        node.unlock_write();
        self.set_lock_type_at(0, BTREE_NODE_INTENT_LOCKED);
    }

    /// 获取指定层的节点引用
    pub fn node_at<'a>(&self, tree: &'a Btree, level: u8) -> Option<&'a BtreeNode> {
        let idx = level as usize;
        if idx >= BTREE_MAX_DEPTH {
            return None;
        }
        self.l[idx].as_ref().map(|lvl| match lvl.node_idx {
            usize::MAX => &tree.root,
            idx => &tree.child_nodes[idx],
        })
    }

    /// 获取叶子节点引用（l[0]）
    pub fn leaf_node<'a>(&self, tree: &'a Btree) -> Option<&'a BtreeNode> {
        self.node_at(tree, 0)
    }
}

// ═══════════════════════════════════════════════════════════════
// BtreeIter — 游标迭代器（带路径记录）
// ═══════════════════════════════════════════════════════════════

/// BtreeIter — B+tree 游标迭代器
///
/// 对应 bcachefs `struct btree_iter` (types.h:602)
/// 创建时遍历 root→leaf 记录路径，沿路径持有读锁。
///
/// # 使用示例
///
/// ```ignore
/// let mut iter = BtreeIter::new(&tree, pos);
/// while let Some(entry) = iter.next() {
///     // process entry
/// }
/// ```
pub struct BtreeIter {
    tree: *const Btree,
    pos: Bpos,
    started: bool,
    /// 在 trans->paths[] 中的索引（仅 trans-backed 模式有效）
    pub(crate) path_idx: usize,
    /// trans-backed: 指向 trans.paths[path_idx] 的原始指针
    path_ptr: *mut BtreeIterPath,
    /// standalone 模式（path_ptr==null）：iter 拥有的私有路径
    own_path: Option<BtreeIterPath>,
    /// seek 时是否使用 intent 锁（trans-backed=true, standalone=false）
    want_intent: bool,
}

/// Safety: tree 和 path_ptr 指向的数据由 BtreeTrans 的 Arc<BchVol> 保证存活
unsafe impl Send for BtreeIter {}
unsafe impl Sync for BtreeIter {}

impl BtreeIter {
    fn tree_ref(&self) -> &Btree {
        unsafe { &*self.tree }
    }
}

impl BtreeIter {
    /// 创建独立迭代器（standalone，供测试和无 trans 场景使用）
    pub fn new(btree: &Btree, pos: Bpos) -> Self {
        let path = BtreeIterPath::traverse(btree, btree.btree_id, &pos, false);
        BtreeIter {
            tree: btree as *const Btree,
            pos,
            started: false,
            path_idx: 0,
            path_ptr: std::ptr::null_mut(),
            own_path: Some(path),
            want_intent: false,
        }
    }

    /// 创建 trans-backed 迭代器（由 BtreeTrans::iter 调用）
    pub(crate) fn from_trans(
        tree: *const Btree,
        pos: Bpos,
        path_idx: usize,
        path_ptr: *mut BtreeIterPath,
    ) -> Self {
        BtreeIter {
            tree,
            pos,
            started: false,
            path_idx,
            path_ptr,
            own_path: None,
            want_intent: true,
        }
    }

    /// 获取路径可变引用
    fn path_mut(&mut self) -> &mut BtreeIterPath {
        if !self.path_ptr.is_null() {
            unsafe { &mut *self.path_ptr }
        } else {
            self.own_path.as_mut().expect("BtreeIter has no path")
        }
    }

    /// 返回路径索引
    pub fn path_index(&self) -> usize {
        self.path_idx
    }

    /// 返回事务拥有的路径地址；standalone 迭代器返回空指针。
    ///
    /// 更新必须复用创建它的事务路径，不能把独立搜索迭代器或其他事务
    /// 的路径索引带入当前事务。
    pub(crate) fn transaction_path_ptr(&self) -> *mut BtreeIterPath {
        self.path_ptr
    }

    /// 返回迭代器关联的 btree_id
    pub fn btree_id(&self) -> BtreeId {
        self.tree_ref().btree_id
    }

    /// 返回迭代器当前位置
    pub fn pos(&self) -> Bpos {
        self.pos
    }

    /// 查看当前位置的条目（精确匹配，仅当 entry.pos == self.pos 时返回）
    pub fn peek(&self) -> Option<BtreeEntry> {
        if let Some(ref cb) = self.tree_ref().lookup_override {
            if let Some(entry) = cb(&self.pos) {
                return Some(entry);
            }
        }
        let tree = self.tree_ref();
        let pos = self.pos;
        let path = if !self.path_ptr.is_null() {
            unsafe { &*self.path_ptr }
        } else {
            self.own_path.as_ref().unwrap()
        };
        path.leaf_node(tree)
            .and_then(|leaf| leaf.lookup(&pos).cloned())
    }
    pub fn seek(&mut self, pos: Bpos) {
        if self.pos == pos {
            return;
        }
        let tree = unsafe { &*self.tree };
        let new_path = BtreeIterPath::traverse(tree, tree.btree_id, &pos, self.want_intent);
        let path = self.path_mut();
        path.unlock_all();
        *path = new_path;
        self.pos = pos;
        self.started = false;
    }

    /// 返回路径中第一个 >= pos 的条目
    fn peek_ge(&self, pos: &Bpos) -> Option<BtreeEntry> {
        if let Some(ref cb) = self.tree_ref().lookup_override {
            if let Some(entry) = cb(pos) {
                return Some(entry);
            }
        }
        let tree = self.tree_ref();
        let path = if !self.path_ptr.is_null() {
            unsafe { &*self.path_ptr }
        } else {
            self.own_path.as_ref().unwrap()
        };
        path.leaf_node(tree)
            .and_then(|leaf| leaf.lookup(pos).or_else(|| leaf.next_entry(pos)).cloned())
    }

    /// 从 path.l[0] 的 BtreeNodeIter 取下一个条目（跳过 BtreePtr）
    fn node_iter_next_entry(&mut self) -> Option<BtreeEntry> {
        let tree = unsafe { &*self.tree };
        let pos = self.pos;
        let path = if !self.path_ptr.is_null() {
            unsafe { &mut *self.path_ptr }
        } else {
            self.own_path.as_mut().unwrap()
        };
        let leaf = match path.leaf_node(tree) {
            Some(l) => l,
            None => return None,
        };
        let nsets = leaf.nsets;
        let l0_mut = match path.l[0].as_mut() {
            Some(l) => l,
            None => return None,
        };
        if l0_mut.iter.nr != nsets {
            l0_mut.iter.init(&leaf.set, nsets, &pos);
        }
        l0_mut.iter.next(&leaf.set, true).map(|(_, e)| e.clone())
    }

    /// 跨节点：寻找比当前 pos 大的下一个条目
    fn cross_to_next(&mut self) -> Option<BtreeEntry> {
        let tree = unsafe { &*self.tree };
        let mut result: Option<BtreeEntry> = None;
        if tree.root.level == 0 {
            result = tree.root.next_entry(&self.pos).cloned();
        } else {
            for leaf in tree.child_nodes.iter().filter(|node| node.level == 0) {
                if let Some(entry) = leaf.next_entry(&self.pos) {
                    if result
                        .as_ref()
                        .map_or(true, |current| entry.pos < current.pos)
                    {
                        result = Some(entry.clone());
                    }
                }
            }
        }
        if let Some(ref entry) = result {
            self.pos = entry.pos;
            self.started = true;
        }
        result
    }

    /// 跨节点：寻找比当前 pos 小的上一个条目
    fn cross_to_prev(&mut self) -> Option<BtreeEntry> {
        let tree = unsafe { &*self.tree };
        let mut result: Option<BtreeEntry> = None;
        if tree.root.level == 0 {
            result = tree.root.prev_entry(&self.pos).cloned();
        } else {
            for leaf in tree.child_nodes.iter().filter(|node| node.level == 0) {
                if let Some(entry) = leaf.prev_entry(&self.pos) {
                    if result
                        .as_ref()
                        .map_or(true, |current| entry.pos > current.pos)
                    {
                        result = Some(entry.clone());
                    }
                }
            }
        }
        if let Some(ref entry) = result {
            self.pos = entry.pos;
            self.started = true;
        }
        result
    }

    /// 范围限制 peek：返回第一个 >= self.pos 且 < end 的条目
    ///
    /// 对应 bcachefs `bch2_btree_iter_peek_max` (iter.h:656)
    pub fn peek_upto(&self, end: Bpos) -> Option<BtreeEntry> {
        let entry = self.peek_ge(&self.pos)?;
        if entry.pos < end {
            Some(entry)
        } else {
            None
        }
    }

    /// 显式路径刷新：从 root 重新遍历到当前 pos
    ///
    /// 对应 bcachefs `bch2_btree_iter_traverse` (iter.h:652)
    /// 在树结构可能变更（如 split/compact）后，用于确保路径正确。
    pub fn traverse(&mut self) {
        let tree = unsafe { &*self.tree };
        let new_path = BtreeIterPath::traverse(tree, tree.btree_id, &self.pos, self.want_intent);
        let path = self.path_mut();
        path.unlock_all();
        *path = new_path;
        self.started = false;
    }

    /// 设置迭代器位置（不触发遍历）
    ///
    /// 对应 bcachefs `bch2_btree_iter_set_pos` (iter.h:690)
    /// 仅更新 self.pos，不重新遍历路径。下一次 peek/next 会触发遍历。
    pub fn set_pos(&mut self, pos: Bpos) {
        self.pos = pos;
        self.started = false;
    }
}

impl Iterator for BtreeIter {
    type Item = BtreeEntry;

    fn next(&mut self) -> Option<Self::Item> {
        if !self.started {
            self.started = true;
            let entry = self.node_iter_next_entry();
            if let Some(ref e) = entry {
                self.pos = e.pos;
                return Some(e.clone());
            }
            return self.cross_to_next();
        }
        if let Some(entry) = self.node_iter_next_entry() {
            self.pos = entry.pos;
            return Some(entry);
        }
        self.cross_to_next()
    }
}

impl BtreeIter {
    /// 后退一个条目
    pub fn prev(&mut self) -> Option<BtreeEntry> {
        let tree = unsafe { &*self.tree };
        let pos = self.pos;
        let path = if !self.path_ptr.is_null() {
            unsafe { &mut *self.path_ptr }
        } else {
            self.own_path.as_mut().unwrap()
        };
        let leaf = match path.leaf_node(tree) {
            Some(l) => l,
            None => return None,
        };
        let l0_mut = match path.l[0].as_mut() {
            Some(l) => l,
            None => return None,
        };
        let nsets = leaf.nsets;
        if l0_mut.iter.nr != nsets {
            l0_mut.iter.init_reverse(&leaf.set, nsets, &pos);
        }
        let result = l0_mut.iter.prev(&leaf.set, true).map(|(_, e)| e.clone());
        if let Some(ref entry) = result {
            self.pos = entry.pos;
            return Some(entry.clone());
        }
        self.cross_to_prev()
    }

    /// 前进到下一个键位置并返回是否还有更多键
    ///
    /// 对应 bcachefs `bch2_btree_iter_advance` (iter.c:2411)
    /// 将当前位置设为后继键，不触发遍历。返回 false 表示已达键空间末尾。
    pub fn advance(&mut self) -> bool {
        if self.pos.offset < u64::MAX {
            self.pos.offset += 1;
        } else if self.pos.inode < u64::MAX {
            self.pos.inode += 1;
            self.pos.offset = 0;
            self.pos.snapshot = 0;
        } else {
            return false;
        }
        self.started = false;
        true
    }

    /// 后退到上一个键位置并返回是否还有更多键
    ///
    /// 对应 bcachefs `bch2_btree_iter_rewind` (iter.c:2425)
    /// 将当前位置设为前驱键，不触发遍历。返回 false 表示已达键空间起点。
    pub fn rewind(&mut self) -> bool {
        if self.pos.offset > 0 {
            self.pos.offset -= 1;
        } else if self.pos.inode > 0 {
            self.pos.inode -= 1;
            self.pos.offset = u64::MAX;
            self.pos.snapshot = u32::MAX;
        } else {
            return false;
        }
        self.started = false;
        true
    }
}

impl Drop for BtreeIter {
    fn drop(&mut self) {
        if self.path_ptr.is_null() {
            if let Some(ref mut path) = self.own_path {
                path.unlock_all();
            }
        }
    }
}

/// 创建迭代器（从最小键开始遍历）
impl From<&Btree> for BtreeIter {
    fn from(tree: &Btree) -> Self {
        BtreeIter::new(
            tree,
            Bpos {
                inode: 0,
                offset: 0,
                snapshot: 0,
            },
        )
    }
}

// ═══════════════════════════════════════════════════════════════
// 测试
// ═══════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::bch_vol::BchVol;
    use crate::block_device::BchDev;
    use crate::btree::key::Bpos;
    use crate::btree::tree::{Btree, BtreeIter};
    use crate::btree::types::BTREE_ID_ALLOC;

    fn make_dev() -> Arc<BchDev> {
        let vol = Arc::new(BchVol::new());
        Arc::new(BchDev::new(vol))
    }

    #[test]
    fn test_empty_tree() {
        let dev = make_dev();
        let tree = Btree::new(BTREE_ID_ALLOC, &dev);
        let mut iter = BtreeIter::new(&tree, Bpos::default());
        assert!(iter.next().is_none());
    }

    #[test]
    fn test_advance_at_end() {
        let dev = make_dev();
        let tree = Btree::new(BTREE_ID_ALLOC, &dev);
        let mut iter = BtreeIter::new(
            &tree,
            Bpos {
                inode: u64::MAX,
                offset: u64::MAX,
                snapshot: u32::MAX,
            },
        );
        assert!(!iter.advance(), "at SPOS_MAX advance should return false");
    }

    #[test]
    fn test_rewind_at_start() {
        let dev = make_dev();
        let tree = Btree::new(BTREE_ID_ALLOC, &dev);
        let mut iter = BtreeIter::new(&tree, Bpos::default());
        assert!(!iter.rewind(), "at POS_MIN rewind should return false");
    }
}
