//! OpenBucket — bcachefs 对齐的开放桶引用计数
//!
//! 对应 bcachefs `fs/alloc/types.h:65-91` `struct open_bucket`。
//!
//! ## 作用
//!
//! 1. 追踪正在分配的桶（防止 TOCTOU 双重分配）
//! 2. 提供引用计数（`pin`），在 extent 写入 btree 后释放
//! 3. 通过 hash 表实现 O(1) `is_open()` 查询
//!
//! ## bcachefs 对照
//!
//! | bcachefs | subvol |
//! |----------|----------|
//! | `open_bucket_idx_t` (u16) | `OpenBucketIdx` (u16) |
//! | `freelist_lock` (spinlock) | `freelist_lock` (Mutex) |
//! | `open_buckets_freelist` | `freelist_head` |
//! | `open_buckets_nr_free` | `nr_free` |
//! | `open_buckets[OPEN_BUCKETS_COUNT]` | `entries: [Mutex<OpenBucket>; N]` |
//! | `open_buckets_hash[OPEN_BUCKETS_COUNT]` | `hash_slots: [AtomicU16; N]` |
//! | `ob->dev` | `ob.dev` |
//! | `ob->bucket` | `ob.bucket` |
//! | `ob->pin` (atomic_t) | `ob.pin` (AtomicU32) |
//! | `ob->freelist` (链接) | `ob.freelist` (next free idx) |
//! | `ob->hash` (链接) | `ob.hash` (next hash chain idx) |
//!
//! 参考: `fs/alloc/types.h:199-203`, `fs/alloc/foreground.c:154-209`

use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::Mutex;

use crate::types::StorageError;

/// 开放桶池大小
///
/// 对齐本地 bcachefs `OPEN_BUCKETS_COUNT`。
/// 必须为 2 的幂，用于 hash 表槽位计算。
pub const OPEN_BUCKETS_COUNT: usize = 4096;

/// 开放桶索引（0 = null sentinel，与 bcachefs 一致）
pub type OpenBucketIdx = u16;

/// Null sentinel 值（bcachefs 0 哨兵）
const NULL_IDX: OpenBucketIdx = 0;

/// 单个开放桶条目 — 对应 bcachefs `struct open_bucket`
///
/// 每个条目记录一个正在被写入的桶。分配时创建（pin=1），
/// extent 写入 btree 后 `put()` 释放 pin→0。
#[derive(Debug)]
pub struct OpenBucket {
    /// 引用计数（bcachefs `atomic_t pin`）
    /// - 初始 = 1（分配时设置）
    /// - 复用同一桶写更多数据 → atomic_inc
    /// - 写入完成 → atomic_dec，归零时回收
    pub pin: AtomicU32,

    /// 数据有效（bcachefs `valid:1`）
    pub valid: AtomicBool,

    /// 设备索引（bcachefs `dev`）
    pub dev: AtomicU8,

    /// 设备内 bucket number（bcachefs `bucket`）
    pub bucket: AtomicU64,

    /// 桶内剩余可用块数（对齐 bcachefs `struct open_bucket.sectors_free`）
    ///
    /// 分配时初始化为对应设备的 `ca->mi.bucket_size`，每次写入后递减。
    /// 当为零时表示桶已满，应从写点分离。
    /// 纯内存字段，不参与序列化。
    pub sectors_free: AtomicU32,

    /// freelist 链接（指向下一个空闲条目）
    /// 0 = null（链表尾）
    pub freelist: AtomicU16,

    /// hash 链链接（指向 hash 冲突链下一个条目）
    /// 0 = null（链尾）
    pub hash: AtomicU16,

    /// 是否在 partial 列表中（bcachefs 无直接对应，由 partial 列表维护）
    /// 当桶仍有空闲空间但已从写点 ptrs 中移除时设为 true，
    /// 在 take_from_partial 取出后或 put 释放后设为 false。
    pub on_partial_list: AtomicBool,

    /// 分配时的 gen 快照（来自 AllocGroup.gens[]，bcachefs `ob->gen`）
    ///
    /// 用于 stale 指针检测：将 ob.gen 与当前 bucket 的 gen（gens[]）
    /// 比对。如果不一致，说明桶已被回收并重新分配，当前引用的指针已失效。
    /// 此字段在 alloc_locked() 中设置，在 put() 中归零。
    pub gen: AtomicU8,
}

impl OpenBucket {
    fn new() -> Self {
        Self {
            pin: AtomicU32::new(0),
            valid: AtomicBool::new(false),
            dev: AtomicU8::new(0),
            bucket: AtomicU64::new(0),
            sectors_free: AtomicU32::new(0),
            freelist: AtomicU16::new(0),
            hash: AtomicU16::new(0),
            on_partial_list: AtomicBool::new(false),
            gen: AtomicU8::new(0),
        }
    }
}

/// 开放桶池 — 对应 bcachefs `bch_fs_allocator` 中的 open_buckets 字段组
///
/// 包含：
/// - 固定大小的 `OpenBucket` 数组（索引从 1 开始，0 为哨兵）
/// - freelist 链表头 + 空闲计数
/// - hash 表（快速 `is_open()` 查询）
/// - `freelist_lock` 保护所有 freelist + hash 操作
pub struct BchOpenBuckets {
    /// 固定大小条目池（索引 1..=OPEN_BUCKETS_COUNT，0 保留为哨兵）
    entries: Vec<OpenBucket>,

    /// Freelist 链表头（指向第一个空闲条目，0 = 空）
    /// bcachefs `open_buckets_freelist`
    freelist_head: Mutex<OpenBucketIdx>,

    /// 空闲条目计数（bcachefs `open_buckets_nr_free`）
    pub nr_free: AtomicU32,

    /// Hash 表槽位 — 每个槽存链表头索引（0 = 空链）
    /// bcachefs `open_buckets_hash[OPEN_BUCKETS_COUNT]`
    hash_slots: Vec<AtomicU16>,

    /// 保护 freelist + hash 操作的自旋锁（bcachefs `freelist_lock`）
    freelist_lock: Mutex<()>,

    /// Partial 列表 — 有剩余空间但已从写点移除的桶
    /// bcachefs 对应: open_buckets_partial 链表
    /// 与 freelist_lock 受同一个 Mutex 保护
    partial_list: Mutex<Vec<OpenBucketIdx>>,

    /// Partial 列表中的条目计数
    pub nr_partial: AtomicU32,
}

impl BchOpenBuckets {
    /// 创建一个新的开放桶池
    ///
    /// 初始化所有条目入 freelist（索引 1..N，0 为哨兵）。
    pub fn new() -> Self {
        // 索引 0 保留为 null sentinel，entries[0] 不使用
        let mut entries = Vec::with_capacity(OPEN_BUCKETS_COUNT + 1);
        entries.push(OpenBucket::new()); // 索引 0 — null sentinel

        for _ in 0..OPEN_BUCKETS_COUNT {
            entries.push(OpenBucket::new());
        }

        // 构建初始 freelist：1 → 2 → 3 → ... → N → 0
        for i in 1..OPEN_BUCKETS_COUNT {
            entries[i]
                .freelist
                .store((i + 1) as OpenBucketIdx, Ordering::Relaxed);
        }
        entries[OPEN_BUCKETS_COUNT]
            .freelist
            .store(0, Ordering::Relaxed);

        let hash_slots = (0..OPEN_BUCKETS_COUNT).map(|_| AtomicU16::new(0)).collect();

        Self {
            entries,
            freelist_head: Mutex::new(1 as OpenBucketIdx), // 第一个实际条目
            nr_free: AtomicU32::new(OPEN_BUCKETS_COUNT as u32),
            hash_slots,
            freelist_lock: Mutex::new(()),
            partial_list: Mutex::new(Vec::new()),
            nr_partial: AtomicU32::new(0),
        }
    }

    /// 分配一个开放桶条目（bcachefs `bch2_open_bucket_alloc`）
    ///
    /// 对应 `fs/alloc/foreground.c:198-209`：
    /// 1. 从 freelist 头弹出
    /// 2. 设置 pin=1, valid=true, dev, bucket
    /// 3. 设置 sectors_free（桶内剩余可用块数）
    /// 4. 加入 hash 表
    /// 5. nr_free--
    ///
    /// 必须在 `freelist_lock` 保护下调用。
    fn alloc_locked(
        &self,
        dev: u8,
        bucket: u64,
        sectors_free: u32,
        gen: u8,
    ) -> Result<OpenBucketIdx, StorageError> {
        let head = *self.freelist_head.lock().unwrap();
        if head == NULL_IDX {
            return Err(StorageError::AddressSpaceExhausted {
                max_raw_addr: OPEN_BUCKETS_COUNT as u64,
            });
        }

        let idx = head;
        let entry = &self.entries[idx as usize];

        // 从 freelist 摘下
        let next = entry.freelist.load(Ordering::Relaxed);
        *self.freelist_head.lock().unwrap() = next;

        // 设置字段
        entry.pin.store(1, Ordering::Release);
        entry.valid.store(true, Ordering::Release);
        entry.dev.store(dev, Ordering::Release);
        entry.bucket.store(bucket, Ordering::Release);
        entry.sectors_free.store(sectors_free, Ordering::Release);
        entry.gen.store(gen, Ordering::Release);

        // 加入 hash 表（bcachefs `open_bucket_hash_add`）
        // 必须在字段设置完成后，确保其他线程通过 hash 读到的是完整初始化的条目
        let slot_idx = hash_slot(dev, bucket);
        let slot = &self.hash_slots[slot_idx];
        let head = slot.load(Ordering::Acquire);
        entry.hash.store(head, Ordering::Release);
        slot.store(idx, Ordering::Release);

        self.nr_free.fetch_sub(1, Ordering::Relaxed);

        Ok(idx)
    }

    /// 分配一个开放桶条目（对外接口，自动加 freelist_lock）
    ///
    /// 对应 bcachefs `__try_alloc_bucket()` 中的 `bch2_open_bucket_alloc`
    pub fn alloc(
        &self,
        dev: u8,
        bucket: u64,
        sectors_free: u32,
        gen: u8,
    ) -> Result<OpenBucketIdx, StorageError> {
        let _guard = self.freelist_lock.lock().unwrap();
        self.alloc_locked(dev, bucket, sectors_free, gen)
    }

    /// 释放开放桶条目（bcachefs `__bch2_open_bucket_put`）
    ///
    /// 对应 `fs/alloc/foreground.c:154-183`：
    /// 1. 标记 valid=false
    /// 2. 从 hash 表移除
    /// 3. 放回 freelist
    /// 4. nr_free++
    pub fn put(&self, idx: OpenBucketIdx) {
        if idx == NULL_IDX || idx as usize >= self.entries.len() {
            return;
        }

        let _guard = self.freelist_lock.lock().unwrap();
        let entry = &self.entries[idx as usize];

        // 标记无效
        entry.valid.store(false, Ordering::Release);
        entry.pin.store(0, Ordering::Release);
        entry.on_partial_list.store(false, Ordering::Release);

        // 从 hash 表移除
        let slot_idx = hash_slot(
            entry.dev.load(Ordering::Acquire),
            entry.bucket.load(Ordering::Acquire),
        );
        let slot = &self.hash_slots[slot_idx];

        // 遍历 hash 链找到本条目并摘下
        let mut prev: OpenBucketIdx = 0;
        let mut cur = slot.load(Ordering::Acquire);
        while cur != NULL_IDX && cur != idx {
            prev = cur;
            cur = self.entries[cur as usize].hash.load(Ordering::Acquire);
        }

        if cur == idx {
            let next = self.entries[idx as usize].hash.load(Ordering::Acquire);
            if prev == NULL_IDX {
                // 在链表头
                slot.store(next, Ordering::Release);
            } else {
                // 在链中间/尾部
                self.entries[prev as usize]
                    .hash
                    .store(next, Ordering::Release);
            }
        }

        // 放回 freelist
        let head = *self.freelist_head.lock().unwrap();
        entry.freelist.store(head, Ordering::Release);
        *self.freelist_head.lock().unwrap() = idx;

        self.nr_free.fetch_add(1, Ordering::Relaxed);
    }

    /// 检查一个桶当前是否开放（bcachefs `bch2_bucket_is_open`）
    ///
    /// 对应 `fs/alloc/foreground.h:274-288`：
    /// 通过 hash 表 O(1) 查找，不锁 freelist_lock（纯读）。
    pub fn is_open(&self, dev: u8, bucket: u64) -> bool {
        self.lookup(dev, bucket).is_some()
    }

    /// 查找开放桶的索引（与 is_open 相同，但返回 idx）
    ///
    /// 用于分配/释放路径中获取 ob_idx 以便 inc/dec pin。
    pub fn lookup(&self, dev: u8, bucket: u64) -> Option<OpenBucketIdx> {
        let slot_idx = hash_slot(dev, bucket);
        let slot = self.hash_slots[slot_idx].load(Ordering::Acquire);

        let mut cur = slot;
        while cur != NULL_IDX {
            let entry = &self.entries[cur as usize];
            if entry.dev.load(Ordering::Acquire) == dev
                && entry.bucket.load(Ordering::Acquire) == bucket
                && entry.valid.load(Ordering::Acquire)
            {
                return Some(cur);
            }
            cur = entry.hash.load(Ordering::Acquire);
        }
        None
    }

    /// 增加指定条目的 pin 引用计数
    ///
    /// 对应 bcachefs `atomic_inc(&ob->pin)`。
    /// 调用者保证 idx 有效。
    pub fn inc_pin(&self, idx: OpenBucketIdx) {
        if idx != NULL_IDX {
            self.entries[idx as usize]
                .pin
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    /// 减少指定条目的 pin 引用计数，归零时自动 put
    ///
    /// 对应 bcachefs `atomic_dec_and_test(&ob->pin)` 后的路径。
    /// 返回 true 表示条目已回收（pin 归零且已 put）。
    pub fn dec_pin_and_put(&self, idx: OpenBucketIdx) -> bool {
        if idx == NULL_IDX {
            return false;
        }
        let prev = self.entries[idx as usize]
            .pin
            .fetch_sub(1, Ordering::Release);
        if prev == 1 {
            // pin 归零 → 回收
            self.put(idx);
            true
        } else {
            false
        }
    }

    /// 获取当前开放桶数量
    pub fn nr_open(&self) -> u32 {
        OPEN_BUCKETS_COUNT as u32 - self.nr_free.load(Ordering::Relaxed)
    }

    /// 读取指定条目的引用（不检查 valid 状态，供内部使用）
    pub fn get_entry(&self, idx: OpenBucketIdx) -> Option<&OpenBucket> {
        if idx == NULL_IDX || (idx as usize) >= self.entries.len() {
            return None;
        }
        Some(&self.entries[idx as usize])
    }

    /// 消耗指定条目的空闲扇区
    ///
    /// 在复用 open_bucket 后调用，递减 `sectors_free` 防止同一桶被反复复用。
    /// 调用者需确保 `sectors` 不超过剩余值（`find_reusable` 已保证此前提）。
    ///
    /// # 参数
    ///
    /// * `idx` — 开放桶索引
    /// * `sectors` — 本次写入消耗的扇区数
    pub fn consume_free_sectors(&self, idx: OpenBucketIdx, sectors: u32) {
        if idx != NULL_IDX && (idx as usize) < self.entries.len() {
            self.entries[idx as usize]
                .sectors_free
                .fetch_sub(sectors, Ordering::Release);
        }
    }

    /// 将桶加入 partial 列表（bcachefs open_bucket_free_unused）
    ///
    /// 桶被从写点移除但仍有空闲空间时，加入 partial 列表供其他写点复用。
    pub fn add_to_partial(&self, idx: OpenBucketIdx) {
        if idx == NULL_IDX || (idx as usize) >= self.entries.len() {
            return;
        }
        let _guard = self.freelist_lock.lock().unwrap();
        let entry = &self.entries[idx as usize];
        if !entry.valid.load(Ordering::Acquire) {
            return;
        }
        entry.on_partial_list.store(true, Ordering::Release);
        self.partial_list.lock().unwrap().push(idx);
        self.nr_partial.fetch_add(1, Ordering::Relaxed);
    }

    /// 从 partial 列表取出一个桶（后进先出）
    ///
    /// 类似 bcachefs bucket_alloc_set_partial 从后往前遍历。
    /// 返回 (idx, dev, bucket) 元组，调用者应 consume_free_sectors。
    pub fn take_from_partial(&self) -> Option<(OpenBucketIdx, u8, u64)> {
        let _guard = self.freelist_lock.lock().unwrap();
        let mut list = self.partial_list.lock().unwrap();
        while let Some(idx) = list.pop() {
            let entry = &self.entries[idx as usize];
            if entry.valid.load(Ordering::Acquire) && entry.sectors_free.load(Ordering::Acquire) > 0
            {
                entry.on_partial_list.store(false, Ordering::Release);
                self.nr_partial.fetch_sub(1, Ordering::Relaxed);
                entry.pin.fetch_add(1, Ordering::AcqRel);
                if !entry.valid.load(Ordering::Acquire) {
                    entry.pin.fetch_sub(1, Ordering::Release);
                    continue;
                }
                let dev = entry.dev.load(Ordering::Acquire);
                let bucket = entry.bucket.load(Ordering::Acquire);
                return Some((idx, dev, bucket));
            }
            self.nr_partial.fetch_sub(1, Ordering::Relaxed);
        }
        None
    }

    /// 从 partial 列表中挑选一个满足空间需求的桶。
    ///
    /// 与 `take_from_partial()` 不同，此方法会在确认 `sectors_needed`
    /// 可满足后才真正移除条目并增加 pin，避免无效尝试导致 pin 泄漏。
    pub fn take_from_partial_matching(
        &self,
        dev: u8,
        sectors_needed: u32,
    ) -> Option<(OpenBucketIdx, u8, u64, u32)> {
        let _guard = self.freelist_lock.lock().unwrap();
        let mut list = self.partial_list.lock().unwrap();
        let mut idx = list.len();
        while idx > 0 {
            idx -= 1;
            let ob_idx = list[idx];
            let entry = &self.entries[ob_idx as usize];
            if !entry.valid.load(Ordering::Acquire) {
                list.swap_remove(idx);
                self.nr_partial.fetch_sub(1, Ordering::Relaxed);
                continue;
            }

            if entry.dev.load(Ordering::Acquire) != dev {
                continue;
            }

            let free = entry.sectors_free.load(Ordering::Acquire);
            if free < sectors_needed {
                continue;
            }

            let dev = entry.dev.load(Ordering::Acquire);
            let bucket = entry.bucket.load(Ordering::Acquire);
            list.swap_remove(idx);
            entry.on_partial_list.store(false, Ordering::Release);
            self.nr_partial.fetch_sub(1, Ordering::Relaxed);
            entry.pin.fetch_add(1, Ordering::AcqRel);
            if !entry.valid.load(Ordering::Acquire) {
                entry.pin.fetch_sub(1, Ordering::Release);
                continue;
            }
            return Some((ob_idx, dev, bucket, free));
        }
        None
    }

    /// 查找可复用的开放桶条目
    ///
    /// 扫描所有有效条目，返回第一个剩余空间 >= `sectors_needed` 的条目。
    /// 如果找到，pin 引用计数递增（防止在复用过程中被释放），
    /// 调用者在完成写入后应 `dec_pin_and_put()`。
    ///
    /// 对应 bcachefs 分配策略中复用部分填充 open_bucket 的路径。
    ///
    /// # 返回
    ///
    /// `Some((idx, dev, bucket))` — 可复用的开放桶信息
    /// `None` — 无合适条目
    pub fn find_reusable(&self, dev: u8, sectors_needed: u32) -> Option<(OpenBucketIdx, u8, u64)> {
        for (i, entry) in self.entries.iter().enumerate().skip(1) {
            let i = i as OpenBucketIdx;
            if !entry.valid.load(Ordering::Acquire) {
                continue;
            }
            if entry.dev.load(Ordering::Acquire) != dev {
                continue;
            }
            if entry.sectors_free.load(Ordering::Acquire) >= sectors_needed {
                // 递增 pin 防止被回收
                entry.pin.fetch_add(1, Ordering::AcqRel);
                // 双检：valid 在加 pin 后可能变化
                if !entry.valid.load(Ordering::Acquire) {
                    entry.pin.fetch_sub(1, Ordering::Release);
                    continue;
                }
                let dev = entry.dev.load(Ordering::Acquire);
                let bucket = entry.bucket.load(Ordering::Acquire);
                return Some((i, dev, bucket));
            }
        }
        None
    }
}

/// 计算 hash 槽位
///
/// 对应 bcachefs `open_bucket_hashslot()`（`foreground.h:266-272`），
/// 使用 `jhash_3words(dev, bucket, bucket>>32, 0)` 取模。
fn hash_slot(dev: u8, bucket: u64) -> usize {
    jhash_3words(dev as u32, bucket as u32, (bucket >> 32) as u32, 0) & (OPEN_BUCKETS_COUNT - 1)
}

#[inline]
fn rol32(word: u32, shift: u32) -> u32 {
    word.rotate_left(shift)
}

#[inline]
fn jhash_mix(mut a: u32, mut b: u32, mut c: u32) -> (u32, u32, u32) {
    a = a.wrapping_sub(c);
    a ^= rol32(c, 4);
    c = c.wrapping_add(b);

    b = b.wrapping_sub(a);
    b ^= rol32(a, 6);
    a = a.wrapping_add(c);

    c = c.wrapping_sub(b);
    c ^= rol32(b, 8);
    b = b.wrapping_add(a);

    a = a.wrapping_sub(c);
    a ^= rol32(c, 16);
    c = c.wrapping_add(b);

    b = b.wrapping_sub(a);
    b ^= rol32(a, 19);
    a = a.wrapping_add(c);

    c = c.wrapping_sub(b);
    c ^= rol32(b, 4);
    b = b.wrapping_add(a);

    (a, b, c)
}

#[inline]
fn jhash_final(mut a: u32, mut b: u32, mut c: u32) -> u32 {
    c ^= b;
    c = c.wrapping_sub(rol32(b, 14));
    a ^= c;
    a = a.wrapping_sub(rol32(c, 11));
    b ^= a;
    b = b.wrapping_sub(rol32(a, 25));
    c ^= b;
    c = c.wrapping_sub(rol32(b, 16));
    a ^= c;
    a = a.wrapping_sub(rol32(c, 4));
    b ^= a;
    b = b.wrapping_sub(rol32(a, 14));
    c ^= b;
    c = c.wrapping_sub(rol32(b, 24));
    c
}

fn jhash_3words(a: u32, b: u32, c: u32, initval: u32) -> usize {
    let mut a = a.wrapping_add(0xdead_beefu32.wrapping_add(12).wrapping_add(initval));
    let mut b = b.wrapping_add(0xdead_beefu32.wrapping_add(12).wrapping_add(initval));
    let mut c = c.wrapping_add(0xdead_beefu32.wrapping_add(12).wrapping_add(initval));
    (a, b, c) = jhash_mix(a, b, c);
    jhash_final(a, b, c) as usize
}

impl std::fmt::Debug for BchOpenBuckets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BchOpenBuckets")
            .field("nr_free", &self.nr_free)
            .field("nr_open", &self.nr_open())
            .field("capacity", &OPEN_BUCKETS_COUNT)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_pool_all_free() {
        let pool = BchOpenBuckets::new();
        assert_eq!(
            pool.nr_free.load(Ordering::Relaxed),
            OPEN_BUCKETS_COUNT as u32
        );
        assert_eq!(pool.nr_open(), 0);
    }

    #[test]
    fn test_alloc_and_put() {
        let pool = BchOpenBuckets::new();
        let idx = pool.alloc(0, 42, 256, 0).unwrap();
        assert!(idx != NULL_IDX);
        assert_eq!(pool.nr_open(), 1);
        assert_eq!(
            pool.nr_free.load(Ordering::Relaxed),
            OPEN_BUCKETS_COUNT as u32 - 1
        );

        // 验证条目内容
        let entry = pool.get_entry(idx).unwrap();
        assert_eq!(entry.pin.load(Ordering::Relaxed), 1);
        assert_eq!(entry.dev.load(Ordering::Relaxed), 0);
        assert_eq!(entry.bucket.load(Ordering::Relaxed), 42);

        // is_open 检查
        assert!(pool.is_open(0, 42));
        assert!(!pool.is_open(0, 43));

        // put 后释放
        pool.put(idx);
        assert!(!pool.is_open(0, 42));
        assert_eq!(pool.nr_open(), 0);
    }

    #[test]
    fn test_exhaustion() {
        let pool = BchOpenBuckets::new();
        // 分配全部
        let mut indices = Vec::new();
        for i in 0..OPEN_BUCKETS_COUNT as u32 {
            indices.push(pool.alloc(0, i as u64, 256, 0).unwrap());
        }
        assert_eq!(pool.nr_open(), OPEN_BUCKETS_COUNT as u32);

        // 再分配应失败
        let result = pool.alloc(0, 999, 256, 0);
        assert!(result.is_err());

        // 放回一个
        pool.put(indices[0]);
        let idx = pool.alloc(0, 999, 256, 0).unwrap();
        assert!(idx != NULL_IDX);
    }

    #[test]
    fn test_hash_collision() {
        let pool = BchOpenBuckets::new();
        // 分配多个桶，即使 hash 冲突也能正确 is_open
        let mut indices = Vec::new();
        for i in 0..10 {
            let idx = pool.alloc(i as u8, i as u64 * 100, 256, 0).unwrap();
            indices.push(idx);
        }
        // 全部应在 hash 表中
        for i in 0..10 {
            assert!(pool.is_open(i as u8, i as u64 * 100));
        }
        // 放回后不在表中
        for i in 0..5 {
            pool.put(indices[i]);
            assert!(!pool.is_open(i as u8, i as u64 * 100));
        }
        // 剩余的仍在
        for i in 5..10 {
            assert!(pool.is_open(i as u8, i as u64 * 100));
        }
    }

    #[test]
    fn test_pin_refcount() {
        let pool = BchOpenBuckets::new();
        let idx = pool.alloc(1, 100, 256, 0).unwrap();
        assert!(pool.is_open(1, 100));

        // inc pin
        pool.inc_pin(idx);
        // dec 但不归零
        let released = pool.dec_pin_and_put(idx);
        assert!(!released);
        assert!(pool.is_open(1, 100));

        // dec 归零
        let released = pool.dec_pin_and_put(idx);
        assert!(released);
        assert!(!pool.is_open(1, 100));
    }
}
