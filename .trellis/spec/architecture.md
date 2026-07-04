# 架构规范：bcachefs 式全程基于 btree

> **本文档描述目标架构，而非当前代码状态。**
> 当前代码仍处于从旧架构（CowMapping + checkpoint + WAL）到目标架构的过渡期。
> `volume/mod.rs` 中仍保留 `Volume::open()` / `Volume::read()` / `Volume::write()` 等过渡方法，
> 它们将在后续重构中移除。
>
> 详细实现计划见 `.trellis/tasks/archive/2026-06/06-21-volume-btree-refactor/design.md`

## 核心理念

**一切皆 btree**（对齐 bcachefs）。

整个系统完全建立在 btree 之上，没有更高层抽象。所有数据（元数据、extent 映射、分配状态、快照、子卷）都存储在 btree 中。Volume 不是 I/O 层——它只是一个命名容器。

```
subvolmountd
  ├── BtreeEngine（5 种 Btree 类型）  ← 所有数据的单一入口
  │     └── BlockDevice                ← 持久化层
  │
  ├── Journal                          ← 仅崩溃恢复用
  │     └── BlockDevice                ← 读取 journal entries 进行恢复
  │
  └── NbdServer                        ← NBD 协议层（块 I/O）
        └── BlockDevice                ← 直接读写，不经过 btree

Volume = BtreeEngine + BlockAllocator + meta（仅聚合，非 I/O 层）
```

## 关键设计决策

### 1. 一切皆 btree

没有 CowMapping、没有独立 I/O 层、没有 checkpoint 序列化——所有状态都通过 btree 表达：

| 数据 | Btree 类型 | Key | Value |
|------|-----------|-----|-------|
| 数据块映射 | Extents | bpos(vol_id, lba, snap) | BtreeValue{paddr, ver} |
| 子卷记录 | Subvolumes | bpos(subvol_id, 0, snap) | SubvolumeValue |
| 快照节点 | Snapshots | bpos(snap_id, 0, 0) | SnapshotNode |
| 快照树 | SnapshotTrees | bpos(tree_id, 0, 0) | SnapshotTree |
| 块分配 | Alloc | bpos(bucket_idx, 0, 0) | AllocEntry |

**Volume 没有任何专属的持久状态**——Volume 只是把这 5 种 btree + 分配器的操作聚合在一起。

### 2. Volume 不是 I/O 层

Volume **没有 open/read/write 行为**。

- **块 I/O** 路径：NBD → BlockDevice，不经过 Volume 也不经过 Btree
- **btree 操作** 路径：直接操作 BtreeEngine
- Volume 的角色仅仅是：把 BtreeEngine + BlockAllocator + meta 组合成一个命名实体

```
NBD read/write:
  Client → NBD protocol → NbdExport → BchVol.read_extent()/write_extent()/trim_extent()
  → BlockVolume → BlockDevice

NBD 边界语义:
  - 协议层先校验请求范围，越界请求返回错误而不是短读/短写
  - `READ` 错误不携带数据体，`TRIM` / `FLUSH` 错误不关闭连接

btree 操作（元数据变更）:
  BtreeEngine.get(ty).insert(key, value)
  BtreeEngine.get(ty).get(key)
  BtreeEngine.get(ty).delete(key)

Volume 做的事:
  - Volume::create() → 初始化 5 种空 btree + BlockAllocator + meta
  - Volume::close() → 持久化 BtreeEngine 状态 + Superblock
  - 快照/子卷操作 → 代理到 BtreeEngine.Snapshots / Subvolumes
```

### 3. Journal 主用于 btree 崩溃恢复，也参与 key cache write-back 协调

Journal **的核心职责**是崩溃恢复，**但 key cache write-back 路径通过 journal pin 参与 flush 协调**（这一约束在 Wave 5 的 key-cache-writeback 工作中已放宽）。

#### 3.0 架构约束偏移

原始约束"Journal 不是常规写入路径的一部分"在 key cache 场景下需要修正：

- **崩溃恢复路径**（不变）：Journal 仍是 crash recovery 的唯一来源。正常写入路径为 `btree insert → COW node → BlockDevice`，不经过 Journal。
- **Key cache write-back 路径**（已放宽）：脏 key cache 条目通过 `flush_cache_dirty_keys()` 写回 btree 时，依赖 journal pin 来协调 flush 时序。
  - `bch2_btree_insert_key_cached()` 在存储脏条目时注册 journal pin callback（设 `flush_pending` 标志）
  - `flush_dirty()` 在写回后通过 `drop_journal_pin()` 释放 pin
  - Journal flush 触发 (`bch2_journal_flush_pins`) → 遍历 pin_fifo → 触发回调 → 设置 `flush_pending: true`
  - 同步点检查 `flush_pending` → 调用 `flush_cache_dirty_keys()` → 脏条目批量写回
- **⚠️ flush_cache_dirty_keys 与 cache invalidation 交互**：`bch2_btree_bset_insert_key_wrapper()` 成功插入后总会调用 `key_cache.invalidate(&pos)`（设 `valid=false`）。`flush_cache_dirty_keys` 必须在此之后调用 `key_cache.insert()` 重新将条目以 clean 状态放回缓存。

```
Key cache write-back 路径:
  insert_key_cached (dirty=true) → pin_entry() 注册回调
    → journal_flush_pins → callback 设 flush_pending
      → 同步点: flush_cache_dirty_keys()
        → bch2_btree_bset_insert_key_wrapper() → invalidate(pos) [副作用]
        → mark_clean(pos)
        → insert(pos, entry) ← 必须重插为 clean
```

**决策理由**:
- bcachefs 中 `btree_key_cache` 同样使用 `journal_pin` + `flush_dirty` 机制
- 这是一个同步 flush 模型（非后台线程），在 `insert_entry_raw()`、`flush_dirty_nodes()`、`bch2_trans_commit()` 等同步点调用
- `Weak<Journal>` 防止 KeyCache 阻止 Journal 析构

```
正常写入路径:
  btree insert → COW node → BlockDevice.write_block()

Journal 路径（仅在启动时）:
  启动 → 检查 Journal seq
    → if seq 表明需要恢复:
        JournalReplayer.read_btree_roots()  → 获取 journal 中的 root 指针
        load_root()                          → 加载 btree 根节点
        JournalReplayer.replay_all_to_vol() → 重放未落盘的 BtreeKeys
    → else: 跳过，直接加载 btree
```

#### 3.1 核心数据结构

Journal 使用 bcachefs 对齐的**多 buffer 流水线 + 原子保留状态**：

```text
JournalResState (AtomicU64, 64-bit bitfield)
┌──────────┬──────┬─────────┬─────────┬─────────┬─────────┐
│ offset   │  idx │buf0 cnt │buf1 cnt │buf2 cnt │buf3 cnt │
│ 22bit    │ 2bit │ 10bit   │ 10bit   │ 10bit   │ 10bit   │
└──────────┴──────┴─────────┴─────────┴─────────┴─────────┘
CAS 循环 → 无锁保留（journal_res_get_fast）

buf[0..BUF_NR]（BUF_NR=4）        in_flight FIFO
┌────────────┐                    ┌──────────┐
│ Accepting  │ ← 当前开放 buf     │ idx=0    │
│ Closing    │ ← 等待 refcount    │ idx=1    │
│ WriteDone  │ ← 写入完成         │ idx=2    │
│ Free       │ ← 可复用           │ ...      │
└────────────┘                    └──────────┘
```

**关键类型**：
- `JournalResState`（AtomicU64 位域）：64-bit 原子保留状态，一条 `cmpxchg` 完成保留
- `JournalReservation`：保留结果（seq, offset, u64s, buf_idx）
- `JournalBuf`：per-buffer（state, data, seq, data_end, notify）
- `BufState`：Free → Accepting → Closing → WriteSubmitted → WriteDone

#### 3.2 Fastpath 无锁保留

`journal_res_get_fast()`（对应 bcachefs `journal_res_get_fast()` journal.h:475-518）：
- 仅操作 `AtomicU64`，无锁定，接受 `&self`
- CAS 循环：检查空间 → 检查 refcount 溢出 → 递增 offset + buf_count
- 成功后调用者必须 `commit()` + `journal_res_put()`

`journal_res_put()` 在 refcount 归零且 buf 处于 Closing 状态时自动推进到 WriteSubmitted（对应 bcachefs `__bch2_journal_buf_put_final()` journal.c:240-256）。

#### 3.3 Buffer 生命周期

```
buf[idx] Free
  → __journal_entry_open_one() → buf[idx] Accepting
    → journal_res_get_fast() 成功 → 保留空间（多线程可同时保留）
    → 空间不足 / 显式 close → buf[idx] Closing
      → journal_res_put() refcount 归零 → buf[idx] WriteSubmitted
        → flush() 写入 bucket → buf[idx] WriteDone
          → 回收 → buf[idx] Free
```

#### 3.4 seq 分配策略

**当前实现**：`seq` 按 journal entry 分配。`journal_entry_open()` 打开新 buf 时分配一次 seq，同一 buf 内所有 reservation 共享 `buf.seq`。

**bcachefs 对齐**：与 bcachefs `__journal_entry_open_one()` 的行为一致：`atomic64_inc_return(&j->seq)` 为新建 buf 分配 seq，而不是在每个 reservation 上递增。

#### 3.5 BtreeTransaction 集成

`BtreeTransaction::bch2_trans_commit()` 会将事务累积的 journal 条目通过 fastpath API 写入 Journal：
- 按 BtreeType 分组，每组调用 `Journal::append(&self, ...)`
- `append()` 和 `append_btree_root()` 接受 `&self`（无锁设计）
- 调用者可在 commit 后 `drain_journal()` 消费条目用于 engine 同步

#### 3.6 配置

- `JOURNAL_STATE_BUF_NR = 4`：4 个 buffer
- `BUF_SIZE = 32768`：32KB per buf（4096 u64s）
- `DEFAULT_JOURNAL_BUCKETS = 32`：预分配 bucket 数
- `BUCKET_BLOCKS = 256`：每个 bucket 256 个 block（1MB）

#### 3.7 bcachefs 源码对照

| 概念 | bcachefs 文件:行号 |
|------|-------------------|
| `union journal_res_state` | `fs/journal/types.h:142-174` |
| `struct journal_res` | `fs/journal/types.h:134-140` |
| `journal_res_get_fast()` | `fs/journal/journal.h:475-518` |
| `journal_state_inc()/dec()` | `fs/journal/journal.h` |
| `JOURNAL_STATE_BUF_NR` | `fs/journal/types.h:20-22` |
| `struct journal_buf` | `fs/journal/types.h:37-76` |
| `__journal_entry_open_one()` | `fs/journal/journal.c:391` |
| `__bch2_journal_buf_put_final()` | `fs/journal/journal.c:240-256` |
| `ring[seq & mask]` | `fs/journal/types.h:293` |

### 4. BtreeEngine 管理 5 种独立 Btree 实例

每种 BtreeType 的 btree 拥有独立根节点和 key 空间，共享同一个并发模型。

```rust
BtreeEngine { trees: [Btree; 5] }
```

BtreeEngine::recover_from_journal() 是对齐 bcachefs 的恢复入口：
1. 合并 superblock root_addrs + journal BtreeRoot 覆盖
2. load_root 加载根节点
3. replay_all_to_vol 重放 BtreeKeys

### 5. 并发控制

SixLock/WaitFifo/DeadlockDetector 已完全对齐 bcachefs：

- **SixLock**: 6 状态锁（read/intent/write + percpu reader count）
- **WaitFifo**: URCU 保护的无锁等待队列
- **DeadlockDetector**: per-thread DFS 栈（8 帧深度），无全局竞争
- **WRITE_BIT preset**: 写者优先，消除写锁饥饿

### 6. 模块文件映射（bcachefs 语义对齐）

```
crates/subvol-core/src/
├── volume/mod.rs         — Volume 容器（bcachefs: fs.c/h, bch2_fs_*）— 聚合 BtreeEngine + BlockAllocator + meta
├── alloc/                — 分配器（bcachefs: alloc_background.c/h, alloc_foreground.c/h, buckets.c/h, bch2_alloc_*）
├── btree/                — COW BTree 全部子模块（bcachefs: btree/*.c/h）
│   ├── mod.rs            — BtreeEngine（5 实例持有者）+ BtreeType（bcachefs: btree.h, btree_id）
│   ├── btree.rs          — Btree 主结构：insert/delete/get/查找 入口（bcachefs: btree.h, bch2_btree_*）
│   ├── bucket_io.rs      — bucket 级 I/O（bcachefs: io.c/h, bch2_btree_io_*）
│   ├── cache.rs          — 节点缓存 LRU + cannibalize + throttle（bcachefs: cache.c/h, bch2_btree_cache_*）
│   ├── gc.rs             — GC mark-and-sweep + 拓扑检查（bcachefs: gc.c/h, bch2_gc_*）
│   ├── interior.rs       — 内部节点 split/merge/increase_depth/set_root（bcachefs: interior.c/h, bch2_btree_interior_*）
│   ├── io.rs             — 节点 read/write/flush/validate（bcachefs: write.c/h, read.c/h, bch2_btree_node_*）
│   ├── iter.rs           — btree_iter: peek/next/prev/skip（bcachefs: iter.c/h, bch2_btree_iter_*）
│   ├── key.rs            — Bpos/BtreeKey/BchVal 类型 + 比较 + 排序（bcachefs: bkey.c/h, bkey_types.h, bch2_bkey_*）
│   ├── key_cache.rs      — key cache: find/drop/insert_key_cached/flush_dirty（bcachefs: key_cache.c/h, bch2_btree_key_cache_*）
│   ├── node.rs           — BtreeNode + bset 操作 + node_iter（bcachefs: bset.c/h, bch2_bset_*）
│   ├── node_scan.rs      — 扫描 btree 节点（bcachefs: node_scan.c/h, bch2_btree_node_scan_*）
│   ├── op.rs             — 操作类型/标志定义（bcachefs: btree.h, btree_update_flags）
│   ├── search.rs         — btree 路径搜索（bcachefs: btree.h, bch2_btree_path_*）
│   ├── snapshot.rs       — 快照树类型（bcachefs: snapshots.c/h snapshot_tree, bch2_snapshot_tree_*）
│   ├── transaction.rs    — BtreeTrans + lockrestart + trans_commit（bcachefs: commit.c/h, bch2_trans_*）
│   ├── types.rs          — BtreePathLevel / BtreeRoot / NodePtr（bcachefs: types.h）
│   ├── update.rs         — btree 内部更新 + 写状态机（bcachefs: update.c/h, bch2_btree_update_*）
│   └── write_buffer.rs   — write buffer flush + journal keys 批处理（bcachefs: write_buffer.c/h, bch2_btree_write_buffer_*）
├── journal/              — WAL 流水线（bcachefs: journal*.c/h, bch2_journal_*）
│   ├── mod.rs            — Journal 模块入口
│   ├── types.rs          — JournalResState / JournalReservation / JournalBuf / BufState / PinFifo
│   ├── jset.rs           — Jset / JsetEntry 序列化（bcachefs: journal_io.c）
│   └── replay.rs         — JournalReplayer 仅崩溃恢复用（bcachefs: journal_replay.c）
├── lock/                 — 并发锁（bcachefs: six.c/h, six_lock_*）
│   ├── mod.rs            — 锁模块入口
│   ├── six.rs            — SixLock（Read/Intent/Write + percpu reader count）
│   ├── wait_fifo.rs      — WaitFifo URCU 等侍队列
│   └── deadlock.rs       — DeadlockDetector per-thread DFS
├── snap/                 — 快照 skip_list（bcachefs: snapshots.c/h, bch2_snapshot_*）
├── subvol/               — 子卷创建/删除/快照（bcachefs: subvolume.c/h, bch2_subvolume_*）
├── recovery/             — 有序恢复 pass + journal replay（bcachefs: recovery.c/h, bch2_fs_recovery）
├── storage/              — 后端存储抽象（block device / superblock / nfs）
└── meta/                 — 卷元数据

crates/subvol-nbd/src/
├── export.rs             — NbdExport（直接持有 BchVol，按 snapshot_id 路由）
├── server.rs             — NBD 协议服务
└── ...

crates/subvolmountd/src/
├── volume.rs             — VolumeManager（管理 Volume 生命周期）
├── server.rs             — HTTP REST API
└── ...
```

## bcachefs 对齐对照

> bcachefs 概念映射基于项目代码推导及 bcachefs 内核源码对照。
> 参考实现位于 `/home/black/Documents/bcachefs-tools`（bcachefs-tools 用户态工具 + 内核 shim 层；核心源码在 `fs/` 子目录）。

### 核心子系统

| bcachefs 概念 | bcachefs 参考文件 | subvol 对应 | 职责 | 状态 |
|--------------|------------------|--------------|------|------|
| btree (btree_id) | `btree.h` | `BtreeType` (5 种: Extents/Alloc/Snapshots/Subvolumes/SnapshotTrees) | btree 类型标识 | 已实现 |
| btree_node | `btree/types.h` `struct btree_node` | `BtreeNode` | btree 节点：bset 头部 + 数据 + 前驱后继指针 | 已实现 |
| bset / bset_tree | `bset.c/h` | `node.rs` | bset 操作：插入/删除/合并/分裂/排序/搜索 | 已实现 |
| btree_key_cache | `key_cache.c/h` `struct bkey_cached` | `key_cache.rs` `CachedEntry` + `KeyCache` | key 级缓存：读加速 + 脏写回 + journal pin | ✅ 已验证 (Batch D) |
| bch2_btree_insert_key_cached | `key_cache.c:843-885` | `key_cache.rs::bch2_btree_insert_key_cached()` | 向 key cache 插入脏条目 + 注册 journal pin | ✅ 已验证 (Batch D) |
| btree_key_cache_flush | `key_cache.c:708-740` `bch2_btree_key_cache_journal_flush` | `key_cache.rs::flush_dirty()` + `btree.rs::flush_cache_dirty_keys()` | 脏 key 写回 btree + 清理 dirty/pin | ✅ 已验证 (Batch D) |
| btree_node_cache | `cache.c/h` `struct btree_cache` | `cache.rs` `BtreeNodeCache` | 节点级缓存：LRU + shrink + cannibalize + throttle | 已实现 |
| btree_update | `update.c/h` `struct btree_update` | `update.rs` `BtreeInteriorUpdate` | btree 内部更新状态机（同步简化版） | 已实现 |
| btree_interior_updates | `interior.c/h` | `interior.rs` | btree split/merge/set_root/increase_depth | 已实现 |
| btree_trans / bch2_trans_* | `commit.c/h` | `transaction.rs` `BtreeTransaction` | btree 事务：重组 + lockrestart + trans_commit | 已实现 |
| btree_iter / bch2_btree_iter_* | `iter.c/h` `struct btree_iter` | `iter.rs` `BtreeIter` | btree 迭代器：peek/next/prev/skip_to_next_leaf | 已实现 |
| btree_path | `btree.h` `struct btree_path` | `search.rs` | btree 路径搜索 + 栈式路径缓存 | 已实现 |
| bkey / bch2_bkey_* | `bkey.c/h` `bkey_types.h` | `key.rs` `BtreeKey` `BtreeValue` | btree key/value 类型 + 比较 + 排序 + pack/unpack | 已实现 |
| bch2_btree_node_read/write | `read.c/h` `write.c/h` | `io.rs` | 节点磁盘 I/O：read_block/write_block/flush/validate | 已实现 |
| bucket_io (btree) | `io.c/h` | `bucket_io.rs` | bucket 级 I/O：为 btree 节点分配/释放/读写 bucket | 已实现 |
| node_scan | `node_scan.c/h` | `node_scan.rs` | 恢复/校验时扫描 btree 节点 | 已实现 |
| write_buffer / bch2_btree_write_buffer_* | `write_buffer.c/h` | `write_buffer.rs` | journal keys 批处理 + 后台 flush | 已实现 |
| journal | `journal.c/h` `struct journal` | `journal/` | WAL 流水线：多 buffer + 原子保留 + flush | 已对齐 (P1) |
| jset / journal_entry | `journal_io.c` `struct jset` | `jset.rs` `Jset` | journal entry 序列化 + CRC + 版本 | 已实现 |
| journal replay | `journal_replay.c` | `replay.rs` `JournalReplayer` | 崩溃恢复：读 journal → 重放 BtreeKeys | 已实现 |
| journal pin | `journal.h` `struct journal_entry_pin` | `PinFifo` + `JournalEntryPin` | pin 机制：阻止 journal 回收，触发 flush callback | 已对齐 |
| six_lock | `six.c/h` | `lock/six.rs` `SixLock` | 6 状态锁 Read/Intent/Write + percpu reader | 已对齐 |
| deadlock detection | `six.c` | `lock/deadlock.rs` `DeadlockDetector` | per-thread DFS 栈，8 帧深度，无全局竞争 | 已对齐 |
| wait fifo (URCU) | `six.c` | `lock/wait_fifo.rs` `WaitFifo` | 无锁等待队列 + handoff 逐个唤醒 | 已对齐 |
| alloc_background | `alloc_background.c/h` | `alloc/` | bucket 分配后台：reserve + GC + discard | 已对齐 |
| alloc_foreground | `alloc_foreground.c/h` | `alloc/foreground.rs` | bucket 分配前台：open_bucket + 写点 + 尝试/回退 | 已对齐 |
| buckets | `buckets.c/h` | `alloc/buckets.rs` | bucket 管理：状态机 + sector 计数 + 生命周期 | 已对齐 |
| snapshot | `snapshots.c/h` `struct snapshot_t` | `snap/` | 快照 skip_list + 祖先缓存 + DAG 管理 | 已验证 |
| subvolume | `subvolume.c/h` | `subvol/` | 子卷：创建/删除/快照 + subvol_ino_map | 已验证 |
| recovery passes | `recovery.c/h` `recovery_passes.h` | `recovery/` | 有序恢复 pass：12 pass + deps 强制执行 | 已验证 |
| bch2_fs_* (btree engine) | `fs.c/h` `struct bch_fs` | `volume/` | Volume 容器 + BtreeEngine + BlockAllocator | 过渡中 |
| superblock | `sb/` 目录（`sb/members.c`, `sb/clean.c` 等） | `storage/superblock.rs` | 磁盘超级块：扁平字段（vol_name/block_size/capacity 为 BchSb 顶层，无嵌套 vol_meta） | ✅ 已扁平化 (2026-07-04) |
| trigger dispatch | `btree/commit.c` + `bkey_methods.h` | `transaction.rs` | 按 bkey update 执行 transactional/atomic trigger | 已实现 |
| trigger extent | `alloc_background.c` | `alloc/mod.rs` `bch2_trigger_extent()` | alloc extent trigger：BucketDiff 推导 | 已验证 |
| bch2_nr_btree_keys_need_flush / _must_wait / _wait_done | `key_cache.c:900-910` | `key_cache.rs::{bch2_nr_btree_keys_need_flush, bch2_btree_key_cache_must_wait, bch2_btree_key_cache_wait_done}` | 按 `nr_dirty` / `nr_keys` 阈值公式驱动 flush 触发与等待 | ✅ 已验证 (Batch D / 本轮更新) |

### 7. Superblock 扁平化（无独立 VolumeMeta）

**决策**：`VolumeMeta` 已完全移除。`BchSb` 直接承载卷的扁平字段，对齐 bcachefs `struct bch_sb`。

| bcachefs 字段 | bcachefs 位置 | subvol 字段 |
|--------------|--------------|--------------|
| `sb.label[]` | `bcachefs_format.h:1176` | `BchSb.vol_name` |
| `sb.block_size` | `bcachefs_format.h:1182` | `BchSb.block_size` |
| `sb.dev_uuid` | `bcachefs_format.h:1187` | `BchSb.uuid` |

**关键语义**：
- 不存在独立的 `VolumeMeta` 运行时结构；卷名、块大小、容量、后端类型等字段直接从 `BchSb` 或 `BchVol` 访问
- `BchSubvolume` 不携带 `name` 字段 — bcachefs `struct bch_subvolume` 无 name；label 属于 superblock
- `StorageService.superblock()` 返回 `&BchSb`；`BchVol` 提供卷属性访问器，避免再引入元数据包装层
