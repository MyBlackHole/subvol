# Quality Guidelines

> Code quality standards for backend development.

---

## Overview

<!--
Document your project's quality standards here.

Questions to answer:
- What patterns are forbidden?
- What linting rules do you enforce?
- What are your testing requirements?
- What code review standards apply?
-->

(To be filled by the team)

---

## Current Test Baseline

- `cargo test -p subvol-core --lib` currently passes with `1227 passed; 0 failed; 0 ignored` on 2026-07-19.
- The old `9 ignored` count is stale and should not be used as the current reference line.
- Keep future spec updates on the latest verified count, not on historical batch snapshots.

## Gotchas

### Test 中的 mount 错误消息可能因 locale 不同（2026-07-19）

CLI 集成测试 `test_cli_fuse_mount_xfs_file_operations` 在非 root 环境下跳过 XFS 挂载，
通过匹配 `mount` 错误消息判断权限不足。如果系统 locale 是中文，"permission denied"
会被翻译为"权限不够"，测试需要同时检查中英文消息。

```rust
// Wrong: only checks English
if lower_error.contains("permission denied") { skip(); }

// Correct: also check common locale translations
if lower_error.contains("permission denied")
    || lower_error.contains("权限不够")
{ skip(); }
```

---

## Design Decisions

### Data extent I/O submits independent blocks before waiting (2026-07-16)

The local bcachefs write path submits each valid replica bio before waiting for
the parent completion (`/home/black/Documents/bcachefs-tools/fs/data/write.c:1341-1478`).
Its completion path records the first error while allowing every submitted bio
to finish. The corresponding volume write path therefore builds one future per
block and joins all results, preserving submission order for the first error
without cancelling still-running writes.

The local read path likewise constructs/splits read bios and completes through
an aggregate read-bio path (`fs/data/read.c:1245-1315`); the block-device
`read_blocks` default follows that model with independent per-block buffers,
then copies completed buffers back in request order. It must not hold a btree
iterator or transaction lock across these awaits.

Required checks:

- acquire and release the device IO reference for every submitted block;
- retain allocation-before-data-IO ordering in `BchVol::write_extent`;
- await all submitted operations even after one reports an error;
- preserve the caller's logical block order and existing error type;
- keep tests under the one-minute timeout.

### Data extent reads retain replica pointers and retry devices (2026-07-16)

Local bcachefs selects a complete extent pointer before submission and retries
another pointer after an IO failure (`/home/black/Documents/bcachefs-tools/fs/data/read.c:592-628`,
`fs/data/read.c:692-740`). The volume read path must therefore:

- capture `dev_idx` and all `ExtentPtrs` entries before dropping the btree transaction;
- acquire a read IO reference immediately before each candidate submission and skip
  offline devices;
- retry candidates in extent-pointer order and return the first successful read;
- return the first device error if every candidate fails, without changing the
  logical range or its extent-relative physical offset.

### User-data writes allocate and submit every requested replica (2026-07-16)

The local option is `data_replicas` (`fs/opts.h:160-166`), and the foreground
write path allocates the requested replica pointers before submitting data IO
(`fs/data/write.c:2736-2821`). `bch2_submit_wbio_replicas()` then gets a device
IO reference for every valid pointer and submits all bios before completion
(`fs/data/write.c:1341-1478`). The equivalent contracts are:

- persist `data_replicas` with the volume storage configuration, defaulting to
  one and validating the local supported range;
- allocate and consume space for each writable device before the first replica
  write; if fewer devices are available, commit only the pointers actually
  allocated (the same degraded-replica outcome as the allocator);
- submit all replica writes before awaiting any completion and preserve the
  first error in pointer order;
- commit multi-pointer extent values as `ExtentPtrs`, so journal replay retains
  the physical device mapping; any later range split must copy those pointers
  before committing the trimmed key.

### Open-bucket allocation must consume sectors before data IO (2026-07-13)

#### 1. Scope / Trigger

- Applies to extent writes that obtain a physical block address from `BchAllocator` and to both L1 write-point reuse and L2 open-bucket reuse.
- The local source contract is `/home/black/Documents/bcachefs-tools/fs/alloc/foreground.h:390-429`: `bch2_ob_ptr()` derives the current offset from bucket sectors, then `bch2_alloc_sectors_append_ptrs_inlined()` consumes write-point and open-bucket sectors before data IO is submitted.

#### 2. Signatures

```rust
pub fn bch2_alloc_sectors_start_trans(
    &self,
    count: u64,
    vol: &BchVol,
    ca: &BchDev,
    request: &AllocRequest,
    wp_id: Option<WritePointSpecifier>,
) -> Result<u64, AllocError>;

pub(crate) fn bch2_consume_written_extent(
    &self,
    ca: &BchDev,
    block_addr: u64,
    blocks_written: u64,
);
```

The signatures remain unchanged. `block_addr` and the returned offset are in blocks; `sectors_free` and `sectors_needed` are in 512-byte sectors.

#### 3. Contracts

- A new open bucket starts with the selected device's `ca.mi.bucket_size` free sectors.
- Detecting an unconsumed new bucket must compare against that per-device sector capacity,
  never against `BLOCKS_PER_BUCKET` or another device's capacity.
- The L1 block offset is `(ca.mi.bucket_size - old_sectors_free) / SECTORS_PER_BLOCK`.
- Open-bucket identity is `(ca.dev_idx, sector_to_bucket(ca, block_addr *
  SECTORS_PER_BLOCK))`; devices with different bucket sizes may have the same bucket number.
- Consume the allocation before `write_blocks(...).await`, matching local bcachefs append-pointer ordering.
- A second allocation from the same bucket must start after the first allocation and must never reuse its physical block address.

#### 4. Validation & Error Matrix

- `old_sectors_free >= sectors_needed` -> atomically consume sectors and return the pre-consumption block offset.
- `old_sectors_free < sectors_needed` -> restore the failed `fetch_sub` and continue the existing fallback path.
- Newly allocated bucket still at full sector capacity -> consume `blocks_written * SECTORS_PER_BLOCK` exactly once.
- Reused bucket already consumed by the allocation path -> do not consume it a second time.
- Device IO failure after consumption -> return the IO error while leaving the allocation consumed, matching bcachefs allocation-before-IO ordering.

#### 5. Good/Base/Bad Cases

- Good: one-block writes receive `base`, then `base + 1`; a snapshot overwrite therefore preserves the ancestor's physical data.
- Base: the first allocation from a new bucket returns the bucket base and reduces `sectors_free` by `SECTORS_PER_BLOCK`.
- Bad: compare sector-valued `sectors_free` to `BLOCKS_PER_BUCKET`; the new bucket remains full and the next write returns the same physical address.

#### 6. Tests Required

- `test_new_open_bucket_consumption_advances_next_allocation` must assert `second == first + 1`.
- Dynamic-geometry tests must cover 1024/4096-sector members, reservation deltas,
  open capacity, reuse offsets, and cross-device writepoint filtering.
- All `try_reuse_current_wp` tests must pass, including the bucket-base offset case.
- `test_nbd_snapshot_consistency` must write, snapshot, overwrite, rollback, and recover the pre-overwrite bytes.
- `timeout 60s cargo test -p subvol-core --lib` and the full NBD integration suite must pass.

#### 7. Wrong vs Correct

```rust
// Wrong: global default geometry is used for an arbitrary device.
let capacity = BLOCKS_PER_BUCKET * SECTORS_PER_BLOCK;
if sectors_free == capacity as u32 { /* consume */ }

// Correct: compare against the selected device's sector capacity, then convert to blocks.
let capacity = unsafe { &*ca.mi.get() }.bucket_size as u64;
if sectors_free == capacity as u32 { /* consume */ }
let block_offset = (capacity - old_sectors_free as u64) / SECTORS_PER_BLOCK;
```

### NBD extent read must release btree path locks before `.await` (2026-07-13)

#### 1. Scope / Trigger

- Applies when `BchVol::read_extent_for_snapshot()` maps Extents btree keys to asynchronous `BlockDevice` reads.
- The local source contract is `/home/black/Documents/bcachefs-tools/fs/data/read.c:1765-1832`: copy/reassemble the selected extent before submitting data IO.

#### 2. Signatures

```rust
pub async fn read_extent_for_snapshot(
    &self,
    vaddr: u64,
    buf: &mut [u8],
    snapshot_id: u32,
) -> Result<(), StorageError>;
```

The public signature is unchanged. The implementation records owned IO-plan values `(backend, paddr, block_count, byte_offset, byte_len)` while the transaction is locked.

#### 3. Contracts

- Extent lookup, visibility selection, hole handling, and physical-address calculation run while the `BtreeTrans` path is valid.
- The plan owns every value needed by device IO; it must not retain a borrowed btree key, iterator, path, or node pointer.
- `BtreeTrans` is dropped before the first device `.await`.
- Device reads retain request order and write into their original non-overlapping buffer ranges.

#### 4. Validation & Error Matrix

- Empty buffer -> return `Ok(())` without lookup or IO.
- Unaligned byte offset/length -> preserve `extent_bytes_to_blocks()` validation error.
- Hole, deleted key, or trim hole -> zero-fill the corresponding output range; do not enqueue device IO.
- Missing primary device for a mapped extent -> fail at plan construction, matching the existing invariant check.
- Device read failure -> return the first `StorageError`; never continue with later planned reads.

#### 5. Good/Base/Bad Cases

- Good: map one or more extents, drop the transaction, then await the planned reads in order.
- Base: an entirely sparse request produces an empty plan and a zero-filled buffer.
- Bad: hold `BtreeTrans` or `BtreeIter` across `read_blocks(...).await`; Tokio may resume on another worker and invalidate six-lock current-owner accounting.

#### 6. Tests Required

- `timeout 60s cargo test -p subvol-core --lib` must pass.
- `timeout 60s cargo test -p subsubvolmountd --test nbd_integration -- --nocapture --test-threads=1` must cover handshake, sparse read, read-after-write, multi-block IO, TRIM, multi-volume isolation, and out-of-bounds connection reuse.
- The integration run must contain no `SixLock::six_unlock_read` underflow or NBD disconnect after a successful write.

#### 7. Wrong vs Correct

```rust
// Wrong: transaction path locks remain live across an async suspension point.
dev.bdev().read_blocks(addr, count, dst).await?;

// Correct: copy the IO mapping, release BtreeTrans, then await backend IO.
reads.push((Arc::clone(dev.bdev()), paddr, count, byte_off, byte_len));
drop(trans);
for (backend, paddr, count, byte_off, byte_len) in reads {
    backend.read_blocks(BlockAddr::new(paddr), count, &mut buf[byte_off..byte_off + byte_len]).await?;
}
```

### Bset layout and physical-delete contract (2026-07-12)

#### 1. Scope / Trigger

- Applies to bset construction, iteration, insert/delete, split/merge repacking, and read/write sorting.
- The only reference is local `/home/black/Documents/bcachefs-tools/fs/btree/bset.c` and `interior.c`.

#### 2. Signatures

```rust
pub fn bch2_bset_insert(&mut self, where_off: u16, insert: &BtreeEntry, clobber_u64s: u16);
pub fn bch2_bset_delete(&mut self, where_off: u16, clobber_u64s: u16);
pub fn bch2_btree_node_iter_init(iter: &mut BtreeNodeIter, b: &BtreeNode, pos: &Bpos);
```

#### 3. Contracts

- `BsetTree.data_offset` points to `BsetHeader`; the first packed key is at `data_offset + BSET_HEADER_U64S`.
- `BsetTree.size` is auxiliary-tree node count, never packed-key count.
- Deleting an old key in the writable bset physically removes it and requires no new bset space.
- Split/merge outputs inherit source routing ranges. They must not replace `min_key/max_key` with actual first/last packed keys.

#### 4. Validation & Error Matrix

- Key scan starting at `data_offset` -> invalid: header would be decoded as a key.
- `pos < min_key || pos > max_key` in node iterator init -> debug failure, matching local `EBUG_ON`.
- Deleted operation on a full writable bset -> allowed; it must not fail the non-deleted insertion capacity check.
- Zero packed-key `u64s` inside a non-empty iterator range -> corrupt layout; never treat it as normal progress.

#### 5. Good/Base/Bad Cases

- Good: RO bset contains old key, writable bset has its own header, merged iterator returns both sets in key order.
- Base: empty writable bset has `end_offset == first_key_offset()` and header `u64s == 0`.
- Bad: writing the first key at byte 0 when `data_offset == 0`; this overwrites the 24-byte bset header.

#### 6. Tests Required

- Multi-set search must construct separate RO and writable bset headers.
- Delete tests must cover a full writable bset and assert physical deletion succeeds without a tombstone allocation.
- Split/merge tests must assert distant keys remain reachable after every merge boundary update.
- Full gate: `cargo test -p subvol-core --lib`.

#### 7. Wrong vs Correct

```rust
// Wrong: starts on the header and rejects deletion when no insertion space remains.
let mut cur = u32::from(set.data_offset) * 8;
if end + insert_u64s > capacity { return false; }

// Correct: keys start after the header; only non-deleted inserts need new space.
let mut cur = u32::from(set.first_key_offset()) * 8;
if entry.key_type != KeyType::Deleted && end + insert_u64s > capacity { return false; }
```

### SortIter ordered-bset merge contract (2026-07-13)

#### 1. Scope / Trigger

- Applies when read completion or the write path merges two or more bsets in one `BtreeNode`.
- The sole references are local `fs/btree/sort.c:21-125` and `fs/btree/sort.h:7-44`.
- The prerequisite is satisfied by `BtreeNode::bch2_bset_insert()`: it inserts at the iterator-selected offset and uses overlapping `ptr::copy` to preserve writable-bset order.

#### 2. Signatures

```rust
pub fn sort_into(&mut self, dst: &mut [u8]) -> Result<(usize, usize), StorageError>;
fn sift<F>(&mut self, from: usize, cmp: F);
fn sort<F>(&mut self, cmp: F);
fn peek(&self) -> Option<u32>;
fn advance<F>(&mut self, cmp: F) -> Result<(), StorageError>;
fn next<F>(&mut self, cmp: F) -> Result<Option<u32>, StorageError>;
fn should_drop_next_key(&self) -> bool;
fn bch2_sort_keys(dst: &mut [u8], iter: &mut SortIter) -> Result<usize, StorageError>;
```

`SortIter` retains local bcachefs fields `used` and `size`; `SortIterEntry.cur/end` are byte-offset equivalents of `sort_iter_set.k/end`.

#### 3. Contracts

- `sort_iter_sort()` is not a binary heap build: iterate `from` from `used - 1` down to `0`, and each `sift()` compares only adjacent entries while moving forward.
- `advance()` progresses only entry zero. If it reaches `end`, remove entry zero by shifting later entries left; otherwise call `sift(0)`.
- For overlapping-key repair, compare equal packed keys by original pointer order. Because every key points into one `node.data` allocation, byte-offset order is pointer order.
- Before advancing, drop entry zero when entry zero and entry one have equal packed positions; this makes the older key disappear and retains the newer key. Deleted keys are not copied.
- Read completion uses the overlap-repair comparator and `peek -> filter/copy -> advance` order from `bch2_key_sort_fix_overlapping()`.
- Write sorting uses packed-position comparison without pointer tie-break and `sort_iter_next -> filter/copy` order from `bch2_sort_keys()`. It filters only the Deleted entry; an older live entry at the same position remains in the output for subsequent compaction.
- `bch2_sort_keys()` returns written u64s, not bytes. Callers convert to bytes only when copying the temporary output.
- Do not allocate a `Vec<u32>` containing every key; memory stays bounded by the bset cursor array (`MAX_BSETS`).

#### 4. Validation & Error Matrix

- `used >= size` on add -> invariant failure, matching local `BUG_ON`.
- Truncated packed-key header -> `StorageError::CorruptData`.
- `u64s == 0` before `end` -> `StorageError::CorruptData`; never stop silently or loop.
- Advancing past a cursor's `end` -> `StorageError::CorruptData`, matching local `BUG_ON(i->k > i->end)`.
- Destination too small -> `StorageError::CorruptData`; do not partially overrun `dst`.

#### 5. Good/Base/Bad Cases

- Good: bsets `[10, 30, 50]` and `[20, 40]` merge to exactly `[10, 20, 30, 40, 50]`.
- Good: write sorting of `[old live, newer Deleted]` outputs the old live key and filters only Deleted; read overlap repair outputs neither.
- Base: zero active bsets writes zero bytes and zero keys.
- Bad: make write sorting reuse read overlap suppression; it removes the older live key before normal node compaction.

#### 6. Tests Required

- Single-bset, empty, and interleaved multi-bset SortIter cases.
- Newer Deleted key suppresses an older live key at the same packed position.
- Direct `bch2_sort_keys` regression must assert the older live key remains, Deleted is filtered, and the return value equals output u64s.
- `timeout 60s cargo test -p subvol-core --lib` must pass.

#### 7. Wrong vs Correct

```rust
// Wrong: write sorting reuses read overlap repair and returns bytes.
let (bytes, _) = iter.sort_into(dst)?;
Ok(bytes)

// Correct: bch2_sort_keys has its own comparator/filter order and u64 return.
iter.sort(packed_position_cmp);
while let Some(offset) = iter.next(packed_position_cmp)? {
    if !key_deleted(offset) {
        copy_key(offset);
    }
}
Ok(dst_offset / 8)
```

### Watermark 水位线分配策略 (2026-06-24)

**问题**: 避免分配器在空间压力下死锁 — 高优先级操作（journal、btree 内部更新）需要预留桶。

**方案**: 7 级 `Watermark` 枚举（stripe=0 → interior_updates=6，低值=高需求），每级保留桶数通过 if 链模拟 C switch fallthrough 累加。分配时检查 `free - reserved(watermark) > 0`。

**模式**:
- `Watermark::reserved_buckets()` — if 链累加，与 bcachefs `bch2_dev_buckets_reserved` 语义一致
- `Watermark::allows(request)` — `request >= self` 允许通过
- 测试中使用 `Watermark::InteriorUpdate`（预留 0）避免小型分配器（1-4 桶/组）测试失败

### Freespace per-group 栈 (2026-06-24)

**问题**: 原 `allocate_bucket()` 使用 O(n) 线性扫描查找空闲 bucket。

**方案**: `AllocGroup.free_list: Vec<u32>` — 存空闲 bucket 索引。分配时 `pop()` O(1)，释放时 `push(bi)`。`free_buckets` 原子计数与此保持一致。

**注意事项**:
- `free_list.pop()` 和 `group.buckets[bi]` 不能在同一闭包中同时可变借用 — 分两步操作（先 pop 索引，再访问 bucket）
- `load_from_btree()` 启动时通过 `filter/bucket.state == Free` 重建 free_list
- 释放路径先进入 `NeedDiscard`，记录当前 journal seq；所有进入 `Free` 的路径都会清空两个 journal seq，`bch2_bucket_do_trim()` 只是其中最主要的落点

### commit() 提交路径 (2026-07-18)

事务提交只保留本地 bcachefs 的单一路径：由 `__bch2_trans_commit()` 完成状态校验、触发器执行、journal 大小计算、重启/重试和最终 reset；底层提交按本地 `do_bch2_trans_commit()` 的顺序完成 journal reservation、写锁、journal replay pre/post、写入及解锁。

subvol 不增加绑定 `BchVol` 的第二套 commit API，也不把触发器拆成自有的三阶段包装。对应控制流以本地 `fs/btree/commit.c:1280-1523` 为准。

### shrink() 两阶段时钟淘汰算法 (2026-06-27)

**问题**: BtreeNodeCache 需要接近 LRU 的淘汰行为，但纯 LRU 实现（`remove_last`）无法利用 recently-accessed 节点的热数据特性。

**方案**: 两阶段时钟（two-phase clock）扫描代替 LRU pop：

```rust
pub fn shrink(&self, target: usize) -> usize {
    let mut inner = self.inner.lock().unwrap();
    let min_keep = 64usize;
    let max_evict = inner.clean.len().saturating_sub(min_keep);
    let target = target.min(max_evict);
    if target == 0 { return 0; }
    
    // 相位 1: 从 LRU front 扫描 target + 64 个节点
    // 访问过的节点 → 清除 accessed 标志（第 2 次再被扫描才淘汰）
    // 未访问的节点 → 淘汰
    let mut scanned = 0usize;
    let scan_limit = target + 64;  // 宽松扫描窗口
    let ids: Vec<u64> = inner.clean_lru.iter().take(scan_limit).copied().collect();
    
    for &id in &ids {
        if scanned >= target { break; }
        scanned += 1;
        let should_evict = inner.clean.get(&id).map_or(false, |node| {
            if node.is_accessed() {
                node.clear_accessed();  // 第一轮清除标志
                false
            } else {
                true  // 第二轮淘汰
            }
        });
        if should_evict {
            inner.clean.remove(&id);
            inner.clean_lru.retain(|&x| x != id);
            freed += 1;
        }
    }
}
```

**关键参数**:
- `min_keep = 64`: 绝对值保护下限，防止 shrink 清空整个 cache
- `scan_limit = target + 64`: 比目标多扫描 64 个节点，给第一次扫描的节点第二次机会
- 两轮访问保护：节点在 `clean_lru` 中的位置不动，accessed 标志清除后下次再被扫描才淘汰
- `system_memory_usage_high()` 的判定要优先看系统可用内存是否低于 1/4，总 cache footprint 再和剩余压力做二次比较，避免把本地固定阈值误当成 upstream 语义
- dirty 节点进入 `BtreeCache` 时要同步打上 `NODE_NEED_REWRITE`，写完成入口 `bch2_btree_node_write_done_clean()` 负责清理该标志，避免“缓存脏”和“节点需重写”语义脱节

**对应 bcachefs**:
- bcachefs `bch2_btree_cache_shrink` 使用 `list_for_each_entry` 扫描 clean list
- bcachefs 使用 `btree_node_accessed` clear + shrink 相同两阶段模式
- subvol 使用 `VecDeque` + `retain`，而非 intrusive linked list

### Node 生命周期标志 (2026-06-27)

**BtreeNode 新增字段**:

```rust
pub struct BtreeNode {
    // ... 已有字段
    pub accessed: AtomicBool,    // shrink 两阶段时钟使用
    pub need_rewrite: bool,      // btree split/update 后可能需要重写
}
```

**语义**:
- `accessed`: 仅由 cache shrink 读取/修改（其他路径不涉及）。`set_accessed()` 在 cache lookup/insert 时调用；`clear_accessed()` 只在 shrink phase 1 调用。
- `need_rewrite`: 由 btree 更新路径在 split/compact 后设置，在 `flush_dirty_nodes()` 中检查。
- `need_rewrite` 也用于恢复阶段的 fake root：`btree_root_alloc_fake()` 必须在节点进入 cache 前设置该标志，避免把占位根误当作可直接持久化的 clean 节点。
- 与 bcachefs `struct btree_node.accessed` 和 `struct btree.rewrite_needed` 对齐。

### trigger_extent — Alloc triggers 的 idempotent entry (2026-06-27)

**问题**: 原 alloc trigger 路径直接用 `dirty_sectors += sectors`，不支持批量 key 更新的 idempotent 重入（多次触发导致重复计数）。

**方案**: `trigger_extent()` 基于 old/new bkey 的 sector 计数推导：

```rust
fn trigger_extent(
    trans: &mut Transaction,
    old: Option<&AllocBkey>,
    new: Option<&AllocBkey>,
) -> BucketDiff {
    // 推导 old/new 的扇区变化
    let old_dirty = old.map_or(0, |b| b.dirty_sectors());
    let new_dirty = new.map_or(0, |b| b.dirty_sectors());
    let diff = new_dirty as i64 - old_dirty as i64;
    BucketDiff { dirty_sectors: diff, ... }
}
```

**关键约束**:
- `trigger_extent` 被 Transactional/Atomic/Gc 三个 phase 各调用一次
- 每个 phase 传递相同的 old/new 对 → 三阶段乘积必须正确（相加为零或符合预期）
- **非幂等补偿**不能出现在单 phase 内；如果 Phase1 做了 `+= diff`，Phase2/3 不能重复做
- bcachefs 使用 `btree_key_cache` 避免重读，subvol 使用 `old_key` 缓存

### try_decrease 写点淘汰机制 (2026-06-26)

**问题**: 分配失败（`AddressSpaceExhausted`）时，写点拴住的空间（stranded space）可能超过空闲空间的 1/8，需要淘汰写点释放桶。

**方案**: `WritePointPool::try_decrease()` 在分配失败 retry 循环中被调用。

**关键决策**:
- **factor = 8**: `stranded * 8 > free_sectors` 判定写点过多（与 bcachefs `try_decrease_writepoints(c, 8)` 一致）
- **最小保护**: `nr_active <= 1` 时不执行淘汰（保留至少一个池写点）
- **释放路径**: 有剩余扇区的桶 → `add_to_partial()`（可复用），已满桶 → `put()`（回 freelist）
- **retry 一次**: 只尝试一次 `try_decrease` + 重试（防无限循环），写入点后重走 Step 2 (try_reuse) + Step 3 (alloc_new_fs)
- **统计口径**: `stranded_space()` / `too_many_writepoints()` 只统计活跃池写点，不把专用写点（btree/journal/GC）计入 stranded space；这与 bcachefs `too_many_writepoints(c, factor)` 使用的 `write_points_nr` 一致
- **open bucket 复用**: `bch2_alloc_sectors_start_trans()` 必须无条件尝试 `bucket_alloc_set_partial()` / open bucket 复用，不应被额外的调用方标志跳过；这与 bcachefs 的 `bucket_alloc_set_partial()` 无条件路径一致

**签名**:
```rust
pub fn try_decrease(
    &mut self,
    bucket_size: u64,     // 扇区单位
    free_sectors: u64,    // 当前空闲扇区数
    open_buckets: &BchOpenBuckets,
) -> bool // true = 成功减少一个写点，调用者应重试分配
```

**注意事项**:
- `try_decrease` 只释放最后一个活跃写点（`nr_active - 1`），不是 LRU 淘汰
- 对应的 `too_many_writepoints()` 是私有方法，由 `try_decrease` 内部调用
- `stranded_space()` 只统计池化活跃写点；专用写点始终存在，但不参与 `too_many_writepoints()` 触发条件

### BtreeInteriorUpdate 生命周期 (2026-06-27)

**问题**: `btree_update.rs` 的 `BtreeInteriorUpdate` 状态机仅含 `Init → NodesAllocated → UpdateParent → Done` 四个同步状态，bcachefs 的 `struct btree_update` 使用 `pending → done → free` 异步状态机（含 disk_reservation、异步写完成回调、write_blocked_list）。

**方案**: 当前 subvolmount 采用同步 interior update 设计：

```rust
pub enum InteriorUpdateState {
    Init,
    NodesAllocated,
    UpdateParent,
    Done,
}
```

**决策理由**:
- subvolmount 当前为同步 interior update 设计，无异步 I/O closure 回调
- 同步路径避免了异步状态管理的复杂性（closure 回调、内存序保证）
- split_root() 直接完成所有节点操作，无需等待 I/O 完成

**差异对比**:
| 维度 | bcachefs `struct btree_update` | subvolmount `BtreeInteriorUpdate` |
|------|-------------------------------|-------------------------------|
| 状态机 | pending → done → free (5 态) | Init → NodesAllocated → UpdateParent → Done (4 态) |
| 磁盘预留 | `disks_res` 显式管理 | 未实现（调用方保证空间） |
| 异步回调 | `closure` 完成时触发 | 无（同步完成） |
| write_blocked | 链表管理等待写入的节点 | 未实现（pending） |
| 并发保护 | 多写线程通过 SIX 锁协调 | SIX 锁协调 |

**未来迁移**:
- 如果将来引入多线程写路径，需要实现完整的 `struct btree_update` 状态机：
  - `disk_reservation` 在 split 前预留 bucket 空间
  - `write_blocked` 链表防止父节点在子节点落盘前被写
  - 异步 `closure` 回调在 I/O 完成时推进状态
  - `bch2_btree_update_start/end` 生命周期管理
- 在此之前，同步设计更简单且正确

### journal_res_get_slowpath 三阶段降级 (2026-06-27)

**问题**: `journal_res_get()` 在 fastpath CAS 失败后，原实现用 100 次自旋重试后直接返回 `Overflow` panic。生产环境中 journal 满时应先尝试 cycle → wait → reclaim 三级降级，panic 仅作为最后手段。

**方案**: 三级 fallback：

```rust
Phase 1: cycle → journal_cycle_locked() 关闭当前 entry 打开新 bucket
Phase 2: wait → 自旋 1024 次等待 in_flight 队列清空
Phase 3: reclaim → bch2_journal_flush_pins() + update_last_seq + advance_dirty_idx
Fallback: 三级都失败 → Err(JournalError::Overflow)
```

**关键点**:
- 每级成功后立即重试 fastpath（`journal_res_get_fast`），避免不必要的降级
- `slowpath_lock` Mutex 保证同一时间只有一个线程执行降级
- `journal_res_get()` 公开入口：先尝试无锁 fastpath，失败后获取 slowpath 锁进入降级

### bch2_journal_flush_pins — Pin 回调链 (2026-06-27)

**问题**: btree 节点写入后 journal entry 被 pin 住无法回收，缺乏 flush 机制释放已完成的 pin。

**方案**: `PinEntry` 携带 `flush_callbacks: Vec<Box<dyn Fn() + Send>>`，`bch2_journal_flush_pins(target_seq)` 遍历 pin_fifo 触发 seq ≤ target 的 flush 回调：

```rust
pub fn bch2_journal_flush_pins(&self, target_seq: u64) -> Result<bool, StorageError> {
    // 收集 seq ≤ target 且 count==0 的前端条目
    // 先触发所有回调（持有锁），再从前端弹出
    // callback 返回 Err 时立即停止并传播错误
}
```

**回调锚点**: btree/journal pin 集成中，`node_write()` 写入节点后注册空回调（`Box::new(|| {})`）作为 pin 生命周期管理的锚点。在 subvolmount 同步写模型中，节点在 pin_add 前已完成写入，回调不做额外 I/O。

### 1变2 快照创建语义 (2026-06-27)

**问题**: bcachefs 快照创建时源子卷的快照指针指向旧节点，需要同时创建两个快照节点（一个给新子卷，一个替换源子卷的快照指针），原实现只创建一个。

**方案**: `bch2_snapshot_node_create(engine, parent_id, subvol, extra_child_subvol)` 通过 `Option<u32>` 控制：

- `None` → 单子节点（向后兼容）
- `Some(src_subvol)` → 双子节点（1变2）：
  1. 分配两个 snapshot ID（id 和 id-1）
  2. src_subvol.snapshot → child1, new_subvol.snapshot → child2
  3. 父节点：subvol=0, flags.clear(SUBVOL), children=[child1, child2]
  4. 使用一次原子事务写入三个条目

### BtreeKey bversion 向后兼容字段 (2026-06-27)

**问题**: bcachefs `struct bkey` 包含 `__u64 version` 字段用于 MVCC 版本追踪，subvolmount `BtreeKey` 缺少此字段。

**方案**: 在 `BtreeKey` 结构体末尾添加 `pub version: u64` 字段，使用 `#[serde(default)]` 确保旧序列化数据兼容：

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(C, packed)]
pub struct BtreeKey {
    pub vaddr: u64,
    pub snapshot_id: u32,
    pub key_type: KeyType,
    #[serde(default)]
    pub version: u64,  // MVCC 版本号，不参与排序
}
```

**不参与比较**: `version` 不参与 `PartialEq`/`Ord`/排序——bcachefs 中 bkey 的排序仅基于 (inode, offset, snapshot)，version 用于写冲突检测。

### Key Cache Write-back — Slot 复用 + Dirty 追踪 + Journal Pin + Two-Phase Flush (2026-06-28)

**问题**: 原 `BtreeKeyCache` 中的 slot 是"一次写入永不释放"模式 — `invalidate()` 从 hash 表移除 entry 后 slot 不再可复用。同时，key cache 缺乏 dirty 追踪和 journal 集成，无法在同步点将脏数据写回 btree。

**方案**: 4 个 Phase 实现 bcachefs 对齐的 key cache write-back 语义。

#### Phase 1: Slot 复用

```rust
pub struct CachedEntry {
    pub valid: AtomicBool,   // ← 新增: false 表示 slot 已释放但 entry 仍占位
    pub key: BtreeKey,
    pub value: RwLock<Option<Vec<u8>>>,
    pub lock: SixLock,       // SixLock 保护
    pub dirty: AtomicBool,   // Phase 2
    pub journal_seq: AtomicU64,
    pub flush_pending: AtomicBool,
}
```

**语义**:
- `valid = AtomicBool::new(true)` — 创建时有效
- `invalidate()` / `bch2_btree_key_cache_drop()`: `valid.store(false, Release)` — 保留 slot 但标记无效
- `find()`: 检查 `valid.load(Acquire)`，false → 返回 None（即使 slot 存在）
- `drop(self)`: `valid.store(false, Release)` — Arc 降零时释放关联状态
- 下次 `insert()` 发现 hash 表已有 invalid entry → 复用 slot（`valid.store(true, Release)`）
- 与 bcachefs `struct bkey_cached.valid` 对齐

#### Phase 2: Dirty 追踪

```rust
pub struct KeyCache {
    // ... 已有字段
    pub nr_dirty: AtomicU64,     // ← 新增: 当前脏 entry 数
}

pub struct CachedEntry {
    pub dirty: AtomicBool,       // true = 有未写回 btree 的修改
    pub journal_seq: AtomicU64,  // 最后修改的 journal seq
    pub flush_pending: AtomicBool, // journal pin callback 已触发，等待 flush
    // ...
}
```

**`bch2_btree_insert_key_cached()` 重写** (对应 bcachefs `bch2_btree_insert_key_cached` `btree_key_cache.c:843-885`):

```rust
pub fn bch2_btree_insert_key_cached(
    &self,
    key: BtreeKey,
    value: Vec<u8>,
) -> Result<(u64, Arc<CachedEntry>), StorageError> {
    // 1. 查找已有 slot
    if let Some(entry) = self.find(&key) {
        // 2. 如果已存在: 覆盖 value, 设 dirty=true, inc nr_dirty
        let mut val = entry.value.write().unwrap();
        *val = Some(value);
        drop(val);
        if !entry.dirty.swap(true, AcqRel) {
            self.nr_dirty.fetch_add(1, AcqRel);
        }
        // 3. 注册 journal pin (Phase 3)
        self.pin_entry(&entry);
        return Ok((k, entry));
    }
    // 4. 不存在: 创建新 slot + dirty=true + insert hash 表
}
```

**`insert()` 覆盖脏 slot**: 如果 hash 表命中一个已有的 dirty entry，必须清除 dirty + 释放 journal pin，再设新的 dirty 标志：

```rust
// insert() 中:
if let Some(entry) = self.find(key) {
    if entry.valid.swap(true, AcqRel) == false {
        // slot 复用
    }
    // 已有效但脏: 清除旧状态再设新值
    if entry.dirty.swap(true, AcqRel) == false {
        self.nr_dirty.fetch_add(1, AcqRel);
    }
    // 释放旧 journal pin (Phase 3)
    self.drop_journal_pin(&entry);
    // 注册新 pin (Phase 3)
    self.pin_entry(&entry);
}
```

**`invalidate()` 清理脏**:
- 如果 entry 是脏的：`dirty.store(false, Release)` + `nr_dirty.fetch_sub(1, AcqRel)` + `drop_journal_pin()`
- `valid` 仍设 false（标记 slot 可复用）
- 与 bcachefs `bch2_btree_key_cache_drop` 的 `clear_bit(KEY_CACHE_DIRTY)` 对齐

**`nr_dirty_keys()` 访问器**:
```rust
pub fn nr_dirty_keys(&self) -> u64 {
    self.nr_dirty.load(Acquire)
}
```

**`bch2_nr_btree_keys_need_flush`** 返回 `max(0, nr_dirty - (1024 + nr_keys / 2))`，`bch2_btree_key_cache_must_wait` / `wait_done` 也按 bcachefs 的 `nr_dirty` + `nr_keys` 阈值公式计算（对应 `btree_key_cache.c:900-910`）。

#### Phase 3: Journal Pin 集成

**KeyCache 新增字段和方法**:

```rust
pub struct KeyCache {
    pub journal: OnceLock<Weak<Journal>>,  // Journal 弱引用
    // ...
}

impl KeyCache {
    pub fn set_journal(&self, journal: &Arc<Journal>) {
        self.journal.set(Arc::downgrade(journal)).ok();
    }

    fn pin_entry(&self, entry: &Arc<CachedEntry>) {
        let Some(j) = self.journal.get().and_then(|w| w.upgrade()) else { return };
        let entry_clone = entry.clone(); // clone Arc 注册入回调
        let barrier = Arc::downgrade(&entry_clone);
        j.pin_add(Box::new(move || {
            if let Some(e) = barrier.upgrade() {
                e.flush_pending.store(true, Release);
            }
        }));
    }

    fn drop_journal_pin(&self, entry: &CachedEntry) {
        let Some(j) = self.journal.get().and_then(|w| w.upgrade()) else { return };
        j.pin_drop();
    }
}
```

**Callback 链**:
- `pin_add(Box<dyn Fn() + Send>)` 注册 flush callback
- `journal_flush_pins(target_seq)` 触发 seq ≤ target 的 callback
- callback 设置 `flush_pending = true` → 同步点检查 → 调用 `flush_cache_dirty_keys()`
- 与 bcachefs `bch2_journal_pin_copy` / `bch2_journal_pin_drop` / `bch2_journal_pin_set` (`journal.h`) 对齐

#### Phase 4: Two-Phase Flush

**两阶段设计解决跨层借用冲突**:

```text
Phase 1 (KeyCache, &self):  collect_dirty()  →  Vec<(BtreeKey, Arc<CachedEntry>)>
Phase 2 (Btree, &mut self):  写回 btree         ← BtreeEngine::insert_entry_skip_cache()
Phase 3 (KeyCache, &self):  mark_clean()      → 清 dirty + drop journal pin
```

**`collect_dirty()`** (Phase 1):
```rust
pub fn collect_dirty(&self) -> Vec<(BtreeKey, Arc<CachedEntry>)> {
    let mut result = Vec::new();
    for (k, entry) in self.entries.read().unwrap().iter() {
        if entry.dirty.load(Acquire) {
            // 清 flush_pending（重入保护）
            entry.flush_pending.store(false, Release);
            result.push((*k, entry.clone()));
        }
    }
    result
}
```

**`mark_clean()`** (Phase 3):
```rust
pub fn mark_clean(&self, keys: &[BtreeKey]) {
    for key in keys {
        if let Some(entry) = self.find(key) {
            if entry.dirty.swap(false, AcqRel) {
                self.nr_dirty.fetch_sub(1, AcqRel);
            }
            self.drop_journal_pin(&entry);
        }
    }
}
```

**`flush_dirty(on_write: impl Fn(...))`** — 收集 + 写回调 + 清理：
```rust
pub fn flush_dirty<F>(&self, mut on_write: F) -> Vec<(BtreeKey, Result<(), StorageError>)>
where F: FnMut(&BtreeKey, &[u8]) -> Result<(), StorageError>
{
    let entries = self.collect_dirty();               // Phase 1
    if entries.is_empty() { return vec![]; }
    
    let keys: Vec<BtreeKey> = entries.iter().map(|(k, _)| *k).collect();
    let results: Vec<(BtreeKey, Result<(), StorageError>)> = entries.iter().map(|(k, entry)| {
        let val = entry.value.read().unwrap();
        let res = val.as_ref().map_or(Ok(()), |v| on_write(k, v));
        (*k, res)
    }).collect();
    
    // Phase 3: 标记成功的为 clean
    let ok_keys: Vec<BtreeKey> = results.iter()
        .filter(|(_, r)| r.is_ok())
        .map(|(k, _)| *k)
        .collect();
    self.mark_clean(&ok_keys);
    
    results
}
```

**`Btree::insert_entry_skip_cache()`** — 写 btree 但不 invalidation cache：

```rust
pub fn insert_entry_skip_cache(
    &mut self,
    key: BtreeKey,
    value: Vec<u8>,
) -> Result<(), StorageError> {
    // 直接插入节点，不走 key cache 路径
    self.insert_entry_into_node(key, &value)
}
```

**`BtreeEngine::flush_cache_dirty_keys()`** — Engine 级别遍历 5 种 btree 同步 flush：

```rust
pub fn flush_cache_dirty_keys(&mut self) -> Vec<(BtreeType, Vec<(BtreeKey, Result<(), StorageError>)>)> {
    let mut results = Vec::new();
    for tree in self.trees.iter_mut() {
        let r = tree.key_cache.flush_dirty(|key, val| {
            tree.insert_entry_skip_cache(*key, val.to_vec())
        });
        if !r.is_empty() {
            results.push((tree.ty(), r));
        }
    }
    results
}
```

**同步点接线**（以下位置会在写入前调用 `flush_cache_dirty_keys()`，新写入口需同步维护）:
- `insert_entry_raw()` 调用前
- `flush_dirty_nodes()` 调用前
- `bch2_trans_commit()` 调用前
- bcachefs 中对应的 `bch2_btree_key_cache_flush()` 在 `btree_key_cache.c:708-740`

**行为约束**:
- `flush_dirty()` 是同步写，不是异步后台线程
- 写失败的条目保持 dirty 状态（不调用 mark_clean）
- collect_dirty 使用 &self（仅读 hash 表），flush 阶段在 Engine 层完成 &mut self 操作
- 与 bcachefs `bch2_btree_key_cache_flush` 和 `bch2_btree_key_cache_journal_flush` 语义对齐

**对应 bcachefs 源码**:

| 概念 | bcachefs 文件:行号 |
|------|-------------------|
| `struct bkey_cached.valid` | `btree_key_cache.c` |
| `struct bkey_cached.dirty` | `btree_key_cache.c:75` |
| `KEY_CACHE_DIRTY` | `btree_key_cache.c` |
| `bch2_btree_insert_key_cached` | `btree_key_cache.c:843-885` |
| `bch2_nr_btree_keys_need_flush` | `btree_key_cache.c:900-910` |
| `bch2_btree_key_cache_flush` | `btree_key_cache.c:708-740` |
| `bch2_journal_pin_copy/drop/set` | `journal.h` |
| `bch2_btree_key_cache_journal_flush` | `btree_key_cache.c` |

### JsetEntryHeader 序列化策略 (2026-07-08)

**问题**: `JsetEntryHeader` 字段布局与 bcachefs `jset_entry` 不一致（字段顺序、单位不同），无法直接映射内存布局。

**方案**: 保持 bincode 按字段声明序序列化，`#[repr(C)]` 仅用于对齐断言（`test_jset_entry_header_size`），不影响 on-disk 格式。关键约束：

| 字段 | bcachefs jset_entry | subvol bincode 序 | 对齐 |
|------|--------------------|--------------------|------|
| `payload_len` (u64s vs bytes) | 偏移 0, LE16 | 偏移 4-5, LE16 | ⚠️ 位置 + 单位不同 |
| `btree_type` | 偏移 2 | 偏移 0 | ❌ 位置不同 |
| `level` (was `flags`) | 偏移 3 | 偏移 3 | ✅ 位置对齐 |
| `entry_type` | 偏移 4 | 偏移 1 | ❌ 位置不同 |
| `version` | 无 | 偏移 2 | subvol 专有 |

**理由**:
- 保持 bincode 降低复杂度（单行序列化 vs 手动 LE 编码）
- on-disk 格式自洽，subvol 无需与 bcachefs 字节兼容
- 逻辑语义和 API 命名对齐比内存布局对齐优先级更高
- 若未来需要格式互读，可单独加转换层

**B1-commit flush 规则**: write buffer btree 的 key 在 `bch2_trans_commit` Phase 5 中只插入 wb inc 列表，**不**同步 flush journal。Journal flush 由 write buffer flush 路径（`bch2_btree_write_buffer_flush`）自然触发。对应 bcachefs 异步 flush 模型。

**B3-quiesce 规则**: `bch2_journal_quiesced` 使用 `flushed_seq_marker`（IO 提交时推进），而非 `seq_ondisk`（IO 完成后推进），对齐 bcachefs `journal_quiesced()` (journal.c:692-701)。

## Verification Status — Batch A (2026-06-27)

### lock/six.rs — bcachefs C 一致性验证（已修复）

以下 4 项已在 Batch A 中修复并通过验证：

| # | 修复项 | C 引用 | 修复内容 | 验证结论 |
|---|--------|--------|----------|----------|
| 1 | WAITING_WRITE_BIT 位位置 | `SIX_LOCK_WAITING_write=1U<<30` | bit 29→30 (0x2000_0000→0x4000_0000) | ✅ 49/49 six 测试通过 |
| 2 | try_lock_intent CAS 模式 | `atomic_try_cmpxchg_acquire` loop | 单次 compare_exchange→compare_exchange_weak 循环 | ✅ 无死锁/回退 |
| 3 | downgrade_write notify | `six_lock_downgrade` 隐式等待者检查 | 增加 self.notify_waiters() 调用 | ✅ 条件隐含在 notify_waiters 内部 |
| 4 | handoff 对齐验证 | `six.c __six_lock_wakeup` | 增加文档对比 C 语义 | ✅ handoff 实现已存在，文档确认对齐 |

### btree 类型系统 — 新增基础设施（已验证）

| # | 新增项 | C 引用 | 说明 | 验证结论 |
|---|--------|--------|------|----------|
| 1 | BtreeNodeType 枚举 | `enum btree_node_type` | 映射 __btree_node_type(level, btree_id) | ✅ 正确 |
| 2 | KEY_TYPE_BTREE_PTR_V3=19 | `KEY_TYPE_btree_ptr_v3=19` | 从 key.rs 导出 | ✅ 正确 |
| 3 | BTREE_ITER_BUF_GRANULARITY=2048 | `bkey_buf.h kmalloc(2048)` | peek_upto buffer 粒度 | ✅ 正确 |
| 4 | Watermark PartialOrd | BCH_WATERMARK_reclaim 比较 | #[derive(PartialOrd)] repr(u8) | ✅ 正确 |

### btree 内部操作 — 历史 TODO / 已闭环项

以下 5 项在 Batch A 中标记为 TODO，因需要跨子系统基础设施支持：

| # | TODO | 文件 | blocker |
|---|------|------|---------|
| 1 | commit WAL 持久性窗口 | transaction.rs | log_operation 先 journal 后 btree 需事务回滚能力 |
| 2 | pre_split journal 预留 | btree.rs | 需要 journal_res_get 集成 |
| 3 | mark_done drop_children | update.rs | 需要 drop_children 函数实现 |
| 4 | journal_seq_verify | update.rs | 需要跨子系统 journal_seq API |
| 5 | gc_gens journal_seq 追踪 | gc.rs | 需要 gc_pos 结构体修改 + 签名变更 |

**验证状态**: 这些条目保留为历史背景；已实现项会在后续验证记录中标记为 ✅，未实现项仍保留明确 blocker 解释，非"slop"。

### 测试覆盖验证

- **lock/six**: 49 tests → 49 ✅ (新增覆盖率：无新增测试，C bit 对齐的回归验证)
- **btree 模块**: 331 tests → 331 ✅ (GC 17/17, key 6/6, node 155/155, io 7/7, iter 5/5, trans 6/6, mod 12/12, writepoint 9/9)
- **全量**: 693 passed, 5 known fail (预存 AddressSpaceExhausted), 6 ignored
- **clippy/fmt**: 0 新增 warning/diff

## Verification Status — Batch B (2026-06-27)

### alloc 模块 — bcachefs C 一致性验证（12 项修复）

2026-06-27 通过 4 个并行子代理实施并在 main-session 验证：

| # | 修复项 | C 引用 | 修复内容 | 验证结论 |
|---|--------|--------|----------|----------|
| P0-1 | BchAllocEntry 字段对齐 | `bch_alloc_v4` | 字段命名和 bitfield 布局对齐 | ✅ |
| P0-2 | reserved_buckets 耗尽策略 | `BCH_ALLOC_RESERVE_*` | `AddressSpaceExhausted` → `AllocError::ReserveExhausted` + alloc_hint 优先 | ✅ |
| P0-3 | derive_data_type 优先级 | `alloc_data_type` | USER>META>PARITY>RESERVED 严格顺序 | ✅ |
| P0-4 | BchAllocEntry journal_seq | journal entry 兼容 | format 写入路径修复 | ✅ |
| P1-5 | BchAllocBucket 状态枚举 | `bucket_state` | `need_discard` / `free_discarded` / `free_available` / `need_gc_gens` / `sb_only` | ✅ |
| P1-6 | bucket_gens 更新策略 | `bch2_bucket_gens` | set-version → lazy dirty + checkpoint 批处理 | ✅ |
| P1-7 | alloc_group 分配亲和性 | `alloc_prio_hint`/`target` 复合 | `foreground::AllocTarget` + `resolve_alloc_group` | ✅ |
| P1-8 | alloc_key_v2 单 entry 路径 | `bch2_alloc_key_v2` | 新增单一 entry 写入路径 | ✅ |
| P1-9 | gc_gens 回收范围 | BITMAP_SIZE | 完整范围覆盖 | ✅ |
| P2-10 | bucket_mark checkpoint 初始化 | `bch2_alloc_read` | 0 号桶初始化补全 | ✅ |
| P2-11 | 最大尝试次数步进回退 | `BCH_ALLOC_ATTEMPTS` | 3→步进降级水位线 | ✅ |
| P2-12 | prio_hint 映射 | `alloc_hint_type` | UNSPECIFIED→USER/SYSTEM/META 映射 | ✅ |

### journal 模块 — bcachefs C 一致性验证（8 项修复）

| # | 修复项 | C 引用 | 修复内容 | 验证结论 |
|---|--------|--------|----------|----------|
| P0-1 | Jset magic/version/csum | `JSET_MAGIC` | `VMNT_JSET_MAGIC` + `JSET_VERSION` + `CSUM_TYPE_*` | ✅ |
| P0-2 | JsetEntry has_last/has_prev | `jset_entry` byte flags | 新增 `#[serde(default)]` byte 字段 | ✅ |
| P1-3 | Pin 预分配 | `JOURNAL_PIN_LIST_SIZE` | `MAX_PIN_ENTRIES=128` 固定预分配 | ✅ |
| P1-4 | replay 特殊 entry | `JOURNAL_ENTRY_TYPE_OVERWRITE` / `BTREE_NODE_REWRITE` | 新增处理路径 | ✅ |
| P1-5 | preres noflush 状态机 | `journal_buf_state_noflush` | `BufState::Noflush` 枚举变体 | ✅ |
| P2-6 | commit callback 机制 | `journal_commit` closure | `write_done_callbacks` Vec + wake_up | ✅ |
| P2-7 | flush 定时器 + 标志 | `JOURNAL_NEEDS_FLUSH_WRITE` | `JOURNAL_NEEDS_FLUSH_WRITE` 常量 | ✅ |
| P2-8 | CRC 分片算法 | crc32c 分片 | 对齐 bcachefs crc32c 分片方式 | ✅ |

### snap 模块 — bcachefs C 一致性验证（7 项修复）

| # | 修复项 | C 引用 | 修复内容 | 验证结论 |
|---|--------|--------|----------|----------|
| P0-1 | BchSnapshotFlags 位布局 | `BCH_SNAPSHOT_SUBVOL=1<<4` | 位从 `1<<4` 开始，前 4 位为 leaf 保留位 | ✅ |
| P0-2 | skip_list 指数步进 | `bch2_snapshot_skiplist_good` | 等距→指数 `1<<i` 步进 | ✅ |
| P1-3 | is_ancestor subvol 间接路径 | `bch2_snapshot_is_ancestor` | `bch2_snapshot_is_ancestor_subvol` | ✅ |
| P1-4 | master_subvol 级联管理 | `bch2_snapshot_tree_master_subvol` | 新增函数 | ✅ |
| P2-5 | skiplist 递归回退重试 | `bch2_snapshot_skiplist_good` | 健壮性检查 + 递归回退 | ✅ |
| P2-6 | snapshot_id bitmap 分配 | bitmap + 回收 | `SnapshotIdBitmap` 新增 | ✅ |
| P2-7 | snapshot_tree 子树注册 | subtree registry | `SubtreeRegistry` + `write_snapshot_tree_value` | ✅ |

### subvol 模块 — bcachefs C 一致性验证（5 项修复）

| # | 修复项 | C 引用 | 修复内容 | 验证结论 |
|---|--------|--------|----------|----------|
| P0-1 | BCACHEFS_ROOT_INO 判据 | `BCACHEFS_ROOT_INO` | 新增常量用于根节点操作判别 | ✅ |
| P1-2 | root snapshot 创建 | `bch2_snapshot_root` | `bch2_snapshot_node_create` 在 subvol_create 中调用 | ✅ |
| P1-3 | subvol_ino_map 清理 | `bch2_subvolume_ino_map` | `register_ino_map` 清理路径 | ✅ |
| P2-4 | 1变2 原子事务写入 | `commit_do` | 事务包含父子卷更新 + 新子卷创建 | ✅ |
| P2-5 | bch2_subvolume_trigger | `bch2_subvolume_trigger` | 新增 snapshot tree 验证路径 | ✅ |

### 兼容层清理

| 模块 | 操作 | 状态 |
|------|------|------|
| snap/mod.rs | 移除 `create_snapshot_btree` / `delete_snapshot_btree` export | ✅ |
| subvol/mod.rs | 移除旧名 export，增加 `bch2_subvolume_trigger` / `BCACHEFS_ROOT_INO` export | ✅ |
| volume/mod.rs | `create_snapshot_btree`→`bch2_snapshot_node_create` / `delete_snapshot_btree`→`bch2_snapshot_node_set_deleted` | ✅ |

当某个公开函数只做参数原样转发、且没有独立的 bcachefs 语义时，优先删除 wrapper 并让调用方直接调用真实实现。
如果仍需要保留过渡别名，必须至少有一个外部调用方或兼容窗口；否则属于冗余 API。

### 测试覆盖验证

- **alloc**: 42 tests → 42 ✅
- **journal**: 28 tests → 28 ✅
- **snap**: 16 tests → 16 ✅（含 skip_list 指数步进测试）
- **subvol**: 13 tests → 13 ✅
- **全量**: 710 passed（较 Batch A +17），5 known fail（预存 AddressSpaceExhausted），6 ignored
- **clippy/fmt**: 0 新增 warning/diff（fmt clean，clippy 仅预先存在的 dead_code/unused）

**验证结论**: PASS_WITH_NOTES
- Minor: `subvol/ops.rs:269` 注释仍含旧名 `delete_snapshot_btree` — 已修复为 `bch2_snapshot_node_set_deleted`
- Minor: `snap/mod.rs` 仍导出 `is_ancestor_from_btree`（subvolmount 扩展函数，非 bcachefs compat 名）

## Verification Status — Batch C (2026-06-27)

### recovery 模块 — bcachefs C 一致性验证（10 项修复）

2026-06-27 通过 4 个并行子代理实施并在 main-session 验证：

| # | 修复项 | C 引用 | 修复内容 | 验证结论 |
|---|--------|--------|----------|----------|
| P0-1 | SnapshotsRead pass | `PASS_ALWAYS #3` | 新增 `BchRecoveryPass::SnapshotsRead=6` + stub 实现 | ✅ 顺序正确 |
| P0-2 | TransMarkDevSbs pass | `PASS_ALWAYS #6` | 新增 `BchRecoveryPass::TransMarkDevSbs=7` + stub 实现 | ✅ 顺序正确 |
| P0-3 | FsJournalAlloc pass | `PASS_ALWAYS #7` | 新增 `BchRecoveryPass::FsJournalAlloc=8` + stub 实现 | ✅ 顺序正确 |
| P0-4 | AccountingRead pass | `PASS_ALWAYS #39` | 新增 `BchRecoveryPass::AccountingRead=9` + stub 实现 | ✅ deps 修复 BIT_ULL[1]→BIT_ULL[5] |
| P0-5 | PresplitShardBoundaries pass | `PASS_ALWAYS #48` | 新增 `BchRecoveryPass::PresplitShardBoundaries=10` + stub 实现 | ✅ 注释修复 |
| P0-6 | LookupRootInode pass | `PASS_ALWAYS #42` | 新增 `BchRecoveryPass::LookupRootInode=11` + stub 实现 | ✅ 最后 pass |
| P1-7 | alloc_read stub | `bch2_alloc_read` | 已挂接到 `passes::alloc_read::run` | ✅ stub 安全 |
| P1-8 | check_topology 增强 | `bch2_check_topology` | 递归 parent-child、child 边界和缺失 child 验证已实现并有回归测试 | ✅ |
| P1-9 | deps 强制执行 | `passes.c` `depends` | 调度器中增加依赖检查：pass 运行前检查所有 deps 位是否已 complete | ✅ 新增强制执行 |
| P1-10 | PASS_UNCLEAN/FSCK/ONLINE/NODEFER flags | `passes_format.h` | `RecoveryPassFlags` 新增 4 个标志常量 | ✅ 对齐 C |

### volume 模块 — bcachefs C 一致性验证（3 项修复）

| # | 修复项 | C 引用 | 修复内容 | 验证结论 |
|---|--------|--------|----------|----------|
| P1-1 | recovery 状态追踪字段 | `bch_fs_recovery` | `recovery_pass_done` / `recovery_passes_complete` / `passes_failing` | ✅ |
| P1-2 | RwWithPendingRecovery 子状态 | `enum bch_fs_state` | `VolumeState::RwWithPendingRecovery=6` | ✅ |
| P2-3 | error_count AtomicU64 | `bch_fs` `fsck_error` | `Volume` 新增 `error_count: AtomicU64` | ✅ |

### storage 模块 — bcachefs C 一致性验证（4 项修复）

| # | 修复项 | C 引用 | 修复内容 | 验证结论 |
|---|--------|--------|----------|----------|
| P1-1 | 备份 superblock 布局 | `BCH_SB_LAYOUT_*` | `BackupSbLayout` 结构 + 多副本写入（BlockAddr 0/4/8） | ✅ |
| P1-2 | 写所有副本 | superblock 写入路径 | `write_to_backend` 遍历所有副本写入 | ✅ |
| P2-3 | UUID 字段 | `sb.uuid` / `sb.user_uuid` | `BchSb` 新增 `uuid: [u8; 16]` + `user_uuid: [u8; 16]` | ✅ serde(default) 兼容 |
| P2-4 | features/compat 标志 | `sb.features[2]` / `sb.compat[2]` | `BchSb` 新增 `features: [u64; 2]` + `compat: [u64; 2]` | ✅ serde(default) 兼容 |

### block_device 模块 — bcachefs C 一致性验证（3 项修复）

| # | 修复项 | C 引用 | 修复内容 | 验证结论 |
|---|--------|--------|----------|----------|
| P1-1 | checksum 读写方法 | `bch2_crc32c` | `block_crc32c` 函数 + `read_block_with_csum` / `write_block_with_csum` | ✅ |
| P1-2 | write_extent checksum 集成 | write_extent 路径 | `Volume::write_extent` 中调用 `write_block_with_csum` | ✅ |
| P2-3 | MockBlockDevice 零填充 | 对齐 FileBlockDevice | 未写入块返回零填充而非 `BlockNotFound` | ✅ |
### 测试覆盖验证

- **recovery**: 17 tests → 17 ✅（新增 6 个 stub pass 后不会 panic `unreachable!()`）
- **volume**: 17 tests → 17 ✅
- **storage::superblock**: 5 tests → 5 ✅
- **block_device**: 42 tests → 42 ✅
- **全量**: 716 passed（较 Batch B +6），5 known fail（预存 AddressSpaceExhausted），6 ignored
- **clippy/fmt**: 0 新增 warning/diff（fmt clean，clippy 仅预先存在的 pre-Batch-C warnings）

### 自修复项

trellis-check 审计中发现并修复了以下问题：

1. **`recovery/mod.rs`** — `accounting_read` 的 `deps` 字段错误引用了 `RECOVERY_PASS_BITS[1]`（BtreeRoots）。C 中 `accounting_read`（稳定 ID 39）的 deps 是 `BIT_ULL(BCH_RECOVERY_PASS_check_topology)`。等价于 subvol 的 `RECOVERY_PASS_BITS[5]`（GcScan，包含拓扑检查）。已修复。

2. **`recovery/passes/snapshots_read.rs:7`** — 注释错误"遍历 Alloc btree 的 snapshots 条目"→ 已修正为"遍历 Snapshots btree 的快照条目"。

3. **`recovery/passes/presplit_shard_boundaries.rs:5`** — 注释与 `deps` 矛盾（说需要 snapshots_read，但 deps 指向 JournalReplay）。已修正为"需要 journal_replay 已完成"对齐 C。

4. **`recovery/mod.rs`** — `depends` 位掩码从未在调度器中执行。PRD #9 要求强制执行。已添加 deps 检查逻辑。

5. **`snap/snapshot.rs`** — `bch2_fix_child_of_deleted_snapshot()` 必须保留 `delete.c:611-662` 的槽位更新顺序：depth 先减去已删祖先数；depth 为 0 时清零全部 `skip[]`；否则只替换值命中 deleted 列表的槽位。替换值从当前 parent 出发，先跳过连续 deleted 祖先，再随机上溯 `0..depth-1` 层且每层继续跳过 deleted 祖先，最后排序。禁止根据 effective parent 整体重建 `skip[]`，因为这会改变未命中槽位并可能从祖先的旧 skip 重新引入已删节点。`test_fix_child_of_deleted_skip_replacement` 必须断言修复后没有槽位仍指向 deleted ID。

6. **`storage/block_io.rs` / `storage/service.rs`** — `BchAllocator::new()` 在很小的 AG 上会被 `Watermark::Normal` 的固定预留卡住，导致 checkpoint / block I/O 测试在无关的地址空间耗尽上失败。测试夹具应使用足够大的单 AG，避免把 allocator 预留策略误判成业务回归。

7. **`recovery/mod.rs`** — `BchRecoveryPassStable::CheckDiretns` 是明显的拼写漂移，已统一为 `CheckDirents`。恢复 pass 的稳定 ID / 枚举命名不能出现这类拼写误差，否则会污染覆盖地图、实现注释和后续审查。

8. **`recovery/mod.rs`** — `restore_progress()` 不能把已持久化的 `superblock.pass_done` 回写成 runtime 顺序派生的更小 stable ID；bcachefs 的 stable pass 编号不是按运行时顺序单调增长的。若恢复调度器临时注入兜底 pass，也要把这些 pass 计入完成掩码，否则可能在部分成功时提前结束恢复。

9. **`recovery/mod.rs`** — `check_snapshots` 的 flags 需要保留 bcachefs 的 `PASS_ALWAYS|PASS_ONLINE|PASS_FSCK|PASS_NODEFER` 组合。`passes_online` 是从 flags 派生的可观测状态，不是装饰性字段；如果 flags 缺了 `PASS_ONLINE`，后续在线调度和状态展示都会失真。

10. **`btree/gc.rs`** — 递归拓扑检查必须先走真实 child 引用，再做平面遍历。`Btree::for_each_entry()` 依赖 `BtreeIter::init()`，而后者会通过 `get_or_create()` 补出缺失的 child 节点；如果先跑平面遍历，`missing child` 类损坏会被掩盖。child 存在性检查必须使用 `NodeCache::get()`，不要在校验前触发自动创建。

### 已知差距（非本次范围）

- 6 个新 recovery passes 均为 stub 实现（`let _ = &state.field; Ok(())`），需要对应 btree/allocator 基础设施就绪后才能启用实质逻辑
- `PASS_ALLOC` 标志在 subvol 的 `RecoveryPassFlags` 中不存在（C 中 `trans_mark_dev_sbs` 和 `fs_journal_alloc` 均含 `PASS_ALLOC`，当前阶段无影响）
- `check_topology` 的递归 parent-child 链接验证已实现并由回归测试覆盖；这里只保留对实现约束的说明，不再把它当作未完成 TODO

**验证结论**: PASS_WITH_NOTES
- Note: 6 个新 recovery passes 为 stub 实现，且 `alloc_read` pass 仍为 stub（PRD 要求真实现但缺少 bucket_gens btree 基础设施）
- Note: 读取路径未集成 checksum 验证（PRD 仅要求 write_extent 路径，已满足）

## Verification Status — Batch D (2026-06-28)

### Key Cache Write-back — 4 个 Phase 实现

| Phase | 变更 | 文件 | 验证结论 |
|-------|------|------|----------|
| P1 | CachedEntry slot 复用 — valid AtomicBool + find() 检查 | key_cache.rs | ✅ valid.store/load Acquire/Release 正确 |
| P1 | invalidate() 设 valid=false 不移除 hash 表 | key_cache.rs | ✅ hash 保留 slot，复用通过 insert 路径验证 |
| P2 | Dirty tracking — dirty/jounral_seq/flush_pending AtomicBool | key_cache.rs | ✅ 三字段齐全，nr_dirty 计数正确 |
| P2 | bch2_btree_insert_key_cached 重写 | key_cache.rs | ✅ cache+dirty+pin 三步完整 |
| P3 | Journal pin 集成 — pin_entry/drop_journal_pin | key_cache.rs | ✅ Weak<CachedEntry> callback 设 flush_pending |
| P3 | bch2_nr_btree_keys_need_flush / _must_wait / _wait_done 真实实现 | key_cache.rs | ✅ 基于 nr_dirty + nr_keys 阈值公式 |
| P4 | collect_dirty + mark_clean + flush_dirty 两阶段 flush | key_cache.rs | ✅ 三阶段避免锁嵌套 |
| P4 | BtreeEngine::flush_cache_dirty_keys | btree/mod.rs | ✅ 遍历 5 tree collect+write+clean |
| P4 | insert_entry_skip_cache | btree/btree.rs | ✅ 写 btree 不 invalidation |
| - | six.rs notify_waiters 公平唤醒 | lock/six.rs | ✅ min_wi_trans_id 对齐 time_before64 |
| - | six.rs try_relock_* 别名移除 | lock/six.rs | ✅ 所有引用已更新 |
| - | Crc32CHasher 真正 CRC32C | journal/jset.rs | ✅ crc32fast→crc::CRC_32_ISCSI |
| - | btree/node.rs CRC32C 对齐 | node.rs | ✅ Crc32CHasher::hash 替换 crc32fast::hash |
| - | Watermark reserved_buckets bcachefs 对齐 | types.rs | ✅ nb/64 + btree_reserve 新策略 |

### 测试覆盖验证

- **key_cache**: 23 tests → 23 ✅（含 slot_reuse, dirty_tracking, journal_pin, flush_dirty, concurrent）
- **全量**: 714 passed（较 Batch C -2 因 Watermark 预留变少），5 known fail（预存 AddressSpaceExhausted），6 ignored
- **clippy/fmt**: 0 新增 warning/diff（仅预先存在的 dead_code/private_interfaces）

### 已知差距（非本次范围）

- 同步点接线已完成（`insert_entry_raw`/`flush_dirty_nodes`/`bch2_trans_commit` 路径调用 `flush_cache_dirty_keys()`），后续新增写入口时需同步补齐
- `bch2_btree_key_cache_journal_flush` 已由 journal reclaim 触发，不再是 stub
- `trigger_key_cache_miss()` — 事务重启机制待事务系统就绪后接入

**验证结论**: PASS_WITH_NOTES
- Note: 同步点已接线，核心 write-back 原语（Phase 1-4）已实现并测试通过
- Note: `collect_dirty` 在持 hash 锁时获取 per-entry 读锁（与 `find()` 不同），经分析无死锁风险（无 per-entry→hash 反向路径）

## Forbidden Patterns

<!-- Patterns that should never be used and why -->

(To be filled by the team)

---

## Required Patterns

### bcachefs API 命名对齐

所有与 bcachefs C 源码对应的函数必须使用 `bch2_` 前缀 + 子系统名：

```rust
// ✅ 正确：对齐 bcachefs 命名
pub fn bch2_btree_node_write(b: &BtreeNode, ...) { }
pub fn bch2_journal_flush(j: &mut Journal, ...) { }
pub fn bch2_subvolume_create(sv: &mut SubvolumeManager, ...) { }

// ❌ 错误：使用自定义命名
pub fn flush_journal(j: &mut Journal, ...) { }
pub fn create_subvolume(sv: &mut SubvolumeManager, ...) { }
```

### 类型字段对齐

结构体字段名和语义必须与 bcachefs 的 `struct` 定义一致：

```rust
// ✅ 正确：Bpos 字段对齐 struct bpos { u64 inode; u64 offset; u32 snapshot; }
pub struct Bpos { pub inode: u64, pub offset: u64, pub snapshot: u32 }

// ❌ 错误：使用自定义字段名
pub struct Bpos { pub vol_id: u64, pub offset: u64, pub snapshot: u32 }
```

### 向后兼容

API 重命名时，通过 `pub use` 保留旧名别名，优先更新内部引用：

```rust
// mod.rs 中提供过渡兼容
pub use snapshot::bch2_snapshot_node_create;
pub(crate) use snapshot::bch2_snapshot_node_create as create_snapshot_btree; // 旧名别名
```

禁止直接删除已被外部依赖的旧 API。先更新所有引用，再移除别名。

### 功能逻辑必须与 bcachefs 完全一致

**原则**: API 命名/类型/签名 100% 对齐 bcachefs，内部实现允许 Rust 惯用写法，但**功能逻辑必须一致**。

```
命名/类型/签名     → 100% bcachefs 一致 (bch2_xxx, Bpos::inode, JournalRes)
内部实现风格       → Rust 惯用写法 (所有权、Option/Result、trait、闭包)
功能逻辑           → 必须与 bcachefs 完全一致（边界条件、错误处理路径、并发语义）
```

**适用边界**:
- bcachefs 的 `static inline` 函数、内部宏展开、平台相关优化不需要逐行复制
- 但外部可见的行为（函数返回值、错误码、并发锁语义、恢复 pass 顺序）必须一致
- btree split/merge 的触发条件、journal reservation 的阈值、alloc watermark 的预留逻辑 — 必须与 bcachefs 一致
- btree split 必须含 compact_fits 检查（compact 后无法容纳新 key 时跳过 compact 直接 split）
- btree split 必须含 format-aware split point（考虑 packed size，防止 split 后某侧仍满）
- btree split 必须含错误回滚机制（split 失败时释放已分配节点）
- btree split / merge 后，更新到 cache 的内部节点应尽快 compact 回 aux 可二分形态，避免 `find_child_node` 退化到增量 set 全量收集/排序
- btree merge 成功后，被吸收的节点必须从 cache 中移除，不得用空 leaf 占位继续保留可见地址
- journal reclaim 必须触发关联 flush_callbacks 再回收 bucket
- journal seq 按 entry 分配（不按 per-reservation），JournalRes 只保留 `seq`
- alloc trigger 修改 Alloc btree 和 Freespace btree 必须保证事务原子性（失败可回滚）

### 子系统功能逻辑约定

以下约定基于 2026-06-26 的 6 子系统功能逻辑审查（对比 bcachefs C 源码），记录每一子系统的关键差异和必须遵守的合约。

#### btree trans/iter — 路径状态一致性

**advance() 必须重新遍历路径**:
- `advance()`/`skip_to_next_leaf()` 不能仅递增 `leaf.offset` — 并发 split/compaction 后 offset 可能指向错误条目
- 每次 advance 后必须 `set_pos()` + `traverse()` 重新查找路径
- `back_up_and_advance()` 访问父节点前必须验证锁 seq

**peek() 必须遵循 bcachefs path/key-cache 顺序**:
- `peek()` 通过 `bch2_btree_path_peek_slot` 和 node iterator 读取当前 path；
  不创建额外的 overlay btree。
- journal replay 通过本地 journal replay 流程直接 materialize 到 btree；
  读路径不增加独立内存覆盖层。

**bch2_trans_relock() 必须验证 seq**:
- `bch2_trans_relock()` 必须检查路径中每层节点的 `locked_seq` 是否仍然有效
- 无效 seq 意味着节点已被并发操作修改，需要 `restart_transaction()`

**共享路径快照必须同步刷新**:
- `BtreeIter` 的局部 `path` 仍然是遍历真源，但用于复用/重启观测的共享路径快照必须在 `init()`、`advance()`、`restart()`、`restart_optimized()` 之后刷新
- 共享快照只用于路径复用语义和测试，不应绕开现有的锁/遍历逻辑

#### btree cache/IO — 脏页管理

**mark_dirty 不能丢弃脏数据**:
```rust
// ❌ 错误：dirty.clear() 丢弃所有脏节点引用 — 数据丢失
if inner.dirty.len() >= MAX_DIRTY { inner.dirty.clear(); flush_all(); }

// ✅ 正确：真正的 flush
if inner.dirty.len() >= MAX_DIRTY { flush_all_dirty(); }
```

**必含 will_make_reachable**:
- COW btree 中，父节点必须先于子节点到达磁盘
- 必须在写入前通过 `will_make_reachable` 确保父节点已落盘
- 缺失此保证 → 崩溃后父节点指向不存在的子节点

#### will_make_reachable 实现模式

**生命周期合约**:

```
① 新节点创建（split/increase_depth/merge）
   → node.set_will_make_reachable()      // 阻止 eviction
   → cache.insert_dirty() / insert()     // 插入 cache

② flush_dirty_nodes()
   → 按 level 升序写入
   → serialize_to_bucket + write_block
   → node.clear_will_make_reachable()    // 已落盘 → 释放 eviction 保护
   → bch2_btree_post_write_cleanup

③ eviction（shrink / evict_one_leaf）
   → if node.will_make_reachable() → skip // 防止首次写入前被驱逐
```

**数据结构**:

```rust
// btree/node.rs — BtreeNode struct 中新增
pub will_make_reachable: AtomicBool,
```

**方法签名**:

```rust
impl BtreeNode {
    pub fn will_make_reachable(&self) -> bool       // load(Acquire)
    pub fn set_will_make_reachable(&self)            // store(true, Release)
    pub fn clear_will_make_reachable(&self)          // store(false, Release)
}
```

**调用点清单**:

| 位置 | 文件 | 操作 |
|------|------|------|
| `split_root()` — 新左右 leaf | btree.rs | `set_will_make_reachable()` 后写入当前 `journal_seq`，再 `insert_dirty()` |
| `split_root()` — 新 internal root | btree.rs | `set_will_make_reachable()` 后写入当前 `journal_seq`，再 `root_modified = true` |
| `btree_increase_depth()` — 新 root | interior.rs | `set_will_make_reachable()` 后 `cache.insert_dirty()` |
| `btree_set_root_for_read()` — 读入 root | interior.rs | 仅接受非当前 root，随后 `reset_key_count()` |
| `flush_dirty_nodes()` — 写入后 | volume/mod.rs | `clear_will_make_reachable()` |
| `shrink()` — 驱逐扫描 | cache.rs | 跳过 `will_make_reachable() == true` |
| `evict_one_leaf_with_jseq()` — leaf 驱逐 | cache.rs | 跳过 `will_make_reachable() == true` |

**设计决策**: `AtomicBool` vs bcachefs tagged pointer

| 维度 | bcachefs (C) | subvolmount (Rust) |
|------|-------------|-----------------|
| 类型 | `unsigned long` tagged pointer (含 `btree_update*`) | `AtomicBool` |
| 闭包引用 | `closure_get(&as->cl)` 持有更新状态机引用 | 无（同步 interior update 无需闭包生命周期管理） |
| 原子清除 | `xchg(&b->will_make_reachable, 0)` + `closure_put()` | `store(false, Release)` |
| 阻止效果 | `btree_node_reclaim()` 跳过 | `shrink()` + `evict_one_leaf()` 跳过 |

**决策理由**: 
- subvolmount 当前为同步 interior update 设计
- 同步路径直接完成所有节点操作，无需等待 I/O 闭包回调
- `AtomicBool` 更简单，且能通过 `Arc<BtreeNode>` 安全操作
- 如果将来引入异步 I/O 写路径，需要升级为类似 bcachefs 的闭包引用计数模式

**flush_dirty_nodes 必须拓扑排序 (P0-2, 2026-06-27)**:
- `flush_dirty_nodes()` 必须按 `node.level` 升序排列：叶子（level 0）先写，父节点/根后写
- 违反此顺序 → 崩溃后根节点指向未落盘的内部节点
```rust
// ✅ 正确：按 level 升序 flush
nodes.sort_by_key(|(_, _, node)| node.level);
for (addr, node_id, node) in &nodes {
    // 先写叶子，再写父节点
}
```

**node_iter_init 的搜索键必须落在节点 key 范围内**:
- `bch2_btree_node_iter_init()` 在节点非空时要求搜索键位于 `[min_key, max_key]`
- 越界搜索键说明调用方的 descent / 重试流程已经偏离节点边界，不应静默修正
- 对应 bcachefs `btree/bset.c:bch2_btree_node_iter_init()`；subvolmount 通过 `debug_assert!` 直接暴露这类偏差

**flush_btree() 批量 flush 在 sync_all 中执行**:
- `sync_all()` 负责触发 `flush_btree()`，后者收集脏节点后按 level 排序 flush
- 缓存 eviction 路径（读缓存满时驱逐脏页面）不得跳过拓扑排序
- `evict_dirty_nodes_bottom_up()` 独立实现自底向上驱逐（优先驱逐叶子级别的脏节点）
  - 使用 `inner.dirty.iter()` 扫描，优先 flush level 0 的脏节点再驱逐
  - 与 `flush_dirty_nodes()` 的拓扑排序互补——后者在 flush 时排序，前者在 evict 时排序

**depth=0 root 节点专用 dirty 跟踪**:
- root 节点（depth=0）不在 `cache.dirty` 中跟踪（避免 `Arc::get_mut` refcount 冲突）
- 使用独立 `root_modified: AtomicBool` 标记
- `ROOT_CACHE_ADDR = u64::MAX` 作根节点 sentinel 地址
- `flush_dirty()` 返回 `dirty_addrs` 时包含 `ROOT_CACHE_ADDR` 标记 root 需写

**Cannibalize 必须含重入保护**:
- Cannibalize（内存压力下替换缓存项）必须检查可重入性
- 递归 cannibalize 导致死锁 → 需要 per-thread cannibalize lock + stack depth guard

#### BtreeNode 序列化 Pipeline

**BtreeNode 序列化使用固定 C 布局（非 bincode）(Design Decision, 2026-07-01)**:

**Context**: 原 `serialize_to_bucket()` 用 bincode 序列化 BtreeNode 整体（含 header, bsets, entries），但 bincode 不是 bcachefs 兼容的磁盘格式。

**Options**:
1. **bincode** — 简单但产生 bcachefs 不兼容的二进制 blob，无法直接与 C 实现的磁盘格式对接
2. **手动固定 C 布局** — 用 `#[repr(C, packed)]` 结构体 + `ptr::write` 直接填充 buf

**Decision**: 使用固定 C 布局（`#[repr(C, packed)]` 结构体 + 直接指针/拷贝写入）。原因是 bcachefs 磁盘格式本身就是固定布局，无需中间序列化层。

**Layout (version=2, 2026-07-01)**:
```rust
// 连续内存布局：
// ┌─────────────────────────────────────────┐
// │ BtreeNodeHeader    (80 B, repr(C,packed))│
// │ BsetHeader         (16 B, repr(C,packed))│
// │ packed entries     (变长)                │
// │ CRC32C (4 B, 覆盖 header+bset+entries)  │
// │ zero pad to BLOCK_SIZE                  │
// └─────────────────────────────────────────┘
```

**Key Contracts**:
- `serialize_to_bucket()`: 返回 `Vec<u8>` (BLOCK_SIZE 字节)，内含 header → bset → entries → CRC，尾部零填充
- `deserialize_from_bucket()`: 读取 version 字段，v1 走 `deserialize_from_bucket_v1` 旧格式兼容，v2 走直接 ptr 读取
- CRC 覆盖范围: header + BsetHeader + entries（不含尾部 padding）
- 版本字段: `BtreeNodeHeader.version` — 1=旧 bincode 格式, 2=新固定 C 布局

**Common Mistake — CRC 覆盖范围不足 (P2, 2026-07-01)**:
```rust
// ❌ 错误：CRC 只覆盖 header
let crc = crc32c(0, &buf[..size_of::<BtreeNodeHeader>()]);

// ✅ 正确：CRC 覆盖 header + BsetHeader + entries 全部有效数据
let crc = crc32c(0, &buf[..data_end]);  // data_end = header + bset_hdr + entries
```

#### btree GC — 不可为空骨架

**GC 必须实现完整 mark-and-sweep**:
- `bch2_gc_btrees()` 必须遍历所有 btree 对 bucket 引用计数做 mark
- `bch2_gc_mark_key()` 必须根据 key 类型递增对应 bucket 的引用计数
- `bch2_gc_alloc_start/done()` 必须复制/合并 alloc btree 的引用计数
- 缺失任何一项 → 崩溃后 allocator 可能分配仍被引用的 bucket（数据覆盖）

**GC 必须含排他锁**:
- GC 运行时需要一个 `gc.lock` rwsem，写锁持有期间阻止其他写操作
- 缺少排他锁 → GC 与其他写操作并发导致引用计数不一致

**GC 必须在 recovery pass 中**:
- recovery 必须包含 `bch2_check_allocations`（即 GC）pass
- GC pass 在 journal replay 之后执行，重建 bucket 引用计数

**GC 必须含 topology check**:
- `bch2_check_topology` 必须验证 btree 节点之间的 prev/next 链接一致性
- 空桩 → 分裂后的 btree 拓扑损坏不可检测

#### journal flush/write/read — 校验与同步

**CRC32 必须覆盖完整 Jset**:
```rust
// ❌ 错误：CRC 只覆盖 entries
pub struct Jset {
    pub seq: u64,
    pub last_seq: u64,
    pub entry_count: u32,
    pub crc: u32,  // 只保护 entries
    pub entries: Vec<JsetEntry>,
}

// ✅ 正确：CRC 覆盖 magic + 全部头部字段 + entries
// bcachefs: crc = crc32c(JSET_MAGIC || seq || last_seq || ... || entries)
```

**CRC32C 硬件路径必须做 init/final 补码 (Common Mistake, 2026-07-01)**:
```rust
// ❌ 错误：硬件 CRC 指令不做标准 CRC32 的初始/最终取补
#[target_feature(enable = "sse4.2")]
unsafe fn crc32c_hw_wrong(data: &[u8], crc: u32) -> u32 {
    let mut crc64 = crc as u64;  // ❌ 应该 !crc
    // ... _mm_crc32_u64 …
    crc64 as u32  // ❌ 应该 !ret
}

// ✅ 正确：对齐 bcachefs crc32c_le_bch 语义
#[target_feature(enable = "sse4.2")]
unsafe fn crc32c_hw_correct(data: &[u8], crc: u32) -> u32 {
    let mut crc64 = (!crc) as u64;  // !crc 是标准 CRC 初始值
    // ... _mm_crc32_u64 …
    let ret = crc64 as u32;
    !ret  // 最终取补
}
```
**原因**: SSE4.2 `_mm_crc32_u64` 指令直接返回多项式除法的余数，不做标准 CRC 的 init/final 取补。纯软件查表实现则隐含了 `!crc` 初始值设计。在自动分发函数中，SW 和 HW 路径必须行为一致——HW 路径必须补上 `!init` 和 `!result`。
**检测**: trellis-check 通过比较 `crc32c_sw` 与 `crc32c_hw_impl` 的 Castagnoli 检验向量输出捕获此 bug。

**bch2_journal_flush 必须避免数据竞争**:
- `flush()` 将在持有 buf lock 的同时读取 buf data，再触发异步 I/O
- 必须在读数据后释放锁，防止 `add_entry()` 并发修改 buf

**journal entry 必须含版本号**:
- JsetEntry 必须包含 `version` 字段，以便未来格式变更时兼容
- 当前缺少数值版本字段

#### lock six — 内存序与降级语义

**write unlock 保留 intent 时必须递 increment seq**:
```rust
// ❌ 错误：释放 write 时不递增 seq
pub fn unlock_write(&self) {
    self.state_write.clear_write_bit();
}

// ✅ 正确：bcachefs 每次 write unlock 都递增 seq，
// 即使 intent 继续持有也一样
pub fn unlock_write(&self) {
    self.seq.fetch_add(1, Release);
    self.state_write.clear_write_bit();
}
```

**必须实现 handoff protocol**:
- 唤醒 waiter 后，必须设 `lock_acquired=true`（写锁独占唤醒，不与其他 waiter 竞争）
- 缺失 handoff → 高竞争场景下可能永久饿死

**WaitFifo 必须继承 bcachefs 唤醒语义**:
- bcachefs 的 `wait_on_bit` + `wake_up_bit` 唤醒单个 waiter，subvol 的 `VecDeque` 实现必须同样为单 waiter 唤醒
- Percpu 路径 memory ordering: bcachefs 用 `smp_mb()` fence，subvol 用 `Acquire` — 在弱排序架构上可能有可见性问题

**notify_waiters handoff 逐个唤醒（P0-2, 2026-06-27, 2026-06-28 更新为公平唤醒）**:
- write/intent 等待者只能逐个唤醒（避免惊群效应）
- 原 `woke_write_intent` 标记（first-come-first-serve slot 顺序 → 改为 `min_wi_trans_id` 公平唤醒
- 先扫描所有 write/intent 等待者找到 `trans_id` 最小的（最老事务），对齐 bcachefs `time_before64` 语义
- read 等待者仍唤醒全部（读者可共享锁）
- bcachefs 原生行为一致：`wake_up_bit` 在 `__wait_on_bit` 中只唤醒一个

**写锁抢占比量与 WRITE_BIT 公平性（P0-1, 2026-06-27）**:
- bcachefs 中写锁的 `WRITE_BIT` 实现写者排他；多个并发写者在抢到 `WRITE_BIT` 前不 sleep
- subvol 的写锁 `lock_write()` 慢路径在 CAS 失败转入 sleep 前，必须：
  1. `atomic::fence(SeqCst)` — 保证对其他线程的 `WRITE_BIT` 设置的可见性
  2. 重新检查 `self.state_write.load(Acquire)` 是否含 `WRITE_BIT`（另一位写者可能刚抢到）
  3. 若已有人持 WRITE_BIT → 继续等待；若无 → 重新尝试 CAS（防早期 sleep 饿死）
- 此 fence + re-check 与 bcachefs `__wait_on_bit` 中 `smp_mb()` 后的条件重检等价
- 缺失此检查 → 高写并发场景下写者可能提前进入 sleep 并导致写吞吐骤降

**lock_slowpath WAITING_WRITE_BIT 清除（P0-1, 2026-06-27）**:
- 双检路径（`trylock_ip` 在设置 WAITING_WRITE_BIT 后成功）必须调用 `clear_waiting_bit()`
- 遗漏清除 → 写锁释放后读者看到残留的 WAITING_WRITE_BIT 并错误等待（写锁幽灵）
- 与 `lock_write()` 的同等路径一致

**lock_acquired 传播（P0-3, 2026-06-27）**:
- `lock_slowpath` 中 `waiter_box` 为局部变量，需标记 `mut`
- handoff 路径（`is_handoff_for_current_thread`）和非 handoff 路径都需 `waiter_box.lock_acquired = true`
- 最终通过 `wait.lock_acquired = waiter_box.lock_acquired` 传播给调用者

#### alloc — BchDataType 与 sector 计数推导

**BchDataType 枚举值必须对齐本地 bcachefs（2026-07-13 复核）**

##### 1. Scope / Trigger

- 修改 alloc data type、journal bucket 分配或 recovery journal 补分配时适用。
- 唯一依据是本地
  `/home/black/Documents/bcachefs-tools/fs/alloc/accounting_format.h:55-75`
  和 `fs/journal/init.c:42-70`。

##### 2. Signatures

```rust
#[repr(u8)]
pub enum BchDataType {
    Free = 0,     Sb = 1,        Journal = 2,
    Btree = 3,    User = 4,      Cached = 5,
    Parity = 6,   Stripe = 7,    NeedGcGens = 8,
    NeedDiscard = 9, Unstriped = 10,
}

impl BchDataType {
    pub fn from_raw(v: u8) -> Option<Self>;
}

pub const BCH_DATA_NR: usize = 11;
```

##### 3. Contracts

- 有效值严格为 `Free(0)` 到 `Unstriped(10)`；`BCH_DATA_NR == 11` 只表示数量，
  不是有效枚举值。
- 禁止添加 `Reserved(11)` 或其他本地 bcachefs 不存在的 data type。
- `Journal::create()` 与 `fs_journal_alloc::run()` 的分配请求必须使用
  `BchDataType::Journal`，对应本地 `req->data_type = BCH_DATA_journal`。
- 不通过兼容分支继续接受旧的非对齐值 11 及以上。

##### 4. Validation & Error Matrix

| 输入/路径 | 结果 |
|---|---|
| `from_raw(0..=10)` | 返回一一对应的有效变体 |
| `from_raw(11..=255)` | 返回 `None` |
| journal 新建 bucket | 请求类型为 `Journal` |
| recovery 补分配 journal bucket | 请求类型为 `Journal` |

##### 5. Good / Base / Bad Cases

- Good：journal 创建沿用原调用顺序，只把请求 data type 设为 `Journal`。
- Base：`from_raw(10) == Some(Unstriped)`，`from_raw(11) == None`。
- Bad：把 `BCH_DATA_NR` 当成可持久化状态，或用 `Reserved` 表示 journal bucket。

##### 6. Tests Required

- 逐项断言 0–10 映射，并断言映射数量等于 `BCH_DATA_NR`。
- 断言 11、12、13、14 和 `u8::MAX` 均被拒绝。
- `timeout 60s cargo test -p subvol-core --lib` 必须通过。

##### 7. Wrong vs Correct

```rust
// Wrong: BCH_DATA_NR 不是有效 data type。
AllocRequest::new(Watermark::Normal, BchDataType::Reserved)

// Correct: 本地 journal/init.c 使用 BCH_DATA_journal。
AllocRequest::new(Watermark::Normal, BchDataType::Journal)
```

**derive_data_type() 必须使用 sector 计数**:
```rust
// ✅ 正确：bcachefs alloc_data_type 逻辑
pub fn derive_data_type(
    dirty_sectors: u32,
    cached_sectors: u32,
    stripe: u32,
    data_type: BchDataType,
) -> BchDataType {
    if stripe > 0 { return BchDataType::Stripe }
    if dirty_sectors > 0 { return data_type }  // 透传
    if cached_sectors > 0 { return BchDataType::Cached }
    BchDataType::Free
}
```
- bcachefs 不显式存储 data_type，而是从 dirty_sectors / cached_sectors / stripe 计数推导
- subvolmount 保留显式 state 字段作为缓存，以 sector 计数为真实来源
- 旧版签名含 `need_discard: bool` 参数，已移除（need_discard 不由 sector 计数推导）

**Bucket / BchAllocEntry 必须含 sector 计数字段**:
```rust
pub struct Bucket {
    pub state: BchDataType,
    pub dirty_sectors: u32,
    pub cached_sectors: u32,
    pub stripe: u32,
    pub journal_seq: u64,
    pub group: u32,
    pub version: u32,
    pub bucket_idx: u64,
}

pub struct BchAllocEntry {
    pub state: BchDataType,
    pub dirty_sectors: u32,
    pub cached_sectors: u32,
    pub stripe: u32,
    pub journal_seq: u64,
    pub group: u32,
    pub version: u32,
}
```
- `Bucket::derive_state()` 方法封装了 sector-count 推导逻辑
- `BchAllocEntry::from_bucket_with_journal_seq()` 用于分配路径中携带 journal_seq 写入 Alloc btree

**may_alloc_bucket() — 分配前 journal seq 安全检查（P0-6, 2026-06-27）**:
- 对应 bcachefs `may_alloc_bucket_journal_seq` (alloc_foreground.c)
- 防止 crash recovery 后分配尚未完成空转移 flush 的 bucket → 数据损坏
```rust
pub fn may_alloc_bucket(bucket: &Bucket, request_journal_seq: u64) -> bool {
    if request_journal_seq == 0 { return true; }  // 无 journal 追踪
    if bucket.journal_seq_empty == 0 { return true; } // 空转移尚未追踪
    bucket.journal_seq_empty <= request_journal_seq    // journal 已推进到 bucket 变空之后
}
```
- `AllocRequest.journal_seq` 由调用方传递（`journal_cur_seq` 或 `last_seq_ondisk`）
- `bucket.journal_seq` 记录最后引用 seq，`bucket.journal_seq_empty` 在 `NeedDiscard` 迁移时写入当前 seq，trim 成 `Free` 时清零
- 在 `allocate_bucket_inner()` 和 reuse 路径中都需调用

#### recovery — 必须集成到 Volume 启动路径

**Recovery 模块不是可选项**:
```rust
// ❌ 错误：recovery 已定义但 Volume::new 不调用 — 死代码
pub struct Volume { ... }
impl Volume {
    pub fn new(path: &Path) -> Result<Self> {
        // 不调用 bch2_fs_recovery()
    }
}

// ✅ 正确：Volume 启动时必须执行 recovery passes
impl Volume {
    pub fn new(path: &Path) -> Result<Self> {
        bch2_fs_recovery(&mut self)?;
    }
}
```

**btree root level 信息不可丢失**:
- `BtreeRoots::load_from_superblock()` 必须提取并存储 `level` 字段
- 丢失 level 导致 btree 加载器无法正确重建非 level-0 root

**必须含 unclean shutdown seq skip**:
- unclean shutdown 后 journal replay 必须跳过 `seq + 64` 并黑名单化 seq 范围
- 防止崩溃前写入 btree 但未 journal 的修改被错误应用

**journal replay 必须避免双重读取**:
- `journal_read` 已经读到 `state.jsets`，`journal_replay` 不应再次从磁盘读取
- 改为从内存 jsets 列表直接应用

**禁止自有 overlay_btree**:
- recovery pass 期间按 bcachefs journal replay 顺序直接 materialize 到 btree；
- 不增加独立的 overlay、读穿透或 drain API；
- 若需要暂存，必须使用本地 bcachefs 对应的 journal/key-cache 结构。

**必须含 journal rewind 支持**:
- 防御未正确实现 FUA/FLUSH 的块设备
- 从最后一个有效 entry 向前扫描找到一致的状态

**示例**:
```rust
// ✅ 正确：签名对齐 + 逻辑一致 + Rust 惯用实现
pub fn bch2_snapshot_is_ancestor(c: &SnapshotTable, id: u32, ancestor: u32) -> bool {
    // Rust 数组下标代替 C 指针运算，但 skip_list 遍历逻辑与 bcachefs 一致
    let mut id = id as usize;
    while id != ancestor as usize {
        let parent = c.snapshot_parent(id);
        if parent == id { return false; }
        id = parent;
    }
    true
}
```

### serde 反序列化结构体

对于反序列化外部二进制格式（如 bcachefs on-disk 格式）的结构体，所有字段必须按正确偏移声明，即使只使用部分字段：

```rust
#[derive(serde::Deserialize)]
struct SnapshotRef {
    #[allow(dead_code)]  // 只读 subvol，但其他字段影响二进制布局
    flags: u32,
    parent: u32,
    children: [u32; 2],
    subvol: u32,
    // ... 其余字段
}
```

**教训**: 不要为了"只用部分字段"就定义裁剪版结构体。bincode 反序列化按字段顺序匹配，少一个字段整个偏移链全错。`SnapshotT` 有 11 个字段，如果定义 8 字段的结构体去反序列化，第 9 个 bincode 字段会被解释成垃圾，导致后续节点遍历指向随机内存。

✅ 正确做法: 永远使用完整结构体，不需要的字段用 `#[allow(dead_code)]` 标注。

### BchSb 新字段必须 `#[serde(default)]`

向 `BchSb`（superblock）添加字段时，必须标记 `#[serde(default)]` 以保证旧版本序列化数据的向后兼容性：

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BchSb {
    // ... 已有字段
    /// GC 位置（用于崩溃恢复后继续中断的 GC）
    #[serde(default)]
    pub gc_pos: GcPos,
    /// gc_pos 是否有效（旧版本无此字段时 false）
    #[serde(default)]
    pub gc_pos_valid: bool,
}
```

**`#[serde(default)]` 要求**：字段类型必须实现 `Default`：
- 整数类型（`u32`, `u64` 等）天然支持
- 自定义类型（如 `GcPos`）需 `#[derive(Default)]`
- 枚举类型（如 `GcPhase`）需 `#[derive(Default)]` + `#[default]` 指定默认变体

**注意**：`#[serde(default)]` 仅在反序列化旧版本数据时生效（当二进制数据中不存在该字段时使用默认值）。新版本序列化始终写入该字段。

### 写入同步点

单次写入和事务提交都通过 `insert_entry_raw()`、`flush_dirty_nodes()`、`bch2_trans_commit()` 路径收敛，依赖同一个 `flush_cache_dirty_keys()` 同步点：

- `insert_entry_raw()` 负责普通写入前的脏缓存清理
- `flush_dirty_nodes()` 负责节点刷新前的脏缓存清理
- `bch2_trans_commit()` 负责事务提交前的脏缓存清理
- 所有变更在同一个 journal entry 中提交，崩溃时整批回滚

### bch2_snapshot_node_create 的双 child 模式

`extra_child_subvol: Option<u32>` 控制创建模式：

- `None` → 创建单个 snapshot node（传统模式，向后兼容测试）
- `Some(src_subvol)` → "1变2" bcachefs 语义：分配两个快照 ID，源 subvol 指向 child1，新 subvol 指向 child2，parent skip 指针两路更新。三路写入通过一次原子事务提交完成

```rust
// ✅ 正确: 创建两个快照 child 并原地更新 parent
let child_id = snapshot_node_create(
    c, t, trans,
    parent_id, Some(src_subvol)
)?;
// 成功: src_subvol 已指向 child1，新 subvol 已指向 child2
```

### Skiplist 指数步进

Skip 列长度固定为 3（`[u32; 3]`），新节点的 skip 直接从父节点继承：
- `skip[0]` = parent_id（直接父节点）
- `skip[1]` = `parent.skip[0]`（父节点的 parent）
- `skip[2]` = `parent.skip[1]`（父节点的 skip[1]）

这天然形成指数级跳转。`bch2_snapshot_skiplist_get` 返回 `Option<[u32; 3]>` 而非 Option<u32>：

```rust
pub fn bch2_snapshot_skiplist_get(c: &SnapshotTable, id: u32) -> Option<[u32; 3]> {
    let parent = c.snapshot_parent(id);
    if parent == id { return None; }
    Some([
        parent,
        c.snapshot_skip(parent, 0),
        c.snapshot_skip(parent, 1),
    ])
}
```

**`build_skip_list_from_btree`** 现在是全量重建逻辑（不再增量修补）：
1. 遍历 btree 中所有快照 key，收集 `(id, parent, children)` 三元组到临时表
2. 显式初始化 skip = `[0u32; 3]`
3. 按拓扑顺序填充 skip（保证父节点的 skip 已就绪）
4. 写回 btree

## Verification Status — Batch D (2026-06-28)

### key_cache 模块 — bcachefs C 一致性验证（9 项新增）

2026-06-28 通过 main-session 直接实施，4 个 Phase 全部完成：

| # | Phase | 新增项 | C 引用 | 验证结论 |
|---|-------|--------|--------|----------|
| 1 | P1-SlotReuse | `CachedEntry.valid` 标志 | `struct bkey_cached.valid` | ✅ |
| 2 | P1-SlotReuse | `find()` 检查 valid | `bch2_btree_key_cache_find` | ✅ |
| 3 | P1-SlotReuse | `invalidate()` 只设 valid=false | `bch2_btree_key_cache_drop` | ✅ |
| 4 | P2-Dirty | `dirty`/`pin_type`/`flush_pending` 字段 | `KEY_CACHE_DIRTY` 标志 | ✅ |
| 5 | P2-Dirty | `nr_dirty` 计数 + `bch2_nr_btree_keys_need_flush` | `bch2_nr_btree_keys_need_flush` | ✅ |
| 6 | P2-Dirty | `bch2_btree_insert_key_cached()` 脏存储重写 | `btree_key_cache.c:843-885` | ✅ |
| 7 | P3-JournalPin | `pin_entry()`/`drop_journal_pin()` + callback 链 | `bch2_journal_pin_copy/drop/set` | ✅ |
| 8 | P4-Flush | `collect_dirty()` + `mark_clean()` 两阶段 | `bch2_btree_key_cache_flush` | ✅ |
| 9 | P4-Flush | `flush_cache_dirty_keys()` Engine 方法 | `bch2_btree_key_cache_journal_flush` | ✅ |

### 测试覆盖验证

- **key_cache**: 23 tests → 23 ✅（新增 9 个测试: slot_reuse, dirty_tracking, journal_seq, insert_key_cached, journal_pin_integration, journal_pin_with_instance, flush_callback, collect_dirty_and_mark_clean, flush_dirty_callback, flush_dirty_skip_failed_writes, engine_flush_cache_dirty_keys）
- **btree 相关**: 全部通过（含新增的 `insert_entry_skip_cache` 和 `insert_entry_into_node` 路径）
- **全量 subvol-core lib**: 714 passed（+4 个新增 flush 测试），5 known fail（预存 AddressSpaceExhausted），6 ignored
- **clippy/fmt**: 0 新增 warning/diff

### 已知差距（非本次范围）

- flush 同步点已接线：`insert_entry_raw()`、`flush_dirty_nodes()`、`bch2_trans_commit()` 路径都会在写入前调用 `flush_cache_dirty_keys()`；后续新增写入口时需同步补齐
- `bch2_btree_key_cache_journal_flush` 的 reclaim 触发语义与 pin 类型分桶
- KCQ (key cache queue) bcachefs 对齐的 shrinker 和后台 flush 线程
- bcachefs 中 `struct btree_update` 在 key cache flush 路径中的异步状态机（subvolmount 当前同步 flush 已足够）

**验证结论**: PASS_WITH_NOTES
- Note: 同步点已接线，后续重点是新写入口的回归约束和更广泛的调度/后台 flush 对齐
- Note: `bch2_trans_commit()` 路径也有回归测试，确保 journal 写入前先 flush dirty key cache
- Note: 架构约束已更新：Journal 不再严格"仅用于崩溃恢复"，Key cache write-back 参与 journal pin 协调

## Verification Status — Batch E (2026-06-28)

### Btree IO 节点读写对齐 — 4 个 Phase 全部实现

| Phase | 新增项 | 函数/结构 | 验证结论 |
|-------|--------|-----------|----------|
| P1-Read | bset 结构验证 | `bch2_validate_bset()` — data_offset/end_offset/8-align/bounds | ✅ |
| P1-Read | key 排序验证 | `bch2_validate_bset_keys()` — 非降序、无重复、format 合法 | ✅ |
| P1-Read | 完整验证流水线 | `bch2_btree_node_read_done()` — nsets→per-bset→sort-merge→drop | ✅ |
| P1-Read | 读取后排序合并 | `bch2_read_done_sort()` — SortIter + compact 回退 | ✅ |
| P1-Read | 范围 key 过滤 | `bch2_btree_node_drop_keys_outside_node()` — 按 min/max_key 裁剪 | ✅ |
| P1-Read | 调试输出 | `bch2_btree_node_header_to_text()` — header field 格式化 | ✅ |
| P2-Write | SortIter 架构 | `SortIter` struct + `init_from_node/add/add_all_bsets/sort_into/total_keys` | ✅ |
| P2-Write | 写入前排序 | `bch2_btree_node_sort_keys()` — 排序合并多 bset 后 compact | ✅ |
| P2-Write | 写入路径集成 | `bch2_btree_node_write_mut()` — 序列化前调用 sort_keys | ✅ |
| P3-IOFlags | write_in_flight 标志 | `NODE_WRITE_IN_FLIGHT=0x04` + `try_lock/unlock_write_in_flight()` CAS | ✅ |
| P3-IOFlags | read_in_flight 标志 | `NODE_READ_IN_FLIGHT=0x08` + `try_lock/unlock_read_in_flight()` CAS | ✅ |
| P3-IOFlags | just_written 标志 | `NODE_JUST_WRITTEN=0x10` — write_mut 后设置 | ✅ |
| P3-IOFlags | io_lock/unlock 实现 | `bch2_btree_node_io_lock/unlock` — spin+CAS (从 no-op 改为真实) | ✅ |
| P3-IOFlags | wait_on_read/write | spin 等待标志位清除 (从 no-op 改为真实) | ✅ |

### 测试覆盖验证

- **btree::io**: 19 tests → 29 ✅（新增 19: SortIter 5, IO flags 5, read_done 3, write 3, post_write_cleanup 2, checksum 1）
- **全量 subvol-core lib**: 740 passed（较 Batch D +26），5 known fail, 6 ignored
- **clippy/fmt**: 0 新增 warning/diff

## Verification Status — Batch G (2026-06-29)

### Key Cache JournalEntryPin 集成

| 变更项 | 说明 | 状态 |
|--------|------|------|
| `CachedEntry` 嵌入 `JournalEntryPin` | 替换 `journal_seq: AtomicU64` | ✅ |
| `JournalEntryPin.pin_type` | 显式分类为 `KeyCache` / `Btree*` / `Other` | ✅ |
| `pin_entry()` 使用 `bch2_journal_pin_add` | 替代过渡 `_seq` API，注册真实 flush callback | ✅ |
| `drop_journal_pin()` 使用 `bch2_journal_pin_drop` | 正确移除侵入式链表节点 | ✅ |
| `bch2_fs_btree_key_cache_exit()` 清理 pin | 先 `drop_all_journal_pins()` 再 `clear()` | ✅ |
| `Drop for KeyCache` 自动清理 | 防止 cleanup 遗漏导致 dangling pointer | ✅ |
| `unsafe impl Sync for CachedEntry` | 安全论证：`flush callback` 受 journal pin Mutex 保护 | ✅ |

### 测试覆盖验证

- **btree::key_cache**: 22 passed (较 Batch F 不变，语义正确性已验证) ✅
- **全量 subvol-core lib**: 762 passed, 5 known fail, 9 ignored ✅
- **clippy/fmt**: 无新增 warning/diff ✅

### Issues Found and Fixed by trellis-check

1. **`bch2_fs_btree_key_cache_exit()` dangling pointer** — `clear()` 直接 drop `Arc<CachedEntry>` 时，`JournalEntryPin.Link` 可能仍在 journal 侵入式链表中。修复为先 `drop_all_journal_pins()` 再 `clear()`，并添加 `Drop for KeyCache` 防御性清理。

### Issues Found and Fixed by trellis-check

1. **`read_in_flight` 标志泄漏** — 原 `bch2_btree_node_read_done()` 在 `validate_bset`/`validate_bset_keys`/`read_done_sort` 返回错误时，`read_in_flight` 不会被清除。修复为双函数模式（`bch2_btree_node_read_done` 作包装 + `_read_done_inner` 作内部实现），使用结果变量确保 `clear_read_in_flight()` 在所有错误路径都被调用。

2. **`BLOCK_SIZE` 未使用导入** — io.rs 中导入了但未使用，已移除。

3. **`bset_idx` 字段 dead_code** — `SortIterEntry.bset_idx` 已移除，`SortIter::add` 只保留排序所需的偏移参数。

4. **`is_multiple_of` clippy 建议** — `end_offset % 8 != 0` 改为 `end_offset.is_multiple_of(8u32)`。

5. **`AtomicBool` 未使用导入** — node.rs 中新增但从未使用（IO 标志位全部通过 `AtomicU8 flags` 实现），已移除。

### 关键设计决策

#### IO 标志位协议：AtomicU8 位操作 + CAS

**不要使用 Mutex 或单独的 AtomicBool 字段**。所有 IO 标志（write_in_flight / read_in_flight / just_written）复用 BtreeNode 已有的 `flags: AtomicU8`：

```rust
pub const NODE_WRITE_IN_FLIGHT: u8 = 0x04;
pub const NODE_READ_IN_FLIGHT: u8 = 0x08;
pub const NODE_JUST_WRITTEN: u8 = 0x10;

// 加锁：CAS 协议
pub fn try_lock_write_in_flight(&self) -> bool {
    self.flags
        .compare_exchange_weak(
            self.flags.load(Relaxed) & !NODE_WRITE_IN_FLIGHT,
            ... | NODE_WRITE_IN_FLIGHT,
            Acquire, Relaxed,
        )
        .is_ok()
}

// 解锁：fetch_and 清除
pub fn unlock_write_in_flight(&self) {
    self.flags.fetch_and(!NODE_WRITE_IN_FLIGHT, Release);
}
```

****为什么不是 Mutex**：bcachefs 使用 `wait_on_bit_lock` 在位标志上进行 spin，不是 Mutex。CAS + 位标志与 bcachefs 的 `clear_bit`/`set_bit`/`wait_on_bit` 协议对应。

#### bch2_btree_node_read_done 双函数模式（防止资源泄漏）

```rust
pub fn bch2_btree_node_read_done(node: &mut BtreeNode) -> Result<(), StorageError> {
    node.try_lock_read_in_flight();
    let result = _read_done_inner(node);  // 真正的验证逻辑
    node.clear_read_in_flight();           // 所有路径都被调用
    result
}
```

这个模式确保 `read_in_flight` 标志在错误路径上也被正确清除。

#### SortIter 使用 raw pointer 操作 packed key

SortIter 在 packed key 级别排序合并，避免 full unpack/repack 的开销：

```rust
pub struct SortIter {
    entries: Vec<SortIterEntry>, // 每个 bset 一个 cur/end 游标
    used: usize,
    size: usize,
    data: *const u8,             // 指向 node.data 的 raw pointer
    data_len: usize,
}
```

- `add(start, end)` — 每个非空 bset 添加一个游标范围
- `add_all_bsets(node)` — 遍历 node 的所有活跃 bset，添加 bset 游标
- `sort_into(dst)` — 按本地 `sort_iter_sort/sift/peek/advance` 顺序合并 packed key
- 重叠修复比较使用 `bkey_cmp_packed`，相等时以原始 byte offset 模拟 C 指针顺序

#### 写入前自动排序

`bch2_btree_node_write_mut` 在序列化前自动调用 `bch2_btree_node_sort_keys(node)`，确保多 bset 被合并为单一排序 bset。与 `serialize_to_bucket` 内部的 `collect_all_entries` 互补——前者减少 bset 数量，后者保证排序去重。

#### JournalEntryPin 嵌入模式（替代 _seq 过渡 API）

`KeyCache` 中，每个 dirty `CachedEntry` 通过嵌入 `JournalEntryPin` 替代独立的 `journal_seq: AtomicU64`：

```rust
struct CachedEntry {
    pin: JournalEntryPin,    // 嵌入 pin (含 intrusive Link + seq + flush callback)
    // ... 其他字段
}
```

关键点：
- `pin_entry()` 使用 `bch2_journal_pin_add(seq, &entry.pin, flush_fn)` 而非 `_seq` 过渡 API
- `drop_journal_pin()` 使用 `bch2_journal_pin_drop(&entry.pin)` 正确移除侵入式链表节点
- `bch2_fs_btree_key_cache_exit()` 和 `Drop for KeyCache` 必须先调用 `drop_all_journal_pins()` 再 `clear()`，防止 `Link` dangling pointer

⚠️ **必须的清理顺序**：`JournalEntryPin` 的 `Link` 是侵入式链表节点，drop `CachedEntry` 前必须已调用 `bch2_journal_pin_drop` 将其从 journal 的 unflushed/flushed 链表中移除。跳过此步骤会导致 journal 链表中的 dangling pointer。

### 已知差距（跨批次跟踪）

| 差距 | 状态 | 批次 |
|------|------|------|
| `bch2_btree_node_read()` 调用 `read_done()` | ✅ 已修复 | Batch F |
| `bch2_btree_node_write`（&self 版）调 sort_keys | 按设计保留（write_mut 替代方案） | — |
| bset checksum 验证在 read/load 边界显式进行（`deserialize_from_extent` / `load_btree_node_from_ptr`） | ✅ 已覆盖 | `fs/btree/read.c:629-724` |
| IO 锁在 write 路径中已被调用，read 路径不需要 | ✅ write 路径已集成 | Batch F |
| sort_iter `bset_idx` 字段移除 | ✅ 已覆盖 | `fs/btree/sort.h:7-43` |
| key_cache journal_flush 空 stub | ✅ 已修复 | Batch G |
| `KeyCache::pin_entry` 使用 JournalEntryPin 替代 _seq 过渡 API | ✅ 已修复 | Batch G |
| `_seq` 过渡 API 未迁移（25处/5文件） | ✅ 全部迁移 | Batch H |
| write_buffer P0 全部功能缺失（6项） | ✅ 全部实现（755行，10测试） | Batch I |
| GC 模块全部 6 项 P0 差距（含 recovery pass 接线） | ✅ 全部实现（880行，13测试） | Batch J |

**验证结论**: PASS（Batch E-J）— 全部 14 项 P0 bcachefs 不一致已修复完成
- Note: Batch E-F 完成 btree IO 4 个 Phase 全部实现和集成
- Note: Batch G 完成 key_cache JournalEntryPin 嵌入，替换过渡 API
- Note: Batch H 完成全代码库 `_seq` → `JournalEntryPin` 迁移，删除过渡 API
- Note: trellis-check 在每个批次中发现并修复了关键 bug

## Verification Status — Batch H (2026-06-29)

### `_seq` 过渡 API 迁移

| 模块 | 迁移内容 | 状态 |
|------|----------|------|
| `btree/io.rs` | 3处 `_add_seq` → `bch2_journal_pin_add` (嵌入 BtreeNode.journal_pin) | ✅ |
| `btree/cache.rs` | 8处 `_drop_seq` → `bch2_journal_pin_drop(&pin)` | ✅ |
| `volume/mod.rs` | 3处 `_set_seq`/`__bch2_journal_pin_put` → `pin_add`/`pin_drop` | ✅ |
| `journal/types.rs` | 删除 `_set_seq`/`_add_seq`/`_drop_seq` 三个过渡函数 | ✅ |
| `journal/reclaim.rs` | `__bch2_journal_pin_put` 改为 `pub(crate)` | ✅ |
| `journal` | `bch2_journal_update_last_seq` 改为私有 | ✅ |

### 测试覆盖验证

- **btree::io**: 29 passed ✅
- **btree::cache**: 27 passed ✅
- **volume**: 17 passed ✅
- **journal**: 73 passed ✅
- **全量 subvol-core lib**: 762 passed, 5 known fail, 9 ignored ✅

### Issues Found and Fixed by trellis-check

1. **`evict_one_leaf_with_jseq` 注释过期** — 注释仍说"返回 journal_seq"，实际返回 `JournalEntryPin`。已更新注释。
2. **`drop_pin_for_node` 注释过期** — 注释仍写"查找 journal_seq"，实际查找 journal pin。已更新注释。

---

## Verification Status — Batch I (2026-06-29)

### write_buffer P0 验证

write_buffer 在前期 Batch/Phase 工作中已完成全部 P0 功能实现，本次验证确认所有 P0-5~P0-10 条目已对齐 bcachefs：

| P0 | 需求 | 实现状态 |
|----|------|----------|
| P0-5 | `bch2_journal_key_to_wb()` — 将 journal key 插入 inc 队列 | ✅ 完整实现：锁定 inc → 追加 key → 解锁 |
| P0-6 | `bch2_btree_write_buffer_flush_locked()` — 7 步 flush 管线 | ✅ 完整实现：move_keys → sort → dedup → fastpath insert → slowpath txn retry |
| P0-7 | `bch2_btree_write_buffer_must_wait()` — 容量检查 | ✅ 基于 inc/flushing 总量 vs capacity * 3/4 |
| P0-8 | `bch2_journal_write_buffer_need_flush()` — 全 wb 检查 | ✅ 检查所有 wb 的 inc.nr / flushing.nr |
| P0-9 | 数据结构对齐（BtreeWriteBufferedKey, WbKeyRef） | ✅ btree_id + bpos 拆分；排序用轻量 WbKeyRef 索引数组 |
| P0-10 | flush → btree insert 核心循环 | ✅ wb_sort + flush_fastpath + flush_slowpath 完整实现 |

### 文件状态

- `btree/write_buffer.rs`: 755 行完整实现（非骨架）
- `wb_sort()`: 按 (btree_id, inode, offset, snapshot) 排序
- `dedup_sorted_refs()`: 相同 pos 的条目保留最新 journal_seq
- `flush_fastpath()`: engine.get_entry noop 检查 + engine.insert_entry
- `flush_slowpath()`: 通过 BtreeTrans.journal_insert + bch2_trans_commit 重试
- 全部公开 API 函数均已实现（非空操作）

### 测试覆盖验证

- **btree::write_buffer**: 10 passed ✅
  - `test_write_buffer_insert_and_flush` — 3 key insert + flush → engine 验证
  - `test_write_buffer_dedup` — 同位置 3 key → 仅保留 journal_seq=30
  - `test_write_buffer_noop_elimination` — engine 已有相同值 → flush 不改变 key_count
  - `test_write_buffer_sort_order` — 无序插入 → 排序后 offset 升序
  - `test_write_buffer_must_wait` / `test_write_buffer_should_flush` — 容量判断
  - `test_write_buffer_flush_locked_empty` — 空 buffer flush 无副作用
  - `test_wb_key_cmp` / `test_write_buffer_create`
- **全量 subvol-core lib**: 762 passed, 5 known fail, 9 ignored ✅（基线无变化）

### 验证结论

**PASS** — write_buffer P0-5~P0-10 全部完成，功能与 bcachefs 对齐。

---

## Verification Status — Batch J (2026-06-29)

### GC Phase 5 收尾 — recovery pass 接线

| 原 Gap | 功能 | 状态 |
|--------|------|------|
| G1 | Mark-and-sweep (bch2_gc_btrees, bch2_gc_mark_key) | ✅ 已有完整实现 |
| G2 | Alloc 检查修复 (bch2_gc_alloc_start/done) | ✅ 已有完整实现 |
| G3 | 拓扑检查 (bch2_check_topology) | ✅ 已有完整实现 |
| G4 | GC 排他锁 (gc.lock rwsem) | ✅ RwLock<()> 已在 BtreeGc |
| G5 | Generation 清理 (bch2_gc_gens) | ✅ 已有完整实现 |
| G6 | GC 在 recovery pass 中 | ✅ `check_topology` pass 集成 `bch2_gc_gens`；死代码 `recovery/passes/gc.rs` 已删除 |

### 测试覆盖验证

- **btree::gc**: 13 passed ✅（gc_gens, check_topology, check_allocations, gc_btrees, mark_key）
- **全量 subvol-core lib**: 762 passed, 5 known fail, 9 ignored ✅（基线无变化）

### 验证结论

**PASS** — 全部 14 项 P0 bcachefs 不一致（Phase 1-5）已修复完成。

---

## Verification Status — Batch K (2026-06-29)

### Lock P1: WRITE_BIT 预设 + 内存序

**变更**:
- `six.rs`: `lock_write()` 慢路径预设 WRITE_BIT（对齐 bcachefs `atomic_add(SIX_LOCK_HELD_write)`）
- `six.rs`: 新增 `try_lock_write_preset()`（慢路径专用 trylock，不检查 WRITE_BIT 预设）
- `six.rs`: `fetch_or(WAITING_WRITE_BIT, Relaxed)` → `SeqCst`
- `six.rs`: `notify_waiters()` 适配 WRITE_BIT 预设场景的 handoff

**验证**:
- ✅ `cargo build -p subvol-core` — 通过（无新警告）
- ✅ `cargo test -p subvol-core --lib` — 762 passed / 5 known fail / 9 ignored（基线不变）
- ✅ 46 个 lock 测试全部通过（含 stress 忽略 8 个）
- ✅ 之前挂起的 `test_lock_write_blocks_and_succeeds` 和 `test_notify_waiters_wakes_writer` 现在通过

**结论**: PASS

---

<!-- What level of testing is expected -->

(To be filled by the team)

---

### bch2_journal_wake_up — 对齐 C 的 `journal_wake()` (2026-07-01)

**C 源码**: `fs/journal/journal.h:118`

```c
static inline void journal_wake(struct journal *j)
{
    closure_wake_up(&j->async_wait);
}
```

**语义**: `journal_wake()` 只唤醒所有在 `j->async_wait` 上等待的 closure，不做状态推进。

**subvolmount 修复模式**:

```rust
// ✅ 正确：只唤醒等待者，不做状态推进
pub fn bch2_journal_wake_up(&self) {
    for idx in 0..JOURNAL_STATE_BUF_NR {
        let buf = self.bufs.get_mut(idx);
        buf.notify.notify_waiters();
    }
}

// ❌ 错误：在 journal_wake_up 中做 Closing→WriteSubmitted 状态转换
// - journal_res_put() 已在 refcount 归零时处理此转换
// - C 的 journal_wake 不管理状态机
pub fn bch2_journal_wake_up(&self) {
    for idx in 0..JOURNAL_STATE_BUF_NR {
        let buf = self.bufs.get_mut(idx);
        if buf.state == BufState::Closing {
            let count = ...;
            if count == 0 {
                buf.state = BufState::WriteSubmitted;  // ❌ 重复逻辑
                buf.notify.notify_waiters();
            }
        }
    }
}
```

**C 中调用 `journal_wake(j)` 的位置**:
| C 函数 | subvol 等价函数 |
|--------|------------------|
| `bch2_journal_error_set()` (journal.c:255) | `bch2_journal_error_set()` |
| `__bch2_journal_flush()` (journal.c:566) | `bch2_journal_flush()` (通过 set_watermark 间接) |
| `bch2_journal_cycle_locked()` (journal.c:673) | `journal_cycle_locked()` |
| `write.c:434` (journal I/O 完成后) | `write_bufs_to_bucket()` (通过 set_watermark 间接) |
| reclaim.c 多处 | `__bch2_journal_reclaim()` (通过 set_watermark 间接) |
| `bch2_journal_set_watermark()` (reclaim.c:104-105) | `bch2_journal_set_watermark()` |

### Journal Jset repr(C) 固定布局序列化 (2026-07-01)

**Context**: Journal Jset 使用 `#[derive(Serialize, Deserialize)]` + bincode 序列化/反序列化两次（一次在 append 时为计算大小，一次在 serialize_padded 时真正写盘）。bincode 不是 bcachefs 兼容的磁盘格式，且 append 路径因 serde 开销慢。

**Options Considered**:
1. **bincode（维持现状）** — 简单但性能差、格式不兼容
2. **repr(C) 双结构** — `JsetHeader` + `JsetEntryHeader` 均为 `#[repr(C)]`，直接 `ptr::copy` 写入 buf
3. **手写字节解析** — 无 repr(C)，纯字节操作

**Decision**: repr(C) 双结构。理由：直接映射磁盘格式，消除 serde 依赖，与 bcachefs `struct jset` + `struct jset_entry` 对齐。

#### Key Contracts

**未对齐读取**：
```rust
// ✅ 正确：data 可能从任意对齐的 Vec<u8> 来
let hdr: JsetHeader = unsafe { ptr::read_unaligned(data.as_ptr() as *const JsetHeader) };

// ❌ 错误：ptr::read 要求目标对齐
let hdr: JsetHeader = unsafe { ptr::read(data.as_ptr() as *const JsetHeader) };
```

**CRC 覆盖范围**：
```rust
// ✅ 正确：覆盖 header（crc32=0）+ 全部 entries，不含 padding
let crc = crc32c(0, &buf[..data_size]);
unsafe { ptr::write_unaligned(&mut buf[24] as *mut u32, crc); }
```

**版本检测**：
```rust
// ✅ 正确：先读 8 字节 magic + 4 字节 seq 后半部分作为 version 判断
let version = unsafe { ptr::read_unaligned::<u32>(data.as_ptr().add(32)) };
let is_v2 = (2..=JSET_VERSION).contains(&version);
```

#### LegacyJset 反序列化陷阱

```rust
// ❌ 错误：LegacyJset.version 定义为 u32（偏移 32 处读 4 字节）
// 旧 v1 bincode 格式在偏移 32 处实际是 2 字节 u16 + 1 字节 u8
// 多读 2 字节导致整个结构体偏移链全错

// ✅ 正确：必须匹配旧 bincode 布局
#[derive(Serialize, Deserialize)]
struct LegacyJset {
    magic: [u8; 8],
    seq: u64,
    last_seq: u64,
    crc32: u32,
    entry_count: u32,
    version: u16,     // ← 必须 u16，匹配旧 bincode 布局
    csum_type: u8,
    // ...
}
```

#### serialize_padded 优化路径

```
旧路径：serialize_padded() 完整序列化 → .len() 算大小 → 预分配
新路径：serialized_padded_len() 直接算大小 → 预分配 → serialize_padded() 填充
```

append 和 bch2_trans_commit 使用 `serialized_padded_len()` 预判 journal reservation 大小，无需预分配 buffer。

### flush_cache_dirty_keys journal_seq 传播 (2026-07-01)

**问题**: `flush_cache_dirty_keys()` 硬编码 `journal_seq = 0` 写入 btree 节点，导致 recovery 时无法正确关联节点到 journal 序列号。

**方案**: 为 `flush_cache_dirty_keys()` 添加 `journal_seq: u64` 参数，各调用点根据上下文传递合适的 seq：

```rust
// 调用点对齐
insert_entry_raw(seq)  → flush_cache_dirty_keys(seq)  // 有明确的 journal_seq
flush_dirty_nodes()    → flush_cache_dirty_keys(0)    // 同步清理脏节点
bch2_trans_commit()    → flush_cache_dirty_keys(0)    // journal 写入前 flush
```

**关键决策**:
- `insert_entry_raw()` 在每次 journal 写入前都 flush 脏 cache entries。对齐 bcachefs 语义：脏 key 必须在写入 journal 前落盘 btree，否则 crash recovery 后 journal 条目引用不存在的 key。
- `flush_dirty_nodes()` 和 `bch2_trans_commit()` 在写入前调用 `flush_cache_dirty_keys(0)`。

**C 对应关系**:
- C 中 `bch2_btree_key_cache_journal_flush` 是 journal pin callback，由 journal reclaim 驱动；subvolmount 现在保留这一触发模型，但用显式 `JournalEntryPin.pin_type` 代替 callback 身份分类
- subvolmount 继续使用同步 flush 入口写回脏 cache，但 reclaim 侧 bucket 分类已与 C 的 key cache / btree pin 语义对齐
- `journal_seq` 参数仍对齐 C 的 `ck->journal.seq` 语义
- `journal_flush_pins()` 的返回值只统计成功完成 cleanup 的 flush 次数；callback 返回错误时，cleanup 先执行，再传播错误，不把失败尝试计入成功数

---

### bcachefs 单位约定 — 字节与扇区混合策略 (2026-07-07)

**问题**: 代码库中字节与扇区单位混用，且 `SECTOR_SIZE` / `SECTORS_PER_BLOCK` 在 `btree/node.rs` 和 `alloc/reservation.rs` 重复定义。

**验证来源**: 本地 bcachefs 源码 `bcachefs-tools/`：

| 字段 | bcachefs 单位 | 证据 |
|------|---------------|------|
| `SECTOR_SHIFT` | 9 | `include/linux/blkdev.h:52` |
| `SECTOR_SIZE` | 512 (1 << 9) | `include/linux/blkdev.h:55` |
| `block_size` | **字节** (u16=4096) | `opts.h:138` — 无 `OPT_SB_FIELD_SECTORS` 标志 |
| `btree_node_size` | 字节(内存)，扇区(磁盘 superblock) | `opts.h:144` — 有 `OPT_SB_FIELD_SECTORS` 标志 |
| `bucket_size` (members) | **扇区** | `members_types.h:14` — 注释"bucket_size: sectors" |
| bkey offset | **扇区** | `bcachefs_format.h:137` — 注释"Btree keys - all units are in sectors" |

**决策**: 不强制全面扇区化。遵循 bcachefs 混合单位策略：

```rust
// types.rs — 中央常量（唯一定义点）
pub const SECTOR_SHIFT: u8 = 9;
pub const SECTOR_SIZE: u64 = 512;
```

- `block_size` → 保持**字节**（与 bcachefs opts.h 一致）
- `btree_node_size` → 内存字节/磁盘扇区（与 bcachefs `OPT_SB_FIELD_SECTORS` 一致）
- 扇区相关常量集中到 `types.rs`
- 各模块在必要时保持本地类型化别名（如 `btree/node.rs` 中 `pub(crate) const SECTOR_SIZE: usize = crate::types::SECTOR_SIZE as usize;`）

**预防的问题**:
- 重复硬编码值不同步（`reservation.rs` 的 8 和 `node.rs` 的 8 值相同但无引用关系）
- 全面扇区化会破坏 superblock 向后兼容（需版本迁移），收益有限

**常量定义位置**:
- `types.rs`: `SECTOR_SHIFT: u8 = 9`, `SECTOR_SIZE: u64 = 512`（中央唯一源）
- `alloc/mod.rs`: `SECTORS_PER_BLOCK: u64 = DEFAULT_BLOCK_SIZE / (crate::types::SECTOR_SIZE as u64)`（与 `DEFAULT_BLOCK_SIZE` 并列）
- `btree/node.rs`: `SECTOR_SIZE: usize = crate::types::SECTOR_SIZE as usize`, `SECTORS_PER_BLOCK: u16 = (BLOCK_SIZE / SECTOR_SIZE) as u16`（本地类型化别名）

---

### PendingRootJournal — 延迟 journal 写机制 (2026-07-09)

**问题**: bcachefs 中 root 变更（split/collapse/increase_depth）不直接写 journal entry，而是由 caller（`btree_update_nodes_written_trans`）在后续 transaction commit 中写 `BCH_JSET_ENTRY_btree_root`。subvol 需要相同的设计：root 变更点不应直接调用 `journal_btree_root()`。

**方案**: `PendingRootJournal` 机制：

```rust
pub(crate) struct PendingRootJournal {
    pub root_addr: u64,
    pub level: u8,
}

// Btree 中存储
pending_root_journal: UnsafeCell<Option<PendingRootJournal>>,
```

- `split_root` / `collapse_root` 写盘节点后**只存 pending**，不写 journal
- caller（`BtreeTrans::commit` Phase 2 或 flush 路径）消费：
  ```rust
  if let Some(info) = btree.take_pending_root_journal() {
      journal.append_btree_root(ty, info.root_addr, info.level, ...).await?;
  }
  ```
- `take_pending_root_journal()` 自动 `Option::take()`，防止重复消费
- 对应 bcachefs 中 `btree_update_nodes_written_trans` 负责写 btree_root journal entry 的设计

**测试模式**: 测试使用 `NoopWriter` + `futures::executor::block_on`，不检查真实 journal。

### Phase 1 Eager Write 与地址不变性 (2026-07-09)

**问题**: `insert_multi` 的 leaf split (Phase 2) 和 `insert_routing_entry_at` 的内部节点分裂中，左右两半都通过 `writer.write_btree_node()` 获取新地址，但父节点的 routing entry 仍指向左节点的**旧地址**，导致地址不匹配 → btree 遍历失败。

**方案**: 分裂后左节点必须保持原始地址不变：

```rust
// ✅ 正确：left stays at original address
self.cache.put_node(node_addr, parent_arc);                // 放回原地址
let right_arc = Arc::new(right_node);
right_arc.set_will_make_reachable();                       // IO 提交前保护
let right_addr = writer.write_btree_node(right_arc.clone(), ...).await?;  // 右半新地址
self.cache.insert(right_addr, right_arc);

// ❌ 错误：both halves get new addresses
let left_addr = writer.write_btree_node(parent_arc.clone(), ...).await?;  // ← 新地址
let right_addr = writer.write_btree_node(right_arc.clone(), ...).await?;
self.cache.insert(left_addr, parent_arc);  // ← 父路由指向旧地址，不匹配
```

**适用范围**:
| 函数 | 左节点处理 |
|------|-----------|
| `insert_multi` Phase 2 leaf split | `put_node(leaf_addr, leaf_arc)` — 原地址 |
| `insert_routing_entry_at` internal split | `put_node(parent_addr, parent_arc)` — 原地址 |
| `insert_routing_entry_at` parent insert (non-split) | `put_node(parent_addr, parent_arc)` — 原地址 |
| `insert_multi`/`delete_multi` non-split path | `put_node(leaf_addr, leaf_arc)` — 原地址 |
| `split_root` | 左右都是新地址（原 root 不在 cache 中，无父路由指向） |
| `collapse_root` | 根折叠，不存在 left/right 分裂 |

**根节点特殊处理**: `split_root` 是例外——原 root 不在 cache 中（存储在 `BtreeRoot.node`），没有外部 routing entry 指向它的地址，所以左右子节点都获得新地址是安全的。

**所有 non-split 路径必须 `put_node` 而非 `writer.write_btree_node`**: 当节点修改后大小仍能容纳（不触发分裂），必须放回原地址，因为其父节点的 routing entry 仍指向原地址。

```rust
// ✅ 正确：non-split insert 保持地址不变
if parent.insert(routing_key, entry) {
    parent.compact();
    self.cache.put_node(parent_addr, parent_arc);  // ← 回原地址！
    return Ok(true);
}
```

### root_lock — 根操作互斥锁 (2026-07-09)

**问题**: `bch2_btree_increase_depth`、`split_root`、`collapse_root` 等函数通过 `unsafe { &mut *btree.root.get() }` 直接修改 `UnsafeCell<BtreeRoot>`，多线程并发时产生 data race。

**方案**: Btree 使用与 bcachefs `btree.cache.root_lock` 对齐的 `root_lock: Mutex<()>`，所有根路径写入前获取锁：

```rust
pub(crate) struct Btree {
    // ...
    root_lock: Mutex<()>,
}

```

**锁覆盖路径**:
| 函数 | 文件 | 行 |
|------|------|----|
| `clear()` | btree.rs | `let _lock = self.root_lock.lock().unwrap()` |
| `load_root()` | btree.rs | `let _lock = self.root_lock.lock().unwrap()` |
| `set_root_internal()` | btree.rs | `let _lock = self.root_lock.lock().unwrap()` |
| `split_root()` | btree.rs | `let _lock = self.root_lock.lock().unwrap()` |
| `collapse_root()` | btree.rs | `let _lock = self.root_lock.lock().unwrap()` |
| `bch2_btree_increase_depth()` | interior.rs | `let _lock = btree.root_lock.lock().unwrap()` |
| `insert_routing_entry_at()` (root path) | btree.rs | 锁内修改→clone→丢锁后 async 写盘→再锁回设 journal |

**行为**:
- 读路径（`root()`、`root_node()`）不走锁（仍通过 `UnsafeCell` 直接读），外部序列化保证安全
- 对应 bcachefs `bch2_btree_set_root_inmem` 中 `scoped_guard(mutex, &c->btree.cache.root_lock)` 的等价设计
- split_root 中需要在锁释放前 drop 早期 `root` 借用（`let _ = root`）以避免编译器抱怨

**关键约束**: `MutexGuard` **不能跨 `.await`** — `std::sync::MutexGuard` 不是 `Send`，跨 `.await` 使 future 非 `Send`，无法在 `tokio::spawn` 中使用。正确模式：锁内修改→clone→丢锁→async 写盘→再锁回设 journal 状态。

**与 bcachefs 的差异**:
| 维度 | bcachefs | subvol |
|------|----------|---------|
| 根保护 | `BTREE_NODE_permanent` 标志防 reclaim | 根在 `BtreeRoot.node` 中，不在 cache 内 |
| 旧根去保护 | `clear_btree_node_permanent(b)` | Arc drop 自动回收 |
| 新根事务可见 | `bch2_trans_node_add(trans, n)` | 已通过 `BtreeRoot.node` 直接可见 |

**已知偏差 — 需后续对齐**:

| 维度 | bcachefs | subvol | 风险等级 |
|------|----------|---------|----------|
| **recalc_btree_reserve** | `bch2_recalc_btree_reserve` 在 `set_root_inmem` 后调用，维护内存预留池防止分裂时 OOM 死锁 | subvol 无内核 MM shrinker，nr_reserve 预留机制不适用。同名函数计算 `should_throttle`（节流标志）— 2026-07-18 归档为架构差异 | 🟢 **低** — 架构差异，subvol 无内核内存压力场景 |
| **fake_root 检查** | `bch2_btree_increase_depth` 先检查 `btree_node_fake(b)`，fake→ split_leaf | 无 fake root 状态，`new()` / `clear()` 直接创建 real root | 🟢 **低** — 架构差异，subvol 不存在根未初始化的中间状态 |
| **roots_b[] packed 数组** | `WRITE_ONCE(roots_b[], pack(b))` — 无锁读优化 | 无 | 🟢 **低** — 性能差异，不影响正确性 |
| **__bch2_btree_node_write re-arm** | write_done 判断 dirty+need_write → 重新触发写入（write_in_flight 持续） | 无 re-arm，每次写独立触发 | 🟢 **低** — Phase 2 补充 |
| **wake_up_bit** | write_done 完成时唤醒 write_in_flight 等待者 | 无 waitqueue，caller spin/yield 轮询 | 🟢 **低** — 性能差异 |
| **btree_update closure 信号** | will_make_reachable bit 0 关联 btree_update→cl，clear 时 closure_put | clear_will_make_reachable 仅清标志，无关闭包信号 | 🟢 **低** — Phase 2 事务链补充 |

**已对齐项**:

| 维度 | bcachefs | subvol | 对齐时间 |
|------|----------|---------|---------|
| **`will_make_reachable` 清理** | 写盘完成后在 `__btree_node_write_done` 中清理 | NoopWriter 同步清理，BtreeWriter 在 IO 回调中清理 | 2026-07-09 |
| **`write_btree_node` 异步提交** | `bch2_btree_node_write` 提交后立即返回，`__btree_node_write_done` 异步完成 | trait 签名 `Arc<BtreeNode>`，IO 不等待完成（fire-and-forget），NoopWriter 兼容 | 2026-07-09 |
| **set→write→clear 顺序** | set（写前）→ submit（IO）→ clear（回调） | 全部 7 处调用点对齐：set→Arc→write_submit→回调中 clear | 2026-07-09 |
| **`__btree_node_write_done` 三步** | will_make_reachable → journal_pin_drop → write_in_flight | `bch2_btree_node_write_done` 严格按三步顺序执行 | 2026-07-09 |
| **journal pin 写前设置** | 写入完成前 journal_pin_add | `BtreeWriter` 在 submit 前设置 pin，write_done 回调中 drop | 2026-07-09 |

## Scenario: journal-pinned btree node 的锁内更新

### 1. Scope / Trigger

- 修改 leaf insert/compact/root split，且 node 已注册 journal pin 或存在只读 `Arc` 时触发。
- 唯一依据：本地 `fs/btree/commit.c:299-365`；node 可写性由 transaction write lock
  保证，journal pin 不改变 node 的可写状态。

### 2. Signatures

- `Btree::insert<W: BtreeNodeWriter>(...) -> Result<bool, StorageError>`
- `Btree::split_root<W: BtreeNodeWriter>(...) -> Result<bool, StorageError>`
- `bch2_btree_add_journal_pin(node: &Arc<BtreeNode>, journal: &Journal, seq: u64)`

### 3. Contracts

- transaction write lock 是 node mutation 的同步条件；`Arc::strong_count == 1` 不是。
- journal pin 只保持日志回收/flush 生命周期，不得导致后续 leaf update 静默失败。
- insert 返回 `false` 时调用方必须进入 split/retry/error 路径，不得把事务报告为成功。

### 4. Validation & Error Matrix

| 条件 | 结果 |
|---|---|
| node 已 journal-pinned，仍持 transaction write lock | 允许继续 insert/compact |
| leaf 空间不足 | 按本地顺序 compact，仍不足则 split |
| split/write 失败 | 错误传播或 transaction restart；不得丢 mapping |
| 未持写锁且存在并发读写 | 禁止取得可变 node 访问 |

### 5. Good/Base/Bad Cases

- Good：第一个 extent 注册 pin 后，第二、第三个 extent 都插入成功并可立即读回。
- Base：无额外 node 引用时，单次 insert 行为不变。
- Bad：用 `Arc::get_mut()` 作为锁条件；第一个 insert 成功，后续 insert 返回 false 且数据块成为孤儿。

### 6. Tests Required

- 回归测试在已注册 journal pin 的 root 上连续插入至少 3 个不同 key，断言 key count
  和逐 key lookup。
- FUSE E2E 必须在 60 秒内完成 `mkfs.xfs`，再挂载并校验目录、文件及内容 hash。

### 7. Wrong vs Correct

```rust
// Wrong: ownership uniqueness 被误当成 btree write lock。
let node = Arc::get_mut(&mut root.node).ok_or_retry()?;

// Correct: 已持 transaction write lock 时更新被锁定的 node；journal pin 可并存。
let node = unsafe { &mut *(Arc::as_ptr(&root.node) as *mut BtreeNode) };
```
| **IO 错误路径** | write_done 设置 noevict + 错误记录 | BtreeWriter 回调 err 路径 io_unlock + error log | 2026-07-09 |

### block_on_safe — 多 runtime 兼容的 async 桥接 (2026-07-09)

**问题**: `Handle::current().block_on()` 在 tokio runtime 之外 panic，而 `futures::executor::block_on` 在 tokio runtime 内部死锁（嵌套阻塞）。

**方案**: `block_on_safe` helper — 先尝试 tokio runtime，无 tokio 时回退：

```rust
fn block_on_safe<F: Future<Output = T>, T>(f: F) -> T {
    match Handle::try_current() {
        Ok(handle) => handle.block_on(f),
        Err(_) => futures::executor::block_on(f),
    }
}
```

**使用位置**: `write_buffer.rs` 中的 flush 路径、任何可能在 tokio runtime 内外两可的上下文。

**注意**: `Handle::current()` 在无 tokio runtime 时 panic——必须用 `try_current()` 版本。**

### Multi-device flush durability (2026-07-16)

本地 bcachefs `fs/journal/journal.c:87` 规定多设备 flush write 对所有设备发出
preflush。卷级 `BlockVolume::flush` 因此必须遍历所有在线 `BchDev`，为每个设备保留
write IO ref 直到 flush 完成，并按设备索引顺序传播首个错误；只刷新 primary 会让
副本写入仍停留在设备缓存中。

### Degraded data writes retain surviving pointers (2026-07-16)

本地 bcachefs `fs/data/write.c:1514-1556` 在副本 IO 出错后调用
`bch2_write_drop_io_error_ptrs()`，只丢弃失败指针；只要仍有 dirty pointer，仍提交
extent。subvol 写入路径必须按提交结果过滤失败副本；多副本配置即使降级为单个成功
指针，也必须使用带设备索引的 raw extent pointer，而不能退化为隐含 primary 设备。

## Code Review Checklist

<!-- What reviewers should check -->

(To be filled by the team)
