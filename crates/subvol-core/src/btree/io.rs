//! Btree I/O (Read/Write) — bcachefs 对齐
//!
//! 对应 bcachefs btree_io.c + btree_read.c 中的公开 API。
//! 当前实现包装 bucket_io.rs 的底层读写，提供 bcachefs 命名对齐的接口。

use std::sync::{Arc, Weak};

use crate::btree::btree::Btree;
use crate::btree::bucket_io;
use crate::btree::cache::bch2_btree_node_write_done_clean;
use crate::btree::interior::btree_node_reset_sib_u64s;
use crate::btree::key::{bkey_unpack, bpos_cmp, BkeyPacked, Bpos, BKEY_FORMAT_CURRENT};
use crate::btree::node::{BsetHeader, BsetTree, BtreeNode, BLOCK_SIZE, MAX_BSETS};
use crate::btree::types::NodeCache;
#[cfg(test)]
use crate::btree::transaction::BtreeTrans;
use crate::io::{submit_bio_write_replicas, Closure};
use crate::journal::reclaim::JournalPinFlushFn;
use crate::journal::Journal;
use crate::types::{AtomicFirstError, BlockAddr, StorageError};
use std::cmp::Ordering;

// ─── Read Path ──────────────────────────────────────────────────────────────

/// bcachefs 对齐: bch2_btree_node_io_lock — 获取节点写入 I/O 锁
///
/// 对应 bcachefs `wait_on_bit_lock_io(&b->flags, BTREE_NODE_write_in_flight)` (read.c:70-73)。
/// 使用 Condvar 等待 write_in_flight 清除后，原子 CAS 设置标志。
/// io_unlock 通过 `clear_write_in_flight()` 调用 `write_condvar.notify_all()` 唤醒等待者。
pub fn bch2_btree_node_io_lock(node: &BtreeNode) {
    let mut guard = node.write_wait_mutex.lock().unwrap();
    while !node.try_lock_write_in_flight() {
        guard = node.write_condvar.wait(guard).unwrap();
    }
    node.set_write_in_flight_inner();
}

/// bcachefs 对齐: bch2_btree_node_io_unlock — 释放节点写入 I/O 锁
pub fn bch2_btree_node_io_unlock(node: &BtreeNode) {
    node.clear_write_in_flight_inner();
    node.clear_write_in_flight();
}

/// bcachefs 对齐: bch2_btree_node_io_try_lock — 非阻塞尝试获取锁
///
/// 对应 bcachefs `mutex_trylock(&b->io_lock)`（commit.c:254-297）。
/// 一次 CAS 尝试，不 spin。flush 回调中使用，避免阻塞 journal reclaim。
pub(crate) fn btree_node_io_try_lock(node: &BtreeNode) -> bool {
    if node.try_lock_write_in_flight() {
        node.set_write_in_flight_inner();
        true
    } else {
        false
    }
}

/// bcachefs 对齐: bch2_btree_node_wait_on_read — 等待节点读取完成
///
/// spin 等待直到 read_in_flight 标志清除。
pub fn bch2_btree_node_wait_on_read(node: &BtreeNode) {
    while node.is_read_in_flight() {
        std::thread::yield_now();
    }
}

/// bcachefs 对齐: bch2_btree_node_wait_on_write — 等待节点写入完成
///
/// spin 等待直到 write_in_flight 标志清除。
pub fn bch2_btree_node_wait_on_write(node: &BtreeNode) {
    while node.is_write_in_flight() {
        std::thread::yield_now();
    }
}

/// bcachefs 对齐: bch2_btree_node_read — 从树上下文读取 btree 节点
///
/// 从事务上下文读取指定地址的节点并返回。节点数据由调用方负责插入缓存。
/// 使用所有在线设备尝试读取，自动降级到可用设备。
#[cfg(test)]
pub(crate) async fn bch2_btree_node_read(
    trans: &BtreeTrans<'_>,
    block_addr: u64,
) -> Result<BtreeNode, StorageError> {
    let vol = trans.vol();
    let devs = {
        let registry = &vol.device_registry;
        registry.resolve_mask(registry.online_mask())
    };
    let mut node = if !devs.is_empty() {
        bucket_io::__bch2_load_btree_node_replicas(devs, block_addr).await?
    } else {
        // 无在线设备时回退到主设备（不应发生在正常操作中）
        let dev = vol
            .primary_device_rcu_noerror()
            .expect("bch2_btree_node_read: primary device not registered");
        bucket_io::__bch2_load_btree_node(dev, block_addr).await?
    };
    bch2_btree_node_read_done(&mut node)?;
    node.try_set_block_addr(block_addr);
    Ok(node)
}

/// bcachefs 对齐: bch2_btree_root_read — 读取 btree 根节点
///
/// 从树级后端读取根节点，并输出 level 信息。
/// 使用所有在线设备尝试读取，自动降级到可用设备。
pub(crate) async fn bch2_btree_root_read(
    btree: &Btree,
    block_addr: u64,
) -> Result<(BtreeNode, u8), StorageError> {
    let mut node = if let Some(vol) = btree.vol_arc() {
        let devs = {
            let registry = &vol.device_registry;
            registry.resolve_mask(registry.online_mask())
        };
        if !devs.is_empty() {
            bucket_io::__bch2_load_btree_node_replicas(devs, block_addr).await?
        } else {
            let dev = vol
                .primary_device_rcu_noerror()
                .expect("bch2_btree_root_read: primary device not registered");
            bucket_io::__bch2_load_btree_node(dev, block_addr).await?
        }
    } else {
        let dev = btree.vol_device();
        bucket_io::__bch2_load_btree_node(dev, block_addr).await?
    };
    let level = node.level;
    bch2_btree_node_read_done(&mut node)?;
    node.try_set_block_addr(block_addr);
    Ok((node, level))
}

// ─── Sort Iter 架构 (bcachefs sort_iter) ────────────────────────────────

/// bcachefs 对齐: sort_iter_set — 单 bset 的 key 范围
///
/// C 对应: `struct sort_iter_set { struct bkey_packed *k, *end; }` (sort.h:13-16)
#[derive(Debug, Clone, Copy)]
struct SortIterEntry {
    cur: u32,
    end: u32,
}

/// bcachefs 对齐: sort_iter — 合并多个有序 bsets
///
/// C 对应: `struct sort_iter` (sort.h:7-16)。
struct SortIter {
    entries: Vec<SortIterEntry>,
    used: usize,
    size: usize,
    data: *const u8,
    data_len: usize,
}

// Safety: SortIter 只持有指向 node.data 的指针，不拥有数据。
// SortIter 的生命周期必须短于 BtreeNode 的生命周期。
unsafe impl Send for SortIter {}
unsafe impl Sync for SortIter {}

impl SortIter {
    /// bcachefs 对齐: sort_iter_init (sort.h:18-23) — 从 BtreeNode 初始化 sort_iter
    pub fn init_from_node(node: &BtreeNode) -> Self {
        SortIter {
            entries: Vec::with_capacity(MAX_BSETS),
            used: 0,
            size: MAX_BSETS,
            data: node.data.as_ptr(),
            data_len: node.data.len(),
        }
    }

    /// bcachefs 对齐: sort_iter_add (sort.h:36-44) — 添加一个 bset 的 key 范围
    pub fn add(&mut self, start_offset: u32, end_offset: u32) {
        if start_offset < end_offset {
            assert!(self.used < self.size);
            self.entries.push(SortIterEntry {
                cur: start_offset,
                end: end_offset,
            });
            self.used += 1;
        }
    }

    /// 从 BtreeNode 的所有活跃 bsets 添加 key 范围
    pub fn add_all_bsets(&mut self, node: &BtreeNode) {
        let nsets = node.nsets() as usize;
        for si in 0..nsets {
            let s = &node.sets[si];
            if s.data_offset != s.end_offset {
                self.add(
                    u32::from(s.first_key_offset()) * 8,
                    u32::from(s.end_offset) * 8,
                );
            }
        }
    }

    /// bcachefs `sort_iter_sift()` (`sort.c:21-32`).
    fn sift<F>(&mut self, from: usize, cmp: F)
    where
        F: Fn(&BkeyPacked, u32, &BkeyPacked, u32) -> Ordering + Copy,
    {
        let data = unsafe { std::slice::from_raw_parts(self.data, self.data_len) };
        let mut i = from;

        while i + 1 < self.used {
            let left_offset = self.entries[i].cur;
            let right_offset = self.entries[i + 1].cur;
            let left = unsafe { &*(data.as_ptr().add(left_offset as usize) as *const BkeyPacked) };
            let right =
                unsafe { &*(data.as_ptr().add(right_offset as usize) as *const BkeyPacked) };
            if cmp(left, left_offset, right, right_offset) != Ordering::Greater {
                break;
            }

            self.entries.swap(i, i + 1);
            i += 1;
        }
    }

    /// bcachefs `sort_iter_sort()` (`sort.c:34-40`).
    fn sort<F>(&mut self, cmp: F)
    where
        F: Fn(&BkeyPacked, u32, &BkeyPacked, u32) -> Ordering + Copy,
    {
        let mut i = self.used;

        while i > 0 {
            i -= 1;
            self.sift(i, cmp);
        }
    }

    /// bcachefs `sort_iter_peek()` (`sort.c:42-45`).
    fn peek(&self) -> Option<u32> {
        if self.used != 0 {
            Some(self.entries[0].cur)
        } else {
            None
        }
    }

    /// bcachefs `sort_iter_advance()` (`sort.c:47-62`).
    fn advance<F>(&mut self, cmp: F) -> Result<(), StorageError>
    where
        F: Fn(&BkeyPacked, u32, &BkeyPacked, u32) -> Ordering + Copy,
    {
        assert!(self.used != 0);

        let data = unsafe { std::slice::from_raw_parts(self.data, self.data_len) };
        let offset = self.entries[0].cur as usize;
        if offset + 3 > data.len() {
            return Err(StorageError::CorruptData(format!(
                "sort_iter: truncated key header at offset {}",
                offset
            )));
        }
        let u64s = data[offset];
        if u64s == 0 {
            return Err(StorageError::CorruptData(format!(
                "sort_iter: zero length key at offset {}",
                offset
            )));
        }

        self.entries[0].cur += u32::from(u64s) * 8;

        if self.entries[0].cur > self.entries[0].end {
            return Err(StorageError::CorruptData(format!(
                "sort_iter: key at offset {} exceeds bset end {}",
                offset, self.entries[0].end
            )));
        }

        if self.entries[0].cur == self.entries[0].end {
            self.entries.remove(0);
            self.used -= 1;
        } else {
            self.sift(0, cmp);
        }

        Ok(())
    }

    /// bcachefs `sort_iter_next()` (`sort.c:64-73`).
    fn next<F>(&mut self, cmp: F) -> Result<Option<u32>, StorageError>
    where
        F: Fn(&BkeyPacked, u32, &BkeyPacked, u32) -> Ordering + Copy,
    {
        let ret = self.peek();

        if ret.is_some() {
            self.advance(cmp)?;
        }

        Ok(ret)
    }

    fn should_drop_next_key(&self) -> bool {
        if self.used < 2 {
            return false;
        }

        let data = unsafe { std::slice::from_raw_parts(self.data, self.data_len) };
        let left =
            unsafe { &*(data.as_ptr().add(self.entries[0].cur as usize) as *const BkeyPacked) };
        let right =
            unsafe { &*(data.as_ptr().add(self.entries[1].cur as usize) as *const BkeyPacked) };

        crate::btree::key::bkey_cmp_packed(&BKEY_FORMAT_CURRENT, left, right) == Ordering::Equal
    }

    // ─── 排序合并 ──────────────────────────────────────────────────

    /// 将 sort_iter 中所有 key 排序合并到 dst。
    ///
    /// 对齐 `bch2_key_sort_fix_overlapping()`：按 packed bpos 和原始指针顺序
    /// 合并，重叠键保留较新的项并过滤 Deleted。
    /// 返回 (写入字节数, 写入 key 数)。
    pub fn sort_into(&mut self, dst: &mut [u8]) -> Result<(usize, usize), StorageError> {
        let data = unsafe { std::slice::from_raw_parts(self.data, self.data_len) };

        for entry in self.entries.iter().take(self.used) {
            let mut cur = entry.cur;
            while cur < entry.end {
                let offset = cur as usize;
                if offset + 3 > data.len() {
                    return Err(StorageError::CorruptData(format!(
                        "sort_iter: truncated key header at offset {}",
                        offset
                    )));
                }
                let u64s = data[offset];
                if u64s == 0 {
                    return Err(StorageError::CorruptData(format!(
                        "sort_iter: zero length key at offset {}",
                        offset
                    )));
                }
                let entry_bytes = u32::from(u64s) * 8;
                if offset + entry_bytes as usize > data.len() || cur + entry_bytes > entry.end {
                    return Err(StorageError::CorruptData(format!(
                        "sort_iter: key at offset {} exceeds bset end {}",
                        offset, entry.end
                    )));
                }
                cur += entry_bytes;
            }
        }

        let cmp = |left: &BkeyPacked, left_offset: u32, right: &BkeyPacked, right_offset: u32| {
            crate::btree::key::bkey_cmp_packed(&BKEY_FORMAT_CURRENT, left, right)
                .then_with(|| left_offset.cmp(&right_offset))
        };

        self.sort(cmp);

        let mut dst_offset = 0usize;
        let mut written_keys = 0usize;
        while let Some(key_off) = self.peek() {
            let pk = unsafe { &*(data.as_ptr().add(key_off as usize) as *const BkeyPacked) };

            if pk.type_ != crate::btree::key::KeyType::Deleted as u8 && !self.should_drop_next_key()
            {
                let entry_bytes = usize::from(pk.u64s) * 8;
                if dst_offset + entry_bytes > dst.len() {
                    return Err(StorageError::CorruptData(
                        "sort_iter: destination buffer overflow".to_string(),
                    ));
                }
                let src = key_off as usize;
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        data.as_ptr().add(src),
                        dst.as_mut_ptr().add(dst_offset),
                        entry_bytes,
                    );
                }
                dst_offset += entry_bytes;
                written_keys += 1;
            }

            self.advance(cmp)?;
        }

        Ok((dst_offset, written_keys))
    }

    /// 返回所有收集的 key 总数
    pub fn total_keys(&self) -> usize {
        let data = unsafe { std::slice::from_raw_parts(self.data, self.data_len) };
        let mut count = 0usize;
        for entry in &self.entries {
            let mut cur = entry.cur;
            while cur < entry.end {
                let offset = cur as usize;
                if offset >= data.len() {
                    break;
                }
                let u64s = data[offset];
                if u64s == 0 {
                    break;
                }
                count += 1;
                cur += (u64s as u32) * 8;
            }
        }
        count
    }
}

// ─── Bset 内部迭代辅助 ──────────────────────────────────────────────────

/// 在 bset 数据范围内遍历所有 packed keys，返回 `(entry_u64s, format, type_, bpos)`。
///
/// 验证每个 key 的字节边界不超过范围，跳过 u64s=0（终止标记）。
fn iter_bset_packed_keys<'a>(
    data: &'a [u8],
    start: u32,
    end: u32,
) -> impl Iterator<Item = Result<(u8, u8, u8, Bpos), StorageError>> + 'a {
    let mut offset = start as usize;
    let end_usize = end as usize;
    std::iter::from_fn(move || {
        if offset >= end_usize {
            return None;
        }
        // 至少需要 3 字节 header (u64s, format+whiteout, type_)
        if offset + 3 > data.len() {
            return Some(Err(StorageError::CorruptData(format!(
                "bset entry at offset {}: only {} bytes remaining, need at least 3",
                offset,
                data.len() - offset
            ))));
        }
        let entry_u64s = data[offset];
        if entry_u64s == 0 {
            // 0 = 终止标记
            return None;
        }
        let entry_bytes = (entry_u64s as u32) * 8;
        if offset + entry_bytes as usize > end_usize {
            return Some(Err(StorageError::CorruptData(format!(
                "bset entry at offset {}: entry size {} bytes exceeds bset end {}",
                offset, entry_bytes, end_usize
            ))));
        }
        let format_whiteout = data[offset + 1];
        let type_ = data[offset + 2];
        // 解包 bpos
        let bpos = unsafe {
            let pk = &*(data.as_ptr().add(offset) as *const BkeyPacked);
            let (pos, _, _, _) = bkey_unpack(&BKEY_FORMAT_CURRENT, pk);
            pos
        };
        offset += entry_bytes as usize;
        Some(Ok((entry_u64s, format_whiteout, type_, bpos)))
    })
}

// ─── Bset 验证 ──────────────────────────────────────────────────────────────

/// bcachefs 对齐: `bch2_validate_bset` — 验证单个 bset 的结构完整性
///
/// 验证内容（针对 subvol BsetTree 格式适配）：
/// - data_offset/end_offset 范围在节点 buffer 内
/// - 空 bset 只保留 header，因此 `first_key_offset() == end_offset`
/// - end_offset 8 字节对齐（每个 entry 都是 8 字节整数倍）
pub fn bch2_validate_bset(node: &BtreeNode, set_idx: usize) -> Result<(), StorageError> {
    if set_idx >= MAX_BSETS {
        return Err(StorageError::CorruptData(format!(
            "bset index {} exceeds MAX_BSETS {}",
            set_idx, MAX_BSETS
        )));
    }
    let set = &node.sets[set_idx];
    let node_size = node.node_size as usize;

    // data_offset 必须在节点 buffer 范围内
    if set.data_offset as usize * 8 > node_size {
        return Err(StorageError::CorruptData(format!(
            "bset[{}] data_offset {} exceeds node_size {}",
            set_idx, set.data_offset, node_size
        )));
    }
    // end_offset 必须在节点 buffer 范围内
    if set.end_offset as usize * 8 > node_size {
        return Err(StorageError::CorruptData(format!(
            "bset[{}] end_offset {} exceeds node_size {}",
            set_idx, set.end_offset, node_size
        )));
    }
    // 如果有 data，必须满足 start < end
    if set.first_key_offset() > set.end_offset {
        return Err(StorageError::CorruptData(format!(
            "bset[{}] header at data_offset={} exceeds end_offset={}",
            set_idx, set.data_offset, set.end_offset
        )));
    }
    Ok(())
}

/// bcachefs 对齐: `bch2_validate_bset_keys` — 验证 bset 内所有 key 的排序顺序
///
/// 遍历 bset 内所有 packed keys：
/// - 验证每个 key 的格式字段合法（format == KEY_FORMAT_CURRENT）
/// - 检查相邻 key 的 bpos 非降序
/// - 检查无相邻重复 key
pub fn bch2_validate_bset_keys(node: &BtreeNode, set_idx: usize) -> Result<(), StorageError> {
    if set_idx >= MAX_BSETS {
        return Err(StorageError::CorruptData(format!(
            "bset index {} exceeds MAX_BSETS {}",
            set_idx, MAX_BSETS
        )));
    }
    let set = &node.sets[set_idx];
    if set.first_key_offset() == set.end_offset {
        return Ok(());
    }
    let data = &node.data;
    let start = u32::from(set.first_key_offset()) * 8;
    let end = u32::from(set.end_offset) * 8;

    let mut prev_bpos: Option<Bpos> = None;
    let mut entry_count: u16 = 0;

    for result in iter_bset_packed_keys(data, start, end) {
        let (entry_u64s, format_whiteout, _type_, bpos) = result?;
        let format = format_whiteout & 0x7F;

        // 验证 format 合法：只能是 KEY_FORMAT_CURRENT(1) 或 KEY_FORMAT_LOCAL_BTREE(0)
        if format != 1 && format != 0 {
            return Err(StorageError::CorruptData(format!(
                "bset[{}] entry {}: invalid format {}",
                set_idx,
                entry_count + 1,
                format
            )));
        }
        // 验证 u64s >= BKEY_U64S (3)
        if entry_u64s < crate::btree::key::BKEY_U64S {
            return Err(StorageError::CorruptData(format!(
                "bset[{}] entry {}: u64s={} less than minimum BKEY_U64S",
                set_idx,
                entry_count + 1,
                entry_u64s
            )));
        }

        entry_count += 1;

        // 检查相邻 key 的 bpos 非降序
        if let Some(prev) = prev_bpos {
            match bpos_cmp(prev, bpos) {
                Ordering::Greater => {
                    return Err(StorageError::CorruptData(format!(
                        "bset[{}] key order violation: entries {} and {} are descending \
                         (prev={} > curr={})",
                        set_idx,
                        entry_count - 1,
                        entry_count,
                        prev,
                        bpos
                    )));
                }
                Ordering::Equal => {
                    return Err(StorageError::CorruptData(format!(
                        "bset[{}] duplicate key at entries {} and {}: bpos={}",
                        set_idx,
                        entry_count - 1,
                        entry_count,
                        bpos
                    )));
                }
                Ordering::Less => {} // 正确顺序
            }
        }
        prev_bpos = Some(bpos);
    }

    Ok(())
}

/// bcachefs 对齐: `bch2_btree_node_read_done` — 节点读取完成后的验证流水线
///
/// 验证流程（对齐 bcachefs `read.c` 的 read_done 验证路径）：
/// 1. 验证 header magic 为 `BTREE_NODE_MAGIC`（兼容性检查—节点层已做，此处后备）
/// 2. 验证 level 无异常
/// 3. 遍历所有活跃 bsets，对每个 bset 调用 `validate_bset` + `validate_bset_keys`
/// 4. 全局 key 排序：调用 `read_done_sort` 将多 bset 合并为单紧凑 bset
/// 5. 重置 sibling live-u64 估计
/// 6. 可选：`drop_keys_outside_node`（由 caller 根据 updated_range 决定调用）
/// 7. 清除 read_in_flight 标志
pub fn bch2_btree_node_read_done(node: &mut BtreeNode) -> Result<(), StorageError> {
    // [0] 基础 sanity 检查
    if node.data.is_empty() {
        node.clear_read_in_flight();
        return Err(StorageError::CorruptData(
            "btree node has empty data buffer".to_string(),
        ));
    }
    if node.node_size == 0 {
        node.clear_read_in_flight();
        return Err(StorageError::CorruptData(
            "btree node has zero node_size".to_string(),
        ));
    }

    // [1] 验证 header 与 data 关系
    let nsets = node.nsets() as usize;
    let result = _read_done_inner(node, nsets);

    // 在所有路径上清除 read_in_flight
    node.clear_read_in_flight();
    result
}

/// read_done 的内部实现，不负责清理 read_in_flight
fn _read_done_inner(node: &mut BtreeNode, nsets: usize) -> Result<(), StorageError> {
    if nsets == 0 || nsets > MAX_BSETS {
        return Err(StorageError::CorruptData(format!(
            "btree node has invalid nsets={}",
            nsets
        )));
    }

    // [2] 遍历所有活跃 bsets, 执行逐 bset 验证
    for si in 0..nsets {
        bch2_validate_bset(node, si)?;
        bch2_validate_bset_keys(node, si)?;
    }

    // [3] 跨 bset 验证: journal_seq 一致性
    // subvol 中所有 bset 共享同一个 journal_seq（节点级），

    // [4] 全局排序合并: 读取后排序合并多个 bset 到单个紧凑 set[0]
    read_done_sort(node)?;

    // [5] 本地 read.c:863：已落盘 key 后续覆盖/删除时必须生成 whiteout。
    node.bch2_set_bset_needs_whiteout(0, true);

    // [6] 本地 read.c:865 在 aux tree 建立后、range drop 前重置 sibling 估计。
    btree_node_reset_sib_u64s(node);

    // [7] 全局 key 范围验证
    let _ = bch2_btree_node_drop_keys_outside_node(node);

    Ok(())
}

// ─── 读取后全局排序合并（bcachefs sort_iter 模式） ─────────────────────────

/// bcachefs 对齐: 读取后全局排序合并
///
/// 对应 bcachefs read_done 中的 sort_iter 模式：
/// 1. sort_iter 收集所有 bsets 的 key 范围
/// 2. 全局排序（使用 packed bpos 比较）
/// 3. 将排序后的 key 写入节点 buffer
/// 4. compact() 完成去重 + 过滤 whiteout + aux tree 构建
///
/// 先使用 sort_iter 做初步排序，再通过 compact() 做最终去重和 aux 构建。
/// 当节点只有单个 bset 时，跳过 sort_iter，直接 compact。
fn read_done_sort(node: &mut BtreeNode) -> Result<(), StorageError> {
    let nsets = node.nsets();
    if nsets <= 1 {
        // 只有单个 bset → 直接 compact（无跨 set 合并需求）
        node.compact();
        return Ok(());
    }

    // 多个 bset: 先用 sort_iter 排序合并
    let total_keys = {
        let mut iter = SortIter::init_from_node(node);
        iter.add_all_bsets(node);
        iter.total_keys()
    };
    if total_keys == 0 {
        node.compact();
        return Ok(());
    }

    // 创建临时缓冲区存储排序后的 packed keys
    let buf_size = node.node_size as usize;
    let mut sorted_buf = vec![0u8; buf_size];
    let data_len = node.data.len();

    let (written, sorted_keys) = {
        let mut iter = SortIter::init_from_node(node);
        iter.add_all_bsets(node);
        iter.sort_into(&mut sorted_buf)?
    };

    // 将排序后的数据写回节点 buffer（从 BSET_HEADER_U64S * 8 开始，为 BsetHeader 预留空间）
    if written > 0 && written <= buf_size {
        let first_key_byte = crate::btree::node::BSET_HEADER_U64S as usize * 8;
        if first_key_byte + written <= buf_size {
            unsafe {
                std::ptr::copy_nonoverlapping(
                    sorted_buf.as_ptr(),
                    node.data.as_mut_ptr().add(first_key_byte),
                    written,
                );
            }
        }
        // 清空 bset 之后的区域（防止 compact 读到 stale 数据）
        let zero_end = buf_size.min(data_len);
        let clear_start = first_key_byte + written;
        if clear_start < zero_end {
            node.data[clear_start..zero_end].fill(0);
        }

        // 更新 set[0] 指向排序后的数据
        node.sets[0].data_offset = 0;
        node.sets[0].end_offset = ((first_key_byte + written) / 8) as u16;
        node.sets[0].size = 0;
        node.sets[0].aux_data_offset = u16::MAX;
        node.sets[0].extra = crate::btree::node::BSET_NO_AUX_TREE_VAL;
        // 清空其他 sets
        for i in 1..MAX_BSETS {
            node.sets[i].data_offset = u16::MAX;
            node.sets[i].end_offset = u16::MAX;
            node.sets[i].aux_data_offset = u16::MAX;
            node.sets[i].size = 0;
            node.sets[i].extra = crate::btree::node::BSET_NO_AUX_TREE_VAL;
        }
        node.nsets = 1;
        node.packed_keys = sorted_keys as u16;
        node.unpacked_keys = 0;
    }

    // compact() 完成去重、过滤 whiteout 和 aux tree 构建
    node.compact();

    Ok(())
}

/// bcachefs 对齐: `bch2_sort_keys` (sort.c:202) — 将 sort_iter 中的 key 排序合并到 dst
///
/// 对应 C: `unsigned bch2_sort_keys(struct bkey_packed *dst, struct sort_iter *iter)`
/// 返回写入的 u64 数（按 bcachefs 语义对齐）。
fn bch2_sort_keys(dst: &mut [u8], iter: &mut SortIter) -> Result<usize, StorageError> {
    let data = unsafe { std::slice::from_raw_parts(iter.data, iter.data_len) };
    let cmp = |left: &BkeyPacked, _left_offset: u32, right: &BkeyPacked, _right_offset: u32| {
        crate::btree::key::bkey_cmp_packed(&BKEY_FORMAT_CURRENT, left, right)
    };
    let mut dst_offset = 0usize;

    iter.sort(cmp);

    while let Some(key_offset) = iter.next(cmp)? {
        let key = unsafe { &*(data.as_ptr().add(key_offset as usize) as *const BkeyPacked) };

        if key.type_ == crate::btree::key::KeyType::Deleted as u8 {
            continue;
        }

        let key_bytes = usize::from(key.u64s) * 8;
        if dst_offset + key_bytes > dst.len() {
            return Err(StorageError::CorruptData(
                "sort_iter: destination buffer overflow".to_string(),
            ));
        }

        unsafe {
            std::ptr::copy_nonoverlapping(
                data.as_ptr().add(key_offset as usize),
                dst.as_mut_ptr().add(dst_offset),
                key_bytes,
            );
        }
        dst_offset += key_bytes;
    }

    Ok(dst_offset / 8)
}

/// 将 node 中所有 bset 排序合并为单个 bset，为写入做准备。
fn sort_node_for_write(node: &mut BtreeNode) -> Result<(), StorageError> {
    let nsets = node.nsets();
    if nsets <= 1 {
        // 单个 bset 无需合并，但可能仍有 uncommitted 的 key
        // 如果 key_count 为 0 或只有一个 set 且已有数据，无需操作
        if node.whiteout_u64s > 0 || node.sets[0].aux_data_offset == 0 {
            node.compact();
        }
        return Ok(());
    }

    // 多个 bset: 使用 sort_iter 排序合并
    // 收集所有 key
    let total_keys = {
        let mut iter = SortIter::init_from_node(node);
        iter.add_all_bsets(node);
        iter.total_keys()
    };
    if total_keys == 0 {
        return Ok(());
    }

    let buf_size = node.node_size as usize;
    let mut sorted_buf = vec![0u8; buf_size];
    let data_len = node.data.len();

    let written_u64s = {
        let mut iter = SortIter::init_from_node(node);
        iter.add_all_bsets(node);
        bch2_sort_keys(&mut sorted_buf, &mut iter)?
    };
    let written = written_u64s * 8;

    if written > 0 && written <= buf_size {
        let first_key_byte = crate::btree::node::BSET_HEADER_U64S as usize * 8;
        if first_key_byte + written <= buf_size {
            unsafe {
                std::ptr::copy_nonoverlapping(
                    sorted_buf.as_ptr(),
                    node.data.as_mut_ptr().add(first_key_byte),
                    written,
                );
            }
        }
        let zero_end = buf_size.min(data_len);
        let clear_start = first_key_byte + written;
        if clear_start < zero_end {
            node.data[clear_start..zero_end].fill(0);
        }

        // 更新为单 bset 结构
        node.sets[0] = BsetTree {
            size: 0,
            extra: crate::btree::node::BSET_NO_AUX_TREE_VAL,
            data_offset: 0,
            aux_data_offset: u16::MAX,
            end_offset: ((first_key_byte + written) / 8) as u16,
        };
        for i in 1..MAX_BSETS {
            node.sets[i] = BsetTree {
                size: 0,
                extra: crate::btree::node::BSET_NO_AUX_TREE_VAL,
                data_offset: u16::MAX,
                aux_data_offset: u16::MAX,
                end_offset: u16::MAX,
            };
        }
        node.nsets = 1;
        node.whiteout_u64s = 0;
    }

    // compact 确保 aux tree 就绪
    node.compact();

    Ok(())
}

// ─── 范围裁剪 ──────────────────────────────────────────────────────────────

/// bcachefs 对齐: `bch2_btree_node_drop_keys_outside_node` — 丢弃超出节点范围的 key
///
/// 遍历节点所有 bsets，丢弃 bpos < node.min_key 或 bpos > node.max_key 的条目。
/// 使用 compact() 重建 aux tree。
pub fn bch2_btree_node_drop_keys_outside_node(node: &mut BtreeNode) -> Result<(), StorageError> {
    let min_key = node.min_key;
    let max_key = node.max_key;

    // 空节点或空范围：跳过
    if node.packed_keys == 0 && node.unpacked_keys == 0 {
        return Ok(());
    }
    // 如果 min_key > max_key 表示没有有效的范围约束
    if bpos_cmp(min_key, max_key) != Ordering::Less {
        // 空节点（min_key = MAX, max_key = MIN, 实际 min > max）或未设置范围
        return Ok(());
    }

    // 收集所有 bset 中的条目，过滤出在范围内的条目
    let mut all: Vec<crate::btree::key::BtreeEntry> = Vec::new();
    for (si, _set) in crate::btree::node::for_each_bset(node) {
        let s = &node.sets[si];
        let mut cur = u32::from(s.first_key_offset()) * 8;
        while cur < u32::from(s.end_offset) * 8 {
            let entry = node.read_packed_entry_raw(cur as usize);
            let entry_bpos = entry.pos;
            // 丢弃范围外的 key
            if bpos_cmp(entry_bpos, min_key) != Ordering::Less
                && bpos_cmp(entry_bpos, max_key) != Ordering::Greater
            {
                all.push(entry);
            }
            let u64s = node.read_entry_u64s(cur as usize);
            cur += (u64s as u32) * 8;
        }
    }

    // 重写节点数据（从 BSET_HEADER_U64S * 8 开始，为 BsetHeader 预留空间）
    let n = all.len();
    let first_key_byte = u32::from(crate::btree::node::BSET_HEADER_U64S) * 8;
    let mut cur = first_key_byte;
    for entry in &all {
        let size = node.write_entry_bytes(cur, entry);
        cur += size;
    }

    node.sets[0] = BsetTree {
        size: 0,
        extra: crate::btree::node::BSET_NO_AUX_TREE_VAL,
        data_offset: 0,
        aux_data_offset: u16::MAX,
        end_offset: (cur / 8) as u16,
    };
    for i in 1..MAX_BSETS {
        node.sets[i] = BsetTree {
            size: 0,
            extra: crate::btree::node::BSET_NO_AUX_TREE_VAL,
            data_offset: u16::MAX,
            aux_data_offset: u16::MAX,
            end_offset: u16::MAX,
        };
    }
    node.nsets = 1;
    node.packed_keys = n as u16;
    node.unpacked_keys = 0;
    node.whiteout_u64s = 0;
    node.bch2_bset_build_aux_tree(0, true);

    Ok(())
}

/// bcachefs 对齐: `bch2_btree_node_header_to_text` (read.c:49, static) — 节点 header 的调试输出
///
/// 格式化输出 BtreeNode 关键字段（magic、version、level、key_count 等）。
/// 用于错误日志和调试。
#[cfg(test)]
pub(crate) fn bch2_btree_node_header_to_text(node: &BtreeNode) -> String {
    format!(
        "BtreeNode(level={}, packed_keys={}, unpacked_keys={}, whiteout={}, nsets={}, \
         min_key={}, max_key={}, journal_seq={}, node_size={}, \
         data_len={})",
        node.level,
        node.packed_keys,
        node.unpacked_keys,
        node.whiteout_u64s,
        node.nsets(),
        node.min_key,
        node.max_key,
        node.journal_seq,
        node.node_size,
        node.data.len(),
    )
}

/// bcachefs 对齐: bch2_btree_flush_all_reads — 刷新所有正在进行的读取操作
pub fn bch2_btree_flush_all_reads() -> bool {
    // subvol: 当前为同步读取，没有飞行中的读操作
    true
}

// ─── Write Path ─────────────────────────────────────────────────────────────

/// bcachefs 对齐: bch2_btree_node_write — 将 btree 节点写入后端
///
/// bcachefs 对齐: __bch2_btree_node_write — 序列化并提交节点写入
///
/// 接受 `Arc<BtreeNode>` 匹配 bcachefs 的 `struct btree *b` 共享所有权模型。
/// IO 完成后通过 closure callback 清理（io_unlock + journal pin）。
/// 外部保持 `async fn` 签名，内部使用 closure 驱动。
///
/// 对应 bcachefs `__bch2_btree_node_write`（write.c:336）。
pub async fn bch2_btree_node_write(
    node: Arc<BtreeNode>,
    journal: Option<&Journal>,
) -> Result<(), StorageError> {
    if let Some(j) = journal {
        let jseq = node.journal_seq;
        if jseq > 0 {
            // pin_type 已在构造函数中根据 level 设置
            j.bch2_journal_pin_add(jseq, &node.journal_pin, None);
        }
    }

    if let Err(e) = __bch2_btree_node_write(node.clone()) {
        if let Some(j) = journal {
            j.bch2_journal_pin_drop(&node.journal_pin);
        }
        return Err(e);
    }

    Ok(())
}

/// bcachefs 对齐: bch2_btree_node_write_mut — 可变引用的写节点
///
/// 接受 `Arc<BtreeNode>`，序列化前执行 sort_iter 排序合并。
/// 对应 bcachefs `__bch2_btree_node_write`（write.c:336）。
pub(crate) async fn btree_node_write_mut(
    node: Arc<BtreeNode>,
    cache: &NodeCache,
    journal: Option<&Journal>,
) -> Result<(), StorageError> {
    cache.inc_in_flight();
    let post_node = node.clone(); // node 被 submit 消费后，Phase 4/5 通过此 clone 访问

    // Phase 1: 排序 + 加锁（块作用域确保 &mut 在 await 前 drop）
    {
        // Safety: 此期间对 Arc 有独占访问权（非回调期间）
        let node_mut = unsafe { &mut *(Arc::as_ptr(&node) as *mut BtreeNode) };
        bch2_btree_node_io_lock(node_mut);
        if let Err(e) = sort_node_for_write(node_mut) {
            bch2_btree_node_io_unlock(node_mut);
            cache.dec_in_flight();
            return Err(e);
        }
        node_mut.set_just_written();
    } // node_mut 在此 drop，不跨 await

    // bcachefs `bch2_btree_insert_key()` 在节点变脏后、提交写 I/O 前注册
    // journal pin，确保写入期间 journal 不会被回收。
    if let Some(j) = journal {
        bch2_btree_add_journal_pin(&post_node, j, post_node.journal_seq);
    }

    // Phase 2: 提交 IO（fire-and-forget，回调驱动 write_done）
    let (tx, rx) = tokio::sync::oneshot::channel();
    let cb_node = node.clone();
    let err_node = node.clone();
    if let Err(e) = submit_btree_node_io(node, move |err| {
        let result = err.map_or(Ok(()), Err);
        btree_node_write_done(&cb_node, &result);
        let _ = tx.send(result);
    }) {
        err_node.clear_will_make_reachable();
        bch2_btree_node_io_unlock(&err_node);
        cache.dec_in_flight();
        return Err(e);
    }

    // Phase 3: 等待 IO 完成（无 raw pointer 或 &mut 跨 await）
    rx.await.unwrap()?;

    // Phase 4: post-write cleanup
    {
        let node_mut = unsafe { &mut *(Arc::as_ptr(&post_node) as *mut BtreeNode) };
        bch2_btree_post_write_cleanup(node_mut);
    }

    cache.dec_in_flight();
    Ok(())
}

/// bcachefs 对齐: bch2_btree_add_journal_pin — 设置 btree 节点 journal pin + flush 回调
///
/// 对应 bcachefs `bch2_btree_add_journal_pin`（commit.c:299-308）。
/// 直接操作嵌入的 `node.journal_pin`，首次注册时设置 flush 回调（捕获 Weak<BtreeNode>）。
/// 后续调用仅更新 journal seq（flush 回调不变），无锁操作。
/// 调用方传入 `node_arc` 持有 Arc 所有权，函数内部 downgrade 为 Weak。
pub(crate) fn bch2_btree_add_journal_pin(node_arc: &Arc<BtreeNode>, j: &Journal, jseq: u64) {
    if jseq == 0 {
        return;
    }
    let flush_cb: Option<JournalPinFlushFn> = if !node_arc.journal_pin.is_active() {
        let weak: Weak<BtreeNode> = Arc::downgrade(node_arc);
        Some(Box::new(move |_j, _pin, _seq| {
            let n = weak
                .upgrade()
                .ok_or_else(|| StorageError::NotFound("btree node gone".into()))?;
            let addr = n.block_addr();
            if addr == 0 {
                return Ok(());
            }
            n.set_need_rewrite();
            if btree_node_io_try_lock(&n) {
                __bch2_btree_node_write_locked(n).ok();
            }
            Ok(())
        }))
    } else {
        None
    };
    j.bch2_journal_pin_add(jseq, &node_arc.journal_pin, flush_cb);
}

/// bcachefs 对齐: __bch2_btree_node_write — 内部写节点（火抛）
///
/// 锁 + 序列化 + 提交 IO，IO 完成后自动调用 `__btree_node_write_done`。
/// 对应 bcachefs `__bch2_btree_node_write`（write.c:336），无闭包参数，不等待 IO。
///
/// journal pin 释放通过 node.journal 弱引用完成（由 BtreeWriter 在写前设置）。
///
/// bcachefs 对齐：错误路径释放 write_in_flight（通过 io_unlock）。
/// `submit_btree_node_io` 失败时保证 io_lock 被释放，避免锁泄漏。
/// bcachefs 对齐: __bch2_btree_node_write_locked — 已持锁的内部写节点
///
/// 调用方必须已持有 io_lock（通过 bch2_btree_node_io_lock 或 try_lock）。
/// IO 完成后 write_done 自动释放锁；错误路径在此函数内释放。
pub(crate) fn __bch2_btree_node_write_locked(node: Arc<BtreeNode>) -> Result<(), StorageError> {
    // bcachefs clears `BTREE_NODE_need_write` before submitting the write;
    // consume the equivalent reclaim-triggered rewrite marker at the same point.
    node.clear_need_rewrite();
    let cb_node = node.clone();
    let err_node = node.clone();
    if let Err(e) = submit_btree_node_io(node, move |err| {
        let result = err.map_or(Ok(()), Err);
        btree_node_write_done(&cb_node, &result);
    }) {
        err_node.clear_will_make_reachable();
        bch2_btree_node_io_unlock(&err_node);
        return Err(e);
    }
    Ok(())
}

/// bcachefs 对齐: __bch2_btree_node_write — 内部写节点（火抛）
///
/// 锁 + 序列化 + 提交 IO，IO 完成后自动调用 `__btree_node_write_done`。
/// 对应 bcachefs `__bch2_btree_node_write`（write.c:336），无闭包参数，不等待 IO。
///
/// journal pin 释放通过 node.journal 弱引用完成（由 BtreeWriter 在写前设置）。
///
/// bcachefs 对齐：错误路径释放 write_in_flight（通过 io_unlock）。
/// `submit_btree_node_io` 失败时保证 io_lock 被释放，避免锁泄漏。
pub fn __bch2_btree_node_write(node: Arc<BtreeNode>) -> Result<(), StorageError> {
    bch2_btree_node_io_lock(&node);
    __bch2_btree_node_write_locked(node)
}

/// 提交 btree 节点 IO（无锁 — 调用方负责 io_lock/unlock）
///
/// 序列化节点 + 分块提交写 IO。IO 全部完成后调用 `cleanup`。
fn submit_btree_node_io(
    node: Arc<BtreeNode>,
    cleanup: impl FnOnce(Option<StorageError>) + Send + 'static,
) -> Result<(), StorageError> {
    let block_addr = node.block_addr();
    if block_addr == 0 {
        return Err(StorageError::NotFound(
            "btree node block address not set".into(),
        ));
    }
    let record = node.serialize_initial_record(block_addr, 1)?;

    // 获取在线 RW 设备列表用于多副本写入；只读/evacuating 成员仍可
    // 作为读取副本，但不能被提交为新的 btree 副本。
    let devs = if let Some(vol) = node.vol_arc() {
        let registry = &vol.device_registry;
        registry
            .online_dev_indices()
            .into_iter()
            .filter_map(|dev_idx| registry.resolve_bch_dev(dev_idx))
            .filter(|dev| dev.member_state() == crate::storage::superblock::BchMemberState::Rw)
            .collect()
    } else {
        // 测试环境：使用单设备
        vec![node.vol_device()]
    };
    if devs.is_empty() {
        return Err(StorageError::NotFound(
            "no online rw device available for btree write".into(),
        ));
    }

    let cl = Closure::new();
    let first_err: Arc<AtomicFirstError> = Arc::new(AtomicFirstError::new());

    for (i, chunk) in record.chunks(BLOCK_SIZE).enumerate() {
        cl.get();
        let mut buf = chunk.to_vec();
        buf.resize(BLOCK_SIZE, 0);
        submit_bio_write_replicas(
            &devs,
            BlockAddr::new(block_addr + i as u64),
            buf,
            &cl,
            &first_err,
        );
    }
    if record.is_empty() {
        cl.get();
        cl.put();
    }

    let cb_err = first_err.clone();
    cl.continue_at(Box::new(move || {
        let err = cb_err.take();
        cleanup(err);
    }));
    cl.put();

    Ok(())
}

/// bcachefs 对齐: __btree_node_write_done — IO 完成后序清理
///
/// 对应 bcachefs `__btree_node_write_done`（write.c:25）：
/// 1. 清除 will_make_reachable（对应 CAS on b->will_make_reachable bit 0）
/// 2. 释放 journal pin（对应 bch2_journal_pin_drop）
/// 3. io_unlock（对应 non-rearm 路径: write_done_clean + flags CAS clear write_in_flight）
///
/// journal 引用直接从 node.journal 获取，无需外部传递。
///
/// IO 错误路径（bcachefs: set_btree_node_noevict + b->written += sectors_to_write）：
/// - Phase 1 当前仅 log + io_unlock（subvol cache 无 noevict 标志）
///
/// Phase 1 简化：
/// - 不处理 re-arm（dirty+need_write 重新触发写入）
/// - 不处理 btree_update closure signaling（will_make_reachable 不关联 btree_update）
/// - 不包含 wake_up_bit（subvol 使用 spin/yield 替代）
pub(crate) fn btree_node_write_done(node: &BtreeNode, result: &Result<(), StorageError>) {
    // bcachefs `__btree_node_write_done` 清理 will_make_reachable 和 journal pin
    // 在成功、失败路径都执行，不能因 I/O 错误提前返回而遗留 pin。
    node.clear_will_make_reachable();
    if let Some(j) = node.vol_journal() {
        if node.journal_pin.is_active() {
            j.bch2_journal_pin_drop(&node.journal_pin);
        }
    }

    if let Err(e) = result {
        tracing::error!(?e, "btree node write IO error");
        bch2_btree_node_io_unlock(node);
        return;
    }

    // 清除 write_in_flight + write_done_clean
    // bcachefs: CAS on b->flags (clear write_in_flight) → non-rearm → write_done_clean
    bch2_btree_node_io_unlock(node);
    bch2_btree_node_write_done_clean(node);
}

/// bcachefs 对齐: 等待 IO 完成的辅助函数（同步语义）
///
/// 提供给需要等待写完成的场景（测试、同步 flush）。
/// 内部调用 submit_btree_node_io + oneshot channel 桥接 + write_done。
#[cfg(test)]
async fn bch2_btree_node_write_await(node: Arc<BtreeNode>) -> Result<(), StorageError> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    bch2_btree_node_io_lock(&node);
    let cb_node = node.clone();
    let err_node = node.clone();
    if let Err(e) = submit_btree_node_io(node, move |err| {
        let result = err.map_or(Ok(()), Err);
        btree_node_write_done(&cb_node, &result);
        let _ = tx.send(result);
    }) {
        err_node.clear_will_make_reachable();
        bch2_btree_node_io_unlock(&err_node);
        return Err(e);
    }
    rx.await.unwrap()
}

/// bcachefs 对齐: bch2_btree_post_write_cleanup — 写入完成后的清理
///
/// bcachefs 对齐: bch2_btree_node_prep_for_write — 节点写前准备（commit.c:110-126）
///
/// 在获取每个 leaf 节点的 write 锁后、进行插入操作前调用：
/// 1. 若节点刚被写入（just_written），执行后处理清理（compact/rebuild aux）
/// 2. 仅在空间需要时初始化下一个增量 bset（want_new_bset 语义）
pub fn bch2_btree_node_prep_for_write(node: &mut BtreeNode) {
    // Step 1: 节点刚被写入 → 后处理清理
    if node.is_just_written() {
        bch2_btree_post_write_cleanup(node);
    }

    // Step 2: want_new_bset — 仅在空间不足时创建新 bset
    if let Some(bne_u64s) = want_new_bset(node) {
        bch2_bset_init_next(node, bne_u64s);
    }
}

/// bcachefs `want_new_bset()` (interior.h:345).
/// 判断是否需要创建新的未写入 bset 用于后续增量插入。
fn want_new_bset(node: &BtreeNode) -> Option<u16> {
    let nsets = node.nsets() as usize;
    if nsets >= MAX_BSETS {
        return None;
    }
    let last = nsets - 1;
    let write_block_u64s = node.write_block_offset() / std::mem::size_of::<u64>();
    let bne_u64s = write_block_u64s.max(usize::from(node.sets[last].end_offset));
    let remaining_space = node.__bch2_btree_u64s_remaining(
        u32::try_from(bne_u64s + crate::btree::node::BSET_HEADER_U64S as usize)
            .expect("btree bset offset exceeds u32"),
    );

    let block_bytes = node
        .vol_arc()
        .map(|c| c.block_size() as usize)
        .unwrap_or(BLOCK_SIZE);
    let block_sectors = block_bytes / crate::types::SECTOR_SIZE as usize;
    let btree_sectors = node.node_size as usize / crate::types::SECTOR_SIZE as usize;

    if node.bset_written(last) {
        if usize::from(node.written) + block_sectors <= btree_sectors {
            return Some(u16::try_from(bne_u64s).expect("btree bset offset exceeds u16"));
        }
    } else if usize::from(node.sets[last].end_offset - node.sets[last].first_key_offset())
        * std::mem::size_of::<u64>()
        > (8 << 9)
        && remaining_space > ((8 << 9) >> 3)
    {
        return Some(u16::try_from(bne_u64s).expect("btree bset offset exceeds u16"));
    }

    None
}

/// 写入完成后对节点进行后处理:
/// - 清除 just_written 标志（对应 clear_btree_node_just_written）
/// - 如果节点有多个 bset（nsets > 1）→ 排序合并到单个紧凑 set[0]（对应 bch2_btree_node_sort）
/// - 丢弃 whiteout（通过 compact 自动过滤 KeyType::Deleted）
/// - 仅在需要时初始化下一个增量 bset（对应 want_new_bset）
///
/// 返回 true 表示迭代器失效（需要重新 init），false 表示无变化。
pub fn bch2_btree_post_write_cleanup(node: &mut BtreeNode) -> bool {
    if !node.is_just_written() {
        return false;
    }

    // bcachefs: BUG_ON(b->whiteout_u64s)
    assert_eq!(
        node.whiteout_u64s, 0,
        "post_write_cleanup: whiteout_u64s should be 0 after write"
    );

    // 写入完成后清除各标志位 — 对应 bcachefs write_done：
    // - clear_btree_node_just_written
    // - clear_btree_node_need_rewrite (write.c:599)
    node.clear_just_written();
    node.clear_need_rewrite();

    let nsets = node.nsets();
    let invalidated = if nsets > 1 {
        // 多个 bset → 合并排序到 set[0]（对应 bch2_btree_node_sort(c, b, 0, b->nsets)）
        node.compact();
        true
    } else if (node.packed_keys > 0 || node.unpacked_keys > 0) && !node.sets[0].has_rw_aux_tree() {
        // 数据完整但 rw aux 树缺失 → 重建
        node.compact();
        true
    } else {
        false
    };

    // 仅在需要时初始化下一个增量 bset（对应 want_new_bset + bch2_bset_init_next）
    if let Some(bne_u64s) = want_new_bset(node) {
        bch2_bset_init_next(node, bne_u64s);
    }

    invalidated
}

/// bcachefs 对齐: bch2_btree_init_next — 初始化节点中的下一个 bset
///
/// 在 post_write_cleanup 后调用（仅在 want_new_bset 返回 true 时）。
/// 将下一个增量 bset 定位到当前所有 bset 之后的 block 对齐起始位置。
pub fn bch2_btree_init_next(node: &mut BtreeNode) {
    if let Some(bne_u64s) = want_new_bset(node) {
        bch2_bset_init_next(node, bne_u64s);
    }
}

fn bch2_bset_init_next(node: &mut BtreeNode, bne_u64s: u16) {
    let nsets = node.nsets() as usize;
    if nsets >= MAX_BSETS {
        return;
    }
    node.sets[nsets] = BsetTree {
        size: 0,
        extra: crate::btree::node::BSET_NO_AUX_TREE_VAL,
        data_offset: bne_u64s,
        aux_data_offset: u16::MAX,
        end_offset: bne_u64s + crate::btree::node::BSET_HEADER_U64S,
    };
    // bcachefs: i->seq = btree_bset_first(b)->seq
    let seq = unsafe {
        let off = node.sets[0].data_offset as usize * 8;
        (*(node.data.as_ptr().add(off) as *const BsetHeader)).seq
    };
    let header = BsetHeader {
        seq,
        journal_seq: 0,
        flags: 0,
        version: 0,
        u64s: 0,
    };
    let header_off = usize::from(bne_u64s) * 8;
    unsafe {
        node.data
            .as_mut_ptr()
            .add(header_off)
            .cast::<BsetHeader>()
            .write_unaligned(header);
    }
    node.nsets += 1;
    node.bch2_bset_build_aux_tree(nsets, true);
}

/// bcachefs 对齐: bch2_btree_flush_all_writes — 刷新所有飞行中的写操作
pub fn bch2_btree_flush_all_writes() -> bool {
    // subvol: 当前为同步写入，没有飞行中的写操作
    true
}

/// bcachefs 对齐: bch2_btree_cancel_all_writes — 取消所有飞行中的写操作
pub fn bch2_btree_cancel_all_writes() {
    // subvol: 当前为同步写入，无需取消
}

// ─── Compat 处理 ─────────────────────────────────────────────────────────

/// bcachefs 对齐: compat_bformat — 兼容性格式化处理
pub fn compat_bformat(
    _level: u8,
    _btree_id: u32,
    _version: u32,
    _big_endian: u32,
    _write: bool,
    _format: &mut crate::btree::key::BkeyFormat,
) {
    // subvol: 当前版本不需要兼容性转换
}

/// bcachefs 对齐: compat_bpos — 兼容性 Bpos 转换
pub fn compat_bpos(
    _level: u8,
    _btree_id: u32,
    _version: u32,
    _big_endian: u32,
    _write: bool,
    _pos: &mut crate::btree::key::Bpos,
) {
    // subvol: 当前版本不需要兼容性转换
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_device::{BchDev, MockBlockDevice};
    use crate::btree::key::{BchVal, BtreeKey, KeyType};
    use crate::btree::node::BLOCK_SIZE;
    use crate::journal::reclaim::{JournalEntryPinList, JournalPinType};
    use crate::journal::Journal;

    #[tokio::test]
    async fn test_bch2_btree_node_read_write() {
        let vol = crate::bch_vol::BchVol::test_trees();
        let trans = crate::btree::transaction::BtreeTrans::new_ro(&vol);
        let backend = vol
            .primary_device_rcu_noerror()
            .expect("test volume primary device")
            .bdev()
            .clone();
        let test_dev = Arc::new(BchDev::new(backend.clone(), 0));
        let mut tmp = BtreeNode::new_leaf();
        tmp.set_test_device(test_dev.clone());
        tmp.insert(BtreeKey::new(1, 1, KeyType::Normal), BchVal::new(0x100, 1));
        tmp.compact();
        let node = Arc::new(tmp);

        node.try_set_block_addr(42);
        bch2_btree_node_write_await(node.clone()).await.unwrap();
        let loaded = bch2_btree_node_read(&trans, 42).await.unwrap();
        assert_eq!(loaded.packed_keys, node.packed_keys);
        assert_eq!(loaded.unpacked_keys, node.unpacked_keys);
    }

    #[tokio::test]
    async fn test_bch2_btree_root_read() {
        let vol = Arc::new(crate::bch_vol::BchVol::test_trees());
        let backend = vol
            .primary_device_rcu_noerror()
            .expect("test volume primary device")
            .bdev()
            .clone();
        let test_dev = Arc::new(BchDev::new(backend.clone(), 0));
        let node = Arc::new(BtreeNode::new_leaf());
        node.set_test_device(test_dev.clone());
        node.try_set_block_addr(99);
        bch2_btree_node_write_await(node).await.unwrap();
        let btree = vol.btree(crate::btree::BtreeId::Extents);
        btree.set_vol_ref(&vol);
        let (_root, level) = bch2_btree_root_read(btree, 99).await.unwrap();
        assert_eq!(level, 0);
    }

    #[tokio::test]
    async fn test_btree_node_write_sets_level_pin_type() {
        let backend = Arc::new(MockBlockDevice::new());
        let journal = Journal::new(vec![100]);
        unsafe {
            assert!((*journal.pin_fifo.get())
                .push_back(JournalEntryPinList::new(1))
                .is_ok());
            assert!((*journal.pin_fifo.get())
                .push_back(JournalEntryPinList::new(1))
                .is_ok());
        }

        let mut tmp_leaf = BtreeNode::new(0);
        tmp_leaf.journal_seq = 1;
        tmp_leaf.set_test_device(Arc::new(BchDev::new(backend.clone(), 0)));
        let leaf = Arc::new(tmp_leaf);
        leaf.try_set_block_addr(123);
        bch2_btree_node_write(leaf.clone(), Some(&journal))
            .await
            .unwrap();
        let leaf_pin_type = leaf.journal_pin.pin_type;
        assert_eq!(leaf_pin_type, JournalPinType::Btree0);

        let mut tmp_interior = BtreeNode::new(5);
        tmp_interior.journal_seq = 2;
        tmp_interior.set_test_device(Arc::new(BchDev::new(backend.clone(), 0)));
        let interior = Arc::new(tmp_interior);
        interior.try_set_block_addr(124);
        bch2_btree_node_write(interior.clone(), Some(&journal))
            .await
            .unwrap();
        let interior_pin_type = interior.journal_pin.pin_type;
        assert_eq!(interior_pin_type, JournalPinType::Btree3);
    }

    #[test]
    fn test_read_done_validates() {
        let mut node = BtreeNode::new_leaf();
        node.min_key = Bpos::new(1, 0, 0);
        node.max_key = Bpos::new(1, 2, 0);
        assert!(bch2_btree_node_read_done(&mut node).is_ok());
        assert_eq!(node.sib_u64s, [0, 0]);
        let text = bch2_btree_node_header_to_text(&node);
        assert!(text.contains("level=0"));
    }

    #[test]
    fn test_validate_bset_rejects_invalid() {
        let mut node = BtreeNode::new_leaf();

        // 插入一些数据使其有有效的 bset 结构
        node.insert(BtreeKey::new(1, 1, KeyType::Normal), BchVal::new(0x100, 1));
        node.insert(BtreeKey::new(2, 1, KeyType::Normal), BchVal::new(0x200, 1));
        node.compact();

        // set[0] 应该有效
        assert!(bch2_validate_bset(&node, 0).is_ok());

        // 越界的 set index
        assert!(bch2_validate_bset(&node, MAX_BSETS).is_err());
        assert!(bch2_validate_bset(&node, MAX_BSETS + 1).is_err());

        // 无效的 data_offset（超出 node_size）
        let mut bad_node = BtreeNode::new_leaf();
        bad_node.sets[0].data_offset = 50000;
        bad_node.sets[0].size = 10;
        assert!(bch2_validate_bset(&bad_node, 0).is_err());

        // end_offset 必须至少容纳完整 BsetHeader。
        let mut short_header = BtreeNode::new_leaf();
        short_header.sets[0].end_offset = short_header.sets[0].data_offset;
        assert!(bch2_validate_bset(&short_header, 0).is_err());
    }

    #[test]
    fn test_validate_bset_keys_rejects_out_of_order() {
        let mut node = BtreeNode::new_leaf();

        // 插入 3 个 key（bch2_bset_insert 保持有序: 1, 2, 3）
        node.insert(BtreeKey::new(1, 1, KeyType::Normal), BchVal::new(0x100, 1));
        node.insert(BtreeKey::new(2, 1, KeyType::Normal), BchVal::new(0x200, 1));
        node.insert(BtreeKey::new(3, 1, KeyType::Normal), BchVal::new(0x300, 1));

        // 手动交换 data buffer 中 entry 1 和 entry 3 → 变成 3, 2, 1（降序）
        let first_key_byte = crate::btree::node::BSET_HEADER_U64S as usize * 8;
        let e1_u64s = node.read_entry_u64s(first_key_byte) as usize;
        let e1_size = e1_u64s * 8;
        let e2_start = first_key_byte + e1_size;
        let e2_u64s = node.read_entry_u64s(e2_start) as usize;
        let e2_size = e2_u64s * 8;
        let e3_start = e2_start + e2_size;
        let e3_u64s = node.read_entry_u64s(e3_start) as usize;
        let e3_size = e3_u64s * 8;

        // 交换 e1 ↔ e3
        let e1_data = node.data[first_key_byte..first_key_byte + e1_size].to_vec();
        let e3_data = node.data[e3_start..e3_start + e3_size].to_vec();
        node.data[first_key_byte..first_key_byte + e1_size].copy_from_slice(&e3_data);
        node.data[e3_start..e3_start + e3_size].copy_from_slice(&e1_data);

        // 手动构造的 bset 无 aux tree → 验证应拒绝降序
        node.sets[0].aux_data_offset = u16::MAX;
        let result = bch2_validate_bset_keys(&node, 0);
        assert!(
            result.is_err(),
            "unsorted entries in set[0] should fail: {:?}",
            result
        );

        // compact 后数据应为升序
        node.compact();
        assert!(bch2_validate_bset_keys(&node, 0).is_ok());
    }

    #[test]
    fn test_read_done_rejects_empty_data() {
        let mut node = BtreeNode::new_leaf();
        node.data.clear();
        assert!(bch2_btree_node_read_done(&mut node).is_err());
    }

    #[test]
    fn test_validate_bset_keys_zero_size_ok() {
        let node = BtreeNode::new_leaf();
        // size=0 的 bset 应该通过验证
        assert!(bch2_validate_bset_keys(&node, 0).is_ok());
    }

    #[tokio::test]
    async fn test_write_sorts_multiple_bsets() {
        // Phase 2 验证：写入前排序合并多个 bset
        let vol = crate::bch_vol::BchVol::test_trees();
        let trans = crate::btree::transaction::BtreeTrans::new_ro(&vol);
        let backend = vol
            .primary_device_rcu_noerror()
            .expect("test volume primary device")
            .bdev()
            .clone();
        let test_dev = Arc::new(BchDev::new(backend.clone(), 0));
        let mut tmp = BtreeNode::new_leaf();
        tmp.set_test_device(test_dev.clone());

        // 插入多个 key 后 compact（set[0] 填满）
        tmp.insert(BtreeKey::new(1, 1, KeyType::Normal), BchVal::new(0x100, 1));
        tmp.insert(BtreeKey::new(3, 1, KeyType::Normal), BchVal::new(0x300, 1));
        tmp.compact();

        // 在 set[1] 追加更多 key（模拟增量写入）
        tmp.insert(BtreeKey::new(2, 1, KeyType::Normal), BchVal::new(0x200, 1));
        tmp.insert(BtreeKey::new(4, 1, KeyType::Normal), BchVal::new(0x400, 1));
        let node = Arc::new(tmp);

        // 不 compact，直接 write（write 内部应该自动排序合并）
        node.try_set_block_addr(77);
        bch2_btree_node_write_await(node.clone()).await.unwrap();

        // 读回：read_done 流水线验证
        let loaded = bch2_btree_node_read(&trans, 77).await.unwrap();
        let roundtrip = loaded.serialize_to_bucket(77).unwrap();
        assert!(bch2_btree_node_read_done(
            &mut BtreeNode::deserialize_from_bucket(&roundtrip).unwrap()
        )
        .is_ok());
    }

    #[tokio::test]
    async fn test_drop_keys_outside_node_removes_out_of_range() {
        let mut node = BtreeNode::new_leaf();

        // 插入并 compact
        node.insert(BtreeKey::new(10, 1, KeyType::Normal), BchVal::new(0x100, 1));
        node.insert(BtreeKey::new(20, 1, KeyType::Normal), BchVal::new(0x200, 1));
        node.insert(BtreeKey::new(30, 1, KeyType::Normal), BchVal::new(0x300, 1));
        node.compact();

        // 设置范围：只保留 15..25
        node.min_key = Bpos {
            inode: 0,
            offset: 15,
            snapshot: 1,
        };
        node.max_key = Bpos {
            inode: 0,
            offset: 25,
            snapshot: 1,
        };

        bch2_btree_node_drop_keys_outside_node(&mut node).unwrap();
        assert_eq!(node.packed_keys, 1); // 只保留 key[20]
    }

    #[test]
    fn test_header_to_text_format() {
        let mut node = BtreeNode::new_leaf();
        node.insert(BtreeKey::new(42, 1, KeyType::Normal), BchVal::new(0x100, 1));
        node.compact();
        let text = bch2_btree_node_header_to_text(&node);
        assert!(text.contains("level=0"));
        assert!(text.contains("packed_keys=1"));
        assert!(text.contains("nsets=1"));
    }

    // ─── Phase 3: IO 标志位测试 ───────────────────────────────────

    #[test]
    fn test_io_lock_unlock() {
        let node = BtreeNode::new_leaf();

        // 初始状态：未被锁
        assert!(!node.is_write_in_flight());

        // 加锁
        bch2_btree_node_io_lock(&node);
        assert!(node.is_write_in_flight());
        assert!(
            node.flags.load(std::sync::atomic::Ordering::Acquire)
                & crate::btree::node::NODE_WRITE_IN_FLIGHT_INNER
                != 0
        );

        // 再次尝试加锁应失败（已被锁）
        assert!(!node.try_lock_write_in_flight());

        // 解锁
        bch2_btree_node_io_unlock(&node);
        assert!(!node.is_write_in_flight());
        assert_eq!(
            node.flags.load(std::sync::atomic::Ordering::Acquire)
                & crate::btree::node::NODE_WRITE_IN_FLIGHT_INNER,
            0
        );

        // 解锁后可重新加锁
        assert!(node.try_lock_write_in_flight());
        assert!(node.is_write_in_flight());
        bch2_btree_node_io_unlock(&node);
    }

    #[test]
    fn test_read_in_flight_flags() {
        let node = BtreeNode::new_leaf();

        assert!(!node.is_read_in_flight());
        assert!(node.try_lock_read_in_flight());
        assert!(node.is_read_in_flight());
        assert!(!node.try_lock_read_in_flight()); // 已被锁

        node.clear_read_in_flight();
        assert!(!node.is_read_in_flight());
    }

    #[test]
    fn test_just_written_flag() {
        let mut node = BtreeNode::new_leaf();

        assert!(!node.is_just_written());
        node.set_just_written();
        assert!(node.is_just_written());

        // post_write_cleanup 应清除 just_written
        bch2_btree_post_write_cleanup(&mut node);
        assert!(!node.is_just_written());
    }

    #[test]
    fn test_wait_on_read_write() {
        let node = BtreeNode::new_leaf();

        // 未设置标志时，wait 应立即返回
        bch2_btree_node_wait_on_read(&node);
        bch2_btree_node_wait_on_write(&node);
        // 如果没死锁即通过
    }

    // ─── Phase 2: sort_iter 测试 ──────────────────────────────────

    #[test]
    fn test_sort_iter_single_bset() {
        let mut node = BtreeNode::new_leaf();
        node.insert(BtreeKey::new(3, 1, KeyType::Normal), BchVal::new(0x300, 1));
        node.insert(BtreeKey::new(1, 1, KeyType::Normal), BchVal::new(0x100, 1));
        node.insert(BtreeKey::new(2, 1, KeyType::Normal), BchVal::new(0x200, 1));

        let mut iter = SortIter::init_from_node(&node);
        iter.add_all_bsets(&node);
        assert_eq!(iter.total_keys(), 3);

        let mut buf = vec![0u8; node.node_size as usize];
        let (written, _sorted_keys) = iter.sort_into(&mut buf).unwrap();
        assert!(written > 0);

        // 验证排序后的 key 顺序正确
        let sorted_node_data = &buf[..written];
        let mut offset = 0usize;
        let mut prev_offset = 0u64;
        while offset + 3 <= sorted_node_data.len() {
            let u64s = sorted_node_data[offset];
            if u64s == 0 {
                break;
            }
            let pk = unsafe {
                &*(sorted_node_data.as_ptr().add(offset) as *const crate::btree::key::BkeyPacked)
            };
            let (bpos, _, _, _) =
                crate::btree::key::bkey_unpack(&crate::btree::key::BKEY_FORMAT_CURRENT, pk);
            assert!(
                bpos.offset >= prev_offset,
                "key order violation at offset {}: {} < {}",
                offset,
                bpos.offset,
                prev_offset
            );
            prev_offset = bpos.offset;
            offset += (u64s as u32) as usize * 8;
        }
    }

    #[test]
    fn test_sort_iter_multiple_bsets() {
        let mut node = BtreeNode::new_leaf();

        // set[0]: keys 10, 30, 50
        node.insert(BtreeKey::new(10, 1, KeyType::Normal), BchVal::new(0x100, 1));
        node.insert(BtreeKey::new(30, 1, KeyType::Normal), BchVal::new(0x300, 1));
        node.insert(BtreeKey::new(50, 1, KeyType::Normal), BchVal::new(0x500, 1));
        node.compact();
        assert!(node.packed_keys > 0 || node.unpacked_keys > 0);

        // 初始化 set[1]（模拟写入后的增量 bset 分配）。
        // bcachefs want_new_bset 对未写入且数据 < 4096 字节的节点可能不创建新 bset，
        // 但 sort_iter 测试需要多 bset 场景验证跨 set 排序行为。
        bch2_btree_init_next(&mut node);
        if node.nsets() == 1 {
            // want_new_bset 拒绝 → 手动分配 set[1] 用于测试排序逻辑
            let last_end = node.sets[0].end_offset;
            let init_u64s =
                last_end.max((node.write_block_offset() / std::mem::size_of::<u64>()) as u16);
            bch2_bset_init_next(&mut node, init_u64s);
        }

        // set[1]: keys 20, 40
        node.insert(BtreeKey::new(20, 1, KeyType::Normal), BchVal::new(0x200, 1));
        node.insert(BtreeKey::new(40, 1, KeyType::Normal), BchVal::new(0x400, 1));

        assert!(node.nsets() > 1);
        assert_eq!(node.nsets(), 2);

        // 使用 sort_iter 收集所有 keys
        let mut iter = SortIter::init_from_node(&node);
        iter.add_all_bsets(&node);
        assert_eq!(iter.total_keys(), 5);

        // 排序并验证顺序
        let mut buf = vec![0u8; node.node_size as usize];
        let (written, _sorted_keys) = iter.sort_into(&mut buf).unwrap();
        assert!(written > 0);

        let sorted_node_data = &buf[..written];
        let mut offset = 0usize;
        let mut prev_offset = 0u64;
        let mut offsets = Vec::new();
        while offset + 3 <= sorted_node_data.len() {
            let u64s = sorted_node_data[offset];
            if u64s == 0 {
                break;
            }
            let pk = unsafe {
                &*(sorted_node_data.as_ptr().add(offset) as *const crate::btree::key::BkeyPacked)
            };
            let (bpos, _, _, _) =
                crate::btree::key::bkey_unpack(&crate::btree::key::BKEY_FORMAT_CURRENT, pk);
            assert!(bpos.offset >= prev_offset, "key order violation");
            offsets.push(bpos.offset);
            prev_offset = bpos.offset;
            offset += (u64s as u32) as usize * 8;
        }
        assert_eq!(offsets, [10, 20, 30, 40, 50]);
    }

    #[test]
    fn test_sort_iter_empty() {
        let node = BtreeNode::new_leaf();
        let mut iter = SortIter::init_from_node(&node);
        iter.add_all_bsets(&node);
        assert_eq!(iter.total_keys(), 0);

        let mut buf = [0u8; 256];
        let (written, _sorted_keys) = iter.sort_into(&mut buf).unwrap();
        assert_eq!(written, 0);
    }

    #[test]
    fn test_sort_iter_newer_deleted_suppresses_older_live_key() {
        let mut node = BtreeNode::new_leaf();
        let key = BtreeKey::new(42, 1, KeyType::Normal);
        node.insert(key, BchVal::new(0x4200, 1));
        node.compact();
        bch2_btree_init_next(&mut node);
        if node.nsets() == 1 {
            let last_end = node.sets[0].end_offset;
            let init_u64s =
                last_end.max((node.write_block_offset() / std::mem::size_of::<u64>()) as u16);
            bch2_bset_init_next(&mut node, init_u64s);
        }

        let si = node.nsets() as usize - 1;
        let where_off = node.sets[si].first_key_offset();
        let deleted = crate::btree::key::BtreeEntry::raw(
            crate::btree::key::Bpos::from_key(&key),
            KeyType::Deleted,
            Vec::new(),
        );
        let written = node.write_entry_bytes(u32::from(where_off) * 8, &deleted);
        node.sets[si].end_offset = where_off + (written / 8) as u16;

        let mut iter = SortIter::init_from_node(&node);
        iter.add_all_bsets(&node);
        let mut buf = vec![0u8; node.node_size as usize];
        let (written, keys) = iter.sort_into(&mut buf).unwrap();
        assert_eq!(written, 0);
        assert_eq!(keys, 0);
    }

    #[test]
    fn test_bch2_sort_keys_only_filters_deleted_key() {
        let mut node = BtreeNode::new_leaf();
        let key = BtreeKey::new(42, 1, KeyType::Normal);
        node.insert(key, BchVal::new(0x4200, 1));
        node.compact();
        bch2_btree_init_next(&mut node);
        if node.nsets() == 1 {
            let last_end = node.sets[0].end_offset;
            let init_u64s =
                last_end.max((node.write_block_offset() / std::mem::size_of::<u64>()) as u16);
            bch2_bset_init_next(&mut node, init_u64s);
        }

        let si = node.nsets() as usize - 1;
        let where_off = node.sets[si].first_key_offset();
        let deleted = crate::btree::key::BtreeEntry::raw(
            crate::btree::key::Bpos::from_key(&key),
            KeyType::Deleted,
            Vec::new(),
        );
        let written = node.write_entry_bytes(u32::from(where_off) * 8, &deleted);
        node.sets[si].end_offset = where_off + (written / 8) as u16;

        let mut iter = SortIter::init_from_node(&node);
        iter.add_all_bsets(&node);
        let mut buf = vec![0u8; node.node_size as usize];
        let written_u64s = bch2_sort_keys(&mut buf, &mut iter).unwrap();

        assert_eq!(written_u64s, usize::from(buf[0]));
        let packed = unsafe { &*(buf.as_ptr() as *const BkeyPacked) };
        let (pos, type_, _, _) = bkey_unpack(&BKEY_FORMAT_CURRENT, packed);
        assert_eq!(pos.offset, 42);
        assert_eq!(type_, KeyType::Normal as u8);
    }

    // ─── Phase 1: read_done_sort 集成测试 ─────────────────────────

    #[tokio::test]
    async fn test_read_done_sort_integration() {
        let vol = crate::bch_vol::BchVol::test_trees();
        let trans = crate::btree::transaction::BtreeTrans::new_ro(&vol);
        let backend = vol
            .primary_device_rcu_noerror()
            .expect("test volume primary device")
            .bdev()
            .clone();
        let test_dev = Arc::new(BchDev::new(backend.clone(), 0));
        let mut tmp = BtreeNode::new_leaf();
        tmp.set_test_device(test_dev.clone());

        // 插入 5 个 key，模拟真实写入到磁盘
        for i in 0..5 {
            tmp.insert(
                BtreeKey::new(i as u64 + 1, 1, KeyType::Normal),
                BchVal::new((i as u64 + 1) * 0x100, 1),
            );
        }
        tmp.compact();
        let node = Arc::new(tmp);

        // 写入磁盘并读回（read_done 在读取路径中被调用）
        node.try_set_block_addr(100);
        bch2_btree_node_write_await(node.clone()).await.unwrap();
        let mut loaded = bch2_btree_node_read(&trans, 100).await.unwrap();

        // 读回后手动调用 read_done 完成验证+排序
        let result = bch2_btree_node_read_done(&mut loaded);
        assert!(result.is_ok(), "read_done failed: {:?}", result);

        // 验证排序正确
        assert_eq!(loaded.packed_keys, 5);
        assert!(loaded
            .search(&BtreeKey::new(1, 1, KeyType::Normal))
            .is_some());
        assert!(loaded
            .search(&BtreeKey::new(5, 1, KeyType::Normal))
            .is_some());
    }

    // ─── Phase 2: 写入前排序测试 ──────────────────────────────────

    #[tokio::test]
    async fn test_bch2_sort_keys_integration() {
        let mut node = BtreeNode::new_leaf();

        // 多个 bset 混合
        node.insert(BtreeKey::new(5, 1, KeyType::Normal), BchVal::new(0x500, 1));
        node.insert(BtreeKey::new(3, 1, KeyType::Normal), BchVal::new(0x300, 1));
        node.compact();
        node.insert(BtreeKey::new(4, 1, KeyType::Normal), BchVal::new(0x400, 1));
        node.insert(BtreeKey::new(1, 1, KeyType::Normal), BchVal::new(0x100, 1));

        // 排序合并
        sort_node_for_write(&mut node).unwrap();

        // 验证合并后数据正确
        assert_eq!(node.nsets(), 1);
        assert_eq!(node.packed_keys, 4);
        assert!(node.search(&BtreeKey::new(1, 1, KeyType::Normal)).is_some());
        assert!(node.search(&BtreeKey::new(5, 1, KeyType::Normal)).is_some());
    }

    #[tokio::test]
    async fn test_write_mut_with_sort() {
        let vol = crate::bch_vol::BchVol::test_trees();
        let trans = crate::btree::transaction::BtreeTrans::new_ro(&vol);
        let backend = vol
            .primary_device_rcu_noerror()
            .expect("test volume primary device")
            .bdev()
            .clone();
        let mut tmp = BtreeNode::new_leaf();
        tmp.set_test_device(Arc::new(BchDev::new(backend.clone(), 0)));
        let cache = NodeCache::new();

        // 写入，compact，再追加（多 bset 场景）
        tmp.insert(
            BtreeKey::new(10, 1, KeyType::Normal),
            BchVal::new(0x1000, 1),
        );
        tmp.insert(
            BtreeKey::new(30, 1, KeyType::Normal),
            BchVal::new(0x3000, 1),
        );
        tmp.compact();
        tmp.insert(
            BtreeKey::new(20, 1, KeyType::Normal),
            BchVal::new(0x2000, 1),
        );
        let node = Arc::new(tmp);

        // write_mut 应在序列化前排序合并
        node.try_set_block_addr(55);
        btree_node_write_mut(node, &cache, None).await.unwrap();

        // 读取并验证
        let loaded = bch2_btree_node_read(&trans, 55).await.unwrap();
        assert_eq!(loaded.packed_keys, 3);
    }

    // ─── Phase 2: CRC32C 验证（在 deserialize 中已有）─────────────

    #[test]
    fn test_checksum_validation_on_deserialize() {
        let mut node = BtreeNode::new_leaf();
        // 插入足够多的 key 确保 bset 数据区域足够大（> 256 字节）
        for i in 0..20 {
            node.insert(
                BtreeKey::new(i as u64, 1, KeyType::Normal),
                BchVal::new(i as u64 * 0x100, 1),
            );
        }
        node.compact();

        let data = node.serialize_to_bucket(42).unwrap();
        assert!(data.len() == BLOCK_SIZE);

        // 正常反序列化应通过
        assert!(BtreeNode::deserialize_from_bucket(&data).is_ok());

        // bset 数据区域起点在 header（~96 字节）后，经 8 字节对齐
        // 使用 offset 128 确保处于 bset 数据区域内部（有 20 个 key）
        let bset_offset = 128usize;
        if bset_offset < data.len() {
            let mut corrupted = data.clone();
            corrupted[bset_offset] ^= 0xFF;
            let result = BtreeNode::deserialize_from_bucket(&corrupted);
            assert!(
                result.is_err(),
                "corrupted bset data should trigger CRC error, got: {:?}",
                result
            );
        }
    }

    // ─── Phase 4: 负面测试 ────────────────────────────────────────

    #[test]
    fn test_negative_read_done_with_additional_bsets() {
        let mut node = BtreeNode::new_leaf();

        // 插入数据，compact，再插入新数据（形成多个 bset）
        node.insert(BtreeKey::new(2, 1, KeyType::Normal), BchVal::new(0x200, 1));
        node.insert(BtreeKey::new(4, 1, KeyType::Normal), BchVal::new(0x400, 1));
        node.compact();
        // 此时 set[0] 有 2 个有效 key: 2, 4

        // compact 后只有 set[0]，在 set[0] 上设置异常来测试 validate_bset 的拒绝
        node.sets[0].data_offset = 100;
        node.sets[0].end_offset = 50; // data_offset > end_offset → 应报错
        node.sets[1].size = 0;

        let result = bch2_btree_node_read_done(&mut node);
        // 应该出错：data_offset > end_offset
        assert!(
            result.is_err(),
            "should reject bset with data_offset > end_offset"
        );
    }

    #[tokio::test]
    async fn test_roundtrip_write_read_with_read_done() {
        let vol = crate::bch_vol::BchVol::test_trees();
        let trans = crate::btree::transaction::BtreeTrans::new_ro(&vol);
        let backend = vol
            .primary_device_rcu_noerror()
            .expect("test volume primary device")
            .bdev()
            .clone();
        let mut tmp = BtreeNode::new_leaf();
        tmp.set_test_device(Arc::new(BchDev::new(backend.clone(), 0)));

        for i in 0..10 {
            tmp.insert(
                BtreeKey::new(i as u64, 1, KeyType::Normal),
                BchVal::new(i as u64 * 0x100, 1),
            );
        }
        tmp.compact();
        let node = Arc::new(tmp);

        // 写入
        node.try_set_block_addr(200);
        bch2_btree_node_write_await(node).await.unwrap();

        // 读取
        let mut loaded = bch2_btree_node_read(&trans, 200).await.unwrap();

        // read_done 验证
        assert!(bch2_btree_node_read_done(&mut loaded).is_ok());

        // 验证所有 key 都存在
        for i in 0..10 {
            assert!(
                loaded
                    .search(&BtreeKey::new(i as u64, 1, KeyType::Normal))
                    .is_some(),
                "key {} should survive roundtrip",
                i
            );
        }
    }

    #[test]
    fn test_post_write_cleanup_single_bset() {
        let mut node = BtreeNode::new_leaf();
        node.insert(BtreeKey::new(1, 1, KeyType::Normal), BchVal::new(0x100, 1));
        node.insert(BtreeKey::new(2, 1, KeyType::Normal), BchVal::new(0x200, 1));
        node.compact();
        // bcachefs: post_write_cleanup 仅对 write 完成的节点生效
        node.set_just_written();

        // 单 bset，无 whiteout，已有 aux → 无需 compact
        assert!(!bch2_btree_post_write_cleanup(&mut node));

        // bcachefs want_new_bset 对数据 < 4096 字节且未写入的节点可能不创建新 bset，
        // 因此 nsets 可能为 1（write block 内无需新 set）或 2（有足够空间分配增量 set）
        assert!(
            node.nsets() >= 1,
            "post_write_cleanup should keep at least one bset"
        );
        assert!(
            !node.is_just_written(),
            "just_written flag should be cleared after cleanup"
        );
        // 若确实创建了增量 set，验证其 offset > data_offset
        if node.nsets() >= 2 {
            assert!(
                node.sets[1].end_offset > node.sets[1].data_offset,
                "init_next should set end_offset past header for the next bset"
            );
        }
    }

    #[test]
    fn test_post_write_cleanup_with_whiteout() {
        let mut node = BtreeNode::new_leaf();
        for i in 0..10 {
            node.insert(
                BtreeKey::new(i as u64, 1, KeyType::Normal),
                BchVal::new(i as u64 * 0x100, 1),
            );
        }
        node.compact();

        // 删除一个 key（产生 whiteout），但数量不足以触发 auto-compact
        node.delete_key(&BtreeKey::new(3, 1, KeyType::Normal));
        // (已 compact，所以空间不足时 delete 的 mark_entry_deleted_inplace 可能触发)

        // 这时 whiteout_u64s > 0 → post_write_cleanup 应触发 compact
        if node.whiteout_u64s > 0 {
            assert!(bch2_btree_post_write_cleanup(&mut node));
        }
    }

    // ─── Phase 4: read_done 对空节点的处理 ────────────────────────

    #[test]
    fn test_read_done_empty_node_ok() {
        let mut node = BtreeNode::new_leaf();
        // 空节点应该通过 read_done（无数据可验证）
        assert!(bch2_btree_node_read_done(&mut node).is_ok());
        assert_eq!(node.packed_keys, 0);
    }

    // ─── Phase 3: 节点标志位组合测试 ──────────────────────────────

    #[test]
    fn test_node_flags_independence() {
        let node = BtreeNode::new_leaf();

        // 各种标志位应独立
        node.set_write_in_flight();
        assert!(node.is_write_in_flight());
        assert!(!node.is_read_in_flight());
        assert!(!node.is_just_written());

        node.set_read_in_flight();
        assert!(node.is_write_in_flight());
        assert!(node.is_read_in_flight());

        node.set_just_written();
        assert!(node.is_just_written());

        // 清除其中一个不影响其他
        node.clear_write_in_flight();
        assert!(!node.is_write_in_flight());
        assert!(node.is_read_in_flight());
        assert!(node.is_just_written());

        node.clear_read_in_flight();
        node.clear_just_written();
        assert!(!node.is_read_in_flight());
        assert!(!node.is_just_written());
    }

    // ─── bch2_sort_keys 单 bset 场景 ──────────────────

    #[test]
    fn test_sort_keys_single_bset() {
        let mut node = BtreeNode::new_leaf();
        node.insert(BtreeKey::new(3, 1, KeyType::Normal), BchVal::new(0x300, 1));
        node.insert(BtreeKey::new(1, 1, KeyType::Normal), BchVal::new(0x100, 1));
        node.compact();
        // 现在只有一个 bset

        sort_node_for_write(&mut node).unwrap();
        assert_eq!(node.nsets(), 1);
        assert_eq!(node.packed_keys, 2);
    }

    // ─── io_lock 重入安全测试 ─────────────────────────────────────

    #[test]
    fn test_io_lock_reentry_safe() {
        let node = BtreeNode::new_leaf();

        // 加锁
        assert!(node.try_lock_write_in_flight());

        // 尝试在同一个线程再次加锁应失败（非可重入）
        assert!(!node.try_lock_write_in_flight());

        // 解锁后可以再加
        node.clear_write_in_flight();
        assert!(node.try_lock_write_in_flight());
        node.clear_write_in_flight();
    }
}
