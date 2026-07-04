//! Replicas 管理表 — 对齐 bcachefs `struct bch_replicas_entry_v1` / `struct bch_replicas_cpu`
//!
//! 跟踪每种数据类型在哪些设备上有多少副本。
//! bcachefs 对齐 (replicas.h / replicas.c)：
//! - 磁盘格式：`BchReplicasEntryV1` (data_type, nr_devs, nr_required, devs[])
//! - 内存格式：`BchReplicasCpu` (排序数组 + 引用计数)
//! - 关键操作：`bch2_devlist_to_replicas` / `bch2_mark_replicas` / `bch2_replicas_entry_get/put`
//!
//! subvol 简化：不涉及 superblock 持久化（后续版本），聚焦内存管理。

use crate::alloc::BchDataType;
use crate::block_device::BchDevsMask;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

/// 最大副本数 — 对齐 bcachefs `BCH_REPLICAS_MAX`
pub const BCH_REPLICAS_MAX: u8 = 4;

/// 副本条目 — 对齐 bcachefs `struct bch_replicas_entry_v1`
///
/// ```c
/// struct bch_replicas_entry_v1 {
///     __u8  data_type;    // BCH_DATA_xxx
///     __u8  nr_devs;      // 设备数量
///     __u8  nr_required;  // 需要的副本数（EC 时为 0）
///     __u8  devs[];       // 设备索引数组
/// } __packed;
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BchReplicasEntry {
    pub data_type: BchDataType,
    pub nr_devs: u8,
    pub nr_required: u8,
    pub devs: Vec<u8>,
}

impl BchReplicasEntry {
    /// 创建新的 replicas 条目。
    /// 对应 bcachefs `bch2_devlist_to_replicas()`。
    pub fn new(data_type: BchDataType, devs: &[u8], nr_required: u8) -> Self {
        let mut sorted = devs.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        Self {
            data_type,
            nr_devs: sorted.len() as u8,
            nr_required,
            devs: sorted,
        }
    }

    /// 从 `BchDevsMask` 创建 replicas 条目。
    /// 对应 bcachefs `bch2_devlist_to_replicas(mask → devs_list)`。
    pub fn from_mask(data_type: BchDataType, mask: &BchDevsMask, nr_required: u8) -> Self {
        let devs: Vec<u8> = mask.iter().collect();
        Self::new(data_type, &devs, nr_required)
    }

    /// 验证条目合法性 — 对齐 bcachefs `bch2_replicas_entry_validate()`
    ///
    /// bcachefs 验证：
    /// 1. nr_devs 不能为 0
    /// 2. nr_required 不能 > nr_devs（当 nr_required > 1 时）
    /// 3. 每个设备索引在有效范围内
    pub fn validate(&self, max_dev_idx: u8) -> Result<(), &'static str> {
        if self.nr_devs == 0 {
            return Err("replicas entry: nr_devs is 0");
        }
        if self.nr_devs != self.devs.len() as u8 {
            return Err("replicas entry: nr_devs mismatch with devs.len()");
        }
        if self.nr_devs > BCH_REPLICAS_MAX {
            return Err("replicas entry: nr_devs exceeds BCH_REPLICAS_MAX");
        }
        if self.nr_required > self.nr_devs {
            return Err("replicas entry: nr_required > nr_devs");
        }
        for &d in &self.devs {
            if d >= max_dev_idx {
                return Err("replicas entry: invalid device index");
            }
        }
        // 验证设备索引已排序且无重复
        for w in self.devs.windows(2) {
            if w[0] >= w[1] {
                return Err("replicas entry: devs not sorted or has duplicates");
            }
        }
        Ok(())
    }

    /// 检查条目是否包含指定设备 — 对齐 `bch2_replicas_entry_has_dev()`
    pub fn has_dev(&self, dev_idx: u8) -> bool {
        self.devs.binary_search(&dev_idx).is_ok()
    }

    /// 条目字节大小 — 对齐 `replicas_entry_bytes()`
    pub fn encoded_size(&self) -> usize {
        3 + self.nr_devs as usize // data_type + nr_devs + nr_required + devs[]
    }
}

/// CPU 副本条目（带引用计数）— 对齐 `struct bch_replicas_entry_cpu`
///
/// ```c
/// struct bch_replicas_entry_cpu {
///     atomic_t                    ref;
///     struct bch_replicas_entry_v1 e;
/// };
/// ```
#[derive(Debug)]
pub struct BchReplicasEntryCpu {
    pub ref_count: AtomicU32,
    pub entry: BchReplicasEntry,
}

impl BchReplicasEntryCpu {
    pub fn new(entry: BchReplicasEntry) -> Self {
        Self {
            ref_count: AtomicU32::new(0),
            entry,
        }
    }

    /// 增加引用计数 — 对齐 `bch2_replicas_entry_get()` 的原子增加
    pub fn get(&self) {
        self.ref_count.fetch_add(1, Ordering::Relaxed);
    }

    /// 减少引用计数 — 对齐 `bch2_replicas_entry_put()`
    /// 返回 true 表示 ref_count 降为 0
    pub fn put(&self) -> bool {
        let prev = self.ref_count.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(prev > 0, "replicas entry put on zero ref");
        prev == 1
    }
}

/// 内存副本表 — 对齐 `struct bch_replicas_cpu`
///
/// ```c
/// struct bch_replicas_cpu {
///     unsigned                    nr;
///     unsigned                    entry_size;
///     struct bch_replicas_entry_cpu *entries;
/// };
/// ```
#[derive(Debug)]
pub struct BchReplicasCpu {
    entries: Vec<Arc<BchReplicasEntryCpu>>,
}

impl BchReplicasCpu {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// 标记一个 replicas 条目 — 对齐 `bch2_mark_replicas()`
    ///
    /// 如果条目已存在，跳过；否则添加新条目。
    pub fn mark(&mut self, new_entry: &BchReplicasEntry) {
        if self.lookup(new_entry).is_some() {
            return;
        }
        self.entries
            .push(Arc::new(BchReplicasEntryCpu::new(new_entry.clone())));
        self.sort();
    }

    /// 获取（增加引用）— 对齐 `bch2_replicas_entry_get()`
    ///
    /// 如果条目不存在，先 mark 再 get。
    pub fn get_or_mark(&mut self, entry: &BchReplicasEntry) -> Arc<BchReplicasEntryCpu> {
        if let Some(existing) = self.lookup(entry) {
            existing.get();
            existing.clone()
        } else {
            let cpu = Arc::new(BchReplicasEntryCpu::new(entry.clone()));
            cpu.get(); // ref = 1
            self.entries.push(cpu.clone());
            self.sort();
            cpu
        }
    }

    /// 释放引用 — 对齐 `bch2_replicas_entry_put()`
    ///
    /// 引用归零时从表中移除条目。
    pub fn put(&mut self, entry: &BchReplicasEntry) {
        if let Some(pos) = self.lookup_pos(entry) {
            if self.entries[pos].put() {
                self.entries.remove(pos);
            }
        }
    }

    /// 批量释放引用 — 对齐 `bch2_replicas_entry_put_many()`
    pub fn put_many(&mut self, entry: &BchReplicasEntry, nr: u32) {
        if let Some(pos) = self.lookup_pos(entry) {
            for _ in 0..nr {
                if self.entries[pos].put() {
                    self.entries.remove(pos);
                    return;
                }
            }
        }
    }

    /// 删除条目 — 对齐 `bch2_replicas_entry_kill()`
    pub fn kill(&mut self, entry: &BchReplicasEntry) {
        if let Some(pos) = self.lookup_pos(entry) {
            self.entries.remove(pos);
        }
    }

    /// 查找条目在表中是否存在
    pub fn contains(&self, entry: &BchReplicasEntry) -> bool {
        self.lookup(entry).is_some()
    }

    /// 迭代所有条目
    pub fn iter(&self) -> impl Iterator<Item = &Arc<BchReplicasEntryCpu>> {
        self.entries.iter()
    }

    /// 复制 entries 数组
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    // ── 内部辅助 ──

    fn sort(&mut self) {
        self.entries.sort_by(|a, b| {
            let cmp_type = (a.entry.data_type as u8).cmp(&(b.entry.data_type as u8));
            cmp_type.then_with(|| a.entry.devs.cmp(&b.entry.devs))
        });
    }

    fn lookup(&self, entry: &BchReplicasEntry) -> Option<&Arc<BchReplicasEntryCpu>> {
        self.entries.iter().find(|e| e.entry == *entry)
    }

    fn lookup_pos(&self, entry: &BchReplicasEntry) -> Option<usize> {
        self.entries.iter().position(|e| e.entry == *entry)
    }
}

impl Default for BchReplicasCpu {
    fn default() -> Self {
        Self::new()
    }
}

/// `bch2_devlist_to_replicas` — 从设备列表创建 replicas 条目
pub fn devlist_to_replicas(data_type: BchDataType, devs: &[u8]) -> BchReplicasEntry {
    BchReplicasEntry::new(data_type, devs, 1)
}

/// `bch2_devlist_to_replicas` — 从 `BchDevsMask` 创建
pub fn devmask_to_replicas(data_type: BchDataType, mask: &BchDevsMask) -> BchReplicasEntry {
    BchReplicasEntry::from_mask(data_type, mask, 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_replicas_entry_new() {
        let e = BchReplicasEntry::new(BchDataType::User, &[3, 1, 2], 1);
        assert_eq!(e.nr_devs, 3);
        assert_eq!(e.devs, vec![1, 2, 3]); // sorted
        assert_eq!(e.nr_required, 1);
    }

    #[test]
    fn test_replicas_entry_new_dedup() {
        let e = BchReplicasEntry::new(BchDataType::Btree, &[0, 0, 1, 1], 2);
        assert_eq!(e.nr_devs, 2);
        assert_eq!(e.devs, vec![0, 1]);
    }

    #[test]
    fn test_replicas_entry_from_mask() {
        let mut mask = BchDevsMask::new();
        mask.set(0);
        mask.set(2);
        mask.set(5);
        let e = BchReplicasEntry::from_mask(BchDataType::Journal, &mask, 1);
        assert_eq!(e.nr_devs, 3);
        assert!(e.has_dev(0));
        assert!(e.has_dev(2));
        assert!(e.has_dev(5));
        assert!(!e.has_dev(1));
    }

    #[test]
    fn test_validate_ok() {
        let e = BchReplicasEntry::new(BchDataType::User, &[0, 1, 2], 2);
        assert!(e.validate(8).is_ok());
    }

    #[test]
    fn test_validate_zero_devs() {
        let e = BchReplicasEntry::new(BchDataType::User, &[], 1);
        assert!(e.validate(8).is_err());
    }

    #[test]
    fn test_validate_nr_required_exceeds() {
        let e = BchReplicasEntry::new(BchDataType::User, &[0, 1], 3);
        assert!(e.validate(8).is_err());
    }

    #[test]
    fn test_validate_invalid_dev_idx() {
        let e = BchReplicasEntry::new(BchDataType::User, &[0, 10], 1);
        assert!(e.validate(5).is_err());
    }

    #[test]
    fn test_validate_duplicates() {
        let e = BchReplicasEntry {
            data_type: BchDataType::User,
            nr_devs: 2,
            nr_required: 1,
            devs: vec![0, 0], // unsorted + duplicate
        };
        assert!(e.validate(8).is_err());
    }

    #[test]
    fn test_has_dev() {
        let e = BchReplicasEntry::new(BchDataType::Btree, &[1, 3, 5], 1);
        assert!(e.has_dev(1));
        assert!(e.has_dev(5));
        assert!(!e.has_dev(0));
        assert!(!e.has_dev(2));
    }

    #[test]
    fn test_encoded_size() {
        let e = BchReplicasEntry::new(BchDataType::User, &[0, 1, 2], 2);
        assert_eq!(e.encoded_size(), 3 + 3); // header + 3 devs
    }

    #[test]
    fn test_cpu_entry_ref_count() {
        let e = BchReplicasEntry::new(BchDataType::Journal, &[0], 1);
        let cpu = BchReplicasEntryCpu::new(e);
        assert_eq!(cpu.ref_count.load(Ordering::Relaxed), 0);
        cpu.get();
        assert_eq!(cpu.ref_count.load(Ordering::Relaxed), 1);
        cpu.get();
        assert_eq!(cpu.ref_count.load(Ordering::Relaxed), 2);
        assert!(!cpu.put()); // ref 2→1, returns false (prev==2)
        assert!(cpu.put()); // ref 1→0, returns true (prev==1)
    }

    #[test]
    fn test_cpu_table_mark() {
        let mut table = BchReplicasCpu::new();
        let e = BchReplicasEntry::new(BchDataType::User, &[0, 1], 1);
        table.mark(&e);
        assert_eq!(table.len(), 1);
        assert!(table.contains(&e));
        // duplicate mark is no-op
        table.mark(&e);
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn test_cpu_table_get_or_mark() {
        let mut table = BchReplicasCpu::new();
        let e = BchReplicasEntry::new(BchDataType::Btree, &[0], 1);
        let cpu1 = table.get_or_mark(&e);
        assert_eq!(table.len(), 1);
        assert_eq!(cpu1.ref_count.load(Ordering::Relaxed), 1);

        let _cpu2 = table.get_or_mark(&e);
        assert_eq!(table.len(), 1); // same entry
        assert_eq!(cpu1.ref_count.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn test_cpu_table_put_removes() {
        let mut table = BchReplicasCpu::new();
        let e = BchReplicasEntry::new(BchDataType::Journal, &[0, 1], 2);
        table.get_or_mark(&e); // ref = 1
        assert_eq!(table.len(), 1);

        table.put(&e); // ref = 0 → removed
        assert_eq!(table.len(), 0);
    }

    #[test]
    fn test_cpu_table_put_many() {
        let mut table = BchReplicasCpu::new();
        let e = BchReplicasEntry::new(BchDataType::User, &[0], 1);
        table.get_or_mark(&e); // ref = 1
        table.get_or_mark(&e); // ref = 2

        table.put_many(&e, 2); // ref = 0 → removed
        assert_eq!(table.len(), 0);
    }

    #[test]
    fn test_cpu_table_kill() {
        let mut table = BchReplicasCpu::new();
        let e = BchReplicasEntry::new(BchDataType::User, &[0, 1], 1);
        table.mark(&e);
        assert_eq!(table.len(), 1);

        table.kill(&e);
        assert_eq!(table.len(), 0);
    }

    #[test]
    fn test_devlist_to_replicas() {
        let e = devlist_to_replicas(BchDataType::Btree, &[2, 0]);
        assert_eq!(e.data_type, BchDataType::Btree);
        assert_eq!(e.devs, vec![0, 2]);
        assert_eq!(e.nr_required, 1);
    }

    #[test]
    fn test_devmask_to_replicas() {
        let mut mask = BchDevsMask::new();
        mask.set(3);
        mask.set(7);
        mask.set(1);
        let e = devmask_to_replicas(BchDataType::Journal, &mask);
        assert_eq!(e.data_type, BchDataType::Journal);
        assert_eq!(e.devs, vec![1, 3, 7]);
    }

    #[test]
    fn test_replicas_max_constant() {
        assert_eq!(BCH_REPLICAS_MAX, 4);
    }

    #[test]
    fn test_table_iter() {
        let mut table = BchReplicasCpu::new();
        let e1 = BchReplicasEntry::new(BchDataType::User, &[0], 1);
        let e2 = BchReplicasEntry::new(BchDataType::Btree, &[1], 1);
        table.mark(&e1);
        table.mark(&e2);
        let entries: Vec<_> = table.iter().map(|cpu| cpu.entry.data_type as u8).collect();
        // Sorted by data_type: Btree(3) < User(4)
        assert_eq!(entries, vec![3, 4]);
    }
}
