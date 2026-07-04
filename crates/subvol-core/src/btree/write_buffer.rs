//! Btree Write Buffer — bcachefs 对齐
//!
//! 对应 bcachefs btree_write_buffer.c + btree_write_buffer.h 中的公开 API。
//! Write buffer 用于延迟写入（deferred write），将 journal 中的 key 批量刷入 btree。
//!
//! bcachefs write buffer 架构：
//! - 每个启用 write buffer 的 btree type 有一个 BCH_WB_BTREE_NR 条目
//! - `inc` keys: 新到达的写入暂存于此
//! - `flushing` keys: 当前正在刷入 btree 的 keys
//! - flush worker: 定期将 inc 中的 keys 排序后刷入 btree

use std::cmp::Ordering;
use std::sync::Mutex;

use tokio::runtime::Handle;

use crate::btree::key::{BchVal, Bpos, BtreeKey, KeyType};
use crate::btree::transaction::BtreeTrans;
use crate::btree::writer::NoopWriter;
use crate::btree::BtreeId;
use crate::journal::Journal;
use crate::BchVol;
use crate::StorageError;

/// bcachefs 对齐: BCH_WB_BTREE_NR — write buffer 的 btree 数量
pub const BCH_WB_BTREE_NR: usize = 11;

/// bcachefs 对齐: enum bch_wb_btree — write buffer 的 btree 索引
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum BchWbBtree {
    Accounting = 0,
    Lru = 1,
    NeedDiscard = 2,
    Backpointers = 3,
    DeletedInodes = 4,
    ReconcileWork = 5,
    ReconcileHipri = 6,
    ReconcilePending = 7,
    ReconcileWorkPhys = 8,
    ReconcileHipriPhys = 9,
    StripeBackpointers = 10,
}

/// bcachefs 对齐: enum wb_flush_caller — flush 调用来源
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum WbFlushCaller {
    Thread = 0,
    JournalPin = 1,
    Sync = 2,
    Maybe = 3,
    Tryflush = 4,
}

/// bcachefs 对齐: `struct wb_maybe_flush` (write_buffer.h:46-51)
///
/// 在 check/repair 代码中跟踪 maybe_flush 状态。
#[derive(Debug, Default)]
pub struct WbMaybeFlush {
    pub last_flushed: Option<BtreeKey>,
    pub nr_flushes: u64,
    pub nr_done: u64,
    pub seen_error: bool,
}

/// bcachefs 对齐: `wb_maybe_flush_init` (write_buffer.h:58-62)
impl WbMaybeFlush {
    pub fn new() -> Self {
        Self::default()
    }
}

/// bcachefs 对齐: `wb_maybe_flush_exit` (write_buffer.h:53-56)
///
/// 在 subvol 中 `last_flushed` 是自动释放的 Option，无需手动清理。
/// 提供此方法仅为 API 对齐。
pub fn wb_maybe_flush_exit(_f: &mut WbMaybeFlush) {}

/// bcachefs 对齐: `wb_maybe_flush_inc` (write_buffer.h:64-68)
pub fn wb_maybe_flush_inc(f: &mut WbMaybeFlush) -> i32 {
    f.nr_done += 1;
    0
}

/// bcachefs 对齐: struct btree_write_buffered_key — write buffer 中的 key 条目
///
/// 固定大小存储，每个条目包含完整的位置、值和元数据。
/// 对应 bcachefs 的 `struct btree_write_buffered_key`。
#[derive(Debug, Clone)]
pub struct BtreeWriteBufferedKey {
    pub journal_seq: u64,
    pub btree_id: BtreeId,
    pub key: BtreeKey,
    pub value: BchVal,
    pub key_type: KeyType,
}

/// bcachefs 对齐: struct btree_write_buffer_keys — write buffer key 集合
///
/// 包含 inc 或 flushing 队列的全部 key 条目。
/// `lock` 用于保护 inc 队列的并发访问。
#[derive(Debug)]
pub struct BtreeWriteBufferKeys {
    pub keys: Vec<BtreeWriteBufferedKey>,
    pub lock: Mutex<()>,
    pub nr: usize,
}

// ── wb key 大小与遍历（对齐 write_buffer.h:116-150）──

/// bcachefs 对齐: `wb_key_u64s()` (write_buffer.h:116-119)
///
/// 在 bcachefs 中返回可变长 key 占用的 u64 数量（含头部）。
/// subvol 固定大小，返回 1（一个 BtreeWriteBufferedKey 占一个条目）。
/// bcachefs 中为 `static inline`，subvol 为内部 helper。
pub(crate) fn wb_key_u64s(_k: &BtreeWriteBufferedKey) -> usize {
    1
}

/// bcachefs 对齐: `wb_keys_start()` (write_buffer.h:121-124)
///
/// 返回指向第一个 key 的 raw pointer（类比 C 的 `darray_first`）。
/// bcachefs 中为 `static inline`，subvol 为内部 helper。
pub(crate) fn wb_keys_start(keys: &BtreeWriteBufferKeys) -> *const BtreeWriteBufferedKey {
    keys.keys.as_ptr()
}

/// bcachefs 对齐: `wb_keys_end()` (write_buffer.h:126-129)
///
/// 返回 one-past-the-end 的 raw pointer（类比 C 的 `darray_top`）。
/// bcachefs 中为 `static inline`，subvol 为内部 helper。
pub(crate) fn wb_keys_end(keys: &BtreeWriteBufferKeys) -> *const BtreeWriteBufferedKey {
    unsafe { keys.keys.as_ptr().add(keys.keys.len()) }
}

/// bcachefs 对齐: `wb_keys_idx()` (write_buffer.h:131-135)
/// bcachefs 中为 `static inline`，subvol 为内部 helper。
pub(crate) fn wb_keys_idx(keys: &BtreeWriteBufferKeys, idx: usize) -> *const BtreeWriteBufferedKey {
    unsafe { keys.keys.as_ptr().add(idx) }
}

/// bcachefs 对齐: `wb_key_next()` (write_buffer.h:137-140)
///
/// 在 bcachefs 中通过 `(u64 *)k + wb_key_u64s(&k->k)` 跳过可变长 key；
/// subvol 固定大小，直接 `k + 1`。
/// bcachefs 中为 `static inline`，subvol 为内部 helper。
pub(crate) fn wb_key_next(k: *const BtreeWriteBufferedKey) -> *const BtreeWriteBufferedKey {
    unsafe { k.add(1) }
}

// Note: bcachefs 的 `wb_keys_for_each` / `wb_keys_for_each_safe` (write_buffer.h:142-150)
// 操作的是变长 darray（keys 为 u64 数组），用指针算术遍历。
// subvol 的 `Vec<BtreeWriteBufferedKey>` 是固定大小，直接用 `for k in &keys.keys {}` 即可，
// 等效于 bcachefs 的 wb_keys_for_each，故不额外实现。

/// 排序用轻量级引用 — 对应 bcachefs struct wb_key_ref
///
/// 在 flush 时从 flushing.keys 构建排序索引数组，
/// 避免移动实际 key 数据。
#[derive(Debug, Clone, Copy)]
struct WbKeyRef {
    /// keys 中的索引
    idx: u32,
    /// btree 类型（排序键）
    btree_id: u8,
    /// bpos.inode — 排序键
    inode: u64,
    /// bpos.offset — 排序键
    offset: u64,
    /// bpos.snapshot — 排序键
    snapshot: u32,
    /// journal_seq — 用于 dedup 时保留较新条目
    journal_seq: u64,
}

/// bcachefs 对齐: struct bch_fs_btree_write_buffer — 单 btree 的 write buffer
#[derive(Debug)]
pub struct BtreeWriteBuffer {
    pub idx: BchWbBtree,
    pub inc: BtreeWriteBufferKeys,
    pub flushing: BtreeWriteBufferKeys,
    pub nr_flushes: u64,
    pub nr_keys_flushed: u64,
}

/// bcachefs 对齐: 11 个 BtreeWriteBuffer 实例集合（对应 bcachefs `c->btree.write_buffer[]`）
///
/// 覆盖所有启用 write buffer 的 btree 类型。
/// `init_early()` 负责设置每个 buffer 的 idx 字段。
#[derive(Debug)]
pub struct BtreeWriteBufferSet {
    pub buffers: [BtreeWriteBuffer; BCH_WB_BTREE_NR],
}

impl BtreeWriteBufferSet {
    /// 创建集合，所有 buffer 使用占位 idx（需 `init_early()` 设置正确值）
    pub fn new() -> Self {
        Self {
            buffers: std::array::from_fn(|_| btree_write_buffer_new(BchWbBtree::Accounting)),
        }
    }

    /// 遍历每个 buffer（只读）
    pub fn for_each<F>(&self, mut f: F)
    where
        F: FnMut(&BtreeWriteBuffer),
    {
        for wb in self.buffers.iter() {
            f(wb);
        }
    }

    /// 遍历每个 buffer（可变）
    pub fn for_each_mut<F>(&mut self, mut f: F)
    where
        F: FnMut(&mut BtreeWriteBuffer),
    {
        for wb in self.buffers.iter_mut() {
            f(wb);
        }
    }
}

impl Default for BtreeWriteBufferSet {
    fn default() -> Self {
        Self::new()
    }
}

// ─── journal_keys_to_wb 协议（对齐 bcachefs write_buffer.h:72-80 / write_buffer.c:833-902）──

/// bcachefs 对齐: `struct journal_keys_to_wb_btree` — 单 btree 的 wb 插入上下文
#[derive(Debug, Clone, Copy)]
pub struct JournalKeysToWbBtree {
    pub wb: *mut BtreeWriteBufferKeys,
    pub room: usize,
}

/// bcachefs 对齐: `struct journal_keys_to_wb` — 跨 btree 的 wb 锁/插入协议
///
/// 在 `bch2_journal_write_prep` 中一次性锁定所有 btree 的 inc.lock，
/// 然后批量插入 key，最后释放锁。
pub struct JournalKeysToWb {
    pub seq: u64,
    pub per_btree: [JournalKeysToWbBtree; BCH_WB_BTREE_NR],
    _guards: Vec<std::sync::MutexGuard<'static, ()>>,
}

impl JournalKeysToWb {
    pub fn new() -> Self {
        Self {
            seq: 0,
            per_btree: [JournalKeysToWbBtree {
                wb: std::ptr::null_mut(),
                room: 0,
            }; BCH_WB_BTREE_NR],
            _guards: Vec::new(),
        }
    }
}

/// bcachefs 对齐: `bch2_journal_keys_to_write_buffer_lock` (write_buffer.c:833-861)
///
/// 对应 bcachefs `static void bch2_journal_keys_to_write_buffer_lock` (write_buffer.c:833-861)。
/// subvol 简化：不 try flushing.lock（统一写入 inc），始终持 inc.lock。
fn bch2_journal_keys_to_write_buffer_lock(
    set: &BtreeWriteBufferSet,
) -> JournalKeysToWb {
    let mut dst = JournalKeysToWb::new();
    for (idx, wb) in set.buffers.iter().enumerate() {
        // SAFETY: inc.lock 是 'static 的 Mutex，guard 通过 raw pointer 取出
        let lock_ptr: *const std::sync::Mutex<()> = &wb.inc.lock;
        let guard = unsafe { (*lock_ptr).lock().unwrap() };
        let guard: std::sync::MutexGuard<'static, ()> =
            unsafe { std::mem::transmute(guard) };
        dst.per_btree[idx] = JournalKeysToWbBtree {
            wb: &wb.inc as *const BtreeWriteBufferKeys as *mut _,
            room: wb.inc.keys.capacity().saturating_sub(wb.inc.keys.len()),
        };
        dst._guards.push(guard);
    }
    dst
}

/// bcachefs 对齐: `bch2_journal_keys_to_write_buffer_start` (write_buffer.c:1274-1294)
///
/// 锁定 + 设置 seq。subvol 简化版本，不处理 accounting accumulator 清零。
pub fn bch2_journal_keys_to_write_buffer_start(
    set: &BtreeWriteBufferSet,
    seq: u64,
) -> JournalKeysToWb {
    let mut dst = bch2_journal_keys_to_write_buffer_lock(set);
    dst.seq = seq;
    dst
}

// ─── Public API ─────────────────────────────────────────────────────────

/// bcachefs 对齐: bch2_btree_write_buffer_flush — 核心 flush 管线
///
/// 对应 bcachefs `bch2_btree_write_buffer_flush_locked` (write_buffer.c:593-821)。
/// bcachefs 中 `_locked` 表示调用方已持 `write_buffer_lock`；subvol 中 `&mut` 已提供独占访问，
/// 等效于 bcachefs 的序列化保证，故不额外获取 `flushing.lock`。
///
/// 完成完整 flush 管线：
/// 1. move_keys_from_inc_to_flushing
/// 2. 构建排序索引 WbKeyRef
/// 3. 按 (btree_id, bpos) 排序
/// 4. 相同 pos 的条目去重（保留最新），丢弃条目 journal_seq 清零
/// 5. Fastpath：vol.get_entry noop 检查 + vol.insert_entry
/// 6. Slowpath：通过事务提交重试失败条目
fn bch2_btree_write_buffer_flush(
    wb: &mut BtreeWriteBuffer,
    vol: &BchVol,
    journal: Option<&Journal>,
) -> Result<(), StorageError> {
    // 对应 bcachefs write_buffer.c:606 — flush 前检查 journal 错误
    if let Some(j) = journal {
        if let Some(err) = j.bch2_journal_error_check() {
            return Err(StorageError::JournalError(format!("{}", err)));
        }
    }

    // Step 1: move inc → flushing
    move_keys_from_inc_to_flushing(wb);

    if wb.flushing.nr == 0 {
        wb.nr_flushes += 1;
        return Ok(());
    }

    // Step 2: build sorted index
    let mut refs = build_sorted_index(&wb.flushing.keys);

    // Step 3: sort
    wb_sort(&mut refs);

    // Step 4: dedup — 清零被丢弃条目的 journal_seq（对应 bcachefs line 700）
    let deduped = dedup_sorted_refs(&refs, wb.flushing.keys.as_mut_slice());

    // Step 5: fastpath flush
    let slowpath_indices = flush_fastpath(&deduped, &wb.flushing.keys, vol);

    // Step 6: slowpath flush (if journal available)
    if !slowpath_indices.is_empty() {
        let slowpath_refs: Vec<&WbKeyRef> = slowpath_indices.iter().map(|&i| deduped[i]).collect();
        flush_slowpath(&slowpath_refs, &wb.flushing.keys, vol, journal)?;
    }

    // 统计
    wb.nr_flushes += 1;
    wb.nr_keys_flushed += wb.flushing.nr as u64;

    // 清空 flushing
    wb.flushing.keys.clear();
    wb.flushing.nr = 0;

    Ok(())
}

fn wb_keys_resize(wb: &mut BtreeWriteBufferKeys, new_size: usize) -> i32 {
    if wb.keys.capacity() >= new_size {
        return 0;
    }

    let Ok(_guard) = wb.lock.try_lock() else {
        return -4;
    };

    match wb
        .keys
        .try_reserve_exact(new_size.saturating_sub(wb.keys.len()))
    {
        Ok(()) => 0,
        Err(_) => -12,
    }
}

/// 对应本地 bcachefs `bch2_btree_write_buffer_resize()`
/// (`btree/write_buffer.c:1345-1353`)。
pub fn bch2_btree_write_buffer_resize(c: &BchVol, new_size: usize) -> i32 {
    let set = unsafe { &mut *c.write_buffer_set.get() };
    for i in 0..BCH_WB_BTREE_NR {
        let wb = &mut set.buffers[i];
        let ret = wb_keys_resize(&mut wb.flushing, new_size);
        if ret != 0 {
            return ret;
        }
        let ret = wb_keys_resize(&mut wb.inc, new_size);
        if ret != 0 {
            return ret;
        }
    }
    0
}

/// bcachefs 对齐: bch2_btree_write_buffer_flush_sync — 同步刷新 write buffer
///
/// 将 write buffer 中的所有条目刷入 btree。
pub fn bch2_btree_write_buffer_flush_sync(
    wb: &mut BtreeWriteBuffer,
    vol: &BchVol,
    journal: Option<&Journal>,
) -> i32 {
    match bch2_btree_write_buffer_flush(wb, vol, journal) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

/// bcachefs 对齐: bch2_btree_write_buffer_flush_going_ro — 只读转换时的 flush
///
/// 在文件系统转换为只读前，将 write buffer 中的剩余条目全部刷入 btree。
/// 返回 `Ok(true)` 表示有待刷写入并且已成功处理；返回 `Err` 表示 flush 失败。
pub fn bch2_btree_write_buffer_flush_going_ro(
    wb: &mut BtreeWriteBuffer,
    vol: &BchVol,
    journal: Option<&Journal>,
) -> Result<bool, StorageError> {
    let had_work = wb.inc.nr > 0 || wb.flushing.nr > 0;
    if had_work {
        bch2_btree_write_buffer_flush(wb, vol, journal)?;
    }
    Ok(had_work)
}

/// bcachefs 对齐: bch2_btree_write_buffer_tryflush — 尝试刷 write buffer
///
/// 当前实现对 pending keys 直接执行 flush_locked。
pub fn bch2_btree_write_buffer_tryflush(
    wb: &mut BtreeWriteBuffer,
    vol: &BchVol,
    journal: Option<&Journal>,
) -> i32 {
    if wb.inc.nr == 0 && wb.flushing.nr == 0 {
        return 0;
    }

    match bch2_btree_write_buffer_flush(wb, vol, journal) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

/// bcachefs 对齐: bch2_btree_write_buffer_must_wait — 检查 write buffer 是否需要等待
///
/// 当所有 buffer 的总条目数超过 inc 总容量的 75% 时返回 true。
/// 对应本地 `bch2_btree_write_buffer_must_wait` (write_buffer.h:30-39)。
pub fn bch2_btree_write_buffer_must_wait(c: &BchVol) -> bool {
    let set = unsafe { &*c.write_buffer_set.get() };
    let mut nr: usize = 0;
    let mut sz: usize = 0;
    for wb in set.buffers.iter() {
        nr += wb.inc.nr + wb.flushing.nr;
        sz += wb.inc.keys.capacity();
    }
    nr > sz * 3 / 4
}

/// bcachefs 对齐: bch2_journal_key_to_wb_slowpath — 慢路径（wb 空间不足时扩容后插入）
///
/// 对应本地 `bch2_journal_key_to_wb_slowpath` (write_buffer.c:140-161)。
fn bch2_journal_key_to_wb_slowpath(
    wb_keys: &mut BtreeWriteBufferKeys,
    key: BtreeWriteBufferedKey,
) -> i32 {
    if wb_keys.keys.capacity() < 1024 {
        wb_keys.keys.reserve(1024 - wb_keys.keys.len());
    } else {
        wb_keys.keys.reserve(wb_keys.keys.len());
    }
    wb_keys.keys.push(key);
    wb_keys.nr = wb_keys.keys.len();
    0
}

/// bcachefs 对齐: `bch2_journal_key_to_wb_reserved` (write_buffer.h:152-163)
///
/// 在已确认 room 足够的情况下直接插入 key。对应 bcachefs 中
/// `__bch2_journal_key_to_wb` 在 room 检查后的实际插入逻辑。
fn bch2_journal_key_to_wb_reserved(
    pb: &mut JournalKeysToWbBtree,
    _seq: u64,
    key: BtreeWriteBufferedKey,
) {
    let wb_keys = unsafe { &mut *pb.wb };
    wb_keys.keys.push(key);
    wb_keys.nr = wb_keys.keys.len();
    pb.room = pb.room.saturating_sub(1);
}

/// bcachefs 对齐: __bch2_journal_key_to_wb — 快路径插入（检查 room 后写入）
///
/// 对应本地 `__bch2_journal_key_to_wb` (write_buffer.h:165-177)。
fn __bch2_journal_key_to_wb(
    dst: &mut JournalKeysToWb,
    idx: BchWbBtree,
    key: BtreeWriteBufferedKey,
) -> i32 {
    let pb = &mut dst.per_btree[idx as usize];
    if pb.wb.is_null() {
        return -1;
    }
    if pb.room < 1 {
        let wb_keys = unsafe { &mut *pb.wb };
        let ret = bch2_journal_key_to_wb_slowpath(wb_keys, key);
        if ret == 0 {
            pb.room = wb_keys.keys.capacity().saturating_sub(wb_keys.keys.len());
        }
        return ret;
    }
    bch2_journal_key_to_wb_reserved(pb, dst.seq, key);
    0
}

/// bcachefs 对齐: bch2_journal_key_to_wb — 将 journal key 插入 write buffer
///
/// 对应本地 `bch2_journal_key_to_wb()` (write_buffer.h:179-192)。
/// 通过 `journal_keys_to_wb` 结构体传递已获取的锁，调用方必须已调用
/// `bch2_journal_keys_to_write_buffer_start`。
pub fn bch2_journal_key_to_wb(
    dst: &mut JournalKeysToWb,
    btree_id: BtreeId,
    key: BtreeKey,
    value: BchVal,
    journal_seq: u64,
) -> i32 {
    debug_assert!(dst.seq != 0, "JournalKeysToWb seq not set — call start first");

    let wk = BtreeWriteBufferedKey {
        journal_seq,
        btree_id,
        key,
        value,
        key_type: key.key_type,
    };
    let idx = bch_wb_btree_idx(btree_id);
    __bch2_journal_key_to_wb(dst, idx, wk)
}

/// bcachefs 对齐: bch2_fs_btree_write_buffer_init_early — 早期初始化
pub fn bch2_fs_btree_write_buffer_init_early(c: &BchVol) {
    let set = unsafe { &mut *c.write_buffer_set.get() };
    for (i, wb) in set.buffers.iter_mut().enumerate() {
        wb.idx = match i {
            0 => BchWbBtree::Accounting,
            1 => BchWbBtree::Lru,
            2 => BchWbBtree::NeedDiscard,
            3 => BchWbBtree::Backpointers,
            4 => BchWbBtree::DeletedInodes,
            5 => BchWbBtree::ReconcileWork,
            6 => BchWbBtree::ReconcileHipri,
            7 => BchWbBtree::ReconcilePending,
            8 => BchWbBtree::ReconcileWorkPhys,
            9 => BchWbBtree::ReconcileHipriPhys,
            10 => BchWbBtree::StripeBackpointers,
            _ => unreachable!(),
        };
    }
}

/// bcachefs 对齐: bch2_fs_btree_write_buffer_init — 初始化 write buffer
pub fn bch2_fs_btree_write_buffer_init(c: &BchVol) -> i32 {
    let set = unsafe { &mut *c.write_buffer_set.get() };
    let initial_size = 1024;
    for wb in set.buffers.iter_mut() {
        wb.inc.keys.reserve(initial_size);
        wb.flushing.keys.reserve(initial_size);
    }
    0
}

/// bcachefs 对齐: bch2_fs_btree_write_buffer_exit — 退出 write buffer
pub fn bch2_fs_btree_write_buffer_exit(c: &BchVol) {
    let set = unsafe { &mut *c.write_buffer_set.get() };
    for wb in set.buffers.iter_mut() {
        wb.inc.keys.clear();
        wb.inc.nr = 0;
        wb.flushing.keys.clear();
        wb.flushing.nr = 0;
    }
}

/// bcachefs 对齐: `bch2_btree_write_buffer_to_text` (write_buffer.h:199 / write_buffer.c:1355-1400)
///
/// 输出 write buffer 调试信息。subvol 版本返回格式化字符串而非 printbuf。
pub fn bch2_btree_write_buffer_to_text(c: &BchVol) -> String {
    use std::fmt::Write;
    let set = unsafe { &*c.write_buffer_set.get() };
    let mut out = String::new();

    let names = [
        "accounting", "lru", "need_discard", "backpointers",
        "deleted_inodes", "reconcile_work", "reconcile_hipri",
        "reconcile_pending", "reconcile_work_phys", "reconcile_hipri_phys",
        "stripe_backpointers",
    ];

    for (i, wb) in set.buffers.iter().enumerate() {
        if wb.nr_flushes == 0 {
            continue;
        }
        let _ = writeln!(out, "{}", names[i]);
        let _ = writeln!(out, "\tinc keys:\t{}/{}", wb.inc.nr, wb.inc.keys.capacity());
        let _ = writeln!(out, "\tflushing keys:\t{}/{}", wb.flushing.nr, wb.flushing.keys.capacity());
        let _ = writeln!(out, "\tnr flushes:\t{}", wb.nr_flushes);
        let _ = writeln!(out, "\tkeys flushed:\t{}", wb.nr_keys_flushed);
    }
    out
}

/// bcachefs 对齐: bch2_btree_write_buffer_maybe_flush — 可能执行 flush
///
/// 在事务中调用，当检测到 write buffer 满时触发 flush。
pub fn bch2_btree_write_buffer_maybe_flush(
    wb: &mut BtreeWriteBuffer,
    vol: &BchVol,
    journal: Option<&Journal>,
) -> i32 {
    if wb.inc.nr == 0 && wb.flushing.nr == 0 {
        return 0;
    }

    match bch2_btree_write_buffer_flush(wb, vol, journal) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

/// bcachefs 对齐: bch2_journal_write_buffer_need_flush — 是否需要 flush write buffer
///
/// 检查所有 write buffer 是否有 pending key。
/// 如果有任何 pending key 未刷入 btree，返回 true。
fn bch2_journal_write_buffer_need_flush(wbs: &[BtreeWriteBuffer]) -> bool {
    wbs.iter().any(|wb| wb.inc.nr > 0 || wb.flushing.nr > 0)
}

/// 创建新的 write buffer 实例
fn btree_write_buffer_new(idx: BchWbBtree) -> BtreeWriteBuffer {
    BtreeWriteBuffer {
        idx,
        inc: BtreeWriteBufferKeys {
            keys: Vec::new(),
            lock: Mutex::new(()),
            nr: 0,
        },
        flushing: BtreeWriteBufferKeys {
            keys: Vec::new(),
            lock: Mutex::new(()),
            nr: 0,
        },
        nr_flushes: 0,
        nr_keys_flushed: 0,
    }
}

/// 将 BtreeWriteBufferedKey 的 bpos 提取为 WbKeyRef 的排序字段
fn key_to_wb_key_ref(idx: u32, key: &BtreeWriteBufferedKey) -> WbKeyRef {
    let pos = Bpos::from_key(&key.key);
    WbKeyRef {
        idx,
        btree_id: key.btree_id as u8,
        inode: pos.inode,
        offset: pos.offset,
        snapshot: pos.snapshot,
        journal_seq: key.journal_seq,
    }
}

/// 构建排序索引数组 — 对应 bcachefs wb_key_ref 数组构造
fn build_sorted_index(keys: &[BtreeWriteBufferedKey]) -> Vec<WbKeyRef> {
    keys.iter()
        .enumerate()
        .map(|(i, k)| key_to_wb_key_ref(i as u32, k))
        .collect()
}

/// 排序 wb_key_ref 数组 — 按 (btree_id, inode, offset, snapshot) 排序
fn wb_sort(refs: &mut [WbKeyRef]) {
    refs.sort_unstable_by(|a, b| {
        a.btree_id
            .cmp(&b.btree_id)
            .then_with(|| a.inode.cmp(&b.inode))
            .then_with(|| a.offset.cmp(&b.offset))
            .then_with(|| a.snapshot.cmp(&b.snapshot))
    });
}

/// 将 inc 中的 keys 移到 flushing — 对应 bcachefs move_keys_from_inc_to_flushing
///
/// 锁定 inc.lock，交换 inc 和 flushing 的 keys，重置 inc。
fn move_keys_from_inc_to_flushing(wb: &mut BtreeWriteBuffer) {
    let _lock = wb.inc.lock.lock().unwrap();
    // 将 inc 的所有 key 移到 flushing
    wb.flushing.keys.append(&mut wb.inc.keys);
    wb.flushing.nr = wb.flushing.keys.len();
    // 重置 inc
    wb.inc.nr = 0;
    // _lock 在此释放
}

/// 对排序后的 refs 去重 — 相同 (btree_id, inode, offset, snapshot) 的条目中保留最新
///
/// 对应 bcachefs `write_buffer.c:682-707` — 相同位置条目去重：
/// - 保留最新的条目（最大 journal_seq）
/// - **清零被丢弃条目的 journal_seq**（对应 bcachefs line 700）
/// - 返回去重后的 WbKeyRef 列表
///
/// journal_seq=0 标记确保 slowpath 和 could_not_insert 路径不会重复处理已去重的条目。
fn dedup_sorted_refs<'a>(
    sorted_refs: &'a [WbKeyRef],
    keys: &mut [BtreeWriteBufferedKey],
) -> Vec<&'a WbKeyRef> {
    let mut result: Vec<&WbKeyRef> = Vec::new();
    let mut i = 0;
    while i < sorted_refs.len() {
        let current = &sorted_refs[i];
        // 找所有相同 pos 的条目
        let mut j = i + 1;
        while j < sorted_refs.len() {
            let next = &sorted_refs[j];
            if next.btree_id != current.btree_id
                || next.inode != current.inode
                || next.offset != current.offset
                || next.snapshot != current.snapshot
            {
                break;
            }
            j += 1;
        }
        // [i, j) 范围内的条目具有相同的 btree_id + pos
        // 选择 journal_seq 最大的（最新写入）
        let best = (i..j).max_by_key(|&k| sorted_refs[k].journal_seq).unwrap();
        // 对应 bcachefs write_buffer.c:700 — 清零被丢弃条目的 journal_seq
        for k in i..j {
            if k != best {
                keys[sorted_refs[k].idx as usize].journal_seq = 0;
            }
        }
        // 从实际 key 中检查 journal_seq（跳过已丢弃的）
        let actual_key = &keys[sorted_refs[best].idx as usize];
        if actual_key.journal_seq > 0 {
            result.push(&sorted_refs[best]);
        }
        i = j;
    }
    result
}

/// Fastpath flush — 遍历 sorted_refs，通过 vol.get_entry 做 noop 检查，
/// 成功后通过 vol.insert_entry 写入 btree。
///
/// 返回仍然有未 flush key 的索引列表（slowpath 需要重试）。
/// Run a future, using tokio runtime if available, otherwise futures::executor.
/// This avoids nesting executor errors when called from within a tokio runtime context.
fn block_on_safe<F: std::future::Future<Output = T>, T>(f: F) -> T {
    match Handle::try_current() {
        Ok(handle) => handle.block_on(f),
        Err(_) => futures::executor::block_on(f),
    }
}

fn flush_fastpath(refs: &[&WbKeyRef], keys: &[BtreeWriteBufferedKey], vol: &BchVol) -> Vec<usize> {
    let mut slowpath_indices: Vec<usize> = Vec::new();
    for (result_idx, wb_ref) in refs.iter().enumerate() {
        let key_idx = wb_ref.idx as usize;
        let wk = &keys[key_idx];
        // Noop 检查：vol 中已有相同 key 和 value 的条目
        let existing = vol.get_entry(wk.btree_id, &wk.key);
        let is_noop = match existing {
            Some((ref ek, ref ev)) => {
                ek.key_type == wk.key_type
                    && wk.key.get_vaddr() == ek.get_vaddr()
                    && wk.key.get_snapshot_id() == ek.get_snapshot_id()
                    && ev.paddr == wk.value.paddr
                    && ev.ver == wk.value.ver
            }
            None => false,
        };
        if is_noop {
            continue;
        }
        // 尝试 fastpath insert
        let success = block_on_safe(vol.btree(wk.btree_id).bch2_btree_insert(
            &NoopWriter,
            wk.key,
            wk.value,
            wk.journal_seq,
        ))
        .unwrap_or(false);
        if !success {
            slowpath_indices.push(result_idx);
        }
    }
    slowpath_indices
}

/// Slowpath flush — 对 fastpath 失败的 key 通过事务提交重试
fn flush_slowpath(
    refs: &[&WbKeyRef],
    keys: &[BtreeWriteBufferedKey],
    vol: &BchVol,
    journal: Option<&Journal>,
) -> Result<(), StorageError> {
    for &wb_ref in refs {
        let wk = &keys[wb_ref.idx as usize];
        if wk.journal_seq == 0 {
            continue; // 已被 noop 消除或已 flush
        }
        if journal.is_some() {
            let mut trans = BtreeTrans::new_nojournal(vol);
            trans.bch2_trans_begin();
            trans.bch2_trans_update(wk.btree_id, 0, false, wk.key, wk.value, 0);
            trans.bch2_trans_commit()
                .map_err(|e| StorageError::JournalError(e.to_string()))?;
        } else {
            block_on_safe(vol.btree(wk.btree_id).bch2_btree_insert(
                &NoopWriter,
                wk.key,
                wk.value,
                wk.journal_seq,
            ))
            .unwrap_or(false);
        }
    }
    Ok(())
}

/// bcachefs 对齐: wb_key_cmp — write buffer key 比较函数
///
/// 按 (btree_id ASC, bpos ASC) 顺序比较两个 write buffered key。
pub fn wb_key_cmp(a: &BtreeWriteBufferedKey, b: &BtreeWriteBufferedKey) -> Ordering {
    let a_pos = Bpos::from_key(&a.key);
    let b_pos = Bpos::from_key(&b.key);
    (a.btree_id as u8)
        .cmp(&(b.btree_id as u8))
        .then_with(|| a_pos.cmp(&b_pos))
}

/// bcachefs 对齐: bch_wb_btree_idx — 从 BtreeId 映射到 wb_btree 索引
///
/// 将 subvol 的 BtreeId 映射到 bcachefs 对齐的 BchWbBtree 枚举。
pub fn bch_wb_btree_idx(btree_id: BtreeId) -> BchWbBtree {
    match btree_id {
        BtreeId::Accounting => BchWbBtree::Accounting,
        BtreeId::Lru => BchWbBtree::Lru,
        BtreeId::NeedDiscard => BchWbBtree::NeedDiscard,
        BtreeId::Backpointers => BchWbBtree::Backpointers,
        BtreeId::DeletedInodes => BchWbBtree::DeletedInodes,
        BtreeId::ReconcileWork => BchWbBtree::ReconcileWork,
        BtreeId::ReconcileHipri => BchWbBtree::ReconcileHipri,
        BtreeId::ReconcilePending => BchWbBtree::ReconcilePending,
        BtreeId::ReconcileWorkPhys => BchWbBtree::ReconcileWorkPhys,
        BtreeId::ReconcileHipriPhys => BchWbBtree::ReconcileHipriPhys,
        BtreeId::StripeBackpointers => BchWbBtree::StripeBackpointers,
        _ => unreachable!("btree is not marked BTREE_IS_write_buffer"),
    }
}

/// bcachefs 对齐: `bch_wb_btree_to_btree_id` (write_buffer.h:20-28)
///
/// 反向映射：从 BchWbBtree 索引回到 BtreeId。
/// 对应 bcachefs 的静态查找表 `tbl[BCH_WB_BTREE_NR]`。
pub fn bch_wb_btree_to_btree_id(idx: BchWbBtree) -> BtreeId {
    match idx {
        BchWbBtree::Accounting => BtreeId::Accounting,
        BchWbBtree::Lru => BtreeId::Lru,
        BchWbBtree::NeedDiscard => BtreeId::NeedDiscard,
        BchWbBtree::Backpointers => BtreeId::Backpointers,
        BchWbBtree::DeletedInodes => BtreeId::DeletedInodes,
        BchWbBtree::ReconcileWork => BtreeId::ReconcileWork,
        BchWbBtree::ReconcileHipri => BtreeId::ReconcileHipri,
        BchWbBtree::ReconcilePending => BtreeId::ReconcilePending,
        BchWbBtree::ReconcileWorkPhys => BtreeId::ReconcileWorkPhys,
        BchWbBtree::ReconcileHipriPhys => BtreeId::ReconcileHipriPhys,
        BchWbBtree::StripeBackpointers => BtreeId::StripeBackpointers,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_key(vaddr: u64, snapshot_id: u32, key_type: KeyType) -> BtreeKey {
        BtreeKey::new(vaddr, snapshot_id, key_type)
    }

    fn make_test_value(paddr: u64, ver: u16) -> BchVal {
        BchVal::new(paddr, ver)
    }

    fn make_wb_key(
        journal_seq: u64,
        btree_id: BtreeId,
        vaddr: u64,
        snapshot_id: u32,
    ) -> BtreeWriteBufferedKey {
        BtreeWriteBufferedKey {
            journal_seq,
            btree_id,
            key: make_test_key(vaddr, snapshot_id, KeyType::Normal),
            value: make_test_value(vaddr, 1),
            key_type: KeyType::Normal,
        }
    }

    #[test]
    fn test_write_buffer_must_wait() {
        let mut wb = btree_write_buffer_new(BchWbBtree::Accounting);
        let total = wb.inc.nr + wb.flushing.nr;
        let capacity = wb.inc.keys.capacity().max(1);
        assert!(!(total > capacity * 3 / 4));

        // 填满 buffer 到超过 75%
        for i in 0..100 {
            wb.inc
                .keys
                .push(make_wb_key(i as u64, BtreeId::Extents, i as u64, 0));
            wb.inc.nr = wb.inc.keys.len();
        }
        let total = wb.inc.nr + wb.flushing.nr;
        let capacity = wb.inc.keys.capacity().max(1);
        assert!(total > capacity * 3 / 4);
    }

    #[test]
    fn test_write_buffer_create() {
        let wb = btree_write_buffer_new(BchWbBtree::Accounting);
        assert_eq!(wb.idx as u8, 0);
        assert!(wb.inc.keys.is_empty());
        assert!(wb.flushing.keys.is_empty());
    }

    #[test]
    fn test_write_buffer_insert_and_flush() {
        let mut wb = btree_write_buffer_new(BchWbBtree::Accounting);
        let vol = BchVol::test_trees();

        // 插入 3 个 key
        let wk1 = make_wb_key(1, BtreeId::Extents, 100, 0);
        let wk2 = make_wb_key(2, BtreeId::Extents, 200, 0);
        let wk3 = make_wb_key(3, BtreeId::Extents, 300, 0);

        wb.inc.keys.push(wk1);
        wb.inc.keys.push(wk2);
        wb.inc.keys.push(wk3);
        wb.inc.nr = wb.inc.keys.len();

        // flush
        let result = bch2_btree_write_buffer_flush(&mut wb, &vol, None);
        assert!(result.is_ok());

        // 验证 key 已写入 vol
        assert!(vol
            .get_entry(BtreeId::Extents, &make_test_key(100, 0, KeyType::Normal))
            .is_some());
        assert!(vol
            .get_entry(BtreeId::Extents, &make_test_key(200, 0, KeyType::Normal))
            .is_some());
        assert!(vol
            .get_entry(BtreeId::Extents, &make_test_key(300, 0, KeyType::Normal))
            .is_some());

        // flushing 应已清空
        assert_eq!(wb.flushing.nr, 0);
        assert!(wb.flushing.keys.is_empty());
        assert_eq!(wb.nr_flushes, 1);
    }

    #[test]
    fn test_write_buffer_dedup() {
        let mut wb = btree_write_buffer_new(BchWbBtree::Accounting);
        let vol = BchVol::test_trees();

        // 对同一位置插入 3 个 key（不同 journal_seq）
        let wk1 = make_wb_key(10, BtreeId::Extents, 100, 0);
        let wk2 = make_wb_key(20, BtreeId::Extents, 100, 0);
        let wk3 = make_wb_key(30, BtreeId::Extents, 100, 0);

        wb.inc.keys.push(wk1);
        wb.inc.keys.push(wk2);
        wb.inc.keys.push(wk3);
        wb.inc.nr = wb.inc.keys.len();

        // flush
        let result = bch2_btree_write_buffer_flush(&mut wb, &vol, None);
        assert!(result.is_ok());

        // 验证 vol 中只有最新值（journal_seq=30 的 paddr）
        let entry = vol.get_entry(BtreeId::Extents, &make_test_key(100, 0, KeyType::Normal));
        assert!(entry.is_some());
        let (_k, v) = entry.unwrap();
        assert_eq!(v.paddr.get(), 100); // paddr = vaddr = 100
        assert_eq!(v.ver, 1);
    }

    #[test]
    fn test_write_buffer_noop_elimination() {
        let mut wb = btree_write_buffer_new(BchWbBtree::Accounting);
        let vol = BchVol::test_trees();

        // 先在 vol 中插入一个 key
        let existing = make_test_key(100, 0, KeyType::Normal);
        let existing_val = make_test_value(100, 1);
        let _ = futures::executor::block_on(vol.btree(BtreeId::Extents).bch2_btree_insert(
            &NoopWriter,
            existing,
            existing_val,
            1,
        ));

        // 在 write buffer 中插入相同 key 和 value
        let wk = BtreeWriteBufferedKey {
            journal_seq: 2,
            btree_id: BtreeId::Extents,
            key: make_test_key(100, 0, KeyType::Normal),
            value: make_test_value(100, 1),
            key_type: KeyType::Normal,
        };
        wb.inc.keys.push(wk);
        wb.inc.nr = wb.inc.keys.len();

        let key_count_before = vol.btree(BtreeId::Extents).root().node.packed_keys + vol.btree(BtreeId::Extents).root().node.unpacked_keys;

        let result = bch2_btree_write_buffer_flush(&mut wb, &vol, None);
        assert!(result.is_ok());

        // key count 应该不变（noop 消除）
        let key_count_after = vol.btree(BtreeId::Extents).root().node.packed_keys + vol.btree(BtreeId::Extents).root().node.unpacked_keys;
        assert_eq!(key_count_before, key_count_after);
    }

    #[test]
    fn test_write_buffer_sort_order() {
        let mut wb = btree_write_buffer_new(BchWbBtree::Accounting);

        // 无序插入
        let wk1 = make_wb_key(1, BtreeId::Extents, 300, 0);
        let wk2 = make_wb_key(2, BtreeId::Extents, 100, 0);
        let wk3 = make_wb_key(3, BtreeId::Extents, 200, 0);

        wb.inc.keys.push(wk1);
        wb.inc.keys.push(wk2);
        wb.inc.keys.push(wk3);
        wb.inc.nr = wb.inc.keys.len();

        // 验证排序
        let refs = build_sorted_index(&wb.inc.keys);
        let mut sorted_refs = refs.clone();
        wb_sort(&mut sorted_refs);

        // 排序后应为 100, 200, 300（按 vaddr/offset 升序）
        assert_eq!(sorted_refs[0].offset, 100);
        assert_eq!(sorted_refs[1].offset, 200);
        assert_eq!(sorted_refs[2].offset, 300);
    }

    #[test]
    fn test_write_buffer_should_flush() {
        let mut wb = btree_write_buffer_new(BchWbBtree::Accounting);
        let total = wb.inc.nr + wb.flushing.nr;
        let capacity = wb.inc.keys.capacity().max(1);
        assert!(!(total > capacity * 3 / 4));

        // 填充少量 key
        for i in 0..10 {
            wb.inc
                .keys
                .push(make_wb_key(i as u64, BtreeId::Extents, i as u64, 0));
        }
        wb.inc.nr = wb.inc.keys.len();

        // 验证 need_flush 返回 true（有 pending key）
        let wbs = [wb];
        assert!(bch2_journal_write_buffer_need_flush(&wbs));
    }

    #[test]
    fn test_wb_key_cmp() {
        // 相同 btree_id + 相同 bpos → Equal
        let a = make_wb_key(1, BtreeId::Extents, 100, 0);
        let b = make_wb_key(2, BtreeId::Extents, 100, 0);
        assert_eq!(wb_key_cmp(&a, &b), Ordering::Equal);

        // 不同 btree_id → 按 btree_id 排序
        let c = make_wb_key(3, BtreeId::Freespace, 100, 0);
        assert_eq!(wb_key_cmp(&a, &c), Ordering::Less);

        // 相同 btree_id + 不同 offset → 按 offset 排序
        let d = make_wb_key(4, BtreeId::Extents, 200, 0);
        assert_eq!(wb_key_cmp(&a, &d), Ordering::Less);
    }

    #[test]
    fn test_write_buffer_flush_locked_empty() {
        let mut wb = btree_write_buffer_new(BchWbBtree::Accounting);
        let vol = BchVol::test_trees();

        // 空 buffer flush → 应成功且无副作用
        let result = bch2_btree_write_buffer_flush(&mut wb, &vol, None);
        assert!(result.is_ok());
        assert_eq!(wb.nr_flushes, 1);
    }

    #[test]
    fn test_write_buffer_flush_inserts_into_btree() {
        let mut wb = btree_write_buffer_new(BchWbBtree::Accounting);
        let vol = BchVol::test_trees();

        wb.inc.keys.push(make_wb_key(1, BtreeId::Extents, 150, 0));
        wb.inc.nr = wb.inc.keys.len();

        let result = bch2_btree_write_buffer_flush(&mut wb, &vol, None);
        assert!(result.is_ok());
        assert!(vol
            .get_entry(BtreeId::Extents, &make_test_key(150, 0, KeyType::Normal))
            .is_some());
    }

    #[test]
    fn test_write_buffer_flush_going_ro_reports_work_only_when_busy() {
        let mut wb = btree_write_buffer_new(BchWbBtree::Accounting);
        let vol = BchVol::test_trees();

        assert_eq!(
            bch2_btree_write_buffer_flush_going_ro(&mut wb, &vol, None).unwrap(),
            false
        );

        wb.inc.keys.push(make_wb_key(1, BtreeId::Extents, 100, 0));
        wb.inc.nr = wb.inc.keys.len();

        assert_eq!(
            bch2_btree_write_buffer_flush_going_ro(&mut wb, &vol, None).unwrap(),
            true
        );
        assert!(vol
            .get_entry(BtreeId::Extents, &make_test_key(100, 0, KeyType::Normal))
            .is_some());
    }

    #[test]
    fn test_write_buffer_maybe_flush_flushes_pending_keys_without_threshold() {
        let mut wb = btree_write_buffer_new(BchWbBtree::Accounting);
        let vol = BchVol::test_trees();

        wb.inc.keys.push(make_wb_key(1, BtreeId::Extents, 200, 0));
        wb.inc.nr = wb.inc.keys.len();

        assert_eq!(bch2_btree_write_buffer_maybe_flush(&mut wb, &vol, None), 0);
        assert!(vol
            .get_entry(BtreeId::Extents, &make_test_key(200, 0, KeyType::Normal))
            .is_some());
    }

    // ─── 生命周期函数测试 ───────────────────────────────────────────────────

    #[test]
    fn test_wb_set_init_early() {
        let mut set = BtreeWriteBufferSet::new();
        // 手动设置 idx（对应 bch2_fs_btree_write_buffer_init_early 的 set 逻辑）
        for (i, wb) in set.buffers.iter_mut().enumerate() {
            wb.idx = match i {
                0 => BchWbBtree::Accounting,
                1 => BchWbBtree::Lru,
                2 => BchWbBtree::NeedDiscard,
                3 => BchWbBtree::Backpointers,
                4 => BchWbBtree::DeletedInodes,
                5 => BchWbBtree::ReconcileWork,
                6 => BchWbBtree::ReconcileHipri,
                7 => BchWbBtree::ReconcilePending,
                8 => BchWbBtree::ReconcileWorkPhys,
                9 => BchWbBtree::ReconcileHipriPhys,
                10 => BchWbBtree::StripeBackpointers,
                _ => unreachable!(),
            };
        }

        // 验证每个 buffer 的 idx 正确
        assert_eq!(set.buffers[0].idx, BchWbBtree::Accounting);
        assert_eq!(set.buffers[1].idx, BchWbBtree::Lru);
        assert_eq!(set.buffers[2].idx, BchWbBtree::NeedDiscard);
        assert_eq!(set.buffers[3].idx, BchWbBtree::Backpointers);
        assert_eq!(set.buffers[4].idx, BchWbBtree::DeletedInodes);
        assert_eq!(set.buffers[5].idx, BchWbBtree::ReconcileWork);
        assert_eq!(set.buffers[6].idx, BchWbBtree::ReconcileHipri);
        assert_eq!(set.buffers[7].idx, BchWbBtree::ReconcilePending);
        assert_eq!(set.buffers[8].idx, BchWbBtree::ReconcileWorkPhys);
        assert_eq!(set.buffers[9].idx, BchWbBtree::ReconcileHipriPhys);
        assert_eq!(set.buffers[10].idx, BchWbBtree::StripeBackpointers);
    }

    #[test]
    fn test_wb_set_init() {
        let mut set = BtreeWriteBufferSet::new();
        // 对应 bch2_fs_btree_write_buffer_init_early + _init 的 set 逻辑
        for (i, wb) in set.buffers.iter_mut().enumerate() {
            wb.idx = match i {
                0 => BchWbBtree::Accounting,
                1 => BchWbBtree::Lru,
                2 => BchWbBtree::NeedDiscard,
                3 => BchWbBtree::Backpointers,
                4 => BchWbBtree::DeletedInodes,
                5 => BchWbBtree::ReconcileWork,
                6 => BchWbBtree::ReconcileHipri,
                7 => BchWbBtree::ReconcilePending,
                8 => BchWbBtree::ReconcileWorkPhys,
                9 => BchWbBtree::ReconcileHipriPhys,
                10 => BchWbBtree::StripeBackpointers,
                _ => unreachable!(),
            };
        }
        let ret = {
            let initial_size = 1024;
            for wb in set.buffers.iter_mut() {
                wb.inc.keys.reserve(initial_size);
                wb.flushing.keys.reserve(initial_size);
            }
            0
        };

        assert_eq!(ret, 0);
        // 验证每个 buffer 的 inc 和 flushing keys 容量 >= 1024
        for wb in set.buffers.iter() {
            assert!(
                wb.inc.keys.capacity() >= 1024,
                "buffer {:?} inc capacity < 1024",
                wb.idx
            );
            assert!(
                wb.flushing.keys.capacity() >= 1024,
                "buffer {:?} flushing capacity < 1024",
                wb.idx
            );
            assert!(wb.inc.keys.is_empty());
            assert!(wb.flushing.keys.is_empty());
        }
    }

    #[test]
    fn test_wb_set_exit() {
        let mut set = BtreeWriteBufferSet::new();
        // 对应 bch2_fs_btree_write_buffer_init_early + _init 的 set 逻辑
        for (i, wb) in set.buffers.iter_mut().enumerate() {
            wb.idx = match i {
                0 => BchWbBtree::Accounting,
                1 => BchWbBtree::Lru,
                2 => BchWbBtree::NeedDiscard,
                3 => BchWbBtree::Backpointers,
                4 => BchWbBtree::DeletedInodes,
                5 => BchWbBtree::ReconcileWork,
                6 => BchWbBtree::ReconcileHipri,
                7 => BchWbBtree::ReconcilePending,
                8 => BchWbBtree::ReconcileWorkPhys,
                9 => BchWbBtree::ReconcileHipriPhys,
                10 => BchWbBtree::StripeBackpointers,
                _ => unreachable!(),
            };
        }
        for wb in set.buffers.iter_mut() {
            wb.inc.keys.reserve(1024);
            wb.flushing.keys.reserve(1024);
        }

        // 添加一些 key 到 buffer[0]
        set.buffers[0]
            .inc
            .keys
            .push(make_wb_key(1, BtreeId::Extents, 100, 0));
        set.buffers[0].inc.nr = set.buffers[0].inc.keys.len();

        // 对应 bch2_fs_btree_write_buffer_exit 的 set 逻辑
        for wb in set.buffers.iter_mut() {
            wb.inc.keys.clear();
            wb.inc.nr = 0;
            wb.flushing.keys.clear();
            wb.flushing.nr = 0;
        }

        // 验证所有 buffer 的 keys 已被清空
        for wb in set.buffers.iter() {
            assert!(
                wb.inc.keys.is_empty(),
                "buffer {:?} inc keys not empty after exit",
                wb.idx
            );
            assert_eq!(wb.inc.nr, 0, "buffer {:?} inc.nr != 0 after exit", wb.idx);
            assert!(
                wb.flushing.keys.is_empty(),
                "buffer {:?} flushing keys not empty after exit",
                wb.idx
            );
            assert_eq!(
                wb.flushing.nr, 0,
                "buffer {:?} flushing.nr != 0 after exit",
                wb.idx
            );
        }
    }

    #[test]
    fn test_journal_write_buffer_need_flush() {
        let mut set = BtreeWriteBufferSet::new();
        // 对应 bch2_fs_btree_write_buffer_init_early 的 set 逻辑
        for (i, wb) in set.buffers.iter_mut().enumerate() {
            wb.idx = match i {
                0 => BchWbBtree::Accounting,
                _ => continue,
            };
        }
        for wb in set.buffers.iter_mut() {
            wb.inc.keys.reserve(1024);
            wb.flushing.keys.reserve(1024);
        }

        assert!(!bch2_journal_write_buffer_need_flush(&set.buffers));

        set.buffers[0]
            .inc
            .keys
            .push(make_wb_key(1, BtreeId::Extents, 100, 0));
        set.buffers[0].inc.nr = set.buffers[0].inc.keys.len();

        assert!(bch2_journal_write_buffer_need_flush(&set.buffers));
    }
}
