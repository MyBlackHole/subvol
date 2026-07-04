//! Journal reclaim 子系统 — bcachefs `reclaim.c` + `reclaim.h` 对齐
//!
//! ## 职责
//!
//! - `JournalEntryPin` / `JournalEntryPinList` 数据结构
//! - `PinListFifo`（替换旧 `PinFifo`）
//! - 核心 pin API：`pin_set` / `pin_drop` / `pin_put` / `pin_add` / `pin_update` / `pin_copy`
//! - Flush 集成：`flush_pins` / `flush_done` / `get_next_pin`
//!
//! ## bcachefs 对齐
//!
//! | subvolmount | bcachefs 文件 |
//! |----------|--------------|
//! | `JournalEntryPin` | `fs/journal/types.h:128-132` |
//! | `JournalEntryPinList` | `fs/journal/types.h:110-121` |
//! | `JournalPinType` | `fs/journal/types.h:100-108` |
//! | `PinListFifo` | `fs/journal/reclaim.c` (implicit) |
//! | Core API | `fs/journal/reclaim.c:664-718` |
//! | Flush 循环 | `fs/journal/reclaim.c:774-858` |
//! | Done 检查 | `fs/journal/reclaim.c:1337-1411` |

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use parking_lot::Mutex;

use super::types::{Journal, JournalCycleFlags};
use crate::replicas::BchReplicasEntry;
use crate::types::StorageError;

// ═══════════════════════════════════════════════════════════
// Part 1: Types
// ═══════════════════════════════════════════════════════════

/// flush callback 类型。
///
/// 参数: (journal, pin, seq)
/// 对应 bcachefs `journal_pin_flush_fn` (types.h:125-126)。
pub type JournalPinFlushFn =
    Box<dyn Fn(&Journal, &JournalEntryPin, u64) -> Result<(), StorageError> + Send>;

/// journal pin 类型枚举，用于按类型分离 unflushed 链表。
///
/// bcachefs 在 shutdown 时按 btree level 从叶子到根序化冲刷。
/// 当前暂定精简变体，未来 btree writeback 集成时丰富。
///
/// 对应 bcachefs `enum journal_pin_type` (types.h:100-108)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum JournalPinType {
    Btree3 = 0,
    Btree2 = 1,
    Btree1 = 2,
    Btree0 = 3,
    KeyCache = 4,
    Other = 5,
}

/// pin type 枚举变体数量（6）。
pub const JOURNAL_PIN_TYPE_NR: usize = 6;

/// journal flush 遍历 pin type 的优先级顺序。
///
/// 对应 bcachefs `journal_flush_done()` 中从 `JOURNAL_PIN_TYPE_NR - 1` 到 `0`
/// 的回收顺序：Other → KeyCache → Btree0 → Btree1 → Btree2 → Btree3。
const JOURNAL_PIN_FLUSH_ORDER: [JournalPinType; JOURNAL_PIN_TYPE_NR] = [
    JournalPinType::Other,
    JournalPinType::KeyCache,
    JournalPinType::Btree0,
    JournalPinType::Btree1,
    JournalPinType::Btree2,
    JournalPinType::Btree3,
];

/// 从 pin 元数据派生 pin type。
///
/// 对应 bcachefs `journal_pin_type()` (reclaim.c:564-577)。
/// subvol 直接把分类存到 `JournalEntryPin` 上，避免依赖 closure 身份。
pub fn journal_pin_type(pin: &JournalEntryPin) -> JournalPinType {
    pin.pin_type
}

/// 根据 btree 节点层级返回对应的 journal pin type。
///
// ═══════════════════════════════════════════════════════════
// Part 2: 侵入式链表
// ═══════════════════════════════════════════════════════════

/// JournalEntryPin 的侵入式链表节点。
///
/// 对应 bcachefs `struct list_head`（内核链表）。
/// unsafe 严格封装在此类型内部，对外暴露安全 API。
pub struct Link {
    prev: UnsafeCell<*const JournalEntryPin>,
    next: UnsafeCell<*const JournalEntryPin>,
}

// 侵入式链表是纯指针操作，不涉及并发数据竞争（各 pin 独立存活周期）
unsafe impl Send for Link {}
unsafe impl Sync for Link {}

impl Link {
    pub fn new() -> Self {
        Self {
            prev: UnsafeCell::new(std::ptr::null()),
            next: UnsafeCell::new(std::ptr::null()),
        }
    }

    /// 读取 prev 指针。
    fn read_prev(&self) -> *const JournalEntryPin {
        unsafe { *self.prev.get() }
    }

    /// 读取 next 指针。
    fn read_next(&self) -> *const JournalEntryPin {
        unsafe { *self.next.get() }
    }

    /// 写入 prev 指针。
    fn write_prev(&self, val: *const JournalEntryPin) {
        unsafe {
            *self.prev.get() = val;
        }
    }

    /// 写入 next 指针。
    fn write_next(&self, val: *const JournalEntryPin) {
        unsafe {
            *self.next.get() = val;
        }
    }

    /// 从链表中移除自身。对应 bcachefs `list_del_init`。
    pub fn remove(&self) {
        let prev = self.read_prev();
        let next = self.read_next();
        if !prev.is_null() {
            prev.link().write_next(next);
        }
        if !next.is_null() {
            next.link().write_prev(prev);
        }
        self.write_prev(std::ptr::null());
        self.write_next(std::ptr::null());
    }

    /// 在 `pos` 之后插入自身。对应 bcachefs `list_add`。
    pub fn insert_after(&self, pos: *const JournalEntryPin) {
        let next = pos.link().read_next();
        self.write_prev(pos);
        self.write_next(next);
        pos.link()
            .write_next(self as *const Link as *const JournalEntryPin);
        if !next.is_null() {
            next.link()
                .write_prev(self as *const Link as *const JournalEntryPin);
        }
    }

    /// 追加到链表尾部。对应 bcachefs `list_add_tail`。
    pub fn append_to_tail(&self, tail: *const JournalEntryPin) {
        let prev = tail.link().read_prev();
        self.write_prev(prev);
        self.write_next(tail);
        tail.link()
            .write_prev(self as *const Link as *const JournalEntryPin);
        if !prev.is_null() {
            prev.link()
                .write_next(self as *const Link as *const JournalEntryPin);
        }
    }

    /// 是否已连接到某个链表中。
    fn is_linked(&self) -> bool {
        !self.read_prev().is_null() || !self.read_next().is_null()
    }
}

/// 为 `*const JournalEntryPin` 添加 link 访问辅助方法。
trait PinPtrExt {
    fn link(self) -> &'static Link;
}

impl PinPtrExt for *const JournalEntryPin {
    fn link(self) -> &'static Link {
        unsafe { &(*self).link }
    }
}

/// 侵入式链表的虚拟头节点（不存储 pin 数据）。
///
/// 封装链表操作，对外暴露安全 push/pop/is_empty API。
/// 哨兵节点永不从链表中移除。
pub struct LinkedListHead {
    /// 哨兵 pin（仅用作链表锚点，seq=0，flush=None）
    sentinel: Box<JournalEntryPin>,
}

impl LinkedListHead {
    pub fn new() -> Self {
        let sentinel = Box::new(JournalEntryPin {
            seq: AtomicU64::new(0),
            pin_type: JournalPinType::Other,
            flush: UnsafeCell::new(None),
            link: Link::new(),
        });
        // 哨兵自指向：空链表状态
        sentinel
            .link
            .write_prev(&*sentinel as *const JournalEntryPin);
        sentinel
            .link
            .write_next(&*sentinel as *const JournalEntryPin);
        Self { sentinel }
    }

    /// 链表是否为空（基于哨兵指针比较）。
    pub fn is_empty(&self) -> bool {
        self.sentinel.link.read_next() == &*self.sentinel as *const JournalEntryPin
    }

    /// 追加 pin 到链表尾部。
    /// 对应 bcachefs `list_add_tail`。
    pub fn push_back(&mut self, pin: &JournalEntryPin) {
        pin.link.append_to_tail(&*self.sentinel);
    }

    pub fn pop_front(&mut self) -> Option<&JournalEntryPin> {
        if self.is_empty() {
            return None;
        }
        let first = self.sentinel.link.read_next();
        first.link().remove();
        Some(unsafe { &*first })
    }

    // 对应 bcachefs `list_del`
    pub fn remove_pin(&mut self, pin: &JournalEntryPin) {
        pin.link.remove();
    }

    pub fn front(&self) -> Option<&JournalEntryPin> {
        if self.is_empty() {
            return None;
        }
        let first = self.sentinel.link.read_next();
        Some(unsafe { &*first })
    }

    pub fn iter(&self) -> LinkedListIter {
        LinkedListIter {
            head: &*self.sentinel,
            current: self.sentinel.link.read_next(),
        }
    }
}

pub struct LinkedListIter {
    head: *const JournalEntryPin,
    current: *const JournalEntryPin,
}

impl Iterator for LinkedListIter {
    type Item = *const JournalEntryPin;

    fn next(&mut self) -> Option<Self::Item> {
        if self.current == self.head {
            return None;
        }
        let result = self.current;
        self.current = self.current.link().read_next();
        Some(result)
    }
}

// ═══════════════════════════════════════════════════════════
// Part 3: JournalEntryPin
// ═══════════════════════════════════════════════════════════

/// 外部子系统（btree node / key cache）嵌入的 journal pin。
///
/// 对应 bcachefs `struct journal_entry_pin` (types.h:128-132)。
/// pin 由子系统自身存储（嵌入 btree_node / bkey_cached），不存储在 journal 内部。
///
/// # 注意
///
/// - `link` 必须是第一个字段（侵入式链表指针转换要求 offset=0）。
/// - `seq` 为 AtomicU64 支持 retry loop 中的并发读（READ_ONCE）。
#[repr(C)]
pub struct JournalEntryPin {
    /// 侵入式链表节点 — 链接到 JournalEntryPinList 的 unflushed/flushed 链表。
    pub(crate) link: Link,
    /// 关联的 journal seq（0 = 未激活）。AtomicU64 支持 retry loop 无锁读。
    /// 对应 bcachefs `pin->seq`。
    pub seq: AtomicU64,
    /// journal pin type — 对应 bcachefs `journal_pin_type()` 分类结果。
    pub pin_type: JournalPinType,
    /// flush callback：journal 冲刷此 pin 时调用。
    ///
    /// 对应 bcachefs `pin->flush`（裸函数指针，无锁）。
    /// 用 `UnsafeCell` 嵌入，由 `PinList.lock` 保护写入语义：
    /// - 写入：`journal_pin_set_locked` 中持有 `PinList.lock` 时写入
    /// - 读取：flush 路径读取有 `PinList.lock` 的 happened-before 保证（pin 的入列/出列均在锁内完成）
    /// - 首次注册后不再修改（后续 `flush_fn=None` 仅更新 seq）
    /// 无需额外 Mutex（bcachefs 同无锁）。
    pub flush: UnsafeCell<Option<JournalPinFlushFn>>,
}

impl JournalEntryPin {
    /// 创建新的 journal pin（未激活，seq=0）。
    pub fn new(flush: Option<JournalPinFlushFn>, pin_type: JournalPinType) -> Self {
        Self {
            link: Link::new(),
            seq: AtomicU64::new(0),
            pin_type,
            flush: UnsafeCell::new(flush),
        }
    }

    /// pin 是否已激活（绑定到某个 seq）。
    pub fn is_active(&self) -> bool {
        self.seq.load(Ordering::Relaxed) != 0
    }
}

// Safety: JournalEntryPin 是 Sync 的，因为：
// - `seq: AtomicU64` 和 `pin_type` 天生 Sync
// - `link: Link` 已显式 `unsafe impl Sync`（见上方）
// - `flush: UnsafeCell<Option<JournalPinFlushFn>>` 的写入仅在 `PinList.lock` 下进行，
//   读取有 PinList lock 的 happened-before 保证（pin 入列/出列均在锁内完成），
//   且 flush_fn 仅写入一次（首次注册），后续 `flush_fn=None` 不修改
unsafe impl Sync for JournalEntryPin {}

// ═══════════════════════════════════════════════════════════
// Part 4: JournalDevs & JournalEntryPinList
// ═══════════════════════════════════════════════════════════

/// 设备列表，字段与 bcachefs `pin_list->devs` 对齐。
/// 对应 bcachefs `pin_list->devs` (types.h:116-119)。
#[derive(Debug, Clone)]
pub(crate) struct JournalDevs {
    pub nr: u8,
    pub data: [u8; 16], // BCH_REPLICAS_MAX 对齐
}

impl Default for JournalDevs {
    fn default() -> Self {
        Self {
            nr: 0,
            data: [0; 16],
        }
    }
}

impl JournalDevs {
    pub fn contains(&self, dev: u8) -> bool {
        self.data[..self.nr as usize].contains(&dev)
    }

    pub fn set_from_slice(&mut self, devs: &[u8]) {
        let n = devs.len().min(self.data.len());
        self.data[..n].copy_from_slice(&devs[..n]);
        self.nr = n as u8;
    }

    pub fn clear(&mut self) {
        self.nr = 0;
    }
}

/// 对应本地 `replicas_entry_refs` (`journal/reclaim.h:83-88`)。
pub(crate) struct ReplicasEntryRef {
    pub nr_refs: u32,
    pub replicas: BchReplicasEntry,
}

/// 对应本地预分配 `darray_replicas_entry_refs`。
pub(crate) struct ReplicasEntryRefs {
    pub entries: Vec<ReplicasEntryRef>,
}

impl ReplicasEntryRefs {
    pub fn new() -> Self {
        Self {
            entries: Vec::with_capacity(16),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

/// 每个 journal seq 的 pin 元数据。
///
/// 存储在 Journal 的 `pin_fifo`（PinListFifo）中，由 seq % PIN_FIFO_SIZE 索引。
///
/// 对应 bcachefs `struct journal_entry_pin_list` (types.h:110-121)。
pub(crate) struct JournalEntryPinList {
    /// Per-pin-list 自旋锁（bcachefs 细粒度并发控制）。
    /// 对应 bcachefs `pin_list->lock` (types.h:111)。
    pub lock: Mutex<()>,
    /// 此 seq 上活跃的 pin 数量（atomic）。
    /// 对应 bcachefs `pin_list->count` (types.h:112)。
    pub count: AtomicU32,
    /// 按 pin type 分离的未写 pin 链表（6 种）。
    /// UnsafeCell 实现锁保护下的内部可变性。
    /// 对应 bcachefs `pin_list->unflushed[JOURNAL_PIN_TYPE_NR]` (types.h:113)。
    pub unflushed: UnsafeCell<[LinkedListHead; JOURNAL_PIN_TYPE_NR]>,
    /// 已 flush 完成但未释放的 pin 链表（callback 已调完）。
    /// UnsafeCell 实现锁保护下的内部可变性。
    /// 对应 bcachefs `pin_list->flushed` (types.h:114)。
    pub flushed: UnsafeCell<LinkedListHead>,
    /// 是否尚未 replay。
    /// 对应 bcachefs `pin_list->unreplayed` (types.h:115)。
    pub unreplayed: bool,
    /// 设备列表。
    /// 对应 bcachefs `pin_list->devs` (types.h:116-119)。
    pub devs: JournalDevs,
    /// Dirty bytes 计数。
    /// 对应 bcachefs `pin_list->bytes` (types.h:120)。
    pub bytes: u32,
}

impl JournalEntryPinList {
    /// 创建新的 pin_list，初始 count = 给定值。
    ///
    /// 对应 bcachefs `journal_pin_list_init` (reclaim.h:25-34)。
    pub fn new(count: u32) -> Self {
        // 初始化 6 个 unflushed 空链表
        let unflushed = UnsafeCell::new([
            LinkedListHead::new(),
            LinkedListHead::new(),
            LinkedListHead::new(),
            LinkedListHead::new(),
            LinkedListHead::new(),
            LinkedListHead::new(),
        ]);
        Self {
            lock: Mutex::new(()),
            count: AtomicU32::new(count),
            unflushed,
            flushed: UnsafeCell::new(LinkedListHead::new()),
            unreplayed: false,
            devs: JournalDevs::default(),
            bytes: 0,
        }
    }

    pub fn has_dev(&self, dev: u8) -> bool {
        self.devs.contains(dev)
    }

    pub fn set_devs(&mut self, devs: &[u8]) {
        self.devs.set_from_slice(devs);
    }

    /// 获取 unflushed[type] 的共享引用（仅读，无需锁）。
    pub fn unflushed_ref(&self, ty: JournalPinType) -> &LinkedListHead {
        unsafe { &(*self.unflushed.get())[ty as usize] }
    }

    /// 获取 unflushed[type] 的可变引用（需在 pin_list->lock 保护下调用）。
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn unflushed_mut(&self, ty: JournalPinType) -> &mut LinkedListHead {
        &mut (*self.unflushed.get())[ty as usize]
    }

    /// 获取 flushed 的共享引用（仅读，无需锁）。
    pub fn flushed_ref(&self) -> &LinkedListHead {
        unsafe { &*self.flushed.get() }
    }

    /// 获取 flushed 的可变引用（需在 pin_list->lock 保护下调用）。
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn flushed_mut(&self) -> &mut LinkedListHead {
        &mut *self.flushed.get()
    }
}

/// 对应本地 `journal_pin_devs_to_replicas()` (`journal/reclaim.h:49-57`)。
pub(crate) fn journal_pin_devs_to_replicas(pin: &JournalEntryPinList) -> BchReplicasEntry {
    BchReplicasEntry::new(
        crate::alloc::BchDataType::Journal,
        &pin.devs.data[..pin.devs.nr as usize],
        1,
    )
}

// ═══════════════════════════════════════════════════════════
// Part 5: PinListFifo
// ═══════════════════════════════════════════════════════════

/// PIN_FIFO 大小（128 = 覆盖当前 journal bucket 数，10KB 内存）。
pub const PIN_FIFO_SIZE: usize = 128;

/// JournalEntryPinList 的 FIFO（替换旧 PinFifo）。
///
/// `front/back` 保存绝对 journal seq，底层槽位按 `seq % PIN_FIFO_SIZE` 索引；
/// 空条件为 `front == back`，满条件为 `back - front == PIN_FIFO_SIZE`。
pub(crate) struct PinListFifo {
    pub entries: [Option<JournalEntryPinList>; PIN_FIFO_SIZE],
    pub front: u64,
    pub back: u64,
}

impl PinListFifo {
    pub fn new(seq: u64) -> Self {
        Self {
            entries: std::array::from_fn(|_| None),
            front: seq,
            back: seq,
        }
    }

    /// FIFO 中有效条目数。
    pub fn len(&self) -> usize {
        (self.back - self.front) as usize
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.front == self.back
    }

    /// 是否已满。
    pub fn is_full(&self) -> bool {
        self.len() == PIN_FIFO_SIZE
    }

    /// 前端条目引用。
    pub fn front(&self) -> Option<&JournalEntryPinList> {
        if self.is_empty() {
            return None;
        }
        self.entries[self.front as usize % PIN_FIFO_SIZE].as_ref()
    }

    /// 前端条目可变引用。
    pub fn front_mut(&mut self) -> Option<&mut JournalEntryPinList> {
        if self.is_empty() {
            return None;
        }
        self.entries[self.front as usize % PIN_FIFO_SIZE].as_mut()
    }

    /// 从后端追加条目（如果 FIFO 满则返回错误）。
    pub fn push_back(&mut self, pl: JournalEntryPinList) -> Result<(), JournalEntryPinList> {
        if self.is_full() {
            return Err(pl);
        }
        self.entries[self.back as usize % PIN_FIFO_SIZE] = Some(pl);
        self.back += 1;
        Ok(())
    }

    /// 从前端弹出条目。
    pub fn pop_front(&mut self) -> Option<JournalEntryPinList> {
        if self.is_empty() {
            return None;
        }
        let entry = self.entries[self.front as usize % PIN_FIFO_SIZE].take();
        self.front += 1;
        entry
    }

    /// 获取指定 seq 对应的 pin_list 条目。
    ///
    /// 通过 `seq % PIN_FIFO_SIZE` 索引。返回 None 如果槽位为空或 seq 不匹配。
    pub fn entry_for_seq(&self, seq: u64) -> Option<&JournalEntryPinList> {
        if seq < self.front || seq >= self.back {
            return None;
        }
        self.entries[seq as usize % PIN_FIFO_SIZE].as_ref()
    }

    /// 获取指定 seq 对应的 pin_list 的可变引用。
    pub fn entry_for_seq_mut(&mut self, seq: u64) -> Option<&mut JournalEntryPinList> {
        if seq < self.front || seq >= self.back {
            return None;
        }
        self.entries[seq as usize % PIN_FIFO_SIZE].as_mut()
    }
}

// ═══════════════════════════════════════════════════════════
// Part 6: Pin active / seq helpers
// ═══════════════════════════════════════════════════════════

/// 检查 pin 是否活跃（seq != 0）。
///
/// 对应 bcachefs `journal_pin_active` (reclaim.h:67-69)。
pub fn journal_pin_active(pin: &JournalEntryPin) -> bool {
    pin.seq.load(Ordering::Relaxed) != 0
}

// ═══════════════════════════════════════════════════════════
// Part 7: 核心 Pin API
// ═══════════════════════════════════════════════════════════

impl Journal {
    // ═══════════════════════════════════════════════════════════
    // 私有辅助
    // ═══════════════════════════════════════════════════════════

    /// 对应本地 `bch2_journal_update_last_seq_ondisk()`
    /// (`journal/reclaim.c:454-489`)。
    pub(crate) fn bch2_journal_update_last_seq_ondisk(
        &self,
        last_seq_ondisk: u64,
        refs: &mut ReplicasEntryRefs,
    ) -> i32 {
        let pin_fifo = unsafe { &mut *self.pin_fifo.get() };
        assert!(last_seq_ondisk <= pin_fifo.back);

        let mut seq = self.last_seq_ondisk.load(Ordering::Acquire);
        while seq < last_seq_ondisk {
            let pin_list = pin_fifo
                .entry_for_seq_mut(seq)
                .unwrap_or_else(|| panic!("journal_seq_pin: seq {seq} out of range"));

            if pin_list.devs.nr != 0 {
                let replicas = journal_pin_devs_to_replicas(pin_list);
                if let Some(entry) = refs
                    .entries
                    .iter_mut()
                    .find(|entry| entry.replicas == replicas)
                {
                    entry.nr_refs += 1;
                } else {
                    if refs.entries.try_reserve(1).is_err() {
                        return -12;
                    }
                    refs.entries.push(ReplicasEntryRef {
                        nr_refs: 1,
                        replicas,
                    });
                }
                pin_list.devs.clear();
            }

            let dirty_entry_bytes = self.dirty_entry_bytes.load(Ordering::Acquire);
            if dirty_entry_bytes < u64::from(pin_list.bytes) {
                pin_list.bytes = dirty_entry_bytes as u32;
            }
            self.dirty_entry_bytes
                .fetch_sub(u64::from(pin_list.bytes), Ordering::AcqRel);
            pin_list.bytes = 0;
            seq += 1;
        }

        0
    }

    /// bcachefs 对齐: bch2_journal_maybe_update_last_seq
    ///
    /// 检查从 `last_seq` 开始的 pin list 是否已完全释放（count==0），
    /// 仅推进内存回收边界 `last_seq`；FIFO front 与 dirty bytes 由写完成路径处理。
    ///
    /// 对应 bcachefs `bch2_journal_maybe_update_last_seq()` (reclaim.c:1069-1086)。
    pub fn bch2_journal_maybe_update_last_seq(&self) {
        let old = self.last_seq.load(Ordering::Acquire);
        let mut seq = old;
        let seq_ondisk = self.seq_ondisk.load(Ordering::Acquire);
        let pin_fifo = self.pin_fifo_ref();

        while seq < pin_fifo.back && seq <= seq_ondisk {
            let pin_list = pin_fifo
                .entry_for_seq(seq)
                .unwrap_or_else(|| panic!("journal_seq_pin: seq {seq} out of range"));
            if pin_list.count.load(Ordering::Acquire) != 0 {
                break;
            }
            seq += 1;
        }

        if old != seq {
            self.last_seq.store(seq, Ordering::Release);
            self.reclaim_flush_wait.notify_all();
        }
    }

    /// 获取 pin_fifo 的共享引用（通过 UnsafeCell 安全读取）。
    /// push_back 只在 journal_entry_open 中发生（被 journal 生命周期序列化），
    /// 其余所有路径（retry loop、pin_put）均为只读。
    pub(crate) fn pin_fifo_ref(&self) -> &PinListFifo {
        unsafe { &*self.pin_fifo.get() }
    }

    /// 获取指定 seq 的 pin_list（假设 seq 有效——在 retry loop 中保证）。
    /// 对应 bcachefs `journal_seq_pin()` (reclaim.h:72)
    fn journal_seq_pin(&self, seq: u64) -> &JournalEntryPinList {
        self.pin_fifo_ref()
            .entry_for_seq(seq)
            .unwrap_or_else(|| panic!("journal_seq_pin: seq {} out of range", seq))
    }

    /// 获取指定 seq 的 pin_list（seq=0 时返回 None）。
    /// 对应 bcachefs `maybe_seq_pin()` (reclaim.c:610)
    fn maybe_seq_pin(&self, seq: u64) -> Option<&JournalEntryPinList> {
        if seq == 0 {
            None
        } else {
            self.pin_fifo_ref().entry_for_seq(seq)
        }
    }

    // ═══════════════════════════════════════════════════════════
    // journal_pin_drop_locked — 锁内 pin 移除
    // ═══════════════════════════════════════════════════════════

    /// 从 pin_list 中移除 pin，检查 flush_in_progress，递减 count。
    ///
    /// # Safety
    ///
    /// - 必须持有 pin_l->lock（如果 pin_l 非 None）。
    /// - 返回 `true` 表示 count 归零且此 pin_l == last_seq 的 pin_list（可触发 last_seq 更新）。
    ///
    /// 对应 bcachefs `journal_pin_drop_locked()` (reclaim.c:512-536)。
    unsafe fn journal_pin_drop_locked(
        &self,
        pin_l: Option<&JournalEntryPinList>,
        pin: &JournalEntryPin,
    ) -> bool {
        let pin_l = match pin_l {
            Some(pl) => pl,
            None => return false,
        };

        if !journal_pin_active(pin) {
            return false;
        }

        // 检查是否正在被 flush（仅用于设置 flush_in_progress_dropped）
        let pin_addr = pin as *const JournalEntryPin as u64;
        if self.flush_in_progress.load(Ordering::Acquire) == pin_addr {
            self.flush_in_progress_dropped
                .store(true, Ordering::Release);
        }

        // 指针解链 — 等价于 bcachefs 的 list_del。
        // bcachefs 的 pin_drop 无条件使用 list_del，不关心
        // pin 当前在 unflushed 还是 flushed 列表。
        // 不维护 LinkedListHead::per-list count，以对齐
        // bcachefs（它没有 per-list count）。
        pin.link.remove();

        // bcachefs reclaim.c:528-529: 如果 reclaim_flush_wait 有等待者，唤醒它。
        // 当 unpin 可能使 journal_next_bucket 成功时，等待 reclaim 的线程需要被唤醒。
        // 使用 Condvar::notify_all() 等价于 __closure_wake_up (无等待者时为 no-op)。
        self.reclaim_flush_wait.notify_all();

        // 递减 count，检查是否需要更新 last_seq（bcachefs 用 j->last_seq 比较）
        let old_count = pin_l.count.fetch_sub(1, Ordering::AcqRel);
        old_count == 1
            && self
                .pin_fifo_ref()
                .entry_for_seq(self.last_seq.load(Ordering::Acquire))
                .map_or(false, |last_l| std::ptr::eq(pin_l, last_l))
    }

    // ═══════════════════════════════════════════════════════════
    // bch2_journal_pin_drop — 释放 pin
    // ═══════════════════════════════════════════════════════════

    /// 释放/撤销 pin（bcachefs 对齐版）。
    ///
    /// 对应 bcachefs `bch2_journal_pin_drop()` (reclaim.c:538-562)。
    /// 接受 `&JournalEntryPin`（侵入式 pin），非旧版 u64 seq。
    pub fn bch2_journal_pin_drop(&self, pin: &JournalEntryPin) {
        loop {
            let seq = pin.seq.load(Ordering::Acquire);
            if seq == 0 {
                break;
            }

            let pin_l = self.journal_seq_pin(seq);
            let guard = pin_l.lock.lock();
            // 获取锁后重新验证 seq
            if pin.seq.load(Ordering::Relaxed) != seq {
                drop(guard);
                continue;
            }

            let reclaim = unsafe { self.journal_pin_drop_locked(Some(pin_l), pin) };
            pin.seq.store(0, Ordering::Release);
            drop(guard);

            if reclaim {
                self.bch2_journal_maybe_update_last_seq();
            }
            break;
        }
    }

    // ═══════════════════════════════════════════════════════════
    // bch2_journal_pin_set_locked — 锁内 pin 设置
    // ═══════════════════════════════════════════════════════════

    /// 在持有 old_l->lock + new_l->lock 的条件下设置 pin。
    ///
    /// # Safety
    ///
    /// - old_l 和 new_l 的锁必须由调用者持有。
    /// - 返回 `true` 表示可能触发了 last_seq 更新。
    ///
    /// 对应 bcachefs `bch2_journal_pin_set_locked()` (reclaim.c:579-608)。
    unsafe fn journal_pin_set_locked(
        &self,
        old_l: Option<&JournalEntryPinList>,
        new_l: &JournalEntryPinList,
        pin: &JournalEntryPin,
        seq: u64,
        flush_fn: Option<JournalPinFlushFn>,
    ) -> bool {
        // 从旧列表中移除（如果 pin 已激活）
        let reclaim = if journal_pin_active(pin) {
            self.journal_pin_drop_locked(old_l, pin)
        } else {
            false
        };

        // 递增新 pin_list 的 count
        new_l.count.fetch_add(1, Ordering::Release);

        // 更新 pin 的 seq
        pin.seq.store(seq, Ordering::Release);
        // 更新 flush callback：传入 Some 时写入 UnsafeCell（受 PinList.lock 保护），
        // 否则保留已有 callback（flush_fn 仅首次注册时写入一次）
        if let Some(flush_fn) = flush_fn {
            unsafe { *pin.flush.get() = Some(flush_fn) };
        }

        let ptype = journal_pin_type(pin);

        // bcachefs reclaim.c:601-603: 若 unflushed[type] 之前为空且有人等待
        // reclaim_flush_wait，则唤醒等待者（新 pin 意味着可能有空间可以回收）。
        // 检查在 push_back 之前做，等价于 bcachefs 的 list_empty 预检查。
        // Condvar::notify_all 在无等待者时是 no-op，故省略 bcachefs 的等待者存在判断。
        if new_l.unflushed_ref(ptype).is_empty() {
            self.reclaim_flush_wait.notify_all();
        }

        // 追加到新 pin_list 的 unflushed[type] 链表
        new_l.unflushed_mut(ptype).push_back(pin);

        reclaim
    }

    // ═══════════════════════════════════════════════════════════
    // bch2_journal_pin_set — 设置/迁移 pin
    // ═══════════════════════════════════════════════════════════

    /// 设置 pin 到指定 seq（若 pin 已绑定到其他 seq，先释放再绑定新 seq）。
    ///
    /// 对应 bcachefs `bch2_journal_pin_set()` (reclaim.c:664-706)。
    /// 接受 `&JournalEntryPin`（侵入式 pin），非旧版 u64 seq。
    pub fn bch2_journal_pin_set(
        &self,
        new_seq: u64,
        pin: &JournalEntryPin,
        flush_fn: Option<JournalPinFlushFn>,
    ) {
        let mut flush_fn = flush_fn;
        loop {
            let old_seq_val = pin.seq.load(Ordering::Acquire);

            let new_l = self.journal_seq_pin(new_seq);
            let old_l = self.maybe_seq_pin(old_seq_val);

            // 按 seq 排序获取锁（防死锁）
            let (_old_guard, _new_guard) = if old_l.is_none() || std::ptr::eq(old_l.unwrap(), new_l)
            {
                (None, Some(new_l.lock.lock()))
            } else if old_seq_val < new_seq {
                let ol = old_l.unwrap();
                let og = ol.lock.lock();
                let ng = new_l.lock.lock();
                (Some(og), Some(ng))
            } else {
                let ng = new_l.lock.lock();
                let ol = old_l.unwrap();
                let og = ol.lock.lock();
                (Some(og), Some(ng))
            };

            // 获取锁后验证无竞态
            let race = old_seq_val != pin.seq.load(Ordering::Relaxed);
            let reclaim = if !race {
                unsafe { self.journal_pin_set_locked(old_l, new_l, pin, new_seq, flush_fn.take()) }
            } else {
                false
            };

            // 锁在 _old_guard, _new_guard drop 时释放

            if !race {
                if reclaim {
                    self.bch2_journal_maybe_update_last_seq();
                }
                break;
            }
        }
    }

    // ═══════════════════════════════════════════════════════════
    // bch2_journal_pin_copy — 将 src pin 复制到 dst pin
    // ═══════════════════════════════════════════════════════════

    /// 将 src pin 复制到 dst（内部用 `pin_set_locked` 实现）。
    ///
    /// 对应 bcachefs `bch2_journal_pin_copy()` (reclaim.c:615-662)。
    pub fn bch2_journal_pin_copy(
        &self,
        dst: &JournalEntryPin,
        src: &JournalEntryPin,
        flush_fn: Option<JournalPinFlushFn>,
    ) {
        let mut flush_fn = flush_fn;
        loop {
            let src_seq = src.seq.load(Ordering::Acquire);
            let dst_seq = dst.seq.load(Ordering::Acquire);

            if src_seq == 0 {
                break;
            }

            let src_l = self.journal_seq_pin(src_seq);
            let dst_l = self.maybe_seq_pin(dst_seq);

            // 按 seq 排序获取锁
            let (_src_guard, _dst_guard) = if dst_l.is_none() || std::ptr::eq(dst_l.unwrap(), src_l)
            {
                (Some(src_l.lock.lock()), None)
            } else if dst_seq < src_seq {
                let dl = dst_l.unwrap();
                let dg = dl.lock.lock();
                let sg = src_l.lock.lock();
                (Some(sg), Some(dg))
            } else {
                let sg = src_l.lock.lock();
                let dl = dst_l.unwrap();
                let dg = dl.lock.lock();
                (Some(sg), Some(dg))
            };

            let race = src_seq != src.seq.load(Ordering::Relaxed)
                || dst_seq != dst.seq.load(Ordering::Relaxed);
            let reclaim = if !race {
                unsafe { self.journal_pin_set_locked(dst_l, src_l, dst, src_seq, flush_fn.take()) }
            } else {
                false
            };

            if !race {
                if reclaim {
                    self.bch2_journal_maybe_update_last_seq();
                }
                break;
            }
        }
    }

    // ═══════════════════════════════════════════════════════════
    // bch2_journal_pin_add — 条件设置（仅当未激活或 seq 后退）
    // ═══════════════════════════════════════════════════════════

    /// 仅在 pin 未激活或 `pin->seq > seq` 时设置。
    ///
    /// 对应 bcachefs `bch2_journal_pin_add()` (reclaim.h:106-112)。
    pub fn bch2_journal_pin_add(
        &self,
        seq: u64,
        pin: &JournalEntryPin,
        flush_fn: Option<JournalPinFlushFn>,
    ) {
        if !journal_pin_active(pin) || pin.seq.load(Ordering::Relaxed) > seq {
            self.bch2_journal_pin_set(seq, pin, flush_fn);
        }
    }

    // ═══════════════════════════════════════════════════════════
    // bch2_journal_pin_update — 条件设置（仅当未激活或 seq 前进）
    // ═══════════════════════════════════════════════════════════

    /// 仅在 pin 未激活或 `pin->seq < seq` 时更新。
    ///
    /// 对应 bcachefs `bch2_journal_pin_update()` (reclaim.h:119-125)。
    pub fn bch2_journal_pin_update(
        &self,
        seq: u64,
        pin: &JournalEntryPin,
        flush_fn: Option<JournalPinFlushFn>,
    ) {
        if !journal_pin_active(pin) || pin.seq.load(Ordering::Relaxed) < seq {
            self.bch2_journal_pin_set(seq, pin, flush_fn);
        }
    }

    // ═══════════════════════════════════════════════════════════
    // __bch2_journal_pin_put — 递减 pin_list count
    // ═══════════════════════════════════════════════════════════

    /// 递减 seq 对应 pin_list 的引用计数（无锁操作）。
    /// 返回 `true` 如果 count 归零。
    ///
    /// 对应 bcachefs `__bch2_journal_pin_put()` (reclaim.h:93-98)。
    pub(crate) fn __bch2_journal_pin_put(&self, seq: u64) -> bool {
        self.pin_fifo_ref()
            .entry_for_seq(seq)
            .map_or(false, |pl| pl.count.fetch_sub(1, Ordering::AcqRel) == 1)
    }

    // ═══════════════════════════════════════════════════════════
    // journal_get_next_pin — 寻找下一个可冲刷的 pin
    // ═══════════════════════════════════════════════════════════

    /// 遍历整个 pin FIFO（从 front 到 back），寻找第一个有 flush callback 的 pin。
    ///
    /// 对应 bcachefs `journal_get_next_pin()` (reclaim.c:730-771)。
    /// bcachefs 使用 `fifo_for_each_entry_ptr(pin_list, &j->pin, *seq)` 从 `j->pin.front`
    /// 开始遍历所有条目，而非仅从 `last_seq` 开始。这对齐确保我们不会遗漏仍然需要
    /// flush 的早期条目（例如 unreplayed entries 之前的 seq）。
    ///
    /// # 参数
    ///
    /// - `seq_to_flush`: flush 的最大 seq
    /// - `allowed_below_seq`: 对 `≤ seq_to_flush` 的条目，只包含此位掩码指定的 pin type
    /// - `allowed_above_seq`: 对 `> seq_to_flush` 的条目，只包含此位掩码指定的 pin type
    ///
    /// 返回 `(pin_ref, seq)`:
    /// - `pin_ref`: 目标 pin 的不可变引用（flush 期间未从链表移除）
    /// - `seq`: pin 绑定的 journal seq
    ///
    /// 设置 `flush_in_progress = pin`，`flush_in_progress_dropped = false`。
    ///
    /// # 对齐修复
    ///
    /// 迭代起点从 `last_seq` 修正为 `pin_fifo.front`（对齐 bcachefs `fifo_for_each_entry_ptr`
    /// 的行为），确保未释放但 count=0 的早期条目也不会被跳过。
    fn journal_get_next_pin(
        &self,
        seq_to_flush: u64,
        allowed_below_seq: u32,
        allowed_above_seq: u32,
    ) -> Option<(&JournalEntryPin, u64)> {
        let pin_fifo = self.pin_fifo_ref();
        let start_seq = pin_fifo.front;
        let end_seq = pin_fifo.back;

        for seq in start_seq..end_seq {
            // bcachefs reclaim.c:752: seq > seq_to_flush 时 break
            if seq > seq_to_flush && allowed_above_seq == 0 {
                break;
            }

            let pin_list = match pin_fifo.entry_for_seq(seq) {
                Some(pl) => pl,
                None => continue,
            };

            let _guard = pin_list.lock.lock();

            if pin_list.unreplayed {
                // bcachefs reclaim.c:749-750: 不能超过 journal replay 进度（避免死锁）
                break;
            }

            for &ty in &JOURNAL_PIN_FLUSH_ORDER {
                // 按 pin type 位掩码过滤 — 对应 bcachefs reclaim.c:756-757
                let bit = 1u32 << (ty as u8);
                let eligible = if seq <= seq_to_flush {
                    (bit & allowed_below_seq) != 0
                } else {
                    (bit & allowed_above_seq) != 0
                };
                if !eligible {
                    continue;
                }

                let head = pin_list.unflushed_ref(ty);
                if head.is_empty() {
                    continue;
                }

                let pin_ptr: *const JournalEntryPin = head
                    .front()
                    .map(|p| p as *const _)
                    .expect("head is_empty 检查通过");
                let pin_ref = unsafe { &*pin_ptr };

                // bcachefs reclaim.c:761: BUG_ON(j->flush_in_progress)
                debug_assert!(
                    self.flush_in_progress.load(Ordering::Acquire) == 0,
                    "journal_get_next_pin: flush_in_progress must be NULL before setting new pin"
                );

                if unsafe { (*pin_ref.flush.get()).is_none() } {
                    continue; // 自钉等无 callback 的 pin，不触发 flush
                }

                self.flush_in_progress
                    .store(pin_ptr as u64, Ordering::Release);
                self.flush_in_progress_dropped
                    .store(false, Ordering::Release);

                drop(_guard);

                return Some((pin_ref, seq));
            }
        }

        None
    }

    // ═══════════════════════════════════════════════════════════
    // journal_flush_pins — 主 flush 循环（内部 worker）
    // ═══════════════════════════════════════════════════════════

    /// 遍历 pin FIFO，flush 所有 ≤ seq_to_flush 的 pin（内部 worker）。
    ///
    /// 对应 bcachefs `journal_flush_pins()` (reclaim.c:774-849) 的精简实现，
    /// 无类型分离（单链表遍历），无阻塞等待。
    ///
    /// # 锁约束
    ///
    /// bcachefs 要求调用者持有 `reclaim_lock`（`lockdep_assert_held`）。
    /// subvol 使用 per-pin-list 细粒度锁保护链表操作，因此不强制调用者
    /// 持有 `reclaim_lock`。`__bch2_journal_reclaim` 内部调用时持有
    /// `reclaim_lock`（防止并发 reclaim），外部 `bch2_journal_flush_pins`
    /// (types.rs) 无锁调用仍是安全的。
    ///
    /// 循环：
    /// 1. `journal_get_next_pin()` 获取下一个待 flush 的 pin
    /// 2. 调用 `flush_fn(j, pin, seq)`（callback 执行）
    /// 3. 回调后：上锁 pin_list → 若 `!flush_in_progress_dropped` 则 `list_move` 到 flushed → 清空 `flush_in_progress`
    /// 4. `wake_up(&pin_flush_wait)` 通知等待者
    ///
    /// 返回 `Ok(flushed_count)`，`flushed_count > 0` 表示至少执行了一次 flush callback。
    /// 调用者处理 loop 逻辑；外部阻塞等待请使用 `bch2_journal_flush_pins` wrapper。
    /// 若 callback 返回错误，立即终止并传播错误（对齐 bcachefs reclaim.c:807-811）。
    pub fn journal_flush_pins(
        &self,
        seq_to_flush: u64,
        allowed_below_seq: u32,
        allowed_above_seq: u32,
    ) -> Result<u32, StorageError> {
        let mut nr_flushed: u32 = 0;

        loop {
            let (pin, seq) =
                match self.journal_get_next_pin(seq_to_flush, allowed_below_seq, allowed_above_seq)
                {
                    Some(result) => result,
                    None => break,
                };

            // 调用 flush callback，先存结果（后续 cleanup 完成后统一传播）
            let cb_result: Result<(), StorageError> = {
                // SAFETY: Pin 的 flush_fn 在 `PinList.lock` 下首次注册，后续不再修改。
                // 当前 pin 已从 unflushed 链表弹出（持有 happened-before），
                // 不会出现并发写入 flush_fn 的情况。
                // 详见 `JournalEntryPin` 的 `unsafe impl Sync` 文档。
                let flush_guard = unsafe { &*pin.flush.get() };
                if let Some(ref flush_fn) = flush_guard {
                    flush_fn(self, pin, seq)
                } else {
                    Ok(())
                }
            };

            // 回调后处理：ALWAYS 执行（即使 callback 返回 Err）
            // 防止 flush_in_progress 泄漏而导致 bch2_journal_pin_flush 死等。
            // 对应 bcachefs reclaim.c:807-811 — C 中 callback 为 void 无错误返回，
            // subvol 的 Result 扩展需要 cleanup 优先于错误传播。
            if let Some(pin_list) = self.pin_fifo_ref().entry_for_seq(seq) {
                let _guard = pin_list.lock.lock();

                let ptype = journal_pin_type(pin);

                if !self.flush_in_progress_dropped.load(Ordering::Acquire) {
                    // pin 未被并发 drop → 从 unflushed 移到 flushed
                    unsafe {
                        pin_list.unflushed_mut(ptype).remove_pin(pin);
                        pin_list.flushed_mut().push_back(pin);
                    }
                }
                // 若 flush_in_progress_dropped == true，pin 已由 pin_drop 移除

                self.flush_in_progress.store(0, Ordering::Release);
                self.flush_in_progress_dropped
                    .store(false, Ordering::Release);
            }

            // 唤醒等待 pin_flush_wait 的线程
            let guard = self.pin_flush_lock.lock().unwrap();
            self.pin_flush_wait.notify_all();
            drop(guard);

            // 传播 callback 错误（cleanup 完成后才传播，防止 leak）
            cb_result?;

            nr_flushed += 1;
        }

        Ok(nr_flushed)
    }

    // ═══════════════════════════════════════════════════════════
    // bch2_journal_pin_flush — 等待 pin flush 完成
    // ═══════════════════════════════════════════════════════════

    /// 等待 `flush_in_progress != pin`（即 pin 不再被 flush）。
    /// 调用前 pin 必须已不活跃（seq=0）。
    ///
    /// 对应 bcachefs `bch2_journal_pin_flush()` (reclaim.c:713-718)。
    pub fn bch2_journal_pin_flush(&self, pin: &JournalEntryPin) {
        debug_assert!(
            !journal_pin_active(pin),
            "bch2_journal_pin_flush: pin must not be active"
        );

        let pin_addr = pin as *const JournalEntryPin as u64;
        let mut guard = self.pin_flush_lock.lock().unwrap();
        while self.flush_in_progress.load(Ordering::Acquire) == pin_addr {
            guard = self.pin_flush_wait.wait(guard).unwrap();
        }
    }

    // ═══════════════════════════════════════════════════════════
    // journal_reclaim_kick — 唤醒后台回收线程
    // ═══════════════════════════════════════════════════════════

    /// 设置 `reclaim_kicked` 标志，通知后台回收线程立即执行回收。
    /// 后台线程将在下一次循环迭代中检查此标志并跳过睡眠等待。
    ///
    /// 对应 bcachefs `journal_reclaim_kick()` (reclaim.h:10-17)。
    pub fn journal_reclaim_kick(&self) {
        self.reclaim_kicked.store(true, Ordering::Release);
        self.reclaim_notify.notify_waiters();
    }

    // ═══════════════════════════════════════════════════════════
    // journal_pins_still_flushing — 检查 pin 是否仍在 flush 中
    // ═══════════════════════════════════════════════════════════

    /// 遍历 pin FIFO，检查是否有任何 pin（unflushed 或 flushed 链表）仍在 flush 中。
    ///
    /// 对应 bcachefs `journal_pins_still_flushing()` (reclaim.c:1307-1335)。
    fn journal_pins_still_flushing(&self, seq_to_flush: u64, types: u32) -> bool {
        let pin_fifo = self.pin_fifo_ref();
        let start = pin_fifo.front;
        let mut end = pin_fifo.back;
        if seq_to_flush != u64::MAX {
            end = end.min(seq_to_flush + 1);
        }
        for seq in start..end {
            let Some(pin_list) = pin_fifo.entry_for_seq(seq) else {
                continue;
            };
            let _guard = pin_list.lock.lock();
            for i in 0..JOURNAL_PIN_TYPE_NR {
                if (1u32 << i) & types != 0
                    && !pin_list
                        .unflushed_ref(unsafe {
                            std::mem::transmute::<u8, JournalPinType>(i as u8)
                        })
                        .is_empty()
                {
                    return true;
                }
            }
            for pin_ptr in pin_list.flushed_ref().iter() {
                let pin = unsafe { &*pin_ptr };
                if (1u32 << (pin.pin_type as u8)) & types != 0 {
                    return true;
                }
            }
        }
        false
    }

    // ═══════════════════════════════════════════════════════════
    // journal_flush_pins_or_still_flushing — 单次 flush + 检查
    // ═══════════════════════════════════════════════════════════

    /// 执行单次 flush pass，然后检查是否仍有 pin 在 flush。
    ///
    /// 对应 bcachefs `journal_flush_pins_or_still_flushing()` (reclaim.c:1337-1342)。
    fn journal_flush_pins_or_still_flushing(&self, seq_to_flush: u64, types: u32) -> bool {
        match self.journal_flush_pins(seq_to_flush, types, 0) {
            Ok(flushed) => flushed > 0 || self.journal_pins_still_flushing(seq_to_flush, types),
            Err(err) => {
                // Rust pin callbacks can report an error where the local C
                // callback is void; publish it through the same journal error
                // gate instead of silently retrying forever.
                self.bch2_journal_error_set(crate::journal::JournalError::Io(err));
                true
            }
        }
    }

    // ═══════════════════════════════════════════════════════════
    // journal_flush_done — flush 完成判定（closure_wait_event 谓词）
    // ═══════════════════════════════════════════════════════════

    /// 判定 flush 是否完成，供 `bch2_journal_flush_pins` 循环等待使用。
    /// 返回 0 = 尚未完成（继续等待），非 0 = 完成。
    ///
    /// 对应 bcachefs `journal_flush_done()` (reclaim.c:1344-1397)。
    fn journal_flush_done(&self, seq_to_flush: u64, did_work: &mut bool) -> i32 {
        if self.bch2_journal_error_check().is_some() {
            return -5;
        }

        let _lock = self.reclaim_lock.lock().unwrap();

        for ty_i in (0..JOURNAL_PIN_TYPE_NR).rev() {
            if self.journal_flush_pins_or_still_flushing(seq_to_flush, 1u32 << ty_i) {
                *did_work = true;
                return 0;
            }
        }

        if seq_to_flush >= self.bch2_journal_cur_seq() {
            let _ = self.bch2_journal_cycle_locked_flags(JournalCycleFlags::MUST_CLOSE);
        }

        let done = !self.replay_done.load(Ordering::Acquire)
            || self.last_seq.load(Ordering::Acquire) > seq_to_flush
            || self.last_seq.load(Ordering::Acquire) == self.pin_fifo_ref().back;
        if done {
            1
        } else {
            0
        }
    }

    // ═══════════════════════════════════════════════════════════
    // bch2_journal_flush_device_pins — 设备级 pin flush
    // ═══════════════════════════════════════════════════════════

    /// 扫描所有 pin 条目，找到指定设备（或不足副本数）的最老 seq，然后 flush。
    /// dev_idx < 0 时检查副本覆盖度，≥0 时检查特定设备。
    ///
    /// 对应 bcachefs `bch2_journal_flush_device_pins()` (reclaim.c:1413-1432)。
    pub fn bch2_journal_flush_device_pins(&self, dev_idx: i32) -> i32 {
        let pin_fifo = self.pin_fifo_ref();
        let metadata_replicas = self
            .vol
            .get()
            .and_then(|w| w.upgrade())
            .map_or(1, |v| v.opts.metadata_replicas);

        let mut seq: u64 = 0;
        for iter in pin_fifo.front..pin_fifo.back {
            let Some(p) = pin_fifo.entry_for_seq(iter) else {
                continue;
            };
            let should_flush = if dev_idx >= 0 {
                p.has_dev(dev_idx as u8)
            } else {
                p.devs.nr < metadata_replicas
            };
            if should_flush {
                seq = iter;
            }
        }

        if seq > 0 {
            let _ = self.bch2_journal_flush_pins(seq);
        }

        if self.bch2_journal_error_check().is_some() {
            return -5;
        }
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 验证 Link 基本操作（new / remove / insert_after）。
    #[test]
    fn test_link_basic() {
        let a = JournalEntryPin::new(None, JournalPinType::Other);
        let b = JournalEntryPin::new(None, JournalPinType::Other);
        let c = JournalEntryPin::new(None, JournalPinType::Other);

        // b insert_after a: a ↔ b
        b.link.insert_after(&a as *const JournalEntryPin);
        assert!(b.link.is_linked());

        // c insert_after b: a ↔ b ↔ c
        c.link.insert_after(&b as *const JournalEntryPin);
        assert!(c.link.is_linked());

        // remove b: a ↔ c
        b.link.remove();
        assert!(!b.link.is_linked());
    }

    #[tokio::test]
    async fn test_journal_reclaim_kick_notifies_waiters() {
        let journal = std::sync::Arc::new(Journal::new(vec![100, 200, 300]));
        let waiter = journal.reclaim_notify.notified();
        journal.journal_reclaim_kick();

        tokio::time::timeout(std::time::Duration::from_millis(50), waiter)
            .await
            .expect("waiter should be notified");
        assert!(journal.reclaim_kicked.load(Ordering::Acquire));
    }

    /// 验证 LinkedListHead push_back / pop_front / is_empty。
    #[test]
    fn test_linked_list_head_basic() {
        let mut list = LinkedListHead::new();
        assert!(list.is_empty());

        let pin = JournalEntryPin::new(None, JournalPinType::Other);
        list.push_back(&pin);
        assert!(!list.is_empty());

        let popped = list.pop_front();
        assert!(popped.is_some());
        assert!(list.is_empty());
    }

    /// 验证 LinkedListHead 多元素 push/pop。
    #[test]
    fn test_linked_list_head_multiple() {
        let mut list = LinkedListHead::new();
        let pins: Vec<JournalEntryPin> = (0..5)
            .map(|_| JournalEntryPin::new(None, JournalPinType::Other))
            .collect();

        for pin in &pins {
            list.push_back(pin);
        }
        let count = 5;
        for _ in 0..count {
            assert!(list.pop_front().is_some());
        }
        assert!(list.is_empty());
    }

    /// 验证 JournalEntryPinList::new 初始化所有 6 个 unflushed 链表为空。
    #[test]
    fn test_pin_list_init() {
        let pl = JournalEntryPinList::new(3);
        assert_eq!(pl.count.load(Ordering::Acquire), 3);
        assert!(!pl.unreplayed);
        assert_eq!(pl.devs.nr, 0);
        assert_eq!(pl.bytes, 0);
        for &ty in &JOURNAL_PIN_FLUSH_ORDER {
            assert!(
                pl.unflushed_ref(ty).is_empty(),
                "unflushed[{ty:?}] should be empty"
            );
        }
        assert!(pl.flushed_ref().is_empty());
    }

    /// 验证 JournalDevs 可按 slice 设置并查询。
    #[test]
    fn test_pin_list_devs_roundtrip() {
        let mut pl = JournalEntryPinList::new(1);
        pl.set_devs(&[2, 4, 8]);
        assert_eq!(pl.devs.nr, 3);
        assert!(pl.has_dev(2));
        assert!(pl.has_dev(4));
        assert!(pl.has_dev(8));
        assert!(!pl.has_dev(1));
        pl.devs.clear();
        assert_eq!(pl.devs.nr, 0);
    }

    /// 验证 PinListFifo 基本 FIFO 语义。
    #[test]
    fn test_pin_list_fifo_basic() {
        let mut fifo = PinListFifo::new(0);
        assert!(fifo.is_empty());
        assert_eq!(fifo.len(), 0);

        let pl = JournalEntryPinList::new(1);
        assert!(fifo.push_back(pl).is_ok());
        assert!(!fifo.is_empty());
        assert_eq!(fifo.len(), 1);

        let popped = fifo.pop_front();
        assert!(popped.is_some());
        assert!(fifo.is_empty());
    }

    /// 验证 PinListFifo full 检测。
    #[test]
    fn test_pin_list_fifo_full() {
        let mut fifo = PinListFifo::new(0);
        for i in 0..PIN_FIFO_SIZE {
            assert!(!fifo.is_full(), "should not be full at i={}", i);
            assert!(fifo.push_back(JournalEntryPinList::new(1)).is_ok());
        }
        assert!(fifo.is_full());
        assert!(fifo.push_back(JournalEntryPinList::new(1)).is_err());
    }

    // ═══════════════════════════════════════════════════════════
    // Step 5: Core Pin API 测试
    // ═══════════════════════════════════════════════════════════

    /// 辅助：创建有 N 个 journal entry 的测试 Journal。
    /// 注意：不能使用 `journal_entry_open()`（private to types.rs），
    /// 直接通过 UnsafeCell 向 pin_fifo 推入条目。
    fn create_test_journal(entry_count: usize) -> Journal {
        let j = Journal::new(vec![100]);
        // new() 已打开 seq 1 并推入 1 个自钉（front=1, back=2）
        // 推入剩余条目
        for _ in 1..entry_count {
            unsafe {
                assert!((*j.pin_fifo.get())
                    .push_back(JournalEntryPinList::new(1))
                    .is_ok());
            }
        }
        j
    }

    /// 5.1: 注册 pin → drop → count==0（pin 活跃状态检查）。
    #[test]
    fn test_pin_set_drop() {
        let j = create_test_journal(2);
        let pin = JournalEntryPin::new(Some(Box::new(|_, _, _| Ok(()))), JournalPinType::Other);

        assert!(!journal_pin_active(&pin));
        j.bch2_journal_pin_set(1, &pin, None);
        assert!(journal_pin_active(&pin));
        assert_eq!(pin.seq.load(Ordering::Acquire), 1);

        let pl = j.pin_fifo_ref().entry_for_seq(1).unwrap();
        assert_eq!(pl.count.load(Ordering::Acquire), 2); // 自钉 + 1

        j.bch2_journal_pin_drop(&pin);
        assert!(!journal_pin_active(&pin));
        assert_eq!(pl.count.load(Ordering::Acquire), 1); // 仅自钉
    }

    /// 5.2: 注册 pin → `__bch2_journal_pin_put` 递减 count。
    #[test]
    fn test_pin_set_put() {
        let j = create_test_journal(2);
        let pin = JournalEntryPin::new(None, JournalPinType::Other);

        j.bch2_journal_pin_set(1, &pin, None);
        let pl = j.pin_fifo_ref().entry_for_seq(1).unwrap();
        assert_eq!(pl.count.load(Ordering::Acquire), 2);

        // 通过 seq 递减 count（模拟 journal write done）
        let zero = j.__bch2_journal_pin_put(1);
        assert!(!zero); // count 2→1, 未归零
        assert_eq!(pl.count.load(Ordering::Acquire), 1);

        // 再掉一次 pin → count 归零
        j.bch2_journal_pin_drop(&pin);
        assert!(!journal_pin_active(&pin));
        assert_eq!(pl.count.load(Ordering::Acquire), 0);
    }

    /// 5.3: 多个 pin 共享同一 seq，count 逐次递减。
    #[test]
    fn test_multi_pin_same_seq() {
        let j = create_test_journal(2);
        let pin1 = JournalEntryPin::new(None, JournalPinType::Other);
        let pin2 = JournalEntryPin::new(None, JournalPinType::Other);
        let pin3 = JournalEntryPin::new(None, JournalPinType::Other);

        j.bch2_journal_pin_set(1, &pin1, None);
        j.bch2_journal_pin_set(1, &pin2, None);
        j.bch2_journal_pin_set(1, &pin3, None);

        let pl = j.pin_fifo_ref().entry_for_seq(1).unwrap();
        // 自钉(1) + 3 外部 = 4
        assert_eq!(pl.count.load(Ordering::Acquire), 4);

        j.bch2_journal_pin_drop(&pin1);
        assert_eq!(pl.count.load(Ordering::Acquire), 3);

        j.bch2_journal_pin_drop(&pin2);
        assert_eq!(pl.count.load(Ordering::Acquire), 2);

        j.bch2_journal_pin_drop(&pin3);
        assert_eq!(pl.count.load(Ordering::Acquire), 1);
    }

    /// 5.4: 从旧 seq 迁移到新 seq，验证链表迁移 + count 变化。
    #[test]
    fn test_pin_update_seq_forward() {
        let j = create_test_journal(3);
        let pin = JournalEntryPin::new(None, JournalPinType::Other);

        j.bch2_journal_pin_set(1, &pin, None);
        assert_eq!(pin.seq.load(Ordering::Acquire), 1);

        let pl1 = j.pin_fifo_ref().entry_for_seq(1).unwrap();
        assert_eq!(pl1.count.load(Ordering::Acquire), 2); // 自钉 + 1

        // 前进到 seq 2
        j.bch2_journal_pin_update(2, &pin, None);
        assert_eq!(pin.seq.load(Ordering::Acquire), 2);

        // 旧 seq count 归位
        assert_eq!(pl1.count.load(Ordering::Acquire), 1);

        // 新 seq count 增加
        let pl2 = j.pin_fifo_ref().entry_for_seq(2).unwrap();
        assert_eq!(pl2.count.load(Ordering::Acquire), 2); // 自钉 + 1
    }

    /// 5.5: 复制 pin（dst 继承 src 的 seq，count 递增）。
    #[test]
    fn test_pin_copy() {
        let j = create_test_journal(2);
        let src = JournalEntryPin::new(None, JournalPinType::Other);
        let dst = JournalEntryPin::new(None, JournalPinType::Other);

        // src 激活在 seq 1
        j.bch2_journal_pin_set(1, &src, None);
        assert_eq!(src.seq.load(Ordering::Acquire), 1);

        // pin_copy 将 src 的 seq 复制给 dst
        // 内部调用 journal_pin_set_locked(dst_l=None, new_l=src_l, pin=dst, seq=src_seq)
        j.bch2_journal_pin_copy(&dst, &src, None);

        // dst 继承 src 的 seq（1），不推入新 seq
        let pl_dst = j.pin_fifo_ref().entry_for_seq(1).unwrap();
        // 自钉(1) + src(1) + dst(1) = 3
        assert_eq!(pl_dst.count.load(Ordering::Acquire), 3);
        assert_eq!(dst.seq.load(Ordering::Acquire), 1);

        // src 仍为 seq 1
        assert_eq!(src.seq.load(Ordering::Acquire), 1);
    }

    /// 5.6: `bch2_journal_pin_add` 条件设置 — 仅当未激活或 seq > 新 seq。
    #[test]
    fn test_pin_add_conditional() {
        let j = create_test_journal(3);
        let pin = JournalEntryPin::new(None, JournalPinType::Other);

        // 未激活 → 应设置
        j.bch2_journal_pin_add(1, &pin, None);
        assert_eq!(pin.seq.load(Ordering::Acquire), 1);

        // 已激活且 pin.seq(1) < 新 seq(2) → add 条件 "pin.seq > seq" 为 false → 不设置
        j.bch2_journal_pin_add(2, &pin, None);
        assert_eq!(pin.seq.load(Ordering::Acquire), 1); // 不变

        // 先前移到 seq 2，再 add seq 1：pin.seq > seq，必须回退。
        j.bch2_journal_pin_set(2, &pin, None);
        j.bch2_journal_pin_add(1, &pin, None);
        assert_eq!(pin.seq.load(Ordering::Acquire), 1);
    }

    /// 5.7: 未激活 pin 的 drop 为无操作。
    #[test]
    fn test_pin_drop_inactive() {
        let j = create_test_journal(1);
        let pin = JournalEntryPin::new(None, JournalPinType::Other);

        assert!(!journal_pin_active(&pin));
        // 不应 panic
        j.bch2_journal_pin_drop(&pin);
        assert!(!journal_pin_active(&pin));
    }

    /// 5.8: flush_in_progress 期间 drop → 设置 dropped 标记。
    #[test]
    fn test_flush_in_progress_dropped() {
        let j = create_test_journal(2);
        let pin = JournalEntryPin::new(None, JournalPinType::Other);

        j.bch2_journal_pin_set(1, &pin, None);
        assert!(journal_pin_active(&pin));

        // 模拟 flush_in_progress = &pin
        let pin_addr = &pin as *const JournalEntryPin as u64;
        j.flush_in_progress.store(pin_addr, Ordering::Release);

        // drop 应检测到 flush_in_progress == pin → 设置 dropped
        assert!(!j.flush_in_progress_dropped.load(Ordering::Acquire));
        j.bch2_journal_pin_drop(&pin);
        assert!(j.flush_in_progress_dropped.load(Ordering::Acquire));
        assert!(!journal_pin_active(&pin));
    }

    /// 5.9: pin_add/pin_update 边界 — seq 不变时不重复设置。
    #[test]
    fn test_pin_update_noop() {
        let j = create_test_journal(2);
        let pin = JournalEntryPin::new(None, JournalPinType::Other);

        // 设置到 seq 1
        j.bch2_journal_pin_set(1, &pin, None);
        let count_before = {
            let pl = j.pin_fifo_ref().entry_for_seq(1).unwrap();
            pl.count.load(Ordering::Acquire)
        };

        // update 到相同 seq → 应不操作（已激活且 pin.seq(1) == seq(1)，条件 pin.seq < seq 为 false）
        j.bch2_journal_pin_update(1, &pin, None);
        let pl = j.pin_fifo_ref().entry_for_seq(1).unwrap();
        assert_eq!(pl.count.load(Ordering::Acquire), count_before);
    }

    // ═══════════════════════════════════════════════════════════
    // Flush 集成测试
    // ═══════════════════════════════════════════════════════════

    use std::sync::atomic::AtomicBool;

    /// flush_pins: callback 被调用 + flushed 链表迁移。
    #[test]
    fn test_flush_pins_callback_and_move() {
        let j = create_test_journal(3);
        let called = std::sync::Arc::new(AtomicBool::new(false));
        let called_clone = called.clone();

        let pin = JournalEntryPin::new(
            Some(Box::new(move |_, _, _| {
                called_clone.store(true, Ordering::Release);
                Ok(())
            })),
            JournalPinType::Other,
        );

        // 注册到 seq 1
        j.bch2_journal_pin_set(1, &pin, None);
        assert!(journal_pin_active(&pin));

        // 验证 flush_pins 前: unflushed[Other] 有一个 pin
        let pl1 = j.pin_fifo_ref().entry_for_seq(1).unwrap();
        assert!(!pl1.unflushed_ref(JournalPinType::Other).is_empty());
        assert!(pl1.flushed_ref().is_empty());

        // flush pins
        let did_work = j.journal_flush_pins(1, !0, 0).unwrap();
        assert!(did_work > 0);
        // callback 应被调用
        assert!(called.load(Ordering::Acquire));

        // pin 应从 unflushed → flushed
        assert!(pl1.unflushed_ref(JournalPinType::Other).is_empty());
        assert!(!pl1.flushed_ref().is_empty());

        // flush_in_progress 应已清空
        assert_eq!(j.flush_in_progress.load(Ordering::Acquire), 0);
        assert!(!j.flush_in_progress_dropped.load(Ordering::Acquire));
    }

    #[test]
    fn test_journal_pin_type_reads_stored_metadata() {
        let pin = JournalEntryPin::new(None, JournalPinType::KeyCache);
        assert_eq!(journal_pin_type(&pin), JournalPinType::KeyCache);
    }

    #[test]
    fn test_flush_pins_prefers_bcachefs_type_order_within_seq() {
        let j = create_test_journal(2);
        let order = std::sync::Arc::new(Mutex::new(Vec::new()));

        let other = {
            let order = order.clone();
            JournalEntryPin::new(
                Some(Box::new(move |_, _, _| {
                    order.lock().push("other");
                    Ok(())
                })),
                JournalPinType::Other,
            )
        };
        let key_cache = {
            let order = order.clone();
            JournalEntryPin::new(
                Some(Box::new(move |_, _, _| {
                    order.lock().push("key_cache");
                    Ok(())
                })),
                JournalPinType::KeyCache,
            )
        };

        j.bch2_journal_pin_set(1, &key_cache, None);
        j.bch2_journal_pin_set(1, &other, None);

        j.journal_flush_pins(1, !0, 0).unwrap();

        let got = order.lock().clone();
        assert_eq!(got, vec!["other", "key_cache"]);
    }

    #[test]
    fn test_flush_pins_orders_key_cache_before_btree_bucket() {
        let j = create_test_journal(2);
        let order = std::sync::Arc::new(Mutex::new(Vec::new()));

        let key_cache = {
            let order = order.clone();
            JournalEntryPin::new(
                Some(Box::new(move |_, _, _| {
                    order.lock().push("key_cache");
                    Ok(())
                })),
                JournalPinType::KeyCache,
            )
        };
        let btree = {
            let order = order.clone();
            JournalEntryPin::new(
                Some(Box::new(move |_, _, _| {
                    order.lock().push("btree2");
                    Ok(())
                })),
                JournalPinType::Btree2,
            )
        };

        j.bch2_journal_pin_set(1, &btree, None);
        j.bch2_journal_pin_set(1, &key_cache, None);

        j.journal_flush_pins(1, !0, 0).unwrap();

        let got = order.lock().clone();
        assert_eq!(got, vec!["key_cache", "btree2"]);
    }

    #[test]
    fn test_flush_pins_routes_key_cache_bucket() {
        let j = create_test_journal(2);
        let pin = JournalEntryPin::new(Some(Box::new(|_, _, _| Ok(()))), JournalPinType::KeyCache);

        j.bch2_journal_pin_set(1, &pin, None);

        let pl = j.pin_fifo_ref().entry_for_seq(1).unwrap();
        assert!(!pl.unflushed_ref(JournalPinType::KeyCache).is_empty());
        assert!(pl.unflushed_ref(JournalPinType::Other).is_empty());

        j.journal_flush_pins(1, !0, 0).unwrap();

        assert!(pl.unflushed_ref(JournalPinType::KeyCache).is_empty());
        assert!(!pl.flushed_ref().is_empty());
    }

    #[test]
    fn test_flush_pins_routes_btree_bucket() {
        let j = create_test_journal(2);
        let pin = JournalEntryPin::new(Some(Box::new(|_, _, _| Ok(()))), JournalPinType::Btree2);

        j.bch2_journal_pin_set(1, &pin, None);

        let pl = j.pin_fifo_ref().entry_for_seq(1).unwrap();
        assert!(!pl.unflushed_ref(JournalPinType::Btree2).is_empty());
        assert!(pl.unflushed_ref(JournalPinType::Other).is_empty());

        j.journal_flush_pins(1, !0, 0).unwrap();

        assert!(pl.unflushed_ref(JournalPinType::Btree2).is_empty());
        assert!(!pl.flushed_ref().is_empty());
    }

    #[test]
    fn test_flush_pins_uses_callback_from_pin_add() {
        let j = create_test_journal(2);
        let called = std::sync::Arc::new(AtomicBool::new(false));
        let called_clone = called.clone();
        let pin = JournalEntryPin::new(None, JournalPinType::KeyCache);

        j.bch2_journal_pin_add(
            1,
            &pin,
            Some(Box::new(move |_, _, _| {
                called_clone.store(true, Ordering::Release);
                Ok(())
            })),
        );

        assert!(journal_pin_active(&pin));
        let pl = j.pin_fifo_ref().entry_for_seq(1).unwrap();
        assert!(!pl.unflushed_ref(JournalPinType::KeyCache).is_empty());

        j.journal_flush_pins(1, !0, 0).unwrap();

        assert!(called.load(Ordering::Acquire));
        assert!(pl.unflushed_ref(JournalPinType::KeyCache).is_empty());
        assert!(!pl.flushed_ref().is_empty());
    }

    /// flush_pins: 多个 seq 各有 pin，全部被 flush。
    #[test]
    fn test_flush_pins_multi_seq() {
        let j = create_test_journal(4);
        let count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));

        let pin1 = {
            let c = count.clone();
            JournalEntryPin::new(
                Some(Box::new(move |_, _, _| {
                    c.fetch_add(1, Ordering::Release);
                    Ok(())
                })),
                JournalPinType::Other,
            )
        };
        let pin2 = {
            let c = count.clone();
            JournalEntryPin::new(
                Some(Box::new(move |_, _, _| {
                    c.fetch_add(1, Ordering::Release);
                    Ok(())
                })),
                JournalPinType::Other,
            )
        };

        // 注册到不同 seq
        j.bch2_journal_pin_set(1, &pin1, None);
        j.bch2_journal_pin_set(2, &pin2, None);

        // flush 全部
        let did_work = j.journal_flush_pins(2, !0, 0).unwrap();
        assert!(did_work > 0);
        assert_eq!(count.load(Ordering::Acquire), 2);
    }

    /// flush_pins: 同一 seq 多个 pin，全部 flush。
    #[test]
    fn test_flush_pins_same_seq_multiple() {
        let j = create_test_journal(2);
        let count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));

        let pin_a = {
            let c = count.clone();
            JournalEntryPin::new(
                Some(Box::new(move |_, _, _| {
                    c.fetch_add(1, Ordering::Release);
                    Ok(())
                })),
                JournalPinType::Other,
            )
        };
        let pin_b = {
            let c = count.clone();
            JournalEntryPin::new(
                Some(Box::new(move |_, _, _| {
                    c.fetch_add(1, Ordering::Release);
                    Ok(())
                })),
                JournalPinType::Other,
            )
        };

        // 两 pin 注册到同一 seq
        j.bch2_journal_pin_set(1, &pin_a, None);
        j.bch2_journal_pin_set(1, &pin_b, None);

        let did_work = j.journal_flush_pins(1, !0, 0).unwrap();
        assert!(did_work > 0);
        assert_eq!(count.load(Ordering::Acquire), 2);
    }

    /// flush_pins: 自钉（无 flush_fn）被跳过，不触发 callback。
    #[test]
    fn test_flush_pins_skips_self_pin() {
        let j = create_test_journal(2);
        // seq 0 的自钉没有 flush_fn，不应被 flush_pins 处理
        let did_work = j.journal_flush_pins(0, !0, 0).unwrap();
        // 不应有工作（自钉无 callback）
        assert_eq!(did_work, 0);
    }

    /// flush_pins: flush_in_progress 和 dropped 标记在 flush_pins 完成后自动清空。
    ///
    /// 对齐 bcachefs `journal_flush_pins()` (reclaim.c:829-848) 的 post-flush cleanup：
    /// `j->flush_in_progress = NULL; j->flush_in_progress_dropped = false;`
    ///
    /// bcachefs 在 `journal_get_next_pin()` 中有 `BUG_ON(j->flush_in_progress)`，
    /// 即调用前 `flush_in_progress` 必须为 NULL。测试需遵守此不变量。
    #[test]
    fn test_flush_pins_resets_dropped_flag() {
        let j = create_test_journal(2);

        // 验证初始状态：flush_in_progress 为 NULL（对齐 bcachefs 不变量）
        assert_eq!(j.flush_in_progress.load(Ordering::Acquire), 0);

        let pin = JournalEntryPin::new(Some(Box::new(|_, _, _| Ok(()))), JournalPinType::Other);
        j.bch2_journal_pin_set(1, &pin, None);

        // flush_pins 内部调用 journal_get_next_pin 时 flush_in_progress 为 NULL，
        // 设置为 pin 地址，flush 完成后清空为 0。验证 post-cleanup 行为。
        j.journal_flush_pins(1, !0, 0).unwrap();

        // flush 完成后标记应已由 journal_flush_pins 的 post-flush cleanup 清空
        assert_eq!(
            j.flush_in_progress.load(Ordering::Acquire),
            0,
            "flush_in_progress must be NULL after journal_flush_pins completes"
        );
        assert!(
            !j.flush_in_progress_dropped.load(Ordering::Acquire),
            "flush_in_progress_dropped must be false after journal_flush_pins completes"
        );
    }

    /// flush_pins: callback 返回错误时不计入成功 flush 数量。
    #[test]
    fn test_flush_pins_error_not_counted_as_success() {
        let j = create_test_journal(2);
        let pin = JournalEntryPin::new(
            Some(Box::new(|_, _, _| {
                Err(StorageError::InvalidArgument("boom".into()))
            })),
            JournalPinType::Other,
        );
        j.bch2_journal_pin_set(1, &pin, None);

        let err = j.journal_flush_pins(1, !0, 0).unwrap_err();
        assert!(matches!(err, StorageError::InvalidArgument(_)));

        let pl = j.pin_fifo_ref().entry_for_seq(1).unwrap();
        assert!(pl.unflushed_ref(JournalPinType::Other).is_empty());
        assert!(!pl.flushed_ref().is_empty());
        assert_eq!(j.flush_in_progress.load(Ordering::Acquire), 0);
        assert!(!j.flush_in_progress_dropped.load(Ordering::Acquire));
    }

    #[test]
    fn test_flush_done_publishes_pin_callback_error() {
        let j = create_test_journal(2);
        let pin = JournalEntryPin::new(
            Some(Box::new(|_, _, _| {
                Err(StorageError::InvalidArgument("boom".into()))
            })),
            JournalPinType::Other,
        );
        j.bch2_journal_pin_set(1, &pin, None);

        let mut did_work = false;
        assert_eq!(j.journal_flush_done(1, &mut did_work), 0);
        assert!(j.bch2_journal_error_check().is_some());
        assert_eq!(j.journal_flush_done(1, &mut did_work), -5);
    }

    /// 5.10: last_seq_ondisk 推进会回收 dirty_entry_bytes。
    #[test]
    fn test_update_last_seq_ondisk_reclaims_dirty_entry_bytes() {
        let j = create_test_journal(2);
        let dirty_bytes = 1024u64;

        {
            let pin_fifo: &mut PinListFifo = unsafe { &mut *j.pin_fifo.get() };
            let front = pin_fifo.front_mut().expect("front pin list should exist");
            front.bytes = dirty_bytes as u32;
            front.count.store(0, Ordering::Release);
        }
        j.dirty_entry_bytes.store(dirty_bytes, Ordering::Release);

        let mut refs = ReplicasEntryRefs::new();
        assert_eq!(j.bch2_journal_update_last_seq_ondisk(2, &mut refs), 0);

        assert_eq!(j.last_seq.load(Ordering::Acquire), 1);
        assert_eq!(j.dirty_entry_bytes.load(Ordering::Acquire), 0);
        assert_eq!(j.pin_fifo_ref().front().unwrap().bytes, 0);
    }

    /// 5.10: last_seq_ondisk 推进时 dirty_entry_bytes 不足应先 clamp，不得下溢。
    #[test]
    fn test_update_last_seq_ondisk_clamps_dirty_entry_bytes() {
        let j = create_test_journal(2);
        let dirty_bytes = 256u64;
        let pin_bytes = 1024u64;

        {
            let pin_fifo: &mut PinListFifo = unsafe { &mut *j.pin_fifo.get() };
            let front = pin_fifo.front_mut().expect("front pin list should exist");
            front.bytes = pin_bytes as u32;
            front.count.store(0, Ordering::Release);
        }
        j.dirty_entry_bytes.store(dirty_bytes, Ordering::Release);

        let mut refs = ReplicasEntryRefs::new();
        assert_eq!(j.bch2_journal_update_last_seq_ondisk(2, &mut refs), 0);

        assert_eq!(j.last_seq.load(Ordering::Acquire), 1);
        assert_eq!(j.dirty_entry_bytes.load(Ordering::Acquire), 0);
        assert_eq!(j.pin_fifo_ref().front().unwrap().bytes, 0);
    }
}
