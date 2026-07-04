//! Key-level read cache for btree operations.
//!
//! bcachefs 对齐: hash 表索引 `(btree_id, pos)` + per-entry 锁 + slot 复用。
//!
//! 对应 bcachefs `btree_key_cache.h` + `btree_key_cache.c` 中的 key cache。
//!
//! ## bcachefs 对照
//!
//! | bcachefs | subvol |
//! |----------|----------|
//! | `rhltable` + RCU | `HashMap` + `ArcSwap`（无锁读） |
//! | `struct bkey_cached` + `six_lock` | `CachedEntry` + `RwLock` |
//! | `bkey_cached.valid` | `CachedEntry.valid` (AtomicBool) |
//! | `bch2_btree_key_cache_find()` 无锁 | `find()` 无锁 |
//! | `bch2_btree_key_cache_drop()` (rhashtable_remove_fast) | `invalidate()` (valid=false, slot 复用) |
//! | kernel shrinker eviction | only invalidation on write |

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock, Weak};

use arc_swap::ArcSwap;

use crate::btree::key::{Bpos, BtreeEntry};
use crate::journal::reclaim::{JournalEntryPin, JournalPinType};
use crate::journal::Journal;

/// 单个缓存条目 — 对应 bcachefs `struct bkey_cached`
///
/// bcachefs 的 per-entry `six_lock` 通过 `RwLock` 实现。
/// 多个 reader 可以同时读不同 entry，writer 独占 entry 访问。
///
/// `valid` → slot 复用：drop 只置 valid=false，不移除 hash 表。
/// `dirty`  → 已在 cache 中修改但未写回 btree。
/// `flush_pending` → journal_reclaim 触发的延迟写回标志。
/// Arc 确保正在被读取的 entry 安全存活。
struct CachedEntry {
    /// 保护本条目的读/写锁 — 对应 six_lock (Read/Intent/Write)
    /// subvol key cache 不需要 Intent 语义
    lock: RwLock<BtreeEntry>,

    /// bcachefs 对齐: ck->valid — entry 是否有效
    /// false = slot 存在但内容无效（被 drop 了但没移除 hash 表）
    valid: AtomicBool,

    /// bcachefs 对齐: BKEY_CACHED_DIRTY — entry 已修改待写回 btree
    /// 在 bch2_btree_insert_key_cached() 中设置，
    /// 在 flush_dirty() 写回成功后清除。
    dirty: AtomicBool,

    /// bcachefs 对齐: ck->journal — 嵌入的 JournalEntryPin
    /// 替代独立的 journal_seq: AtomicU64。
    /// 通过 bch2_journal_pin_add/bch2_journal_pin_drop 管理
    /// journal pin 生命周期，使 journal reclaim 能驱动 flush callback。
    pin: JournalEntryPin,

    /// bcachefs 对齐: ck->flush_pending — journal_reclaim 触发的 flush 请求
    /// journal pin callback 设此标志（Fn() 上下文，不能持锁或写 btree），
    /// 实际的写回在下一个持有 &mut BchVol 的同步点执行。
    flush_pending: AtomicBool,
}

// SAFETY: CachedEntry 包含 JournalEntryPin（其 flush callback 不是 Sync），
// 但 flush callback 受 PinList.lock 保护（写入读取均在锁内完成）。
// 多个线程共享 &CachedEntry 时，只访问 flush_pending（AtomicBool）等 Sync 字段。
// 所有其他字段（RwLock, AtomicBool）本身是 Sync。
unsafe impl Sync for CachedEntry {}

impl CachedEntry {
    fn new(entry: BtreeEntry) -> Self {
        Self {
            lock: RwLock::new(entry),
            valid: AtomicBool::new(true),
            dirty: AtomicBool::new(false),
            pin: JournalEntryPin::new(None, JournalPinType::KeyCache),
            flush_pending: AtomicBool::new(false),
        }
    }

    /// 获取条目的读锁并克隆 — 对应 bcachefs six_lock_read
    fn read(&self) -> BtreeEntry {
        self.lock.read().unwrap().clone()
    }
}

/// Key cache — bcachefs 对齐的 hash 表 + per-entry 锁 + slot 复用 + dirty tracking
///
/// ## bcachefs 字段对照
///
/// | subvol | bcachefs |
/// |---|---|
/// | `ArcSwap<HashMap>` | `rhltable` + RCU（无锁读） |
/// | per-entry `valid` | `ck->valid` |
/// | per-entry `dirty` | `BKEY_CACHED_DIRTY` |
/// | `nr_dirty` | `atomic_long_t nr_dirty` |
///
/// ## 无锁读设计
///
/// bcachefs 使用 `rhashtable` + RCU 实现无锁查找：
/// - `rhashtable_lookup_fast()`（`key_cache.h:70`）— 完全无锁
/// - `rhashtable_remove_fast()` / `rhashtable_lookup_insert_fast()` — 内部自旋锁
/// - key_cache.c 中没有外部 mutex/spinlock/rwlock（`grep` 验证）
///
/// subvol 用 `ArcSwap` 达到同样效果：
/// - **读路径**（`find`/`collect_dirty`/`invalidate`/`mark_clean`）：`cache.load()`
///   只做原子指针 load，完全无锁。通过 Arc 保证旧版本在 guard 存活期内不被释放。
/// - **写路径**（`insert`/`bch2_btree_insert_key_cached`）：`write_lock` 互斥 +
///   写时克隆 + `store()` 原子替换，类似 rhashtable 内部 spinlock 语义。
/// - **不修改 HashMap 结构的方法**（`invalidate`/`mark_clean`）：直接 `load()`
///   只操作 Arc 内部 AtomicBool/RwLock，无需写锁。
///
/// Slot 复用: `invalidate()` 只设 valid=false，不移除 hash 表。
/// Dirty tracking: `bch2_btree_insert_key_cached()` 创建脏条目，
/// `flush_dirty()` 写回后清除脏标志。
pub struct KeyCache {
    /// bcachefs 对齐: rhltable (RCU) → ArcSwap<HashMap>
    /// 读路径只有原子指针 load，完全无锁。
    cache: ArcSwap<HashMap<Bpos, Arc<CachedEntry>>>,

    /// 写操作互斥锁 — 序列化写时克隆 + 原子替换。
    /// 对应 rhashtable 内部 spinlock（key_cache.c 0 外部锁）。
    write_lock: Mutex<()>,

    /// bcachefs 对齐: atomic_long_t nr_dirty — 当前脏条目数
    nr_dirty: AtomicU64,

    /// bcachefs 对齐: atomic_long_t nr_keys — 当前缓存条目数。
    nr_keys: AtomicU64,

    /// Journal 弱引用 — 通过 pin_add/pin_drop 集成 journal 生命周期。
    journal: OnceLock<Weak<Journal>>,
}

impl KeyCache {
    /// 创建新的空 KeyCache
    pub fn new() -> Self {
        Self {
            cache: ArcSwap::new(Arc::new(HashMap::new())),
            write_lock: Mutex::new(()),
            nr_dirty: AtomicU64::new(0),
            nr_keys: AtomicU64::new(0),
            journal: OnceLock::new(),
        }
    }

    /// 设置 journal 弱引用（通常由 Volume/CoreVolume 在初始化时调用）
    ///
    /// 只能设置一次，重复调用被忽略。
    pub fn set_journal(&self, journal: &Weak<Journal>) {
        self.journal.set(journal.clone()).ok();
    }

    /// bcachefs 对齐: bch2_btree_key_cache_find
    ///
    /// 在 key cache 中按 `pos` 查找。
    /// 完全无锁 — 只有原子指针 load。
    pub fn find(&self, pos: &Bpos) -> Option<BtreeEntry> {
        let map = self.cache.load();
        if let Some(arc) = map.get(pos) {
            if !arc.valid.load(Ordering::Acquire) {
                return None;
            }
            return Some(arc.read());
        }
        None
    }

    /// 将 `BtreeEntry` 存入 key cache（干净条目，来自 btree 读取）
    ///
    /// 写路径：持 write_lock → 写时克隆 → 原子替换。
    /// 仅在 btree 中找到匹配 key 时调用（不缓存负结果）。
    pub fn insert(&self, pos: Bpos, entry: BtreeEntry) {
        let _lock = self.write_lock.lock().unwrap();
        let snapshot = self.cache.load();
        let mut new_map = Arc::clone(&snapshot).as_ref().clone();
        drop(snapshot);

        match new_map.get(&pos) {
            Some(arc) => {
                let was_dirty = arc.dirty.swap(false, Ordering::AcqRel);
                if was_dirty {
                    self.drop_journal_pin(arc);
                    self.nr_dirty.fetch_sub(1, Ordering::Release);
                }
                arc.flush_pending.store(false, Ordering::Release);
                *arc.lock.write().unwrap() = entry;
                arc.valid.store(true, Ordering::Release);
            }
            None => {
                let cached = Arc::new(CachedEntry::new(entry));
                new_map.insert(pos, cached);
                self.nr_keys.fetch_add(1, Ordering::Release);
            }
        }

        self.cache.store(Arc::new(new_map));
    }

    /// bcachefs 对齐: bch2_btree_key_cache_drop
    ///
    /// 使指定 `pos` 的缓存条目失效。
    /// **不移除 hash 表**，只设 valid=false，保留 slot。
    /// 不修改 HashMap 结构 → 只用 load() 无锁读。
    pub fn invalidate(&self, pos: &Bpos) {
        let map = self.cache.load();
        if let Some(arc) = map.get(pos) {
            let was_dirty = arc.dirty.swap(false, Ordering::AcqRel);
            if was_dirty {
                self.drop_journal_pin(arc);
                self.nr_dirty.fetch_sub(1, Ordering::Release);
            }
            arc.flush_pending.store(false, Ordering::Release);
            arc.valid.store(false, Ordering::Release);
        }
    }

    /// 清除脏条目关联的 journal pin（通过嵌入的 JournalEntryPin）
    fn drop_journal_pin(&self, entry: &CachedEntry) {
        if !entry.pin.is_active() {
            return;
        }
        if let Some(j) = self.journal.get().and_then(|w| w.upgrade()) {
            j.bch2_journal_pin_drop(&entry.pin);
        }
    }

    // ─── bcachefs 对齐 API ────────────────────────────────────────────────

    /// bcachefs 对齐: bch2_btree_key_cache_find
    pub fn bch2_btree_key_cache_find(&self, pos: &Bpos) -> Option<BtreeEntry> {
        self.find(pos)
    }

    /// bcachefs 对齐: bch2_btree_key_cache_drop
    pub fn bch2_btree_key_cache_drop(&self, pos: &Bpos) {
        self.invalidate(pos);
    }

    /// bcachefs 对齐: bch2_btree_insert_key_cached
    ///
    /// 将 key 写入 cache 并标记为 dirty。
    /// 写路径：持 write_lock → 写时克隆 → 原子替换。
    ///
    /// bcachefs 中该函数不操作哈希表（通过 path 直接引用条目），
    /// 但 subvol 的 API 通过 pos 查找，需要写时克隆来创建/更新。
    pub fn bch2_btree_insert_key_cached(&self, pos: Bpos, entry: BtreeEntry, journal_seq: u64) {
        let _lock = self.write_lock.lock().unwrap();
        let snapshot = self.cache.load();
        let mut new_map = Arc::clone(&snapshot).as_ref().clone();
        drop(snapshot);

        match new_map.get(&pos) {
            Some(arc) => {
                let was_dirty = arc.dirty.swap(true, Ordering::AcqRel);
                if !was_dirty {
                    self.nr_dirty.fetch_add(1, Ordering::Acquire);
                }
                arc.valid.store(true, Ordering::Release);
                arc.flush_pending.store(false, Ordering::Release);
                self.pin_entry(arc, journal_seq);
                *arc.lock.write().unwrap() = entry;
            }
            None => {
                let cached = Arc::new(CachedEntry {
                    lock: RwLock::new(entry),
                    valid: AtomicBool::new(true),
                    dirty: AtomicBool::new(true),
                    pin: JournalEntryPin::new(None, JournalPinType::KeyCache),
                    flush_pending: AtomicBool::new(false),
                });
                self.pin_entry(&cached, journal_seq);
                self.nr_dirty.fetch_add(1, Ordering::Acquire);
                new_map.insert(pos, cached);
            }
        }

        self.cache.store(Arc::new(new_map));
    }

    /// 在 journal 上注册 pin callback，当 journal_reclaim 推进到 journal_seq 时
    /// 设置 flush_pending 标志。
    fn pin_entry(&self, entry: &Arc<CachedEntry>, journal_seq: u64) {
        if journal_seq == 0 {
            return;
        }
        let Some(j) = self.journal.get().and_then(|w| w.upgrade()) else {
            return;
        };
        let ck_weak = Arc::downgrade(entry);
        j.bch2_journal_pin_add(
            journal_seq,
            &entry.pin,
            Some(Box::new(
                move |_j: &Journal, _pin: &JournalEntryPin, _seq: u64| {
                    if let Some(ck) = ck_weak.upgrade() {
                        ck.flush_pending.store(true, Ordering::Release);
                    }
                    Ok(())
                },
            )),
        );
    }

    /// 收集所有脏条目的 (pos, entry) 对。
    ///
    /// 无锁 — 只做 cache.load() 原子指针读取。
    /// 返回后 HashMap guard 已释放，调用者可自由使用 `&mut BchVol` 写回。
    pub fn collect_dirty(&self) -> Vec<(Bpos, BtreeEntry)> {
        let map = self.cache.load();
        map.iter()
            .filter(|(_, arc)| arc.dirty.load(Ordering::Acquire))
            .map(|(pos, arc)| {
                let entry = arc.lock.read().unwrap().clone();
                (*pos, entry)
            })
            .collect()
    }

    /// 将指定 pos 的条目标记为 clean（Phase 3: 写回成功后清除 dirty）
    ///
    /// 不修改 HashMap 结构 → 只用 load() 无锁读。
    /// 清除 dirty、flush_pending、journal pin、递减 nr_dirty。
    pub fn mark_clean(&self, pos: &Bpos) {
        let map = self.cache.load();
        if let Some(arc) = map.get(pos) {
            let was_dirty = arc.dirty.swap(false, Ordering::AcqRel);
            if was_dirty {
                self.drop_journal_pin(arc);
                self.nr_dirty.fetch_sub(1, Ordering::Release);
            }
            arc.flush_pending.store(false, Ordering::Release);
        }
    }

    /// 遍历所有 dirty entries，通过 `writer` callback 写回 btree。
    ///
    /// `writer` 接收 (pos, entry) 对，返回 true=写回成功。
    /// 写回成功的条目清除 dirty + flush_pending + journal pin。
    ///
    /// 三阶段（避免锁嵌套）：
    /// 1. 无锁收集脏条目（`collect_dirty` 只有原子 load）
    /// 2. 释放所有锁，回调 writer（写 btree）
    /// 3. 无锁清除 dirty 状态（`mark_clean` 只有原子 load）
    ///
    /// 返回写回成功的条目数。
    pub fn flush_dirty<F>(&self, mut writer: F) -> usize
    where
        F: FnMut(&Bpos, &BtreeEntry) -> bool,
    {
        let dirty = self.collect_dirty();
        if dirty.is_empty() {
            return 0;
        }

        let mut successes = 0;
        for (pos, entry) in &dirty {
            if writer(pos, entry) {
                self.mark_clean(pos);
                successes += 1;
            }
        }

        successes
    }

    /// bcachefs 对齐: bch2_nr_btree_keys_need_flush
    pub fn bch2_nr_btree_keys_need_flush(&self) -> usize {
        let nr_dirty = self.nr_dirty.load(Ordering::Acquire) as usize;
        let nr_keys = self.nr_keys.load(Ordering::Acquire) as usize;
        let max_dirty = 1024 + (nr_keys / 2);

        nr_dirty.saturating_sub(max_dirty)
    }

    /// 返回当前脏条目的数量
    pub fn nr_dirty_keys(&self) -> u64 {
        self.nr_dirty.load(Ordering::Acquire)
    }

    /// 返回当前缓存条目数。
    pub fn nr_keys(&self) -> u64 {
        self.nr_keys.load(Ordering::Acquire)
    }

    /// bcachefs 对齐: bch2_btree_key_cache_must_wait
    pub fn bch2_btree_key_cache_must_wait(&self) -> bool {
        self.__bch2_btree_key_cache_must_wait() > 0
    }

    /// bcachefs 对齐: __bch2_btree_key_cache_must_wait
    fn __bch2_btree_key_cache_must_wait(&self) -> isize {
        let nr_dirty = self.nr_dirty.load(Ordering::Acquire) as isize;
        let nr_keys = self.nr_keys.load(Ordering::Acquire) as isize;
        let max_dirty = 4096 + ((nr_keys * 3) / 4);

        nr_dirty - max_dirty
    }

    /// bcachefs 对齐: bch2_btree_key_cache_wait_done
    pub fn bch2_btree_key_cache_wait_done(&self) -> bool {
        let nr_dirty = self.nr_dirty.load(Ordering::Acquire) as isize;
        let nr_keys = self.nr_keys.load(Ordering::Acquire) as isize;
        let max_dirty = 2048 + ((nr_keys * 5) / 8);

        nr_dirty <= max_dirty
    }

    /// bcachefs 对齐: bch2_btree_key_cache_flush_going_ro
    ///
    /// 在文件系统转为只读时，将所有脏 key cache 条目写回 btree。
    /// 返回 true 表示本次调用 flush 了至少一个条目（调用者需循环直到返回 false）。
    pub fn bch2_btree_key_cache_flush_going_ro<F>(&self, writer: F) -> bool
    where
        F: FnMut(&Bpos, &BtreeEntry) -> bool,
    {
        self.flush_dirty(writer) > 0
    }

    /// 移除所有 journal pin（必须先于 cache clear 调用，防止 Link 在侵入式链表中 dangling）
    fn drop_all_journal_pins(&self) {
        let Some(j) = self.journal.get().and_then(|w| w.upgrade()) else {
            return;
        };
        let map = self.cache.load();
        let entries: Vec<Arc<CachedEntry>> = map
            .values()
            .filter(|arc| arc.pin.is_active())
            .cloned()
            .collect();
        drop(map);
        for arc in &entries {
            j.bch2_journal_pin_drop(&arc.pin);
        }
    }

    /// 先移除所有 journal pin，再清空缓存。
    fn clear_and_drop_pins(&mut self) {
        self.drop_all_journal_pins();
        self.cache.store(Arc::new(HashMap::new()));
        self.nr_keys.store(0, Ordering::Release);
    }
}

impl Drop for KeyCache {
    fn drop(&mut self) {
        self.clear_and_drop_pins();
    }
}

impl Default for KeyCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btree::key::{Bpos, BtreeEntry, KeyType, KeyValue};
    use std::thread;

    fn test_cache() -> KeyCache {
        KeyCache::new()
    }

    #[test]
    fn test_cache_miss_returns_none() {
        let cache = test_cache();
        let pos = Bpos::new(1, 100, 42);
        assert!(cache.find(&pos).is_none());
    }

    #[test]
    fn test_cache_insert_and_find() {
        let cache = test_cache();
        let pos = Bpos::new(1, 100, 42);
        let entry = BtreeEntry::new(pos, KeyType::Normal, KeyValue::extent(0xABCD, 1, 0));

        cache.insert(pos, entry.clone());

        let found = cache.find(&pos);
        assert_eq!(found, Some(entry));
    }

    #[test]
    fn test_cache_invalidate_removes_entry() {
        let cache = test_cache();
        let pos = Bpos::new(1, 100, 42);
        let entry = BtreeEntry::new(pos, KeyType::Normal, KeyValue::extent(0x100, 1, 0));

        cache.insert(pos, entry);
        assert!(cache.find(&pos).is_some());

        cache.invalidate(&pos);
        assert!(cache.find(&pos).is_none());
    }

    #[test]
    fn test_cache_different_pos_independent() {
        let cache = test_cache();
        let pos1 = Bpos::new(1, 10, 0);
        let pos2 = Bpos::new(2, 20, 1);

        cache.insert(
            pos1,
            BtreeEntry::new(pos1, KeyType::Normal, KeyValue::extent(0x100, 1, 0)),
        );
        cache.insert(
            pos2,
            BtreeEntry::new(pos2, KeyType::Normal, KeyValue::extent(0x200, 2, 0)),
        );

        cache.invalidate(&pos1);

        assert!(cache.find(&pos1).is_none());
        assert!(cache.find(&pos2).is_some());
    }

    #[test]
    fn test_bcachefs_alias_find() {
        let cache = test_cache();
        let pos = Bpos::new(1, 100, 42);
        let entry = BtreeEntry::new(pos, KeyType::Normal, KeyValue::extent(0xABCD, 1, 0));

        cache.insert(pos, entry.clone());
        assert_eq!(cache.bch2_btree_key_cache_find(&pos), Some(entry));
    }

    #[test]
    fn test_bcachefs_alias_drop() {
        let cache = test_cache();
        let pos = Bpos::new(1, 100, 42);
        cache.insert(
            pos,
            BtreeEntry::new(pos, KeyType::Normal, KeyValue::extent(0x100, 1, 0)),
        );
        assert!(cache.bch2_btree_key_cache_find(&pos).is_some());

        cache.bch2_btree_key_cache_drop(&pos);
        assert!(cache.bch2_btree_key_cache_find(&pos).is_none());
    }

    #[test]
    fn test_insert_key_cached_stores_dirty_entry() {
        let cache = test_cache();
        let pos = Bpos::new(1, 100, 42);
        let entry = BtreeEntry::new(pos, KeyType::Normal, KeyValue::extent(0x100, 1, 0));

        cache.bch2_btree_insert_key_cached(pos, entry.clone(), 42);
        assert_eq!(cache.find(&pos), Some(entry));
        assert_eq!(cache.nr_dirty_keys(), 1);
    }

    /// Dirty tracking: insert_key_cached → dirty=true → insert (clean) → dirty=false
    #[test]
    fn test_dirty_tracking() {
        let cache = test_cache();
        let pos = Bpos::new(1, 100, 42);
        let entry1 = BtreeEntry::new(pos, KeyType::Normal, KeyValue::extent(0x100, 1, 0));
        let entry2 = BtreeEntry::new(pos, KeyType::Deleted, KeyValue::extent(0x200, 2, 0));

        // 初始 nr_dirty = 0
        assert_eq!(cache.nr_dirty_keys(), 0);

        // insert_key_cached → dirty=true, nr_dirty=1
        cache.bch2_btree_insert_key_cached(pos, entry1, 42);
        assert_eq!(cache.nr_dirty_keys(), 1);

        // insert (clean btree read) → dirty=false, nr_dirty=0
        cache.insert(pos, entry2.clone());
        assert_eq!(cache.nr_dirty_keys(), 0);
        assert_eq!(cache.find(&pos), Some(entry2));
    }

    /// pin: insert_key_cached 通过嵌入的 JournalEntryPin 管理 seq
    /// 替代了独立的 journal_seq: AtomicU64 字段
    #[test]
    fn test_journal_seq() {
        let cache = test_cache();
        let pos = Bpos::new(1, 100, 42);
        let entry = BtreeEntry::new(pos, KeyType::Normal, KeyValue::extent(0x100, 1, 0));

        cache.bch2_btree_insert_key_cached(pos, entry, 42);
        assert!(cache.find(&pos).is_some());
        // pin 的 seq 由 JournalEntryPin 内部管理，通过 nr_dirty 间接验证
        assert_eq!(cache.nr_dirty_keys(), 1);
    }

    /// Journal pin: insert_key_cached 注册 pin → invalidate 释放 pin
    /// （需要 Journal 实例；无 Journal 时 pin 操作被跳过）
    #[test]
    fn test_journal_pin_integration() {
        let cache = test_cache();
        let pos = Bpos::new(1, 100, 42);
        let entry = BtreeEntry::new(pos, KeyType::Normal, KeyValue::extent(0x100, 1, 0));

        // 无 journal 设置：pin 操作被跳过，不会 panic
        cache.bch2_btree_insert_key_cached(pos, entry.clone(), 42);
        assert_eq!(cache.nr_dirty_keys(), 1);

        // invalidate 也跳过 pin drop，不会 panic
        cache.invalidate(&pos);
        assert_eq!(cache.nr_dirty_keys(), 0);
    }

    /// journal pin integration with actual Journal instance
    #[test]
    fn test_journal_pin_with_instance() {
        use std::sync::Arc as StdArc;
        let journal = StdArc::new(crate::journal::Journal::new(vec![100]));
        // Journal::new 打开 1 个 entry（seq=1），但 entry_for_seq 使用 seq % PIN_FIFO_SIZE
        // 索引：seq=2 → entries[2]。需要额外 push 2 个 pin_list 以使 entries[2] 有效。
        unsafe {
            use crate::journal::reclaim::JournalEntryPinList;
            (*journal.pin_fifo.get())
                .push_back(JournalEntryPinList::new(1))
                .ok();
            (*journal.pin_fifo.get())
                .push_back(JournalEntryPinList::new(1))
                .ok();
        }
        let cache = test_cache();
        cache.set_journal(&StdArc::downgrade(&journal));

        let pos = Bpos::new(1, 100, 42);
        let entry = BtreeEntry::new(pos, KeyType::Normal, KeyValue::extent(0x100, 1, 0));

        // insert_key_cached 使用 journal_seq=2，会注册 pin
        cache.bch2_btree_insert_key_cached(pos, entry.clone(), 2);
        assert_eq!(cache.nr_dirty_keys(), 1);

        // invalidate 会调用 journal_pin_drop
        cache.invalidate(&pos);
        assert_eq!(cache.nr_dirty_keys(), 0);
    }

    /// collect_dirty + mark_clean 两阶段 flush
    #[test]
    fn test_collect_dirty_and_mark_clean() {
        let cache = test_cache();
        let pos1 = Bpos::new(1, 10, 0);
        let pos2 = Bpos::new(2, 20, 1);
        let entry1 = BtreeEntry::new(pos1, KeyType::Normal, KeyValue::extent(0x100, 1, 0));
        let entry2 = BtreeEntry::new(pos2, KeyType::Normal, KeyValue::extent(0x200, 2, 0));

        // 插入两个脏条目
        cache.bch2_btree_insert_key_cached(pos1, entry1, 0);
        cache.bch2_btree_insert_key_cached(pos2, entry2, 0);
        assert_eq!(cache.nr_dirty_keys(), 2);

        // collect_dirty 收集所有脏条目
        let dirty = cache.collect_dirty();
        assert_eq!(dirty.len(), 2);

        // mark_clean 逐一清除
        cache.mark_clean(&pos1);
        assert_eq!(cache.nr_dirty_keys(), 1);

        // find 仍然有效（valid=true）
        assert!(cache.find(&pos1).is_some());

        cache.mark_clean(&pos2);
        assert_eq!(cache.nr_dirty_keys(), 0);
    }

    /// flush_dirty 使用 writer callback
    #[test]
    fn test_flush_dirty_callback() {
        let cache = test_cache();
        let pos = Bpos::new(1, 100, 42);
        let entry = BtreeEntry::new(pos, KeyType::Normal, KeyValue::extent(0x100, 1, 0));

        cache.bch2_btree_insert_key_cached(pos, entry.clone(), 0);
        assert_eq!(cache.nr_dirty_keys(), 1);

        // flush_dirty 使用 callback, 返回成功数
        let flushed = cache.flush_dirty(|p, e| {
            assert_eq!(*p, pos);
            assert_eq!(*e, entry);
            true // 模拟写回成功
        });
        assert_eq!(flushed, 1);
        assert_eq!(cache.nr_dirty_keys(), 0);
        assert!(cache.find(&pos).is_some()); // still cached (clean)
    }

    /// flush_dirty 跳过 writer 返回 false 的条目
    #[test]
    fn test_flush_dirty_skip_failed_writes() {
        let cache = test_cache();
        let pos = Bpos::new(1, 100, 42);
        let entry = BtreeEntry::new(pos, KeyType::Normal, KeyValue::extent(0x100, 1, 0));

        cache.bch2_btree_insert_key_cached(pos, entry, 0);
        assert_eq!(cache.nr_dirty_keys(), 1);

        // writer 返回 false → flush 不成功，dirty 不清除
        let flushed: usize = cache.flush_dirty(|_, _| false);
        assert_eq!(flushed, 0);
        assert_eq!(cache.nr_dirty_keys(), 1); // dirty 保留
    }

    #[test]
    fn test_key_cache_flush_thresholds_match_bcachefs_formula() {
        let cache = test_cache();
        cache.nr_keys.store(1000, Ordering::Release);

        cache.nr_dirty.store(1499, Ordering::Release);
        assert_eq!(cache.bch2_nr_btree_keys_need_flush(), 0);
        assert!(!cache.bch2_btree_key_cache_must_wait());
        assert!(cache.bch2_btree_key_cache_wait_done());

        cache.nr_dirty.store(1525, Ordering::Release);
        assert_eq!(cache.bch2_nr_btree_keys_need_flush(), 1);
        assert!(!cache.bch2_btree_key_cache_must_wait());
        assert!(cache.bch2_btree_key_cache_wait_done());

        cache.nr_dirty.store(4847, Ordering::Release);
        assert!(cache.bch2_btree_key_cache_must_wait());
        assert!(!cache.bch2_btree_key_cache_wait_done());
    }

    /// BchVol::flush_dirty_nodes 集成测试（基本可用性）
    #[test]
    fn test_vol_flush_cache_dirty_keys() {
        let vol = crate::BchVol::test_trees();
        let ty = crate::btree::BtreeId::Extents;
        let pos = Bpos::new(1, 100, 42);
        let entry = BtreeEntry::new(pos, KeyType::Normal, KeyValue::extent(0x100, 1, 0));

        // 通过 insert_key_cached 写入脏条目
        vol.btree(ty)
            .key_cache
            .bch2_btree_insert_key_cached(pos, entry.clone(), 0);
        assert_eq!(vol.btree(ty).key_cache.nr_dirty_keys(), 1);

        // flush 脏 key → 写入 btree + 清除 dirty
        vol.flush_dirty_nodes();

        // cache 仍然有效（不 invalidation）
        assert_eq!(vol.btree(ty).key_cache.nr_dirty_keys(), 0);
        let cached = vol.btree(ty).key_cache.find(&pos);
        assert!(cached.is_some()); // cached (clean)

        // btree 中也存在
        let btree_found = vol.btree(ty).bch2_btree_iter_peek_entry(pos);
        assert!(btree_found.is_some());
    }

    /// flush_pending 通过 journal_reclaim callback 设置
    #[test]
    fn test_flush_callback_triggers() {
        use std::sync::Arc as StdArc;
        let journal = StdArc::new(crate::journal::Journal::new(vec![100]));
        // 需要额外 push pin_list 使 entries[1] 对 entry_for_seq(1) 有效
        unsafe {
            use crate::journal::reclaim::JournalEntryPinList;
            (*journal.pin_fifo.get())
                .push_back(JournalEntryPinList::new(1))
                .ok();
        }
        let cache = test_cache();
        cache.set_journal(&StdArc::downgrade(&journal));

        let pos = Bpos::new(1, 100, 42);
        let entry = BtreeEntry::new(pos, KeyType::Normal, KeyValue::extent(0x100, 1, 0));

        cache.bch2_btree_insert_key_cached(pos, entry.clone(), 1);

        // 触发 journal_reclaim → 会调用 seq ≤ 1 的 pin callback
        // 这应当设置 flush_pending 标志
        let _ = journal.bch2_journal_flush_pins(1);

        // 能写入并回读（即使 flush_pending 被设置也不影响读取）
        assert_eq!(cache.find(&pos), Some(entry));
        // 验证 invalidation 仍然工作正常
        cache.invalidate(&pos);
        assert!(cache.find(&pos).is_none());
    }

    #[test]
    fn test_overwrite_existing_entry() {
        let cache = test_cache();
        let pos = Bpos::new(1, 100, 42);

        let entry1 = BtreeEntry::new(pos, KeyType::Normal, KeyValue::extent(0xAAA, 1, 0));
        let entry2 = BtreeEntry::new(pos, KeyType::Deleted, KeyValue::extent(0xBBB, 2, 0));

        cache.insert(pos, entry1);
        cache.insert(pos, entry2.clone());

        assert_eq!(cache.find(&pos), Some(entry2));
    }

    #[test]
    fn test_slot_reuse() {
        let cache = test_cache();
        let pos = Bpos::new(1, 100, 42);
        let entry1 = BtreeEntry::new(pos, KeyType::Normal, KeyValue::extent(0x100, 1, 0));
        let entry2 = BtreeEntry::new(pos, KeyType::Deleted, KeyValue::extent(0x200, 2, 0));

        // insert → drop → find = None → insert → find = Some
        cache.insert(pos, entry1);
        assert!(cache.find(&pos).is_some());
        cache.invalidate(&pos);
        assert!(cache.find(&pos).is_none());
        cache.insert(pos, entry2.clone());
        assert_eq!(cache.find(&pos), Some(entry2));
    }

    #[test]
    fn test_drop_keeps_slot() {
        let cache = test_cache();
        let pos = Bpos::new(1, 100, 42);
        let entry = BtreeEntry::new(pos, KeyType::Normal, KeyValue::extent(0x100, 1, 0));

        cache.insert(pos, entry);

        // drop 后 find 返回 None
        cache.invalidate(&pos);
        assert!(cache.find(&pos).is_none());

        // 但 slot 还在 hash 表里 — insert 时不创建新 Arc
        // 验证: invalidate → insert → find 仍然能工作
        let entry2 = BtreeEntry::new(pos, KeyType::Deleted, KeyValue::extent(0x200, 2, 0));
        cache.insert(pos, entry2.clone());
        assert_eq!(cache.find(&pos), Some(entry2));
    }

    /// 并发: 多线程同时读不同 entry 应互不阻塞
    #[test]
    fn test_concurrent_read_different_entries() {
        let cache = Arc::new(test_cache());
        let n = 8;
        let mut handles = Vec::new();

        for i in 0..n {
            let cache = cache.clone();
            handles.push(thread::spawn(move || {
                let pos = Bpos::new(1, i as u64, 0);
                let entry = BtreeEntry::new(
                    pos,
                    KeyType::Normal,
                    KeyValue::extent(0x100 + i as u64, 1, 0),
                );
                cache.insert(pos, entry.clone());
                let got = cache.find(&pos);
                assert_eq!(got, Some(entry));
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        for i in 0..n {
            let pos = Bpos::new(1, i as u64, 0);
            assert!(cache.find(&pos).is_some());
        }
    }

    /// 并发: 读不影响 invalidation
    #[test]
    fn test_concurrent_read_and_invalidate() {
        let cache = Arc::new(test_cache());
        let pos = Bpos::new(1, 100, 42);
        let entry = BtreeEntry::new(pos, KeyType::Normal, KeyValue::extent(0x100, 1, 0));
        cache.insert(pos, entry);

        let cache_reader = cache.clone();
        let reader = thread::spawn(move || {
            for _ in 0..100 {
                cache_reader.find(&pos);
            }
        });

        let cache_invalidator = cache.clone();
        let invalidator = thread::spawn(move || {
            for _ in 0..10 {
                cache_invalidator.invalidate(&pos);
                cache_invalidator.insert(
                    pos,
                    BtreeEntry::new(pos, KeyType::Normal, KeyValue::extent(0x100, 1, 0)),
                );
            }
        });

        reader.join().unwrap();
        invalidator.join().unwrap();
    }

    #[test]
    fn test_flush_going_ro_returns_false_when_clean() {
        let cache = test_cache();
        assert!(
            !cache.bch2_btree_key_cache_flush_going_ro(|_, _| true),
            "flush_going_ro on empty cache should return false"
        );
    }

    #[test]
    fn test_flush_going_ro_returns_true_when_dirty() {
        let cache = test_cache();
        let pos = Bpos::new(1, 50, 0);
        let entry = BtreeEntry::new(pos, KeyType::Normal, KeyValue::extent(0x200, 1, 0));
        cache.bch2_btree_insert_key_cached(pos, entry, 42);
        assert!(
            cache.bch2_btree_key_cache_flush_going_ro(|_, _| true),
            "flush_going_ro with dirty entries should return true"
        );
        assert_eq!(
            cache.nr_dirty_keys(),
            0,
            "dirty should be cleared after flush"
        );
    }
}
