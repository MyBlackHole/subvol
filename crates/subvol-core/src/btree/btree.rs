//! Btree — bcachefs 对齐的 B-tree 公共 API
//!
//! 提供 get/insert/delete 高级接口，内部使用 BtreeTrans + BtreeIter。

use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use tokio::sync::Notify;

use crate::bch_vol::BchVol;
use crate::block_device::BchDev;
use crate::btree::io::{bch2_btree_add_journal_pin, bch2_btree_node_prep_for_write};
use crate::btree::interior::btree_node_needs_merge;
use crate::btree::iter::BtreeIter;
use crate::btree::key::{
    entry_packed_size, BchVal, Bpos, BtreeEntry, BtreeKey, ExtentValue, KeyType, KeyValue,
};
use crate::btree::key_cache::KeyCache;
use crate::btree::node::{
    bch2_btree_node_iter_advance, bch2_btree_node_iter_init, bch2_btree_node_iter_init_from_start,
    bch2_btree_node_iter_peek_all, bset_u64s, BtreeNode, BtreeNodeIter,
};
use crate::btree::transaction::BtreeTrans;
use crate::btree::types::{BtreeRoot, NodeCache, PendingRootJournal, ROOT_CACHE_ADDR};
use crate::btree::update::{BtreeInteriorUpdate, BtreeUpdateMode, InteriorUpdateType};
use crate::btree::writer::BtreeNodeWriter;
use crate::{StorageError, Watermark};

/// RAII guard for split-allocated nodes — bcachefs 对齐的错误路径回滚
///
/// split 过程中如果 parent update 失败，guard 在 drop 时自动释放
/// 已分配的右节点（从 cache 中移除）。
/// 操作成功时调用 `disarm()` 禁用回滚。
struct SplitGuard {
    cache: Arc<NodeCache>,
    right_addr: u64,
    disarmed: bool,
}

/// bcachefs `bch_fs_btree_interior_updates` 的 Rust 状态。
///
/// 本地 bcachefs 以 `interior_updates.lock` 保护 update 总表、unwritten
/// 链表和每个节点的 `write_blocked` 链表，并以 waitlist 唤醒等待者。
/// 这里保留同样的三组关系；节点本身只保留 fast-path 的非空 bit。
struct InteriorUpdates {
    state: Mutex<InteriorUpdatesState>,
    wait: Notify,
}

struct InteriorUpdatesState {
    next_id: u64,
    list: HashMap<u64, Vec<u64>>,
    node_blocked: HashMap<u64, Vec<u64>>,
}

impl InteriorUpdates {
    fn new() -> Self {
        Self {
            state: Mutex::new(InteriorUpdatesState {
                next_id: 1,
                list: HashMap::new(),
                node_blocked: HashMap::new(),
            }),
            wait: Notify::new(),
        }
    }

    /// 对应 bcachefs `bch2_btree_update_start()` 加入 `list`。
    fn start(&self) -> u64 {
        let mut state = self.state.lock().unwrap();
        let id = state.next_id;
        state.next_id = state.next_id.wrapping_add(1).max(1);
        state.list.insert(id, Vec::new());
        id
    }

    /// 对应 `btree_update_updated_node()` 的
    /// `list_add(&as->write_blocked_list, &b->write_blocked)`。
    fn block_node(&self, update_id: u64, node_id: u64) {
        let mut state = self.state.lock().unwrap();
        let nodes = state
            .list
            .get_mut(&update_id)
            .expect("interior update must be registered before blocking a node");
        if !nodes.contains(&node_id) {
            nodes.push(node_id);
        }
        let blocked = state.node_blocked.entry(node_id).or_default();
        if !blocked.contains(&update_id) {
            blocked.push(update_id);
        }
    }

    /// 从节点的 write_blocked 链表摘除一个 update。
    /// 返回值表示该节点是否已经没有其他阻塞 update。
    fn unblock_node(&self, update_id: u64, node_id: u64) -> bool {
        let mut state = self.state.lock().unwrap();
        if let Some(nodes) = state.list.get_mut(&update_id) {
            nodes.retain(|id| *id != node_id);
        }
        let empty = if let Some(blocked) = state.node_blocked.get_mut(&node_id) {
            blocked.retain(|id| *id != update_id);
            blocked.is_empty()
        } else {
            true
        };
        if empty {
            state.node_blocked.remove(&node_id);
        }
        empty
    }

    /// 对应 `btree_update_reparent()`：旧节点被替换时，迁移其
    /// write-blocked 关系到当前 update，并唤醒 flush waiters。
    fn reparent(&self, node_id: u64, update_id: u64) {
        let mut state = self.state.lock().unwrap();
        let blockers = state.node_blocked.remove(&node_id).unwrap_or_default();
        if let Some(nodes) = state.list.get_mut(&update_id) {
            if !nodes.contains(&node_id) {
                nodes.push(node_id);
            }
        }
        if !blockers.is_empty() || state.list.contains_key(&update_id) {
            state.node_blocked.insert(node_id, vec![update_id]);
        }
        drop(state);
        self.wait.notify_waiters();
    }

    /// 对应 `bch2_btree_update_free()` 从 update list 摘除并唤醒 waiters。
    fn finish(&self, update_id: u64) {
        let mut state = self.state.lock().unwrap();
        if let Some(nodes) = state.list.remove(&update_id) {
            for node_id in nodes {
                if let Some(blocked) = state.node_blocked.get_mut(&node_id) {
                    blocked.retain(|id| *id != update_id);
                    if blocked.is_empty() {
                        state.node_blocked.remove(&node_id);
                    }
                }
            }
        }
        drop(state);
        self.wait.notify_waiters();
    }

    /// 对应 `bch2_btree_interior_updates_pending()`。
    fn pending(&self) -> bool {
        !self.state.lock().unwrap().list.is_empty()
    }

    /// 对应 `bch2_btree_interior_updates_flush()` 的 closure waitlist。
    async fn flush(&self) -> bool {
        let mut did_wait = false;
        loop {
            let notified = self.wait.notified();
            if !self.pending() {
                return did_wait;
            }
            did_wait = true;
            notified.await;
        }
    }

    /// 对应 `bch2_btree_interior_updates_flush()` 的 waitlist 语义。
    async fn wait_on_node(&self, node: &BtreeNode) {
        while node.btree_node_write_blocked() {
            let notified = self.wait.notified();
            if !node.btree_node_write_blocked() {
                break;
            }
            notified.await;
        }
    }
}

/// 节点级 `write_blocked` 持有者，对齐 bcachefs 的
/// `btree_update::write_blocked_list` 生命周期。
struct NodeWriteBlockedGuard {
    state: Arc<std::sync::atomic::AtomicBool>,
    updates: Arc<InteriorUpdates>,
    update_id: u64,
    node_id: u64,
    nodes_written: Arc<std::sync::atomic::AtomicBool>,
    completed: bool,
}

impl Drop for NodeWriteBlockedGuard {
    fn drop(&mut self) {
        if self.completed {
            self.nodes_written.store(true, Ordering::Release);
        }
        let clear = self.updates.unblock_node(self.update_id, self.node_id);
        if clear {
            self.state.store(false, Ordering::Release);
        }
        self.updates.finish(self.update_id);
    }
}

impl NodeWriteBlockedGuard {
    /// 对齐 bcachefs `btree_update_done()` → worker completion：
    /// write-in-flight 时把 guard 留到节点 IO 完成后再释放。
    fn release_after_write(self, node: Arc<BtreeNode>) {
        self.release_after_writes(vec![node]);
    }

    /// 一个 root update 可能同时提交多个 new nodes；对应 bcachefs
    /// `closure_get(&as->cl)` 为每个 `btree_update_write_new_node()` 保留
    /// 一个完成引用，必须等全部 IO 完成后才进入 nodes-written 阶段。
    fn release_after_writes(self, nodes: Vec<Arc<BtreeNode>>) {
        if nodes.iter().all(|node| !node.is_write_in_flight()) {
            let mut guard = self;
            guard.completed = true;
            drop(guard);
            return;
        }

        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn_blocking(move || {
                for node in nodes {
                    node.wait_on_write(None);
                }
                let mut guard = self;
                guard.completed = true;
                drop(guard);
            });
        } else {
            for node in nodes {
                node.wait_on_write(None);
            }
            let mut guard = self;
            guard.completed = true;
            drop(guard);
        }
    }
}

impl SplitGuard {
    fn new(cache: Arc<NodeCache>, right_addr: u64) -> Self {
        Self {
            cache,
            right_addr,
            disarmed: false,
        }
    }

    fn disarm(&mut self) {
        self.disarmed = true;
    }
}

impl Drop for SplitGuard {
    fn drop(&mut self) {
        if !self.disarmed {
            self.cache.take_node(self.right_addr);
        }
    }
}

/// B-tree 主结构 — 对应 bcachefs `bch_fs` 中的 btree 实例
pub struct Btree {
    /// B-tree 根（UnsafeCell — bcachefs 对齐的内部分锁）
    pub(crate) root: UnsafeCell<BtreeRoot>,
    /// 节点缓存
    cache: Arc<NodeCache>,
    /// Key 级读缓存：减少热 key 的 btree 全路径下降
    pub(crate) key_cache: KeyCache,
    /// depth=0 时 root 节点是否被修改且未 flush（替代 cache dirty tracking）
    root_modified: AtomicBool,
    /// 本 Btree 的类型（Extents / Snapshots / Alloc 等）
    btype: crate::btree::BtreeId,
    /// Phase 1 根指针 journal 对齐：待 caller 写入 journal 的根变更信息
    pub(crate) pending_root_journal: UnsafeCell<Option<PendingRootJournal>>,
    /// Phase 2 journal safety net：当前根节点的磁盘地址和 level
    pub(crate) current_root_disk: UnsafeCell<Option<(u64, u8)>>,
    /// 根操作锁：序列化所有根写入操作（set_root_internal、load_root、clear、increase_depth）
    pub(crate) root_lock: Mutex<()>,
    /// bcachefs `c->btree.interior_updates`：异步内部更新总表及等待通知。
    interior_updates: Arc<InteriorUpdates>,
    /// 绑定的主设备强引用，避免 `BchVol` 解包后 Weak 失效
    device: OnceLock<Arc<BchDev>>,
    /// 读取上下文：对齐 bcachefs `b->c` 的树级卷引用
    vol: OnceLock<Weak<BchVol>>,
    #[cfg(test)]
    /// 测试/脱离卷场景的设备引用
    test_device: OnceLock<Arc<BchDev>>,
}

impl Btree {
    /// 创建一个新的空 B-tree
    pub fn new_with_type(btype: crate::btree::BtreeId) -> Self {
        let cache = Arc::new(NodeCache::new());
        let node = Arc::new(BtreeNode::new_leaf());
        Self {
            root: UnsafeCell::new(BtreeRoot { node, depth: 0 }),
            cache,
            key_cache: KeyCache::new(),
            root_modified: AtomicBool::new(false),
            btype,
            pending_root_journal: UnsafeCell::new(None),
            current_root_disk: UnsafeCell::new(None),
            root_lock: Mutex::new(()),
            interior_updates: Arc::new(InteriorUpdates::new()),
            device: OnceLock::new(),
            vol: OnceLock::new(),
            #[cfg(test)]
            test_device: OnceLock::new(),
        }
    }

    /// 创建一个新的空 B-tree（向后兼容，默认 Extents）
    pub fn new() -> Self {
        Self::new_with_type(crate::btree::BtreeId::Extents)
    }

    /// subvol 内部: 清空整棵树 — 用新的空根节点替换现有根
    pub fn clear(&self) {
        let _lock = self.root_lock.lock().unwrap();
        let new_root = BtreeRoot {
            node: Arc::new(BtreeNode::new_leaf()),
            depth: 0,
        };
        let root = unsafe { &mut *self.root.get() };
        *root = new_root;
    }

    /// 设置树级卷引用。
    pub fn set_vol_ref(&self, vol: &Arc<BchVol>) {
        self.vol.set(Arc::downgrade(vol)).ok();
        if let Some(dev) = vol.primary_device_rcu_noerror() {
            self.device.set(dev).ok();
        }
        unsafe { &*self.root.get() }.node.set_vol_ref(vol);
    }

    pub(crate) fn set_device_ref(&self, dev: Arc<BchDev>) {
        self.device.set(dev).ok();
    }

    #[cfg(test)]
    /// 设置测试用设备引用。
    pub fn set_test_device(&self, dev: Arc<BchDev>) {
        self.test_device.set(dev.clone()).ok();
        self.set_device_ref(dev);
    }

    pub(crate) fn vol_device(&self) -> Arc<BchDev> {
        if let Some(dev) = self.device.get().cloned() {
            return dev;
        }
        if let Some(vol) = self.vol.get().and_then(|w| w.upgrade()) {
            if let Some(dev) = vol.primary_device_rcu_noerror() {
                return dev;
            }
        }
        #[cfg(test)]
        {
            if let Some(dev) = self.test_device.get().cloned() {
                return dev;
            }
        }
        panic!("Btree: vol not set — call set_vol_ref before IO")
    }

    /// 当前绑定的设备索引（多设备对齐：来自注册表解析）
    pub(crate) fn dev_idx(&self) -> u8 {
        // 优先从 device 字段获取 dev_idx（set_vol_ref/set_device_ref 时设置）
        if let Some(dev) = self.device.get() {
            return dev.dev_idx;
        }
        // 回退到 vol 解析（primary device）
        if let Some(vol) = self.vol.get().and_then(|w| w.upgrade()) {
            if let Some(dev) = vol.primary_device_rcu_noerror() {
                return dev.dev_idx;
            }
        }
        #[cfg(test)]
        {
            if let Some(dev) = self.test_device.get() {
                return dev.dev_idx;
            }
        }
        0
    }

    /// bcachefs 恢复路径对齐: from_root — 从已有的根节点创建
    /// bcachefs 对齐: bch2_btree_set_root_for_read — 从已有根节点构造 Btree
    pub fn bch2_btree_set_root_for_read(
        root: BtreeRoot,
        cache: Arc<NodeCache>,
        btype: crate::btree::BtreeId,
    ) -> Self {
        Self {
            root: UnsafeCell::new(root),
            cache: cache.clone(),
            key_cache: KeyCache::new(),
            root_modified: AtomicBool::new(false),
            btype,
            pending_root_journal: UnsafeCell::new(None),
            current_root_disk: UnsafeCell::new(None),
            root_lock: Mutex::new(()),
            interior_updates: Arc::new(InteriorUpdates::new()),
            device: OnceLock::new(),
            vol: OnceLock::new(),
            #[cfg(test)]
            test_device: OnceLock::new(),
        }
    }
    /// 获取根节点（通过 UnsafeCell raw ptr — 不创建长期借引用）
    pub fn root(&self) -> &BtreeRoot {
        unsafe { &*self.root.get() }
    }

    /// 取回绑定的 `BchVol` 引用（读路径用于构造事务上下文）。
    pub(crate) fn vol_arc(&self) -> Option<Arc<BchVol>> {
        self.vol.get().and_then(|w| w.upgrade())
    }

    /// Phase 1 根指针 journal 对齐：返回本 Btree 的类型
    pub fn btype(&self) -> crate::btree::BtreeId {
        self.btype
    }

    /// Phase 1 根指针 journal 对齐：提取待写入 journal 的根变更信息
    ///
    /// 调用后内部状态重置为 None。caller 应在适当时机通过
    /// `append_btree_root` 写入 journal。
    pub fn take_pending_root_journal(&self) -> Option<PendingRootJournal> {
        unsafe { &mut *self.pending_root_journal.get() }.take()
    }

    /// Phase 2 journal safety net：返回当前已知的根磁盘地址和 level
    pub fn current_root_disk_info(&self) -> Option<(u64, u8)> {
        unsafe { *self.current_root_disk.get() }
    }

    /// 设置根节点 node_size（仅用于测试）
    #[cfg(test)]
    pub(crate) fn set_root_node_size(&self, size: u32) {
        let root = unsafe { &mut *self.root.get() };
        if let Some(node) = Arc::get_mut(&mut root.node) {
            node.node_size = size;
        }
    }

    fn node_progress(node: &BtreeNode) -> (u16, u16, u16) {
        let written = node.written;
        let sectors = (node.node_size / 512).min(u16::MAX as u32) as u16;
        let remaining = node
            .node_size
            .saturating_sub(node.total_data_bytes())
            .div_ceil(8)
            .min(u16::MAX as u32) as u16;
        (written, sectors, remaining)
    }

    fn init_interior_update(
        update: &mut BtreeInteriorUpdate,
        mode: BtreeUpdateMode,
        node: &BtreeNode,
    ) {
        update.set_mode(mode);
        update.set_node_span(node.min_key, node.max_key);
        update.set_update_level_span(node.level, node.level);
        let (node_written, node_sectors, node_remaining) = Self::node_progress(node);
        update.set_node_progress(node_written, node_sectors, node_remaining);
    }

    /// bcachefs 对齐: bch2_btree_root_read — 从 backend 读取 BtreeNode 并设为 tree root
    ///
    /// root_addr=0 时跳过（空 btree）。
    /// depth 从 node.level 获取。
    pub async fn bch2_btree_root_read(
        &self,
        root_addr: u64,
        level: Option<u8>,
    ) -> Result<(), StorageError> {
        if root_addr == 0 {
            return Ok(());
        }
        let node = if self.vol_arc().is_some() {
            crate::btree::io::bch2_btree_root_read(self, root_addr)
                .await?
                .0
        } else {
            let dev = self.vol_device();
            let mut node = crate::btree::bucket_io::__bch2_load_btree_node(dev, root_addr).await?;
            crate::btree::io::bch2_btree_node_read_done(&mut node)?;
            node.try_set_block_addr(root_addr);
            node
        };
        let depth = level.unwrap_or(node.level);
        let root_node = Arc::new(BtreeNode {
            level: depth,
            ..node
        });
        crate::btree::interior::bch2_btree_set_root_inmem(self, root_node, root_addr);
        Ok(())
    }

    /// 获取 B-tree 深度
    pub fn depth(&self) -> u8 {
        unsafe { (*self.root.get()).depth }
    }

    /// bcachefs 对齐: bch2_btree_iter_peek — 查找 key
    ///
    /// 返回精确匹配 target 的 (key, value)。
    /// 未找到返回 None。
    /// 通过 KeyCache 减少热 key 的 btree 全路径下降。
    /// 仅缓存正结果（有匹配 entry），不缓存负结果（对齐 bcachefs）。
    pub fn bch2_btree_iter_peek(&self, target: &BtreeKey) -> Option<(BtreeKey, BchVal)> {
        let pos = Bpos::from_key(target);
        // ── Key cache check ──
        if let Some(entry) = self.key_cache.find(&pos) {
            if entry.key_type == target.key_type {
                if let KeyValue::Extent(extent) = entry.value {
                    let key = BtreeKey::from_bpos(pos, entry.key_type);
                    return Some((key, extent.to_bchval()));
                }
            }
        }
        // ── Normal btree lookup (内联自 get_entry_inner) ──
        let search_target = BtreeKey::from_bpos(pos, KeyType::Normal);
        let root_ref = unsafe { &*self.root.get() };
        let mut trans = BtreeTrans::new_with_cache(Arc::clone(&self.cache));
        let iter = trans.bch2_trans_get_iter(root_ref, &search_target, false, self.btype);
        let mut candidate: Option<BtreeEntry> = None;
        loop {
            let entry = iter.peek_entry()?;
            if entry.pos == pos {
                match entry.key_type {
                    KeyType::Normal | KeyType::Set => {
                        candidate = Some(entry);
                    }
                    KeyType::Deleted | KeyType::Whiteout => {
                        candidate = None;
                    }
                }
            }
            if !iter.advance() {
                break;
            }
        }
        let result = candidate.and_then(|entry| {
            if entry.key_type == target.key_type {
                Some(entry.to_key_value())
            } else {
                None
            }
        });
        // ── Cache positive result only (bcachefs 对齐: 不缓存负结果) ──
        if let Some((k, v)) = result {
            let entry = BtreeEntry::new(
                pos,
                k.key_type,
                KeyValue::Extent(ExtentValue {
                    paddr: v.paddr.get(),
                    size: 1,
                    ver: v.ver,
                    dev_idx: 0,
                    crc32c: 0,
                    crc_offset_blocks: 0,
                }),
            );
            self.key_cache.insert(pos, entry);
        }
        result
    }

    /// bcachefs 对齐: bch2_btree_iter_peek_entry — 通过 Bpos 精确匹配
    ///
    /// 如果目标位置有 Deleted/Whiteout 条目，会自动跳过并检查下一条。
    /// 这实现了 bcachefs 风格的更新模式：删除（追加 Deleted 墓碑）+ 插入（追加新值）
    /// 后，本函数始终返回最新的非删除条目。
    /// 通过 KeyCache 减少热 key 的 btree 全路径下降。
    /// 仅缓存正结果（有匹配 entry），不缓存负结果（对齐 bcachefs）。
    pub fn bch2_btree_iter_peek_entry(&self, pos: Bpos) -> Option<BtreeEntry> {
        // ── Key cache check ──
        if let Some(entry) = self.key_cache.find(&pos) {
            return Some(entry);
        }
        // ── Normal btree search (内联自 get_entry_inner) ──
        let target = BtreeKey::from_bpos(pos, KeyType::Normal);
        let root_ref = unsafe { &*self.root.get() };
        let mut trans = BtreeTrans::new_with_cache(Arc::clone(&self.cache));
        let iter = trans.bch2_trans_get_iter(root_ref, &target, false, self.btype);
        let mut candidate: Option<BtreeEntry> = None;
        loop {
            let entry = iter.peek_entry()?;
            if entry.pos == pos {
                match entry.key_type {
                    KeyType::Normal | KeyType::Set => {
                        candidate = Some(entry);
                    }
                    KeyType::Deleted | KeyType::Whiteout => {
                        candidate = None;
                    }
                }
            }
            if !iter.advance() {
                break;
            }
        }
        // ── Cache positive result only (bcachefs 对齐: 不缓存负结果) ──
        if let Some(ref entry) = candidate {
            self.key_cache.insert(pos, entry.clone());
        }
        candidate
    }

    /// 带 BtreeTrans 重启感知的缓存查找（TC4: trigger_key_cache_miss 连接）
    ///
    /// 当 key cache miss 时触发事务重启（`trigger_key_cache_miss`），
    /// 使事务循环在 commit 时重试查找路径。缓存未命中且 btree 实际有值时，
    /// 第一次调用标记重启，插入缓存后，第二次调用（重启后）命中缓存返回。
    ///
    /// 这是 `get_entry` 的变体——不改变原方法签名。
    /// 对应 bcachefs 在 key cache miss 后的重启机制。
    /// 暴露 root 引用和 cache 引用供外部迭代器使用。
    pub fn root_and_cache(&self) -> (&BtreeRoot, &Arc<NodeCache>) {
        let root_ref = unsafe { &*self.root.get() };
        (root_ref, &self.cache)
    }

    pub fn bch2_btree_iter_peek_with_restart(
        &self,
        pos: Bpos,
        trans: &mut BtreeTrans,
    ) -> Option<BtreeEntry> {
        // ── Key cache check ──
        if let Some(entry) = self.key_cache.find(&pos) {
            return Some(entry);
        }
        // ── Cache miss: trigger restart signal for transaction loop ──
        trans.trigger_key_cache_miss();
        // ── Normal btree search (内联自 get_entry_inner) ──
        let target = BtreeKey::from_bpos(pos, KeyType::Normal);
        let root_ref = unsafe { &*self.root.get() };
        let iter = trans.bch2_trans_get_iter(root_ref, &target, false, self.btype);
        let mut candidate: Option<BtreeEntry> = None;
        loop {
            let entry = iter.peek_entry()?;
            if entry.pos == pos {
                match entry.key_type {
                    KeyType::Normal | KeyType::Set => {
                        candidate = Some(entry);
                    }
                    KeyType::Deleted | KeyType::Whiteout => {
                        candidate = None;
                    }
                }
            }
            if !iter.advance() {
                break;
            }
        }
        // ── Cache positive result only (bcachefs 对齐: 不缓存负结果) ──
        if let Some(ref entry) = candidate {
            self.key_cache.insert(pos, entry.clone());
        }
        candidate
    }

    /// bcachefs 对齐: get_entry_allow_whiteout — 获取条目（允许 Whiteout，不跳过已删除条目）。
    /// 用于需要读取已删除快照节点信息的场景（祖先链遍历）。
    /// subvol 内部 pub(crate) 函数。
    pub(crate) fn get_entry_allow_whiteout(&self, pos: Bpos) -> Option<BtreeEntry> {
        let target = BtreeKey::from_bpos(pos, KeyType::Normal);
        let root_ref = unsafe { &*self.root.get() };
        let mut trans = BtreeTrans::new_with_cache(Arc::clone(&self.cache));
        let iter =
            trans.bch2_trans_get_iter(root_ref, &target, false, crate::btree::BtreeId::Extents);
        let mut candidate: Option<BtreeEntry> = None;
        loop {
            let entry = iter.peek_entry()?;
            if entry.pos == pos {
                // 接受任何 key_type（包括 Whiteout/Deleted）
                candidate = Some(entry);
            }
            if !iter.advance() {
                return candidate;
            }
        }
    }

    /// bcachefs 对齐: bch2_btree_bset_insert_key_wrapper — depth=0 单 leaf 模式 entry 插入
    /// 成功插入后使对应 Bpos 的 key cache 失效。
    pub fn bch2_btree_bset_insert_key_wrapper(&self, entry: BtreeEntry, journal_seq: u64) -> bool {
        let pos = entry.pos;
        let (inserted, root_node) = {
            let _root_lock = self.root_lock.lock().unwrap();
            let root = unsafe { &mut *self.root.get() };
            if root.depth > 0 {
                return false;
            }
            if root.node.btree_node_write_blocked() {
                return false;
            }
            let node = unsafe { &mut *(Arc::as_ptr(&root.node) as *mut BtreeNode) };
            bch2_btree_node_prep_for_write(node);
            if node.insert_entry(&entry) {
                
                node.journal_seq = journal_seq;
                (true, root.node.clone())
            } else {
                node.compact();
                if node.insert_entry(&entry) {
                    
                    node.journal_seq = journal_seq;
                    (true, root.node.clone())
                } else {
                    (false, root.node.clone())
                }
            }
        };
        if inserted {
            if let Some(vol) = self.vol_arc() {
                bch2_btree_add_journal_pin(&root_node, vol.journal_ref(), journal_seq);
            }
            self.key_cache.invalidate(&pos);
        }
        inserted
    }

    /// 插入 key/value — 支持单级和多级 B-tree
    ///
    /// depth=0 单 leaf 模式：直接插入，满时 compact 重试，仍满则 split_root
    /// depth>0 多级树模式：`find_leaf_addr` → take_node → insert → put_node
    /// 成功插入后使对应 Bpos 的 key cache 失效。
    ///
    /// 返回 Ok(true) = 插入成功，Ok(false) = 插入失败（无额外空间），Err = I/O 错误
    ///
    /// Phase 1: writer 参数仅用于 split_root 写盘路径；depth=0 普通插入保持 sync。
    pub async fn bch2_btree_insert<W: BtreeNodeWriter>(
        &self,
        writer: &W,
        key: BtreeKey,
        value: BchVal,
        journal_seq: u64,
    ) -> Result<bool, StorageError> {
        let pos = Bpos::from_key(&key);
        let new_key_u64s = entry_packed_size(&BtreeEntry::from((key, value))) as u32 / 8;
        // bcachefs commit.c:332-368 uses a path-held leaf; depth=0 has no
        // separate path in Rust, so serialize the root decision with the same
        // root publication lock used by split/collapse.
        let depth = {
            let _root_lock = self.root_lock.lock().unwrap();
            unsafe { (*self.root.get()).depth }
        };
        let result = if depth == 0 {
            // depth=0: single leaf mode — use scoped root borrow so split_root can't alias
            let insert_ok = {
                let _root_lock = self.root_lock.lock().unwrap();
                let root = unsafe { &mut *self.root.get() };
                if root.node.btree_node_write_blocked() {
                    return Ok(false);
                }
                // The caller holds the transaction's write lock.  Mirror
                // bch2_btree_insert_key_leaf(): mutate the locked node even
                // while journal/read references keep Arc handles alive.
                let node = unsafe { &mut *(Arc::as_ptr(&root.node) as *mut BtreeNode) };
                bch2_btree_node_prep_for_write(node);
                if node.insert(key, value) {
                    
                    node.journal_seq = journal_seq;
                    self.root_modified.store(true, Ordering::Release);
                    true
                } else {
                    node.compact();
                    // A1: compact_fits — 只有 compact 释放了足够空间才重试 insert
                    if node.bch2_btree_node_compact_fits(new_key_u64s) && node.insert(key, value) {
                        
                        node.journal_seq = journal_seq;
                        self.root_modified.store(true, Ordering::Release);
                        true
                    } else {
                        false // needs split root
                    }
                }
            };
            let result = if insert_ok {
                if let Some(vol) = self.vol_arc() {
                    let root = unsafe { &*self.root.get() };
                    bch2_btree_add_journal_pin(&root.node, vol.journal_ref(), journal_seq);
                }
                true
            } else {
                // root borrow dropped — safe to call split_root
                match self
                    .split_root(writer, Some((key, value)), journal_seq)
                    .await
                {
                    Ok(true) => {
                        
                        true
                    }
                    Ok(false) => false,
                    Err(e) => return Err(e),
                }
            };
            result
        } else {
            'insert_multi: loop {
                let mut path: Vec<u64> = Vec::new();
                let leaf_addr = match self.bch2_btree_path_traverse_one(&key, &mut path) {
                    Some(addr) => addr,
                    None => {
                        eprintln!("FAIL: bch2_btree_path_traverse_one returned None");
                        break 'insert_multi false;
                    }
                };

                // ── Phase 1: bch2_btree_insert_key_leaf ──
                if self.bch2_btree_insert_key_leaf(leaf_addr, key, value, journal_seq) {
                    
                    break 'insert_multi true;
                }

                // ── Phase 2: bch2_btree_split_leaf ──
                let split_result = self
                    .bch2_btree_split_leaf(writer, leaf_addr, key, value, journal_seq, &mut path)
                    .await?;
                break 'insert_multi split_result;
            }
        };
        if result {
            self.key_cache.invalidate(&pos);
        }
        Ok(result)
    }

    /// 删除 key — 支持单级和多级 B-tree。
    /// bcachefs 对齐: bch2_btree_delete_at — 找到 leaf，删除 key，然后尝试合并。
    ///
    /// 返回 Ok(true) = 删除成功，Ok(false) = key 不存在，Err = I/O 错误
    pub async fn bch2_btree_delete<W: BtreeNodeWriter>(
        &self,
        writer: &W,
        key: &BtreeKey,
        journal_seq: u64,
    ) -> Result<bool, StorageError> {
        let pos = Bpos::from_key(key);
        let depth = {
            let _root_lock = self.root_lock.lock().unwrap();
            unsafe { (*self.root.get()).depth }
        };
        let result = if depth == 0 {
            let _root_lock = self.root_lock.lock().unwrap();
            let root = unsafe { &mut *self.root.get() };
            let deleted = if let Some(node) = Arc::get_mut(&mut root.node) {
                bch2_btree_node_prep_for_write(node);
                node.delete_key(key)
            } else {
                false
            };
            if deleted {
                let node = unsafe { &mut *(Arc::as_ptr(&root.node) as *mut BtreeNode) };
                node.journal_seq = journal_seq;
                self.root_modified.store(true, Ordering::Release);
            }
            deleted
        } else {
            self.bch2_btree_delete_at(writer, key, journal_seq).await?
        };
        if result {
            if depth == 0 {
                if let Some(vol) = self.vol_arc() {
                    let root = unsafe { &*self.root.get() };
                    bch2_btree_add_journal_pin(&root.node, vol.journal_ref(), journal_seq);
                }
            }
            self.key_cache.invalidate(&pos);
        }
        Ok(result)
    }

    /// 根节点分裂：当前根（leaf 或 internal）已满，分裂为两个同级节点，提升为新根
    ///
    /// depth=0 时：leaf → 两个 leaf，新 internal 根，depth→1
    /// depth≥1 时：internal → 两个 internal，新 internal 根，depth→+1
    /// key/value 是触发分裂的超额 entry（data 或 routing）
    ///
    /// 返回 Ok(true) = split 成功，Ok(false) = 无需分裂，Err = I/O 错误
    ///
    /// Phase 1 改造：使用 writer.write_btree_node() 获取真实磁盘地址，
    /// 不再通过 cache.alloc_addr() + insert_dirty；成功后将根变更信息存入
    /// pending_root_journal 供 caller 写入 journal。
    async fn split_root<W: BtreeNodeWriter>(
        &self,
        writer: &W,
        entry: Option<(BtreeKey, BchVal)>,
        journal_seq: u64,
    ) -> Result<bool, StorageError> {
        // bcachefs interior.c:1962-2174 packs new destination nodes before
        // bch2_btree_set_root() makes them visible. Keep the currently
        // published root untouched while asynchronous node writes are in
        // flight; mutating it here would expose a half-split tree to readers.
        let update_id = self.interior_updates.start();
        let (root_node, acquired) = {
            let _root_lock = self.root_lock.lock().unwrap();
            let root = unsafe { &*self.root.get() };
            let root_node = root.node.clone();
            if !root_node.set_btree_node_write_blocked() {
                (root_node, false)
            } else {
                self.interior_updates.block_node(update_id, ROOT_CACHE_ADDR);
                (root_node, true)
            }
        };
        if !acquired {
            self.interior_updates.finish(update_id);
            self.interior_updates.wait_on_node(&root_node).await;
            return Box::pin(self.split_root(writer, entry, journal_seq)).await;
        }
        let mut node = root_node.as_ref().clone();
        let mut update =
            BtreeInteriorUpdate::new(self.btype, InteriorUpdateType::Split, journal_seq);

        Self::init_interior_update(&mut update, BtreeUpdateMode::Root, &node);
        let root_write_blocked = NodeWriteBlockedGuard {
            state: root_node.write_blocked_state(),
            updates: self.interior_updates.clone(),
            update_id,
            node_id: ROOT_CACHE_ADDR,
            nodes_written: update.nodes_written_state(),
            completed: false,
        };
        // 保存原始 node_size（split 后 node 变为 left 节点）
        let old_node_size = node.node_size;
        let (median_key, mut right_node) = match node.split() {
            Some((k, n)) => (k, n),
            None => return Ok(false),
        };
        // 传播 node_size：分裂出的右侧节点应与原节点大小一致
        right_node.node_size = old_node_size;

        let mut left_node = node.clone();
        if let Some((key, value)) = entry {
            if key >= median_key {
                right_node.insert(key, value);
            } else {
                left_node.insert(key, value);
            }
        }
        // 分裂出的新节点继承当前 journal_seq，保持与内联的 insert 分裂路径一致。
        left_node.journal_seq = journal_seq;
        right_node.journal_seq = journal_seq;

        // bcachefs 对齐：新分裂出的节点写盘前设置 will_make_reachable
        let left_arc = Arc::new(left_node);
        let right_arc = Arc::new(right_node);
        left_arc.set_will_make_reachable();
        right_arc.set_will_make_reachable();
        // 写盘提交（fire-and-forget），IO 回调中清理 will_make_reachable
        let left_addr = writer
            .write_btree_node(left_arc.clone(), Watermark::Btree)
            .await?;
        let right_addr = writer
            .write_btree_node(right_arc.clone(), Watermark::Btree)
            .await?;
        let left_for_wait = left_arc.clone();
        let right_for_wait = right_arc.clone();
        // 新写盘的节点插入 cache（will_make_reachable 在 IO 回调中清理，
        // accessed 标志 + clock 算法保护后续驱逐）
        self.cache.insert(left_addr, left_arc);
        self.cache.insert(right_addr, right_arc);

        // 记录新节点到 BtreeInteriorUpdate
        update.add_new_node(crate::btree::types::BtreePtrV2 {
            block_addr: left_addr,
            sectors_written: 0,
            level: node.level,
            dev_idx: self.dev_idx(),
            generation: 0,
        });
        update.add_new_node(crate::btree::types::BtreePtrV2 {
            block_addr: right_addr,
            sectors_written: 0,
            level: node.level,
            dev_idx: self.dev_idx(),
            generation: 0,
        });
        update.set_median_key(median_key);
        update.mark_nodes_allocated();

        // 新根 level = 原根 level + 1
        // 当 depth=0 时原根 level=0 → 新根 level=1 ✓
        // 当 depth=2 时原根 level=1 → 新根 level=2 ✓
        let mut internal = BtreeNode::new_internal();
        internal.level = node.level + 1;
        // 新根使用与原节点相同的 node_size（保持一致性）
        internal.node_size = old_node_size;
        if let Some(vol) = self.vol_arc() {
            internal.set_vol_ref(&vol);
        }

        // 跟踪 write_entry 实际返回的大小，支持变长 entry
        let mut cur = u32::from(crate::btree::node::BSET_HEADER_U64S) * 8;
        cur += internal.write_entry(cur, &BtreeKey::MIN_KEY, &BchVal::new(left_addr, 0), 0);
        cur += internal.write_entry(cur, &median_key, &BchVal::new(right_addr, 0), 0);

        use crate::btree::node::BsetTree;
        internal.sets[0] = BsetTree {
            size: 0,
            extra: crate::btree::node::BSET_NO_AUX_TREE_VAL,
            data_offset: 0,
            aux_data_offset: u16::MAX,
            end_offset: (cur / 8) as u16,
        };
        internal.packed_keys = 2;
        internal.unpacked_keys = 0;
        internal.journal_seq = journal_seq;
        internal.compact();

        update.mark_updating_parent();
        let new_root = Arc::new(internal);
        // bcachefs 对齐：写盘前设置 will_make_reachable，IO 回调中清理
        new_root.set_will_make_reachable();
        // 先写盘（fire-and-forget 异步 IO），再用 root_lock 保护根指针切换，
        // 不在锁内 await，避免 tokio 调度器死锁
        let root_addr = writer
            .write_btree_node(new_root.clone(), Watermark::Btree)
            .await?;
        let new_root_for_wait = new_root.clone();
        crate::btree::interior::bch2_btree_set_root_inmem(self, new_root, root_addr);
        update.mark_done();
        root_write_blocked.release_after_writes(vec![
            left_for_wait,
            right_for_wait,
            new_root_for_wait,
        ]);
        Ok(true)
    }

    /// Presplit shard boundaries — 在恢复阶段预分割跨越分片边界的 leaf 节点
    ///
    /// 检查 depth=0 的 leaf 节点中的 entries 是否跨越 SHARD_FACTOR（1024）分片边界。
    /// 如果跨越且 split 点位于合理位置（距两端至少 20%），则执行节点分裂。
    /// 分裂后的两棵子树位于不同的 shard 中，未来写入可直接定位到对应子树。
    ///
    /// 仅在 recovery 的 presplit_shard_boundaries pass 中调用。
    /// 对应 bcachefs `bch2_presplit_shard_boundaries()`。
    ///
    /// 返回 Ok(true) = 已执行 split，Ok(false) = 无需 split，Err = I/O 错误
    pub async fn presplit_shard_boundaries<W: BtreeNodeWriter>(
        &self,
        writer: &W,
    ) -> Result<bool, StorageError> {
        if unsafe { (*self.root.get()).depth } != 0 {
            return Ok(false);
        }

        const SHARD_FACTOR: u64 = 1024;

        // 收集 entries 检查 shard 边界跨越，同时保留用于 split
        let entries: Vec<BtreeEntry> = {
            let mut entries = Vec::new();
            self.for_each_btree_key_entry(|e| entries.push(e));
            entries
        };

        let n = entries.len();
        if n < 3 {
            return Ok(false);
        }

        let mut found_split = false;
        for i in 1..n {
            let prev_off = entries[i - 1].pos.offset;
            let curr_off = entries[i].pos.offset;
            // 跨越 shard 边界：prev 在 N*SHARD_FACTOR 之前，curr 在其之后
            if prev_off / SHARD_FACTOR < curr_off / SHARD_FACTOR {
                // 仅在 split 点距两端至少 20% 时执行
                if i > n / 5 && i < n * 4 / 5 {
                    found_split = true;
                    break;
                }
            }
        }

        if !found_split {
            return Ok(false);
        }

        // 执行分裂但不插入额外 entry（split_root(None) 跳过额外 entry 插入）
        // journal_seq=0：recovery 期间的预分裂，无有效 journal seq 可用。
        // 分裂操作在 recovery 完成后会由正常的 journal 路径重新追踪。
        self.split_root(writer, None, 0).await
    }

    /// 使用事务执行操作
    ///
    /// 提供对 BtreeTrans 的底层访问，用于需要多个 iter 的场景。
    /// 返回 `Result<R, StorageError>`，其中 `Err` 来自事务重启限制溢出。
    pub fn bch2_trans_commit<F, R>(&self, f: F) -> Result<R, StorageError>
    where
        F: FnOnce(&mut BtreeTrans) -> R,
    {
        let mut trans = BtreeTrans::new_with_cache(self.cache.clone());
        trans.bch2_trans_begin();
        let result = f(&mut trans);
        trans.__bch2_trans_commit()?;
        Ok(result)
    }

    /// 在事务上下文中插入 key/value — 节点分裂时通知事务重启
    ///
    /// 比 `insert()` 多了事务集成：当单 leaf 插入触发 `split_root` 时，
    /// 通过 `trans.trigger_node_split()` 通知事务路径可能需要重新遍历。
    ///
    /// 返回 Ok(true) = 插入成功，Ok(false) = 需重试，Err = I/O 错误
    pub async fn bch2_btree_insert_trans<'a, W: BtreeNodeWriter>(
        &self,
        writer: &W,
        key: BtreeKey,
        value: BchVal,
        trans: Option<&'a mut BtreeTrans<'a>>,
        journal_seq: u64,
    ) -> Result<bool, StorageError> {
        let pos = Bpos::from_key(&key);
        let depth = {
            let _root_lock = self.root_lock.lock().unwrap();
            unsafe { (*self.root.get()).depth }
        };
        let result = if depth == 0 {
            let insert_ok = {
                let _root_lock = self.root_lock.lock().unwrap();
                let root = unsafe { &mut *self.root.get() };
                if root.node.btree_node_write_blocked() {
                    return Ok(false);
                }
                let node = unsafe { &mut *(Arc::as_ptr(&root.node) as *mut BtreeNode) };
                bch2_btree_node_prep_for_write(node);
                if node.insert(key, value) {
                    
                    node.journal_seq = journal_seq;
                    self.root_modified.store(true, Ordering::Release);
                    true
                } else {
                    node.compact();
                    if node.insert(key, value) {
                        
                        node.journal_seq = journal_seq;
                        self.root_modified.store(true, Ordering::Release);
                        true
                    } else {
                        false
                    }
                }
            };
            let result = if insert_ok {
                true
            } else {
                // root borrow dropped — safe to call split_root
                match self
                    .split_root(writer, Some((key, value)), journal_seq)
                    .await
                {
                    Ok(true) => {
                        
                        if let Some(trans) = trans {
                            trans.trigger_node_split();
                        }
                        true
                    }
                    Ok(false) => false,
                    Err(e) => return Err(e),
                }
            };
            result
        } else {
            let result = 'insert_multi: loop {
            let mut path: Vec<u64> = Vec::new();
                let leaf_addr = match self.bch2_btree_path_traverse_one(&key, &mut path) {
                    Some(addr) => addr,
                    None => {
                        eprintln!("FAIL: bch2_btree_path_traverse_one returned None");
                        break 'insert_multi false;
                    }
                };

                // ── Phase 1: bch2_btree_insert_key_leaf ──
                if self.bch2_btree_insert_key_leaf(leaf_addr, key, value, journal_seq) {
                    
                    break 'insert_multi true;
                }

                // ── Phase 2: bch2_btree_split_leaf ──
                let split_result = self
                    .bch2_btree_split_leaf(writer, leaf_addr, key, value, journal_seq, &mut path)
                    .await?;
                break 'insert_multi split_result;
            };
            if result {
                if let Some(trans) = trans {
                    trans.trigger_node_split();
                }
            }
            result
        };
        if result {
            self.key_cache.invalidate(&pos);
        }
        Ok(result)
    }

    /// bcachefs 对齐: for_each_btree_key 宏模式 — 遍历所有 key/value
    /// bcachefs 对齐: for_each_btree_key — 遍历所有 key-value 对
    pub fn for_each_btree_key<F>(&self, mut f: F)
    where
        F: FnMut(BtreeKey, BchVal),
    {
        let root_ref = unsafe { &*self.root.get() };
        let mut trans = BtreeTrans::new_with_cache(Arc::clone(&self.cache));
        let iter = trans.bch2_trans_get_iter(
            root_ref,
            &BtreeKey::MIN_KEY,
            false,
            crate::btree::BtreeId::Extents,
        );
        while let Some((k, v)) = iter.peek() {
            if k.key_type != KeyType::Deleted {
                f(k, v);
            }
            if !iter.advance() {
                break;
            }
        }
    }

    /// bcachefs 对齐: for_each_btree_key 宏模式 — 遍历所有 BtreeEntry
    /// bcachefs 对齐: for_each_btree_key_entry — 遍历所有 BtreeEntry
    pub fn for_each_btree_key_entry<F>(&self, mut f: F)
    where
        F: FnMut(BtreeEntry),
    {
        let root_ref = unsafe { &*self.root.get() };
        let mut trans = BtreeTrans::new_with_cache(Arc::clone(&self.cache));
        let iter = trans.bch2_trans_get_iter(
            root_ref,
            &BtreeKey::MIN_KEY,
            false,
            self.btype,
        );
        while let Some(entry) = iter.peek_entry() {
            if entry.key_type != KeyType::Deleted {
                f(entry);
            }
            if !iter.advance() {
                break;
            }
        }
    }

    /// bcachefs 对齐: bch2_btree_node_compact — 对单 leaf（depth=0）根节点执行 compact
    pub fn compact(&self) {
        let depth = unsafe { (*self.root.get()).depth };
        if depth == 0 {
            let root = unsafe { &mut *self.root.get() };
            let node = unsafe { &mut *(Arc::as_ptr(&root.node) as *mut BtreeNode) };
            node.compact();
        }
    }

    /// 获取节点缓存引用
    pub fn cache(&self) -> &NodeCache {
        &self.cache
    }

    /// 节点缓存引用（NodeCache 内部使用 Mutex，不需要 &mut）
    pub fn cache_mut(&self) -> &NodeCache {
        &self.cache
    }

    /// 获取节点缓存的 Arc 克隆（供 BtreeIter 构造使用）
    pub fn node_cache_arc(&self) -> Arc<NodeCache> {
        self.cache.clone()
    }

    /// 对应本地 bcachefs `bch2_btree_interior_updates_flush()`
    /// (`fs/btree/interior.c:3740-3748`)。
    pub(crate) async fn bch2_btree_interior_updates_flush(&self) -> bool {
        self.interior_updates.flush().await
    }

    /// drain 并返回所有脏节点（按 level 升序排列，包含 depth=0 时被修改的 root）
    /// bcachefs 对齐: bch2_btree_flush_all — 返回所有脏节点待写盘
    pub fn bch2_btree_flush_all(&self) -> Vec<(u64, Arc<BtreeNode>)> {
        let mut result = self.cache.flush_dirty();
        // depth=0 root 不在 cache 中，通过 root_modified 跟踪
        if self.root_modified.swap(false, Ordering::Acquire) {
            let root = unsafe { &*self.root.get() };
            result.push((ROOT_CACHE_ADDR, root.node.clone()));
        }
        if let Some(vol) = self.vol_arc() {
            for (_, node) in &result {
                node.set_vol_ref(&vol);
            }
        }
        result
    }

    // ─── Interior 模块辅助方法 ─────────────────────────────────

    /// 内部方法：从 root 下降到 leaf，返回路径（供 interior 模块使用）
    /// 内部方法：直接设置根节点（供 interior 模块使用）
    pub(crate) fn set_root_internal(&self, node: Arc<BtreeNode>) {
        let _lock = self.root_lock.lock().unwrap();
        let depth = node.level;
        let root = unsafe { &mut *self.root.get() };
        root.node = node;
        root.depth = depth;
    }

    /// 内部方法：获取根节点的可变访问（仅用于测试/内部操作）
    #[allow(clippy::mut_from_ref)]
    #[allow(dead_code)]
    pub(crate) fn root_node_mut_internal(&self) -> &mut BtreeNode {
        // 仅用于测试和内部操作（node_size 调整等）
        // 使用 Arc::get_mut 需要在没有其它引用时才能成功
        let root = unsafe { &mut *self.root.get() };
        Arc::get_mut(&mut root.node)
            .expect("root_node_mut_internal: root Arc has multiple references")
    }

    // ─── 多级树辅助 ───────────────────────────────────────

    /// 从 root 下降到 leaf，记录所有经过的 internal node 地址
    ///
    /// path 填充为 [level(depth-1), level(depth-2), ..., level1] 的地址。
    /// depth=1 时 path 为空（root 自身就是 leaf 的 direct parent）。
    ///
    /// 自愈：遍历中遇到 key_count == 0 的空节点时，
    /// 自动调用 bch2_foreground_maybe_merge 清理后重新查找子节点。
    pub(crate) fn bch2_btree_path_traverse_one(
        &self,
        target: &BtreeKey,
        path: &mut Vec<u64>,
    ) -> Option<u64> {
        let root_ref = unsafe { &*self.root.get() };
        if root_ref.depth == 0 {
            return None;
        }
        let mut current = root_ref.node.clone();
        path.clear();
        let mut level = root_ref.depth;
        loop {
            // 自愈：当前节点为空时清理并回退
            if current.packed_keys == 0 && current.unpacked_keys == 0 {
                if path.is_empty() {
                    return None;
                }
                let empty_addr = path[path.len() - 1];
                path.pop();
                let ancestors: &[u64] = &path;
                if !self.bch2_foreground_maybe_merge(empty_addr, ancestors) {
                    return None;
                }
                current = if path.is_empty() {
                    let root_ref = unsafe { &*self.root.get() };
                    root_ref.node.clone()
                } else {
                    self.cache.get_or_create(path[path.len() - 1], level)
                };
                level += 1;
                continue;
            }

            let (child_addr, _child_idx) = BtreeIter::find_child_node(&current, target);
            if child_addr > 10000 {
                eprintln!(
                    "CORRUPT: level={} child_addr={} huge address, current node key_count={} level={} node_size={}",
                    level, child_addr, current.packed_keys as u32 + current.unpacked_keys as u32, current.level, current.node_size
                );
            }
            if child_addr == 0 {
                return None;
            }
            if level == 1 {
                return Some(child_addr);
            }
            path.push(child_addr);
            current = self.cache.get_or_create(child_addr, level - 1);
            level -= 1;

            if current.packed_keys > 0 || current.unpacked_keys > 0 {
                let target_pos = target.get_vaddr();
                let node_min_off = current.min_key.offset;
                let node_max_off = current.max_key.offset;
                if target_pos < node_min_off || target_pos > node_max_off {}
            }
        }
    }

    /// bcachefs 对齐: bch2_btree_insert_key_leaf (commit.c:332)
    ///
    /// 在叶子节点中插入键。成功返回 true。
    /// 对应 bcachefs 的: bch2_btree_bset_insert_key + journal_seq + journal_pin
    fn bch2_btree_insert_key_leaf(
        &self,
        leaf_addr: u64,
        key: BtreeKey,
        value: BchVal,
        journal_seq: u64,
    ) -> bool {
        let mut leaf_arc = match self.cache.take_node(leaf_addr) {
            Some(n) => n,
            None => return false,
        };
        if leaf_arc.btree_node_write_blocked() {
            self.cache.put_node(leaf_addr, leaf_arc);
            return false;
        }
        let new_key_u64s = entry_packed_size(&BtreeEntry::from((key, value))) as u32 / 8;
        let (inserted, should_compact) = {
            let leaf = match Arc::get_mut(&mut leaf_arc) {
                Some(n) => n,
                None => {
                    self.cache.put_node(leaf_addr, leaf_arc);
                    return false;
                }
            };
            bch2_btree_node_prep_for_write(leaf);

            let live_u64s = leaf.total_data_bytes() / 8;
            if crate::btree::node::should_split(live_u64s, leaf.node_size) {
                (false, false)
            } else {
                let old_live_u64s = leaf.live_data_bytes() / 8;
                let old_u64s = bset_u64s(leaf.current_bset());
                let inserted = if leaf.insert(key, value) {
                    true
                } else {
                    leaf.compact();
                    // A1: compact_fits — 只有 compact 释放了足够空间才重试 insert
                    leaf.bch2_btree_node_compact_fits(new_key_u64s) && leaf.insert(key, value)
                };
                if !inserted {
                    (false, false)
                } else {
                    let new_live_u64s = leaf.live_data_bytes() / 8;
                    let new_u64s = bset_u64s(leaf.current_bset());
                    let live_u64s_added =
                        i64::from(new_live_u64s) - i64::from(old_live_u64s);
                    let u64s_added = i64::from(new_u64s) - i64::from(old_u64s);

                    // bcachefs commit.c:361-364: keep sibling live-size estimates
                    // monotonic with shrinking updates; boundary U16_MAX is preserved.
                    if live_u64s_added < 0 {
                        let live_u64s = new_live_u64s.min(u32::from(u16::MAX)) as u16;
                        for sib_u64s in &mut leaf.sib_u64s {
                            if *sib_u64s != u16::MAX {
                                *sib_u64s = (i64::from(*sib_u64s) + live_u64s_added)
                                    .max(i64::from(live_u64s))
                                    .min(i64::from(u16::MAX))
                                    as u16;
                            }
                        }
                    }

                    let dead_u64s = new_u64s.saturating_sub(new_live_u64s);
                    let should_compact = u64s_added > live_u64s_added
                        && dead_u64s > 64
                        && dead_u64s * 3 > new_u64s;
                    leaf.journal_seq = journal_seq;
                    (true, should_compact)
                }
            }
        };
        if inserted {
            if let Some(vol) = self.vol_arc() {
                bch2_btree_add_journal_pin(&leaf_arc, vol.journal_ref(), journal_seq);
            }

            // bcachefs commit.c:366-368 + sort.h:should_compact_bset_lazy:
            // compact only after dead whiteouts exceed both thresholds.
            if should_compact {
                Arc::get_mut(&mut leaf_arc)
                    .expect("leaf must remain uniquely owned after cache take")
                    .compact();
            }
            self.cache.put_node(leaf_addr, leaf_arc);
            return true;
        }
        self.cache.put_node(leaf_addr, leaf_arc);
        false // needs split
    }

    /// bcachefs 对齐: bch2_btree_split_leaf (interior.c:2281)
    /// 叶节点分裂，触发递归路由更新
    async fn bch2_btree_split_leaf<W: BtreeNodeWriter>(
        &self,
        writer: &W,
        leaf_addr: u64,
        key: BtreeKey,
        value: BchVal,
        journal_seq: u64,
        path: &mut Vec<u64>,
    ) -> Result<bool, StorageError> {
        let update_id = self.interior_updates.start();

        // 创建 BtreeInteriorUpdate 跟踪分裂生命周期
        // journal_seq 来自插入操作的 journal 预留，确保 crash recovery 可追踪此分裂。
        // 对应 bcachefs bch2_btree_node_split_pre (split.c) 中的 journal 预留。
        let mut update =
            BtreeInteriorUpdate::new(self.btype, InteriorUpdateType::Split, journal_seq);
        let old_node = crate::btree::types::BtreePtrV2 {
            block_addr: leaf_addr,
            sectors_written: 0,
            level: 0,
            dev_idx: self.dev_idx(),
            generation: 0,
        };
        update.add_old_node(old_node);

        let mut leaf_arc = match self.cache.take_node(leaf_addr) {
            Some(n) => n,
            None => {
                self.interior_updates.finish(update_id);
                return Ok(false);
            }
        };
        if leaf_arc.btree_node_write_blocked() {
            self.cache.put_node(leaf_addr, leaf_arc);
            self.interior_updates.finish(update_id);
            let node = self.cache.get(leaf_addr);
            if let Some(node) = node {
                self.interior_updates.wait_on_node(&node).await;
            }
            return Box::pin(self.bch2_btree_split_leaf(
                writer,
                leaf_addr,
                key,
                value,
                journal_seq,
                path,
            ))
            .await;
        }
        if !leaf_arc.set_btree_node_write_blocked() {
            self.cache.put_node(leaf_addr, leaf_arc);
            self.interior_updates.finish(update_id);
            return Ok(false);
        }
        self.interior_updates.block_node(update_id, leaf_addr);
        let leaf_write_blocked = NodeWriteBlockedGuard {
            state: leaf_arc.write_blocked_state(),
            updates: self.interior_updates.clone(),
            update_id,
            node_id: leaf_addr,
            nodes_written: update.nodes_written_state(),
            completed: false,
        };
        let leaf = Arc::get_mut(&mut leaf_arc).unwrap();
        Self::init_interior_update(&mut update, BtreeUpdateMode::Node, leaf);

        let (median_key, mut right_node) = match leaf.split() {
            Some((k, n)) => (k, n),
            None => {
                eprintln!(
                    "FAIL: leaf.split() returned None at leaf_addr={}",
                    leaf_addr
                );
                self.cache.put_node(leaf_addr, leaf_arc);
                return Ok(false);
            }
        };
        // 确保右侧 leaf 使用相同的 node_size（split 创建 DEFAULT_NODE_SIZE 节点）
        right_node.node_size = leaf.node_size;

        if key >= median_key {
            right_node.insert(key, value);
        } else {
            leaf.insert(key, value);
        }
        leaf.journal_seq = journal_seq;
        right_node.journal_seq = journal_seq;
        leaf.compact();
        right_node.compact();

        // Phase 1: left node stays at original address (routing entries still
        // point to it).  bcachefs marks every destination node dirty in
        // bch2_btree_update_write_new_node(); keeping the original address
        // requires the same dirty-cache handoff here.
        self.cache.insert_dirty(leaf_addr, leaf_arc);
        // Right node gets a new address via the writer (parent gets new routing entry)
        let right_arc = Arc::new(right_node);
        right_arc.set_will_make_reachable();
        let right_addr = writer
            .write_btree_node(right_arc.clone(), Watermark::Btree)
            .await?;
        // will_make_reachable 在 IO 回调中清理
        self.cache.insert(right_addr, right_arc.clone());
        // A2: SplitGuard 确保 split 失败时释放已分配的右节点
        let mut guard = SplitGuard::new(self.cache.clone(), right_addr);

        // 记录新节点到 update
        let new_node_right = crate::btree::types::BtreePtrV2 {
            block_addr: right_addr,
            sectors_written: 0,
            level: 0,
            dev_idx: self.dev_idx(),
            generation: 0,
        };
        update.add_new_node(new_node_right);
        update.set_median_key(median_key);
        update.mark_nodes_allocated();

        // ── Phase 3: insert routing entry into parent ──
        update.mark_updating_parent();
        // path 为空时（depth=1），parent 就是 root
        let pos = if path.is_empty() { 0 } else { path.len() - 1 };
        let mut routing_key = median_key;
        let mut child_addr = right_addr;
        let mut pos = pos;
        let phase3_result = loop {
            if pos >= path.len() {
                // === Root 作为 parent ===
                // bcachefs 对齐：锁内修改 root.node 并 clone，丢锁后 async 写盘，
                // 再锁回设 journal 状态。避免 MutexGuard 跨 .await（P1）。
                let outcome = {
                    let _lock = self.root_lock.lock().unwrap();
                    let root = unsafe { &mut *self.root.get() };
                    let parent = match Arc::get_mut(&mut root.node) {
                        Some(n) => n,
                        None => break Ok(false),
                    };
                    bch2_btree_node_prep_for_write(parent);
                    let entry = BchVal::new(child_addr, 0);
                    if parent.insert(routing_key, entry) {
                        parent.compact();
                        Some((parent.clone(), parent.level))
                    } else {
                        parent.compact();
                        let entry = BchVal::new(child_addr, 0);
                        if parent.insert(routing_key, entry) {
                            parent.compact();
                            Some((parent.clone(), parent.level))
                        } else {
                            None
                        }
                    }
                };
                break match outcome {
                    Some((cloned, level)) => {
                        let cloned_arc = Arc::new(cloned);
                        cloned_arc.set_will_make_reachable();
                        let root_addr = writer
                            .write_btree_node(cloned_arc, Watermark::Btree)
                            .await?;
                        // bcachefs `btree_update_will_free_node()` reparents
                        // blockers before publishing the replacement root.
                        self.interior_updates
                            .reparent(ROOT_CACHE_ADDR, update_id);
                        let _lock = self.root_lock.lock().unwrap();
                        unsafe {
                            *self.pending_root_journal.get() =
                                Some(PendingRootJournal { root_addr, level });
                            *self.current_root_disk.get() = Some((root_addr, level));
                        }
                        Ok(true)
                    }
                    None => {
                        self.split_root(
                            writer,
                            Some((routing_key, BchVal::new(child_addr, 0))),
                            journal_seq,
                        )
                        .await
                    }
                };
            }

            // === Cache 中的 internal node 作为 parent ===
            let parent_addr = path[pos];
            let mut parent_arc = match self.cache.take_node(parent_addr) {
                Some(n) => n,
                None => {
                    eprintln!("FAIL: take_node({}) in routing path", parent_addr);
                    break Ok(false);
                }
            };
            let parent = match Arc::get_mut(&mut parent_arc) {
                Some(n) => n,
                None => {
                    self.cache.put_node(parent_addr, parent_arc);
                    eprintln!("FAIL: Arc::get_mut parent{}", parent_addr);
                    break Ok(false);
                }
            };
            bch2_btree_node_prep_for_write(parent);

            let entry = BchVal::new(child_addr, 0);
            let new_key_u64s =
                entry_packed_size(&BtreeEntry::from((routing_key, entry))) as u32 / 8;
            if parent.insert(routing_key, entry) {
                parent.compact();
                // bcachefs `bch2_btree_node_set_dirty()` keeps the parent
                // dirty until the interior update becomes writeable.
                self.cache.insert_dirty(parent_addr, parent_arc);
                break Ok(true);
            }
            parent.compact();
            // A1: compact_fits — 只有 compact 释放了足够空间才重试 insert
            if parent.bch2_btree_node_compact_fits(new_key_u64s) {
                let entry = BchVal::new(child_addr, 0);
                if parent.insert(routing_key, entry) {
                    parent.compact();
                    self.cache.insert_dirty(parent_addr, parent_arc);
                    break Ok(true);
                }
            }

            // Internal node 也满了 → 分裂
            let (median_key_internal, mut right_node) = match parent.split() {
                Some((k, n)) => (k, n),
                None => {
                    self.cache.put_node(parent_addr, parent_arc);
                    break Ok(false);
                }
            };
            // 确保右侧节点使用相同的 level 和 node_size
            right_node.node_size = parent.node_size;
            debug_assert_eq!(
                right_node.level, parent.level,
                "split right_node level {} != parent level {}",
                right_node.level, parent.level
            );
            debug_assert_eq!(
                right_node.node_size, parent.node_size,
                "split right_node node_size {} != parent node_size {}",
                right_node.node_size, parent.node_size
            );

            if routing_key >= median_key_internal {
                right_node.insert(routing_key, BchVal::new(child_addr, 0));
            } else {
                parent.insert(routing_key, BchVal::new(child_addr, 0));
            }
            parent.compact();
            right_node.compact();

            // Phase 1: left half stays at original address and is dirty;
            // bcachefs writes it only after the new right child is durable.
            self.cache.insert_dirty(parent_addr, parent_arc);
            // Right half gets a new address via the writer
            let right_arc = Arc::new(right_node);
            right_arc.set_will_make_reachable();
            let right_addr_internal = writer
                .write_btree_node(right_arc.clone(), Watermark::Btree)
                .await?;
            // will_make_reachable 在 IO 回调中清理
            self.cache.insert(right_addr_internal, right_arc);

            // 递归向上：将 median_key + right_addr 插入到祖父母
            routing_key = median_key_internal;
            child_addr = right_addr_internal;
            if pos > 0 {
                pos -= 1;
            } else {
                pos = path.len();
            }
        }?;

        if phase3_result {
            guard.disarm();
        }

        update.mark_done();
        leaf_write_blocked.release_after_write(right_arc);
        Ok(phase3_result)
    }

    /// bcachefs 对齐: bch2_btree_delete_at (update.c:725)
    /// 在叶子节点删除 key（插入 whiteout），触发 merge + collapse
    async fn bch2_btree_delete_at<W: BtreeNodeWriter>(
        &self,
        writer: &W,
        key: &BtreeKey,
        journal_seq: u64,
    ) -> Result<bool, StorageError> {
        let mut path: Vec<u64> = Vec::new();
        let leaf_addr = match self.bch2_btree_path_traverse_one(key, &mut path) {
            Some(addr) if addr > 0 => addr,
            _ => return Ok(false),
        };
        let mut leaf_arc = match self.cache.take_node(leaf_addr) {
            Some(n) => n,
            None => return Ok(false),
        };
        if leaf_arc.btree_node_write_blocked() {
            self.cache.put_node(leaf_addr, leaf_arc);
            return Ok(false);
        }
        let (deleted, should_compact) = {
            let leaf = match Arc::get_mut(&mut leaf_arc) {
                Some(n) => n,
                None => {
                    self.cache.put_node(leaf_addr, leaf_arc);
                    return Ok(false);
                }
            };
            bch2_btree_node_prep_for_write(leaf);
            let old_live_u64s = leaf.live_data_bytes() / 8;
            let old_u64s = crate::btree::node::bset_u64s(leaf.current_bset());
            let deleted = leaf.delete_key(key);
            if !deleted {
                (false, false)
            } else {
                let new_live_u64s = leaf.live_data_bytes() / 8;
                let new_u64s = crate::btree::node::bset_u64s(leaf.current_bset());
                let live_u64s_added =
                    i64::from(new_live_u64s) - i64::from(old_live_u64s);
                let u64s_added = i64::from(new_u64s) - i64::from(old_u64s);

                // bcachefs interior.c:2242-2245: deletion whiteouts reduce
                // non-boundary sibling estimates, never below zero.
                if live_u64s_added < 0 {
                    for sib_u64s in &mut leaf.sib_u64s {
                        if *sib_u64s != u16::MAX {
                            *sib_u64s = (i64::from(*sib_u64s) + live_u64s_added)
                                .max(0)
                                .min(i64::from(u16::MAX))
                                as u16;
                        }
                    }
                }

                let dead_u64s = new_u64s.saturating_sub(new_live_u64s);
                let should_compact = u64s_added > live_u64s_added
                    && dead_u64s > 64
                    && dead_u64s * 3 > new_u64s;
                leaf.journal_seq = journal_seq;
                (true, should_compact)
            }
        };
        if deleted {
            if let Some(vol) = self.vol_arc() {
                bch2_btree_add_journal_pin(&leaf_arc, vol.journal_ref(), journal_seq);
            }
            if should_compact {
                Arc::get_mut(&mut leaf_arc)
                    .expect("leaf must remain uniquely owned after cache take")
                    .compact();
            }
        }
        self.cache.put_node(leaf_addr, leaf_arc);

        // bcachefs 对齐: 删除后尝试合并 + root collapse
        let depth = unsafe { (*self.root.get()).depth };
        if deleted && depth > 0 {
            // Cascade merge: leaf 合并可能导致其祖先节点也 underfull
            self.bch2_foreground_maybe_merge(leaf_addr, &path);
            for level in 1..unsafe { (*self.root.get()).depth as usize } {
                if level > path.len() {
                    break;
                }
                let node_addr = path[path.len() - level];
                let ancestors = &path[..path.len() - level];
                self.bch2_foreground_maybe_merge(node_addr, ancestors);
            }
            // root collapse: 当 root 只剩 1 个 child 时提升 child 为根
            loop {
                let root_ref = unsafe { &*self.root.get() };
                if root_ref.depth == 0 || (root_ref.node.packed_keys + root_ref.node.unpacked_keys) != 1 {
                    break;
                }
                let sole_child_addr = {
                    let mut node_iter = BtreeNodeIter::default();
                    bch2_btree_node_iter_init_from_start(&mut node_iter, &root_ref.node);
                    match bch2_btree_node_iter_peek_all(&node_iter, &root_ref.node) {
                        Some(_) => {
                            let (_k, v) = root_ref
                                .node
                                .read_packed_entry(node_iter.data[0].k as usize * 8);
                            v.paddr()
                        }
                        None => break,
                    }
                };
                if let Some(child_arc) = self.cache.take_node(sole_child_addr) {
                    child_arc.set_will_make_reachable();
                    let root_addr = writer
                        .write_btree_node(child_arc.clone(), Watermark::Btree)
                        .await?;
                    crate::btree::interior::bch2_btree_set_root_inmem(self, child_arc, root_addr);
                }
            }
        }
        Ok(deleted)
    }

    /// bcachefs 对齐: bch2_foreground_maybe_merge — 检查节点是否需要合并并执行
    ///
    /// 1. 检查 node 是否低于合并阈值 (should_merge)
    /// 2. 从父节点收集条目找兄弟节点
    /// 3. can_absorb + absorb 执行合并
    /// 4. 更新父节点路由
    pub(crate) fn bch2_foreground_maybe_merge(&self, node_addr: u64, ancestors: &[u64]) -> bool {
        let find_sib = |parent: &BtreeNode, child: u64, is_left: bool| -> Option<u64> {
            let n = parent.packed_keys as usize;
            if n == 0 {
                return None;
            }
            let mut entries: Vec<(BtreeKey, u64)> = Vec::with_capacity(n);
            let mut node_iter = BtreeNodeIter::default();
            bch2_btree_node_iter_init_from_start(&mut node_iter, parent);
            while bch2_btree_node_iter_peek_all(&node_iter, parent).is_some() {
                let (k, v) = parent.read_packed_entry(node_iter.data[0].k as usize * 8);
                entries.push((k, v.paddr()));
                bch2_btree_node_iter_advance(&mut node_iter, parent);
            }
            entries.sort_by_key(|a| a.0);
            let pos = entries.iter().position(|(_, addr)| *addr == child)?;
            if is_left {
                if pos > 0 {
                    Some(entries[pos - 1].1)
                } else {
                    None
                }
            } else {
                if pos + 1 < entries.len() {
                    Some(entries[pos + 1].1)
                } else {
                    None
                }
            }
        };

        // keylist 模式父节点路由更新（对齐 bcachefs keylist_add + insert_node 模式）
        // deletes: 待删除的子节点地址列表
        // inserts: (routing_key, child_addr) 新路由条目列表
        let update_routing =
            |parent: &mut BtreeNode, deletes: &[u64], inserts: &[(BtreeKey, u64)]| -> bool {
                // 扫描父节点，找到所有待删除地址对应的 Bpos
                let mut delete_positions: Vec<Bpos> = Vec::new();
                {
                    let mut node_iter = BtreeNodeIter::default();
                    bch2_btree_node_iter_init_from_start(&mut node_iter, parent);
                    while bch2_btree_node_iter_peek_all(&node_iter, parent).is_some() {
                        let (k, v) = parent.read_packed_entry(node_iter.data[0].k as usize * 8);
                        if deletes.contains(&v.paddr()) {
                            delete_positions.push(Bpos::from_key(&k));
                        }
                        bch2_btree_node_iter_advance(&mut node_iter, parent);
                    }
                }
                // A parent identity mismatch is an update failure in
                // bcachefs; never continue by inserting replacements while
                // one of the old child routes is still present elsewhere.
                if delete_positions.len() != deletes.len() {
                    return false;
                }
                // Phase A: 标记所有旧条目为 Deleted
                for &pos in &delete_positions {
                    let mut ni = BtreeNodeIter::default();
                    bch2_btree_node_iter_init(&mut ni, parent, &pos);
                    let del = BtreeEntry::raw(pos, KeyType::Deleted, Vec::new());
                    parent.bch2_btree_bset_insert_key(&mut ni, &del);
                }
                parent.compact();
                // Phase B: 插入所有新路由条目
                for &(ref key, addr) in inserts {
                    let entry = BchVal::new(addr, 0);
                    if parent.insert(*key, entry) {
                        continue;
                    }
                    parent.compact();
                    if !parent.insert(*key, entry) {
                        return false;
                    }
                }
                parent.compact();
                true
            };

        // Phase 1: 取节点检查是否 underfull
        let mut node_arc = match self.cache.take_node(node_addr) {
            Some(n) => n,
            None => return false,
        };
        let node = match Arc::get_mut(&mut node_arc) {
            Some(n) => n,
            None => {
                self.cache.put_node(node_addr, node_arc);
                return false;
            }
        };
        let is_empty = node.packed_keys == 0 && node.unpacked_keys == 0;
        let vol = self.vol_arc();
        let merge_threshold = vol
            .as_ref()
            .map(|vol| u32::from(vol.btree_foreground_merge_threshold))
            .unwrap_or((node.node_size / 8) / 3);
        let merge_needed = vol
            .as_ref()
            .map(|vol| btree_node_needs_merge(vol, node, 0))
            .unwrap_or_else(|| {
                u32::from(node.sib_u64s[0].min(node.sib_u64s[1])) <= merge_threshold
            });
        if !is_empty && !merge_needed {
            self.cache.put_node(node_addr, node_arc);
            return false;
        }
        node.compact();

        // bcachefs interior.c:2945-2955: boundary siblings are poisoned
        // before candidate lookup, so later merge attempts return cheaply.
        if node.min_key == Bpos::MIN {
            node.sib_u64s[0] = u16::MAX;
        }
        if node.max_key == Bpos::MAX {
            node.sib_u64s[1] = u16::MAX;
        }

        // bcachefs interior.c:2465: do not fetch a sibling whose cached live
        // estimate already exceeds the foreground merge threshold.
        // Phase 2: 收集所有同级兄弟节点 (左→当前→右)，对齐 bcachefs srcs
        let parent_is_root = ancestors.is_empty();
        let (left_sib, right_sib) = if parent_is_root {
            let root_ref = unsafe { &*self.root.get() };
            (
                (u32::from(node.sib_u64s[0]) <= merge_threshold)
                    .then(|| find_sib(&root_ref.node, node_addr, true))
                    .flatten(),
                (u32::from(node.sib_u64s[1]) <= merge_threshold)
                    .then(|| find_sib(&root_ref.node, node_addr, false))
                    .flatten(),
            )
        } else {
            let parent_addr = ancestors[ancestors.len() - 1];
            match self.cache.get(parent_addr) {
                Some(parent_arc) => (
                    (u32::from(node.sib_u64s[0]) <= merge_threshold)
                        .then(|| find_sib(&parent_arc, node_addr, true))
                        .flatten(),
                    (u32::from(node.sib_u64s[1]) <= merge_threshold)
                        .then(|| find_sib(&parent_arc, node_addr, false))
                        .flatten(),
                ),
                None => {
                    self.cache.put_node(node_addr, node_arc);
                    return false;
                }
            }
        };

        // 以左→当前→右顺序组装 srcs
        let mut srcs: Vec<(u64, Arc<BtreeNode>)> = Vec::with_capacity(3);
        if let Some(addr) = left_sib {
            match self.cache.take_node(addr) {
                Some(arc) => srcs.push((addr, arc)),
                None => { /* node not in cache, skip */ }
            }
        }
        srcs.push((node_addr, node_arc));
        if let Some(addr) = right_sib {
            match self.cache.take_node(addr) {
                Some(arc) => srcs.push((addr, arc)),
                None => { /* skip */ }
            }
        }

        if srcs.len() == 1 {
            for (addr, arc) in srcs {
                self.cache.put_node(addr, arc);
            }
            return false;
        }

        // 校验独占所有权
        for (_, arc) in &mut srcs {
            if Arc::get_mut(arc).is_none() {
                for (addr, arc) in srcs {
                    self.cache.put_node(addr, arc);
                }
                return false;
            }
        }

        // bcachefs interior.c:3084-3203 keeps the parent path locked until
        // bch2_btree_insert_node() (interior.c:2191-2265) publishes replacement
        // routes. Hold the non-root parent itself so it cannot disappear between
        // destructive source packing and routing.
        let mut parent_hold: Option<(u64, Arc<BtreeNode>)> = if parent_is_root {
            let root = unsafe { &mut *self.root.get() };
            if Arc::get_mut(&mut root.node).is_some() {
                None
            } else {
                for (addr, arc) in srcs {
                    self.cache.put_node(addr, arc);
                }
                return false;
            }
        } else {
            let parent_addr = ancestors[ancestors.len() - 1];
            match self.cache.take_node(parent_addr) {
                Some(mut parent_arc) => {
                    if Arc::get_mut(&mut parent_arc).is_some() {
                        Some((parent_addr, parent_arc))
                    } else {
                        self.cache.put_node(parent_addr, parent_arc);
                        for (addr, arc) in srcs {
                            self.cache.put_node(addr, arc);
                        }
                        return false;
                    }
                }
                None => {
                    for (addr, arc) in srcs {
                        self.cache.put_node(addr, arc);
                    }
                    return false;
                }
            }
        };

        let src_addrs: Vec<u64> = srcs.iter().map(|(addr, _)| *addr).collect();

        // bcachefs interior.c:3088-3101 verifies parent identity before node
        // packing, and interior.c:2191-2265 returns the update error while old
        // source nodes are intact. Probe the exact route operation on a private
        // parent before any source node is modified, preserving that rollback
        // boundary when an old route is absent or replacements do not fit.
        let route_fits = |parent_hold: &Option<(u64, Arc<BtreeNode>)>,
                          deletes: &[u64],
                          inserts: &[(BtreeKey, u64)]| {
            if parent_is_root {
                let root = unsafe { &*self.root.get() };
                let mut probe = root.node.as_ref().clone();
                update_routing(&mut probe, deletes, inserts)
            } else {
                let Some((_, parent_arc)) = parent_hold.as_ref() else {
                    return false;
                };
                let mut probe = parent_arc.as_ref().clone();
                update_routing(&mut probe, deletes, inserts)
            }
        };

        // ---- Phase 3: compute_merge + pack (bcachefs 对齐) ----
        let mut src_count = srcs.len();
        let node_size = {
            let (_, ref n) = srcs[0];
            n.node_size
        };
        let mut total_u64s: u32 = srcs.iter().map(|(_, n)| n.live_data_bytes() / 8).sum();
        // compute_merge: ceiling_div(total_u64s, MERGE_HIGHER threshold)
        let higher_u64s = node_size * crate::btree::node::MERGE_HIGHER_NUM
            / crate::btree::node::MERGE_HIGHER_DEN
            / 8;
        let mut nr_dsts = std::cmp::max(1, total_u64s.div_ceil(higher_u64s)) as usize;
        if nr_dsts >= src_count {
            // bcachefs interior.c:2829-2847: with three sources, remove the
            // larger sibling candidate once, then recompute against half-node
            // capacity before giving up.
            if src_count == 3 {
                let pivot_idx = srcs
                    .iter()
                    .position(|(addr, _)| *addr == node_addr)
                    .expect("merge source list must contain pivot");
                let remove_idx = if srcs[0].1.live_data_bytes() / 8
                    > srcs[2].1.live_data_bytes() / 8
                {
                    0
                } else {
                    2
                };
                let sibling_live_u64s = srcs[remove_idx].1.live_data_bytes() / 8;
                let pivot_live_u64s = srcs[pivot_idx].1.live_data_bytes() / 8;
                let hysteresis = (node_size / 8) / 3 + ((node_size / 8) / 3 >> 2);
                let mut estimate = pivot_live_u64s + sibling_live_u64s;
                if estimate > hysteresis {
                    estimate -= (estimate - hysteresis) / 2;
                }
                Arc::get_mut(&mut srcs[pivot_idx].1)
                    .expect("merge pivot must remain uniquely owned")
                    .sib_u64s[if remove_idx < pivot_idx { 0 } else { 1 }] = estimate
                    .min(u32::from(u16::MAX - 1))
                    as u16;

                let (addr, arc) = srcs.remove(remove_idx);
                self.cache.put_node(addr, arc);
                src_count -= 1;
                total_u64s = srcs.iter().map(|(_, n)| n.live_data_bytes() / 8).sum();
                let half_node_u64s = (node_size / 8) / 2;
                nr_dsts = std::cmp::max(1, total_u64s.div_ceil(half_node_u64s)) as usize;
            }

            if nr_dsts >= src_count {
                // bcachefs merge_fail_reset_sib_u64s(): update each surviving
                // sibling estimate with hysteresis to avoid immediate retries.
                let pivot_idx = srcs
                    .iter()
                    .position(|(addr, _)| *addr == node_addr)
                    .expect("merge source list must contain pivot");
                let pivot_live_u64s = srcs[pivot_idx].1.live_data_bytes() / 8;
                let hysteresis = (node_size / 8) / 3 + ((node_size / 8) / 3 >> 2);
                let sibling_estimates: Vec<(usize, u32)> = srcs
                    .iter()
                    .enumerate()
                    .filter(|(idx, _)| *idx != pivot_idx)
                    .map(|(idx, (_, sibling))| {
                        let mut estimate = pivot_live_u64s + sibling.live_data_bytes() / 8;
                        if estimate > hysteresis {
                            estimate -= (estimate - hysteresis) / 2;
                        }
                        (idx, estimate.min(u32::from(u16::MAX - 1)))
                    })
                    .collect();
                let pivot = Arc::get_mut(&mut srcs[pivot_idx].1)
                    .expect("merge pivot must remain uniquely owned");
                for (idx, estimate) in sibling_estimates {
                    pivot.sib_u64s[if idx < pivot_idx { 0 } else { 1 }] = estimate as u16;
                }
                if let Some((addr, parent_arc)) = parent_hold.take() {
                    self.cache.put_node(addr, parent_arc);
                }
                for (addr, arc) in srcs {
                    self.cache.put_node(addr, arc);
                }
                return false;
            }
        }
        assert!(nr_dsts <= 2, "nr_dsts should be 1 or 2");

        let mut survivors: Vec<(u64, BtreeKey)> = Vec::with_capacity(nr_dsts);
        let last_idx = src_count - 1;

        if nr_dsts == 1 {
            // N→1: 所有 srcs 吸收到最右节点
            let last_idx = src_count - 1;
            let planned_inserts = vec![
                (
                    BtreeKey::from_bpos(srcs[0].1.min_key, KeyType::Normal),
                    srcs[last_idx].0,
                ),
            ];
            if !route_fits(&parent_hold, &src_addrs, &planned_inserts) {
                if let Some((addr, parent_arc)) = parent_hold.take() {
                    self.cache.put_node(addr, parent_arc);
                }
                for (addr, arc) in srcs {
                    self.cache.put_node(addr, arc);
                }
                return false;
            }
            for i in (0..last_idx).rev() {
                let (head, tail) = srcs.split_at_mut(last_idx);
                let sv = Arc::get_mut(&mut tail[0].1).unwrap();
                let ot = Arc::get_mut(&mut head[i].1).unwrap();
                if !sv.can_absorb(ot) {
                    let _ = ot;
                    let _ = sv;
                    if let Some((addr, parent_arc)) = parent_hold.take() {
                        self.cache.put_node(addr, parent_arc);
                    }
                    for (addr, arc) in srcs {
                        self.cache.put_node(addr, arc);
                    }
                    return false;
                }
                let merged_min = std::cmp::min(sv.min_key, ot.min_key);
                let merged_max = std::cmp::max(sv.max_key, ot.max_key);
                sv.absorb(ot);
                sv.min_key = merged_min;
                sv.max_key = merged_max;
            }
            {
                let addr = srcs[last_idx].0;
                let sv = Arc::get_mut(&mut srcs[last_idx].1).unwrap();
                sv.compact();
                survivors.push((addr, BtreeKey::from_bpos(sv.min_key, KeyType::Normal)));
            }
        } else {
            // N→2: 收集所有条目，排序去重，平衡分裂
            let mut entries: Vec<BtreeEntry> = Vec::new();
            for (_, n) in &srcs {
                let mut ni = BtreeNodeIter::default();
                bch2_btree_node_iter_init_from_start(&mut ni, n);
                while bch2_btree_node_iter_peek_all(&ni, n).is_some() {
                    let (k, v) = n.read_packed_entry(ni.data[0].k as usize * 8);
                    if k.key_type != KeyType::Deleted {
                        entries.push(BtreeEntry::from((k, v.to_bchval())));
                    }
                    bch2_btree_node_iter_advance(&mut ni, n);
                }
            }
            entries.sort_by_key(|e| e.pos);
            entries.dedup_by(|a, b| a.pos == b.pos);
            let (mid, _) = match BtreeNode::find_balanced_split(&entries, node_size) {
                Some(r) => r,
                None => {
                    if let Some((addr, parent_arc)) = parent_hold.take() {
                        self.cache.put_node(addr, parent_arc);
                    }
                    for (addr, arc) in srcs {
                        self.cache.put_node(addr, arc);
                    }
                    return false;
                }
            };
            let planned_inserts = vec![
                (
                    BtreeKey::from_bpos(entries[0].pos, KeyType::Normal),
                    srcs[0].0,
                ),
                (
                    BtreeKey::from_bpos(entries[mid].pos, KeyType::Normal),
                    srcs[last_idx].0,
                ),
            ];
            if !route_fits(&parent_hold, &src_addrs, &planned_inserts) {
                if let Some((addr, parent_arc)) = parent_hold.take() {
                    self.cache.put_node(addr, parent_arc);
                }
                for (addr, arc) in srcs {
                    self.cache.put_node(addr, arc);
                }
                return false;
            }
            {
                let (first, rest) = srcs.split_at_mut(1);
                let right_idx = rest.len() - 1;
                let left = Arc::get_mut(&mut first[0].1).unwrap();
                let right = Arc::get_mut(&mut rest[right_idx].1).unwrap();
                left.pack_entries_into(entries, mid, right);
                left.compact();
                right.compact();
            }
            {
                let addr0 = srcs[0].0;
                let s0 = Arc::get_mut(&mut srcs[0].1).unwrap();
                survivors.push((addr0, BtreeKey::from_bpos(s0.min_key, KeyType::Normal)));
            }
            {
                let addr_last = srcs[last_idx].0;
                let sl = Arc::get_mut(&mut srcs[last_idx].1).unwrap();
                survivors.push((addr_last, BtreeKey::from_bpos(sl.min_key, KeyType::Normal)));
            }
        }

        // ---- Phase 4: keylist 父节点路由更新 ----
        // bcachefs 对称 diff: 删除所有旧路由，插入所有新路由
        let deletes: Vec<u64> = src_addrs.clone();
        let inserts: Vec<(BtreeKey, u64)> = survivors
            .iter()
            .map(|&(addr, ref key)| (*key, addr))
            .collect();

        if parent_is_root {
            let root = unsafe { &mut *self.root.get() };
            if let Some(parent) = Arc::get_mut(&mut root.node) {
                let updated = update_routing(parent, &deletes, &inserts);
                debug_assert!(updated, "preflighted root route update must fit");
                if !updated {
                    for (addr, arc) in srcs {
                        self.cache.put_node(addr, arc);
                    }
                    return false;
                }
                self.root_modified.store(true, Ordering::Release);
            } else {
                debug_assert!(false, "root parent became unavailable after preflight");
                for (addr, arc) in srcs {
                    self.cache.put_node(addr, arc);
                }
                return false;
            }
        } else {
            let (parent_addr, mut parent_arc) = parent_hold
                .take()
                .expect("non-root merge must hold its parent");
            let updated = Arc::get_mut(&mut parent_arc)
                .map(|parent| update_routing(parent, &deletes, &inserts))
                .unwrap_or(false);
            debug_assert!(updated, "preflighted parent route update must fit");
            if !updated {
                self.cache.insert_dirty(parent_addr, parent_arc);
                for (addr, arc) in srcs {
                    self.cache.put_node(addr, arc);
                }
                return false;
            }
            self.cache.insert_dirty(parent_addr, parent_arc);
        }

        // ---- Phase 5: 幸存节点写入 dirty cache ----
        for (addr, arc) in srcs {
            if survivors.iter().any(|&(s_addr, _)| s_addr == addr) {
                self.cache.insert_dirty(addr, arc);
            }
        }
        true
    }
}

// SAFETY: Btree uses UnsafeCell<BtreeRoot> for interior mutability (bcachefs 对齐).
// 所有 mutation 通过 UnsafeCell raw pointer 进行，调用方保证同一节点不并发写。
// AtomicU32/AtomicBool/Arc 等字段本身是线程安全的。
unsafe impl Sync for Btree {}

impl Default for Btree {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Btree {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let root = unsafe { &*self.root.get() };
        f.debug_struct("Btree")
            .field("depth", &root.depth)
            .field("packed_keys", &root.node.packed_keys)
            .field("unpacked_keys", &root.node.unpacked_keys)
            .field("cache_size", &self.cache.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btree::key::KeyType;
    use crate::btree::node::{BsetHeader, BsetTree, BSET_HEADER_U64S};
    use crate::btree::writer::NoopWriter;
    use crate::btree::BtreeId;
    use crate::types::BlockAddr;

    #[test]
    fn test_interior_update_wait_rechecks_after_wakeup() {
        let updates = Arc::new(InteriorUpdates::new());
        let node = Arc::new(BtreeNode::new_leaf());
        let update_id = updates.start();
        assert!(node.set_btree_node_write_blocked());
        updates.block_node(update_id, 7);

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let waiter_updates = updates.clone();
            let waiter_node = node.clone();
            let waiter = tokio::spawn(async move {
                waiter_updates.wait_on_node(&waiter_node).await;
            });

            tokio::task::yield_now().await;
            assert!(!waiter.is_finished());

            node.clear_btree_node_write_blocked();
            updates.finish(update_id);
            waiter.await.unwrap();
        });
    }

    #[tokio::test]
    async fn test_leaf_update_registers_journal_pin_at_commit_time() {
        let vol = Arc::new(BchVol::test_trees());
        vol.attach_tree_refs(&vol);
        let tree = vol.btree(BtreeId::Extents);

        assert!(tree
            .bch2_btree_insert(
                &NoopWriter,
                BtreeKey::new(1, 1, KeyType::Normal),
                BchVal::new(10, 0),
                1,
            )
            .await
            .unwrap());

        let root = tree.root();
        assert_eq!(root.node.journal_pin.seq.load(Ordering::Acquire), 1);
        assert_eq!(
            vol.journal_ref()
                .pin_fifo_ref()
                .entry_for_seq(1)
                .unwrap()
                .count
                .load(Ordering::Acquire),
            2,
            "entry self-pin and dirty btree node pin must both be present"
        );
    }

    #[tokio::test]
    async fn test_journal_pinned_root_accepts_later_leaf_updates() {
        let vol = Arc::new(BchVol::test_trees());
        vol.attach_tree_refs(&vol);
        let tree = vol.btree(BtreeId::Extents);

        for offset in 1..=3 {
            assert!(tree
                .bch2_btree_insert(
                    &NoopWriter,
                    BtreeKey::new(offset, 1, KeyType::Normal),
                    BchVal::new(100 + offset, 0),
                    1,
                )
                .await
                .unwrap());
        }

        assert_eq!(tree.root().node.packed_keys, 3);
        for offset in 1..=3 {
            assert!(tree
                .bch2_btree_iter_peek(&BtreeKey::new(offset, 1, KeyType::Normal))
                .is_some());
        }
    }

    #[test]
    fn test_depth_zero_wrapper_serializes_concurrent_root_updates() {
        let tree = Arc::new(Btree::new_with_type(BtreeId::Extents));
        let inserted = std::thread::scope(|scope| {
            let mut workers = Vec::new();
            for offset in 1..=8 {
                let tree = Arc::clone(&tree);
                workers.push(scope.spawn(move || {
                    tree.bch2_btree_bset_insert_key_wrapper(
                        BtreeEntry::new(
                            Bpos::new(0, offset, 1),
                            KeyType::Normal,
                            KeyValue::Raw(vec![offset as u8]),
                        ),
                        1,
                    )
                }));
            }
            workers
                .into_iter()
                .map(|worker| worker.join().expect("root update worker panicked"))
                .filter(|inserted| *inserted)
                .count()
        });

        assert_eq!(inserted, 8);
        assert_eq!(tree.root().node.packed_keys, 8);
    }

    /// 手动构造一个 2 层 B+tree（internal root + 2 leaves）
    fn make_two_level_tree() -> Btree {
        let cache = Arc::new(NodeCache::new());

        // left: keys 10, 20, 30
        let mut left = BtreeNode::new_leaf();
        left.insert(BtreeKey::new(10, 1, KeyType::Normal), BchVal::new(100, 0));
        left.insert(BtreeKey::new(20, 1, KeyType::Normal), BchVal::new(200, 0));
        left.insert(BtreeKey::new(30, 1, KeyType::Normal), BchVal::new(300, 0));
        let left = Arc::new(left);

        // right: keys 40, 50
        let mut right = BtreeNode::new_leaf();
        right.insert(BtreeKey::new(40, 1, KeyType::Normal), BchVal::new(400, 0));
        right.insert(BtreeKey::new(50, 1, KeyType::Normal), BchVal::new(500, 0));
        let right = Arc::new(right);

        let left_addr = cache.alloc_addr();
        let right_addr = cache.alloc_addr();
        cache.insert(left_addr, left);
        cache.insert(right_addr, right);

        // internal root
        let mut internal = BtreeNode::new_internal();
        let mut cur = u32::from(BSET_HEADER_U64S) * 8;
        cur += internal.write_entry(cur, &BtreeKey::MIN_KEY, &BchVal::new(left_addr, 0), 0);
        cur += internal.write_entry(
            cur,
            &BtreeKey::new(40, 1, KeyType::Normal),
            &BchVal::new(right_addr, 0),
            0,
        );
        internal.sets[0] = BsetTree {
            size: 0,
            extra: crate::btree::node::BSET_NO_AUX_TREE_VAL,
            data_offset: 0,
            aux_data_offset: u16::MAX,
            end_offset: (cur / 8) as u16,
        };
        let header = BsetHeader {
            seq: 0,
            journal_seq: 0,
            flags: 0,
            version: 0,
            u64s: internal.sets[0].end_offset - BSET_HEADER_U64S,
        };
        unsafe {
            internal
                .data
                .as_mut_ptr()
                .cast::<BsetHeader>()
                .write_unaligned(header);
        }
        internal.packed_keys = 2;
        internal.unpacked_keys = 0;

        Btree::bch2_btree_set_root_for_read(
            BtreeRoot {
                node: Arc::new(internal),
                depth: 1,
            },
            cache,
            crate::btree::BtreeId::Extents,
        )
    }

    #[test]
    fn test_btree_multi_level_insert() {
        let b = make_two_level_tree();

        // 插入到左叶子
        assert!(futures::executor::block_on(b.bch2_btree_insert(
            &NoopWriter,
            BtreeKey::new(15, 1, KeyType::Normal),
            BchVal::new(150, 0),
            0
        ))
        .unwrap());
        let found = b.bch2_btree_iter_peek(&BtreeKey::new(15, 1, KeyType::Normal));
        assert!(found.is_some(), "inserted key 15 should be found");
        assert_eq!(found.unwrap().1, BchVal::new(150, 0));

        // 插入到右叶子
        assert!(futures::executor::block_on(b.bch2_btree_insert(
            &NoopWriter,
            BtreeKey::new(45, 1, KeyType::Normal),
            BchVal::new(450, 0),
            0
        ))
        .unwrap());
        let found = b.bch2_btree_iter_peek(&BtreeKey::new(45, 1, KeyType::Normal));
        assert!(found.is_some(), "inserted key 45 should be found");
        assert_eq!(found.unwrap().1, BchVal::new(450, 0));

        // 现有 key 仍可读
        assert!(b
            .bch2_btree_iter_peek(&BtreeKey::new(10, 1, KeyType::Normal))
            .is_some());
        assert!(b
            .bch2_btree_iter_peek(&BtreeKey::new(50, 1, KeyType::Normal))
            .is_some());
    }

    #[test]
    fn test_btree_multi_level_delete() {
        let b = make_two_level_tree();

        // 删除左叶子中的 key
        assert!(futures::executor::block_on(b.bch2_btree_delete(
            &NoopWriter,
            &BtreeKey::new(10, 1, KeyType::Normal),
            0
        ))
        .unwrap());
        assert!(
            b.bch2_btree_iter_peek(&BtreeKey::new(10, 1, KeyType::Normal))
                .is_none(),
            "deleted key 10 gone"
        );

        // 删除右叶子中的 key
        assert!(futures::executor::block_on(b.bch2_btree_delete(
            &NoopWriter,
            &BtreeKey::new(50, 1, KeyType::Normal),
            0
        ))
        .unwrap());
        assert!(
            b.bch2_btree_iter_peek(&BtreeKey::new(50, 1, KeyType::Normal))
                .is_none(),
            "deleted key 50 gone"
        );

        // 其他 key 不受影响
        assert!(b
            .bch2_btree_iter_peek(&BtreeKey::new(20, 1, KeyType::Normal))
            .is_some());
        assert!(b
            .bch2_btree_iter_peek(&BtreeKey::new(40, 1, KeyType::Normal))
            .is_some());

        // 删除不存在的 key
        assert!(!futures::executor::block_on(b.bch2_btree_delete(
            &NoopWriter,
            &BtreeKey::new(999, 1, KeyType::Normal),
            0
        ))
        .unwrap());
    }

    #[test]
    fn test_btree_multi_level_insert_after_delete() {
        let b = make_two_level_tree();

        // 删除后重新插入同一 key
        assert!(futures::executor::block_on(b.bch2_btree_delete(
            &NoopWriter,
            &BtreeKey::new(20, 1, KeyType::Normal),
            0
        ))
        .unwrap());
        assert!(b
            .bch2_btree_iter_peek(&BtreeKey::new(20, 1, KeyType::Normal))
            .is_none());

        assert!(futures::executor::block_on(b.bch2_btree_insert(
            &NoopWriter,
            BtreeKey::new(20, 1, KeyType::Normal),
            BchVal::new(999, 0),
            0
        ))
        .unwrap());
        let found = b.bch2_btree_iter_peek(&BtreeKey::new(20, 1, KeyType::Normal));
        assert!(found.is_some(), "re-inserted key 20 should be found");
        assert_eq!(found.unwrap().1, BchVal::new(999, 0));
    }

    /// 填充 leaf 直到触发 split → 验证 routing entry 正确插入 parent
    #[test]
    fn test_btree_multi_level_leaf_split() {
        let b = make_two_level_tree();

        // 左叶子现有 3 keys (10,20,30)，填充至满再 split
        // 256KB node / 29b entry ≈ 9039 max
        let fill_count = 9040;
        for i in 31..=fill_count {
            assert!(futures::executor::block_on(b.bch2_btree_insert(
                &NoopWriter,
                BtreeKey::new(i, 1, KeyType::Normal),
                BchVal::new(i * 10, 0),
                0
            ))
            .unwrap());
        }

        // 验证 split 后的 key 可读
        let mid = fill_count / 2 + 30;
        assert!(
            b.bch2_btree_iter_peek(&BtreeKey::new(mid, 1, KeyType::Normal))
                .is_some(),
            "key {} should be findable after split",
            mid
        );
        assert!(
            b.bch2_btree_iter_peek(&BtreeKey::new(fill_count, 1, KeyType::Normal))
                .is_some(),
            "key {} should be findable after split",
            fill_count
        );

        // 原有 key 仍可读
        assert!(b
            .bch2_btree_iter_peek(&BtreeKey::new(10, 1, KeyType::Normal))
            .is_some());
        assert!(b
            .bch2_btree_iter_peek(&BtreeKey::new(50, 1, KeyType::Normal))
            .is_some());

        // depth 仍为 1（parent 有空间放 routing entry）
        assert_eq!(b.depth(), 1);
    }

    #[test]
    fn test_btree_new() {
        let b = Btree::new();
        assert_eq!(b.depth(), 0);
        assert_eq!(b.root().node.packed_keys, 0);
    }

    #[test]
    fn test_btree_insert_and_count() {
        let b = Btree::new();
        let k = BtreeKey::new(100, 1, KeyType::Normal);
        let v = BchVal::new(0xABCD, 1);
        assert!(futures::executor::block_on(b.bch2_btree_insert(&NoopWriter, k, v, 0)).unwrap());
        assert!(b.root().node.packed_keys > 0);
    }

    #[test]
    fn test_btree_get_empty() {
        let b = Btree::new();
        let result = b.bch2_btree_iter_peek(&BtreeKey::new(100, 1, KeyType::Normal));
        assert!(result.is_none());
    }

    #[test]
    fn test_btree_get_after_insert() {
        let b = Btree::new();
        let k = BtreeKey::new(42, 1, KeyType::Normal);
        let v = BchVal::new(0xFF, 1);
        assert!(futures::executor::block_on(b.bch2_btree_insert(&NoopWriter, k, v, 0)).unwrap());
        let found = b.bch2_btree_iter_peek(&k);
        assert!(found.is_some());
        assert_eq!(found.unwrap().0, k);
    }

    #[test]
    fn test_btree_delete_no_panic() {
        let b = Btree::new();
        let k = BtreeKey::new(42, 1, KeyType::Normal);
        futures::executor::block_on(b.bch2_btree_insert(&NoopWriter, k, BchVal::new(0xFF, 1), 0))
            .unwrap();
        futures::executor::block_on(b.bch2_btree_delete(&NoopWriter, &k, 0)).unwrap();
    }

    #[test]
    fn test_btree_transaction() {
        let b = Btree::new();
        let result = b.bch2_trans_commit(|trans| {
            let iter = trans.bch2_trans_get_iter(
                b.root(),
                &BtreeKey::new(100, 1, KeyType::Normal),
                false,
                BtreeId::Extents,
            );
            assert!(iter.peek().is_none());
        });
        assert!(result.is_ok());
    }

    #[test]
    fn test_btree_multiple_inserts() {
        let b = Btree::new();
        for i in 0..10 {
            let k = BtreeKey::new(i, 1, KeyType::Normal);
            assert!(futures::executor::block_on(b.bch2_btree_insert(
                &NoopWriter,
                k,
                BchVal::new(i * 10, 1),
                0
            ))
            .unwrap());
        }
        assert_eq!(b.root().node.packed_keys, 10);
    }

    /// 验证递归分裂传播：构建小节点树，插入大量 key 触发 3 层分裂
    ///
    /// 1. 设 root.node_size = 2048（~64 entries/node）
    /// 2. 插入足够 key 触发 leaf split → internal split → root split
    /// 3. 验证 depth=3 且所有 key 可读
    #[test]
    fn test_split_propagation_3level() {
        let b = Btree::new();
        // 使用小 node_size 加速分裂
        // node_size=512 → ~16 entries/node
        // ExtentValue 16B 格式，512B 节点约 10 entries/node
        // depth 0→1: ~10 inserts
        // depth 1→2: ~8 leaf splits × 10 inserts ≈ 80
        // depth 2→3: ~8 internal splits × 8 leaf splits × 10 inserts ≈ 640
        // 总计 ~730 inserts，用 1300 保证触发
        let small_size = 512u32;
        let root = unsafe { &mut *b.root.get() };
        Arc::get_mut(&mut root.node).unwrap().node_size = small_size;

        let total_keys = 700u64;
        for i in 0..total_keys {
            if i % 500 == 0 && false {
                eprintln!(
                    "DEBUG: i={}, depth={}, cache_len={}",
                    i,
                    b.depth(),
                    b.cache().len()
                );
            }
            assert!(
                futures::executor::block_on(b.bch2_btree_insert(
                    &NoopWriter,
                    BtreeKey::new(i, 1, KeyType::Normal),
                    BchVal::new(i * 10, 0),
                    0
                ))
                .unwrap(),
                "insert failed at i={}, depth={}, cache_len={}",
                i,
                b.depth(),
                b.cache().len()
            );
        }
        assert_eq!(
            b.depth(),
            3,
            "tree should have depth 3 after recursive split propagation (got depth={})",
            b.depth()
        );

        // 验证所有 key 可达
        for i in 0..total_keys {
            let found = b.bch2_btree_iter_peek(&BtreeKey::new(i, 1, KeyType::Normal));
            assert!(
                found.is_some(),
                "key {} should be reachable after split propagation",
                i
            );
            assert_eq!(found.unwrap().1, BchVal::new(i * 10, 0));
        }
    }

    // ─── Wave 0: routing entry insertion 边界安全测试 ─────────────

    /// 验证 routing entry insertion 在 pos=0 时不触发 wrapping panic：
    /// depth=2 树，强制分裂 level-1 internal node → 触发 pos 从 0 回溯到 root
    #[test]
    fn test_insert_routing_no_wrap_panic() {
        let b = Btree::new();
        // node_size=256 → ~8 entries/node，确保内部节点也能分裂
        {
            let root = unsafe { &mut *b.root.get() };
            Arc::get_mut(&mut root.node).unwrap().node_size = 256;
        }

        // 插入大量 key 强制多级分裂（depth 从 0→1→2→...），
        // 覆盖 routing entry insertion 中 pos=0 后的边界分支
        // ExtentValue 16B 格式，256B 节点约 5 entries/node
        let total = 600u64;
        for i in 0..total {
            assert!(
                futures::executor::block_on(b.bch2_btree_insert(
                    &NoopWriter,
                    BtreeKey::new(i, 1, KeyType::Normal),
                    BchVal::new(i * 10, 0),
                    0
                ))
                .unwrap(),
                "insert failed at i={}, depth={}",
                i,
                b.depth()
            );
        }
        // 深度应 ≥2（验证经过内部节点分裂）
        assert!(
            b.depth() >= 2,
            "tree should have depth >=2 after forcing internal splits (got depth={})",
            b.depth()
        );

        // 所有 key 仍可达
        for i in 0..total {
            let found = b.bch2_btree_iter_peek(&BtreeKey::new(i, 1, KeyType::Normal));
            assert!(found.is_some(), "key {} should be reachable", i);
            assert_eq!(found.unwrap().1, BchVal::new(i * 10, 0));
        }
    }

    /// 验证单 leaf → split_root 时空 path 正确：depth=0 插入满触发 split_root
    #[test]
    fn test_insert_routing_path_empty() {
        let b = Btree::new();
        // 使用小 node_size 加速分裂
        let root = unsafe { &mut *b.root.get() };
        Arc::get_mut(&mut root.node).unwrap().node_size = 256;

        let total = 200u64;
        for i in 0..total {
            assert!(
                futures::executor::block_on(b.bch2_btree_insert(
                    &NoopWriter,
                    BtreeKey::new(i, 1, KeyType::Normal),
                    BchVal::new(i * 5, 0),
                    0
                ))
                .unwrap(),
                "insert failed at i={}",
                i
            );
        }
        // 应触发 root split（depth 0→1）
        assert!(
            b.depth() >= 1,
            "tree should have depth >=1 after split_root"
        );

        // 所有 key 可达
        for i in 0..total {
            let found = b.bch2_btree_iter_peek(&BtreeKey::new(i, 1, KeyType::Normal));
            assert!(
                found.is_some(),
                "key {} should be reachable after split_root",
                i
            );
        }
    }

    /// Phase 1 eager write：split_root 写入子节点后 clean 插入 cache，
    /// 新根地址存入 PendingRootJournal。验证 journal_seq 传播和根记录。
    #[test]
    fn test_split_root_propagates_journal_seq() {
        let b = Btree::new();
        let split_seq = 77u64;
        {
            let root_ref = unsafe { &mut *b.root.get() };
            let root_node = Arc::get_mut(&mut root_ref.node).unwrap();
            root_node.node_size = 320;
            root_node.journal_seq = 13;
            // CRC extent value 24B 格式，320B 节点约 5 entries
            for i in 0..5u64 {
                assert!(
                    root_node.insert(BtreeKey::new(i, 1, KeyType::Normal), BchVal::new(i * 3, 0),)
                );
            }
        }
        assert!(futures::executor::block_on(b.split_root(&NoopWriter, None, split_seq)).unwrap());

        // Phase 1: 子节点已 eager write + clean 插入 cache，无 dirty 节点
        let flushed = b.bch2_btree_flush_all();
        assert!(
            flushed.is_empty(),
            "split_root eager-writes children — no dirty nodes"
        );

        // 验证 PendingRootJournal 记录了新根
        let prj = b
            .take_pending_root_journal()
            .expect("should set PendingRootJournal");
        assert!(prj.root_addr > 0, "root_addr should be set");
        assert!(prj.level > 0, "new root level > 0");

        // 验证根节点的 journal_seq
        let root_ref = unsafe { &*b.root.get() };
        assert_eq!(
            root_ref.node.journal_seq, split_seq,
            "root node inherits split_seq"
        );
    }

    // ─── Wave 1: 字节分割 + 命名循环 + debug 断言 ─────────────

    /// 验证 byte-size 分割后两个半节点的字节用量相近（偏差 ≤20%）
    ///
    /// Given: 包含奇数个等大小条目的节点（9 entries × 32 bytes = 288 total）
    /// When:  执行 split()
    /// Then:  左右半节点的字节用量应在 half_bytes 的 ±20% 以内
    #[test]
    fn test_split_balanced_byte_size() {
        let mut node = BtreeNode::new_leaf();
        // 插入 9 个 key（奇数个，展示字节分割与计数分割的差异）
        for i in 0..9 {
            assert!(node.insert(
                BtreeKey::new(i as u64, 1, KeyType::Normal),
                BchVal::new(i as u64 * 10, 0),
            ));
        }
        let (median_key, right_half) = node.split().expect("split should succeed with 9 entries");
        assert!(median_key.get_vaddr() > 0, "median_key should be valid");

        let left_bytes = node.total_data_bytes();
        let right_bytes = right_half.total_data_bytes();
        let total_bytes = left_bytes + right_bytes;
        let half_bytes = total_bytes / 2;

        // Both halves should be within 20% of the ideal half size
        let left_diff_pct = if left_bytes > half_bytes {
            (left_bytes - half_bytes) as f64 / half_bytes as f64 * 100.0
        } else {
            (half_bytes - left_bytes) as f64 / half_bytes as f64 * 100.0
        };
        let right_diff_pct = if right_bytes > half_bytes {
            (right_bytes - half_bytes) as f64 / half_bytes as f64 * 100.0
        } else {
            (half_bytes - right_bytes) as f64 / half_bytes as f64 * 100.0
        };

        // 容差 35%：find_balanced_split 以 60/40 为目标（bcachefs 对齐），
        // 等大小离散条目下 9→6/3 分最大偏差 33.3%（192/96 vs 144）
        assert!(
            left_diff_pct <= 35.0,
            "left half byte usage {} bytes is {:.1}% off from half {} bytes",
            left_bytes,
            left_diff_pct,
            half_bytes
        );
        assert!(
            right_diff_pct <= 35.0,
            "right half byte usage {} bytes is {:.1}% off from half {} bytes",
            right_bytes,
            right_diff_pct,
            half_bytes
        );

        // All entries should be searchable in their respective halves
        // find_balanced_split 以 60/40 为目标，9 个等大条目 → 6 left (0-5), 3 right (6-8)
        for i in 0..6 {
            let found = node.search(&BtreeKey::new(i, 1, KeyType::Normal));
            assert!(found.is_some(), "left entry {} should survive split", i);
        }
        for i in 6..9 {
            let found = right_half.search(&BtreeKey::new(i, 1, KeyType::Normal));
            assert!(found.is_some(), "right entry {} should survive split", i);
        }
    }

    /// 验证 routing entry insertion 主循环干净迭代（不卡死、不 panic）：
    /// 构建 depth=2 树，强制 internal node 分裂 → routing loop 必须多次迭代
    ///
    /// Given: node_size=512 的 Btree
    /// When:  插入足够的 key 强制 depth 增长到 ≥2（经过内部节点分裂）
    /// Then:  所有 key 可达，key_count 正确，depth≥2
    #[test]
    fn test_insert_routing_clean_loop() {
        let b = Btree::new();
        {
            let root = unsafe { &mut *b.root.get() };
            Arc::get_mut(&mut root.node).unwrap().node_size = 512;
        }

        // Insert enough to force internal node splits and depth ≥2
        // node_size=512 → ~16 entries/node
        let total = 2000u64;
        for i in 0..total {
            assert!(
                futures::executor::block_on(b.bch2_btree_insert(
                    &NoopWriter,
                    BtreeKey::new(i, 1, KeyType::Normal),
                    BchVal::new(i * 10, 0),
                    0,
                ))
                .unwrap(),
                "insert failed at i={}, depth={}",
                i,
                b.depth()
            );
        }

        // The routing loop should have cleanly processed all iterations
        assert!(
            b.depth() >= 2,
            "depth should be >=2 after forcing internal splits (got depth={})",
            b.depth()
        );

        // Verify all keys are reachable
        for i in 0..total {
            let found = b.bch2_btree_iter_peek(&BtreeKey::new(i, 1, KeyType::Normal));
            assert!(found.is_some(), "key {} lost after routing loop", i);
            assert_eq!(found.unwrap().1, BchVal::new(i * 10, 0));
        }
    }

    /// 验证 at_path_boundary（pos=0 → root 回溯）不 panic：
    /// 构建小节点树 → 插入大量 key 强制通过 path[0] 分裂边界
    ///
    /// 与 test_split_propagation_3level 类似，但明确断言
    /// at_path_boundary（routing entry insertion 中 pos=0 时的
    /// root 回溯分支）不会引发 panic 或死循环。
    #[test]
    fn test_insert_routing_at_path_boundary() {
        let b = Btree::new();
        // node_size=384 → ~12 entries/node，更快触发深度分裂
        let root = unsafe { &mut *b.root.get() };
            Arc::get_mut(&mut root.node).unwrap().node_size = 512;

        // 强制多级分裂（depth 0→1→2→3），
        // 确保 routing loop 的 at_path_boundary（pos=0 → root）被触发
        let total = 4000u64;
        for i in 0..total {
            assert!(
                futures::executor::block_on(b.bch2_btree_insert(
                    &NoopWriter,
                    BtreeKey::new(i, 1, KeyType::Normal),
                    BchVal::new(i * 10, 0),
                    0,
                ))
                .unwrap(),
                "insert failed at i={}, depth={}",
                i,
                b.depth()
            );
        }

        // 断言：at_path_boundary 分支没有 panic
        // 如果到达这里，说明边界分支执行成功
        assert!(
            b.depth() >= 2,
            "depth should be >=2 after crossing path boundary (got depth={})",
            b.depth()
        );

        // 验证所有 key 仍可达
        for i in 0..total {
            let found = b.bch2_btree_iter_peek(&BtreeKey::new(i, 1, KeyType::Normal));
            assert!(
                found.is_some(),
                "key {} lost after at_path_boundary split",
                i
            );
        }
    }

    // ─── Wave 2: Leaf merge after delete ─────────────────────────

    /// 验证叶子合并：左 leaf underfull 后与右兄弟合并
    ///
    /// 1. 创建小节点树（node_size=512, ~16 entries/node）
    /// 2. 插入 30 个 key → 分裂为 2 个 leaf（depth=1）
    /// 3. 从左 leaf 删除 10 个 key → 左 leaf ~5 entries < 6（underfull）
    /// 4. 验证合并发生：所有剩余 key（10..29）仍可达
    #[test]
    fn test_leaf_merge_after_delete() {
        let b = Btree::new();
        {
            let root = unsafe { &mut *b.root.get() };
            Arc::get_mut(&mut root.node).unwrap().node_size = 512;
        }

        // 插入 30 个 key → 2 个 leaf（depth=1）
        for i in 0..30u64 {
            assert!(
                futures::executor::block_on(b.bch2_btree_insert(
                    &NoopWriter,
                    BtreeKey::new(i, 1, KeyType::Normal),
                    BchVal::new(i * 10, 0),
                    0,
                ))
                .unwrap(),
                "insert failed at i={}",
                i
            );
        }
        assert_eq!(b.depth(), 1, "should be depth=1 after split");

        // 从左 leaf 删除 10 个 key（keys 0..9）
        // 左 leaf 原 ~15 entries → 余 ~5 entries → underfull（< 6）
        for i in 0..10u64 {
            assert!(
                futures::executor::block_on(b.bch2_btree_delete(
                    &NoopWriter,
                    &BtreeKey::new(i, 1, KeyType::Normal),
                    0
                ))
                .unwrap(),
                "delete failed at i={}",
                i
            );
        }

        // 合并后幸存 leaf 应有 keys 10..29（20 个 key）

        // 验证已删除 key 不可达
        for i in 0..10u64 {
            assert!(
                b.bch2_btree_iter_peek(&BtreeKey::new(i, 1, KeyType::Normal))
                    .is_none(),
                "deleted key {} should not exist",
                i
            );
        }

        // 验证剩余 key 全部可达
        for i in 10..30u64 {
            let found = b.bch2_btree_iter_peek(&BtreeKey::new(i, 1, KeyType::Normal));
            assert!(found.is_some(), "key {} should be reachable after merge", i);
            assert_eq!(
                found.unwrap().1,
                BchVal::new(i * 10, 0),
                "key {} should have correct value",
                i
            );
        }
    }

    /// 验证不触发合并：阈值以上时不执行合并
    ///
    /// 1. 创建小节点树（node_size=512）
    /// 2. 插入 30 个 key → 2 个 leaf
    /// 3. 只删除 1 个 key → leaf 仍在阈值以上 → 不触发合并
    #[test]
    fn test_leaf_no_merge_above_threshold() {
        let b = Btree::new();
        {
            let root = unsafe { &mut *b.root.get() };
            Arc::get_mut(&mut root.node).unwrap().node_size = 512;
        }

        for i in 0..30u64 {
            assert!(futures::executor::block_on(b.bch2_btree_insert(
                &NoopWriter,
                BtreeKey::new(i, 1, KeyType::Normal),
                BchVal::new(i * 10, 0),
                0,
            ))
            .unwrap());
        }
        let cache_len_before = b.cache().len();

        // 只删除 1 个 key → leaf 有 ~14 entries → 远高于阈值（<6）
        assert!(futures::executor::block_on(b.bch2_btree_delete(
            &NoopWriter,
            &BtreeKey::new(0, 1, KeyType::Normal),
            0
        ))
        .unwrap());

        // 应不触发合并（cache 不变）
        assert_eq!(
            b.cache().len(),
            cache_len_before,
            "cache should not change when no merge occurs"
        );

        // 验证已删除 key
        assert!(b
            .bch2_btree_iter_peek(&BtreeKey::new(0, 1, KeyType::Normal))
            .is_none());

        // 验证剩余 key
        for i in 1..30u64 {
            let found = b.bch2_btree_iter_peek(&BtreeKey::new(i, 1, KeyType::Normal));
            assert!(found.is_some(), "key {} should exist", i);
        }
    }

    /// 验证与左兄弟合并：最右侧 leaf underfull 时与左兄弟合并
    ///
    /// 1. 插入足够 key 创建 5+ 个 leaf（depth=2+）
    /// 2. 删除最后 ~30 个 key → 最右侧 leaf underfull
    /// 3. 应与左兄弟合并，所有剩余 key 可达
    #[test]
    fn test_leaf_merge_left_sibling() {
        let b = Btree::new();
        {
            let root = unsafe { &mut *b.root.get() };
            Arc::get_mut(&mut root.node).unwrap().node_size = 256;
        }

        // node_size=256 → ~8 entries/node
        // 插入 120 个 key → 多个 leaf, depth≥2
        let total = 120u64;
        for i in 0..total {
            assert!(
                futures::executor::block_on(b.bch2_btree_insert(
                    &NoopWriter,
                    BtreeKey::new(i, 1, KeyType::Normal),
                    BchVal::new(i * 10, 0),
                    0,
                ))
                .unwrap(),
                "insert failed at i={}",
                i
            );
        }
        assert!(b.depth() >= 2, "should have depth >= 2 (got {})", b.depth());

        // 删除最右侧 ~30 个 key → 最右侧 leaf underfull
        let delete_start = 90u64;
        for i in delete_start..total {
            assert!(
                futures::executor::block_on(b.bch2_btree_delete(
                    &NoopWriter,
                    &BtreeKey::new(i, 1, KeyType::Normal),
                    0
                ))
                .unwrap(),
                "delete failed at i={}",
                i
            );
        }

        // 验证剩余 key 全部可达
        for i in 0..delete_start {
            let found = b.bch2_btree_iter_peek(&BtreeKey::new(i, 1, KeyType::Normal));
            assert!(found.is_some(), "key {} should be reachable", i);
            assert_eq!(found.unwrap().1, BchVal::new(i * 10, 0));
        }
    }

    /// 验证最左侧 leaf 合并：左边界 leaf underfull 时与右兄弟合并
    ///
    /// 1. 创建小节点树（node_size=512）
    /// 2. 插入 30 个 key → 2 个 leaf
    /// 3. 从左 leaf 删除 12 个 key → 左 leaf ~3 entries → underfull
    /// 4. 应与右兄弟（右侧 leaf）合并
    #[test]
    fn test_leaf_merge_edge_min_key() {
        let b = Btree::new();
        {
            let root = unsafe { &mut *b.root.get() };
            Arc::get_mut(&mut root.node).unwrap().node_size = 512;
        }

        for i in 0..30u64 {
            assert!(futures::executor::block_on(b.bch2_btree_insert(
                &NoopWriter,
                BtreeKey::new(i, 1, KeyType::Normal),
                BchVal::new(i * 10, 0),
                0,
            ))
            .unwrap());
        }
        assert_eq!(b.depth(), 1);

        // 从左 leaf 删除 12 个 key → 余 ~3 → underfull
        for i in 0..12u64 {
            assert!(
                futures::executor::block_on(b.bch2_btree_delete(
                    &NoopWriter,
                    &BtreeKey::new(i, 1, KeyType::Normal),
                    0
                ))
                .unwrap(),
                "delete failed at i={}",
                i
            );
        }

        // 合并后应有 18 个 key（keys 12..29）

        // 验证已删除 key 不可达
        for i in 0..12u64 {
            assert!(
                b.bch2_btree_iter_peek(&BtreeKey::new(i, 1, KeyType::Normal))
                    .is_none(),
                "deleted key {} should not exist",
                i
            );
        }

        // 验证剩余 key 全部可达
        for i in 12..30u64 {
            let found = b.bch2_btree_iter_peek(&BtreeKey::new(i, 1, KeyType::Normal));
            assert!(found.is_some(), "key {} should be reachable", i);
            assert_eq!(found.unwrap().1, BchVal::new(i * 10, 0));
        }
    }

    /// 验证 3 层树的 cascade merge + collapse_root：
    ///
    /// 1. 创建深度 3 的树（node_size=512, ~5000 keys）
    /// 2. 删除大量右侧 key → 触发 leaf merge cascade
    /// 3. cascade 向上传播 → 内部节点合并 → collapse_root 缩减深度
    /// 4. 验证所有剩余 key 仍可达
    #[test]
    fn test_cascade_merge_3level() {
        let b = Btree::new();
        {
            let root = unsafe { &mut *b.root.get() };
            Arc::get_mut(&mut root.node).unwrap().node_size = 512;
        }

        // CRC extent value 28B 格式，512B 节点约 10 entries/node，~700 插入达 depth 3
        let total = 700u64;
        for i in 0..total {
            assert!(
                futures::executor::block_on(b.bch2_btree_insert(
                    &NoopWriter,
                    BtreeKey::new(i, 1, KeyType::Normal),
                    BchVal::new(i * 10, 0),
                    0,
                ))
                .unwrap(),
                "insert failed at i={}",
                i
            );
        }
        assert_eq!(
            b.depth(),
            3,
            "tree should have depth 3 before cascade (got {})",
            b.depth()
        );

        // 小批量删除，每步检查 integrity
        let check_keys: &[u64] = &[312, 313, 314, 315, 320, 350, 400, 500];

        // 逐个删除，每 5 个检查一次 routing integrity
        for i in 250..400u64 {
            assert!(
                futures::executor::block_on(b.bch2_btree_delete(
                    &NoopWriter,
                    &BtreeKey::new(i, 1, KeyType::Normal),
                    0
                ))
                .unwrap(),
                "delete failed at i={}",
                i
            );
            if i % 5 == 0 {
                for &ck in check_keys {
                    if ck > i {
                        let k = BtreeKey::new(ck, 1, KeyType::Normal);
                        if b.bch2_btree_iter_peek(&k).is_none() {
                            let mut path = Vec::new();
                            let leaf_addr = b.bch2_btree_path_traverse_one(&k, &mut path);
                            eprintln!(
                                "FAIL at i={}: key {} unreachable, depth={}, cache_len={}, path={:?}, leaf_addr={:?}",
                                i,
                                ck,
                                b.depth(),
                                b.cache().len(),
                                path,
                                leaf_addr
                            );
                            let root_ref = unsafe { &*b.root.get() };
                            eprintln!(
                                "Root key_count={} level={}",
                                root_ref.node.packed_keys + root_ref.node.unpacked_keys, root_ref.node.level
                            );
                            // Check path addrs
                            for (pi, &paddr) in path.iter().enumerate() {
                                match b.cache().get(paddr) {
                                    Some(n) => {
                                        eprintln!(
                                            "Path[{}] addr={} key_count={} level={}",
                                            pi, paddr, n.packed_keys + n.unpacked_keys, n.level
                                        );
                                        // Dump routing entries for internal nodes
                                        if n.level > 0 {
                                            eprintln!(
                                            "  Routing entries (key_count={}):",
                                            n.packed_keys + n.unpacked_keys
                                            );
                                            for si in 0..3 {
                                                let s = &n.sets[si];
                                                eprintln!(
                                                    "    set[{}]: data_offset={} end_offset={} aux_data_offset={} size={}",
                                                    si,
                                                    s.data_offset,
                                                    s.end_offset,
                                                    s.aux_data_offset,
                                                    s.size
                                                );
                                            }

                                            let mut node_iter = BtreeNodeIter::default();
                                            bch2_btree_node_iter_init_from_start(
                                                &mut node_iter,
                                                n.as_ref(),
                                            );
                                            while bch2_btree_node_iter_peek_all(
                                                &node_iter,
                                                n.as_ref(),
                                            )
                                            .is_some()
                                            {
                                                let (rk, rv) = n.read_packed_entry(
                                                    node_iter.data[0].k as usize * 8,
                                                );
                                                let va = unsafe {
                                                    std::ptr::addr_of!(rk.vaddr).read_unaligned()
                                                };
                                                let si = unsafe {
                                                    std::ptr::addr_of!(rk.snapshot_id)
                                                        .read_unaligned()
                                                };
                                                eprintln!(
                                                    "    entry={} key=({},{}) value=addr({})",
                                                    node_iter.data[0].k,
                                                    va,
                                                    si,
                                                    rv.paddr()
                                                );
                                                bch2_btree_node_iter_advance(
                                                    &mut node_iter,
                                                    n.as_ref(),
                                                );
                                            }
                                        }
                                    }
                                    None => eprintln!("Path[{}] addr={} NOT IN CACHE!", pi, paddr),
                                }
                            }
                            // Dump leaf content
                            if let Some(leaf_addr) = leaf_addr {
                                match b.cache().get(leaf_addr) {
                                    Some(leaf) => {
                                        eprintln!(
                                            "Leaf addr={} key_count={}:",
                                            leaf_addr, leaf.packed_keys + leaf.unpacked_keys
                                        );
                                        let mut node_iter = BtreeNodeIter::default();
                                        bch2_btree_node_iter_init_from_start(
                                            &mut node_iter,
                                            leaf.as_ref(),
                                        );
                                        while bch2_btree_node_iter_peek_all(
                                            &node_iter,
                                            leaf.as_ref(),
                                        )
                                        .is_some()
                                        {
                                            let (lk, _lv) = leaf.read_packed_entry(
                                                node_iter.data[0].k as usize * 8,
                                            );
                                            let va = unsafe {
                                                std::ptr::addr_of!(lk.vaddr).read_unaligned()
                                            };
                                            let si = unsafe {
                                                std::ptr::addr_of!(lk.snapshot_id).read_unaligned()
                                            };
                                            eprintln!(
                                                "  key=({},{}) type={:?}",
                                                va, si, lk.key_type
                                            );
                                            bch2_btree_node_iter_advance(
                                                &mut node_iter,
                                                leaf.as_ref(),
                                            );
                                        }
                                    }
                                    None => eprintln!("Leaf addr={} NOT IN CACHE!", leaf_addr),
                                }
                            }
                            // Scan ALL leaves for keys 312-319
                            eprintln!("Scanning all cache entries for keys 312-319:");
                            for addr in 0..100u64 {
                                if let Some(n) = b.cache().get(addr) {
                                    if n.level == 0 {
                                        let mut node_iter = BtreeNodeIter::default();
                                        bch2_btree_node_iter_init_from_start(
                                            &mut node_iter,
                                            n.as_ref(),
                                        );
                                        while bch2_btree_node_iter_peek_all(&node_iter, n.as_ref())
                                            .is_some()
                                        {
                                            let (lk, _lv) = n.read_packed_entry(
                                                node_iter.data[0].k as usize * 8,
                                            );
                                            let va = unsafe {
                                                std::ptr::addr_of!(lk.vaddr).read_unaligned()
                                            };
                                            if va >= 312 && va <= 319 {
                                                eprintln!(
                                                    "  FOUND key={} at leaf_addr={}",
                                                    va, addr
                                                );
                                            }
                                            bch2_btree_node_iter_advance(
                                                &mut node_iter,
                                                n.as_ref(),
                                            );
                                        }
                                    }
                                }
                            }
                            // Find which leaf key 312 SHOULD route to, by checking root+level-1 entries
                            eprintln!("Reachability for keys around 312:");
                            for seq in [
                                0u64, 250, 300, 310, 311, 312, 313, 314, 315, 316, 320, 350, 400,
                                4999,
                            ] {
                                let found =
                                    b.bch2_btree_iter_peek(&BtreeKey::new(seq, 1, KeyType::Normal));
                                eprintln!(
                                    "  key {} -> {}",
                                    seq,
                                    if found.is_some() { "OK" } else { "MISSING" }
                                );
                            }
                            panic!("key {} unreachable at i={}", ck, i);
                        }
                    }
                }
            }
        }
        // Bypass the rest — bulk delete after cascade
        // CRC extent value 28B: total=700, keep=250 (keys 0..249 already alive)
        let keep = 250u64;
        for i in 400..total {
            assert!(
                futures::executor::block_on(b.bch2_btree_delete(
                    &NoopWriter,
                    &BtreeKey::new(i, 1, KeyType::Normal),
                    0
                ))
                .unwrap(),
                "delete failed at i={}",
                i
            );
            if i < 500 {
                assert!(
                    b.bch2_btree_iter_peek(&BtreeKey::new(500, 1, KeyType::Normal))
                        .is_some(),
                    "key 500 became unreachable after deleting {}",
                    i
                );
            }
        }

        // cascade merge + collapse_root 后深度应 ≤ 3
        // (简化版 bch2_foreground_maybe_merge 仅合并 leaf，内部节点通过 root collapse 缩减)
        assert!(
            b.depth() <= 3,
            "depth should be <= 3 after cascade collapse (got {})",
            b.depth()
        );

        // 验证剩余 key 全部可达
        for i in 0..keep {
            let found = b.bch2_btree_iter_peek(&BtreeKey::new(i, 1, KeyType::Normal));
            assert!(
                found.is_some(),
                "key {} should be reachable after cascade collapse (depth={})",
                i,
                b.depth()
            );
            assert_eq!(found.unwrap().1, BchVal::new(i * 10, 0));
        }

        // 验证已删除 key 不可达
        for i in keep..(keep + 50).min(total) {
            assert!(
                b.bch2_btree_iter_peek(&BtreeKey::new(i, 1, KeyType::Normal))
                    .is_none(),
                "deleted key {} should not exist after cascade",
                i
            );
        }
    }

    // ─── load_root 测试 ──────────────────────────────────────────

    #[tokio::test]
    async fn test_btree_load_root_from_backend() {
        let vol = Arc::new(BchVol::test_trees());
        let backend = vol
            .primary_device_rcu_noerror()
            .expect("test volume primary device")
            .bdev()
            .clone();
        let btree = vol.btree(BtreeId::Extents);
        btree.set_vol_ref(&vol);

        let mut node = BtreeNode::new_leaf();
        node.insert(BtreeKey::new(10, 1, KeyType::Normal), BchVal::new(100, 0));
        node.insert(BtreeKey::new(20, 1, KeyType::Normal), BchVal::new(200, 0));
        node.compact();

        let data = node.serialize_to_bucket(100).unwrap();
        backend
            .write_block(BlockAddr::new(100), &data)
            .await
            .unwrap();

        let original_ptr = Arc::as_ptr(&btree.root().node);
        btree.bch2_btree_root_read(100, None).await.unwrap();
        let new_ptr = Arc::as_ptr(&btree.root().node);

        assert_eq!(btree.depth(), 0);
        assert_ne!(original_ptr, new_ptr, "root node should be replaced");
        assert!(btree
            .bch2_btree_iter_peek(&BtreeKey::new(10, 1, KeyType::Normal))
            .is_some());
        assert!(btree
            .bch2_btree_iter_peek(&BtreeKey::new(20, 1, KeyType::Normal))
            .is_some());
    }

    #[tokio::test]
    async fn test_btree_load_root_corrupt() {
        let vol = Arc::new(BchVol::test_trees());
        let backend = vol
            .primary_device_rcu_noerror()
            .expect("test volume primary device")
            .bdev()
            .clone();
        let btree = vol.btree(BtreeId::Extents);
        btree.set_vol_ref(&vol);

        backend
            .write_block(BlockAddr::new(999), &[0xFF; 64])
            .await
            .unwrap();
        let result = btree.bch2_btree_root_read(999, None).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_btree_load_root_skip_zero() {
        let vol = Arc::new(BchVol::test_trees());
        let btree = vol.btree(BtreeId::Extents);
        btree.set_vol_ref(&vol);

        let result = btree.bch2_btree_root_read(0, None).await;
        assert!(result.is_ok());
        assert_eq!(btree.depth(), 0);
    }

    #[tokio::test]
    async fn test_btree_load_root_respects_explicit_level() {
        let vol = Arc::new(BchVol::test_trees());
        let backend = vol
            .primary_device_rcu_noerror()
            .expect("test volume primary device")
            .bdev()
            .clone();
        let btree = vol.btree(BtreeId::Extents);
        btree.set_vol_ref(&vol);

        let mut node = BtreeNode::new_leaf();
        node.insert(BtreeKey::new(10, 1, KeyType::Normal), BchVal::new(100, 0));
        node.compact();

        let data = node.serialize_to_bucket(100).unwrap();
        backend
            .write_block(BlockAddr::new(100), &data)
            .await
            .unwrap();

        btree.bch2_btree_root_read(100, Some(2)).await.unwrap();
        assert_eq!(btree.depth(), 2);
        assert_eq!(btree.root().node.level, 2);
    }
}
