# BchVol — 统一资源上下文覆盖地图

> 生成日期: 2026-07-17
> 源文件: `crates/subvol-core/src/bch_vol.rs`
> 参考实现: bcachefs `fs/init/fs.c` + `fs/init/fs.h`
>
> 2026-07-17: `bch2_read`/`bch2_write` API 签名完全对齐 bcachefs；
> 删除 5 个非 bcachefs 函数（read_extent_with_snapshot, write_extent_to_key, read_bytes,
> write_bytes, trim_range_for_snapshot）；移除 `BlockVolume` trait；
> 新增 `bch2_btree_delete_range` 对齐 `fs/btree/update.h:262`。

## 覆盖统计

| 状态 | 数量 | 说明 |
|------|------|------|
| ✅ | 26 | 完全对齐 |
| ⚠️ | 1 | 已知偏差 |
| ❓ | 0 | 未验证 |
| ➖ | 11 | subvolmount 特有 |
| **总计** | **37** | |

## 函数状态表

### 生命周期（11）

| 函数 | bcachefs 对应 | 参考 | 状态 |
|------|---------------|------|------|
| `alloc` | `bch2_fs_alloc` | `fs.c:1425-1440` | ✅ |
| `start` | `bch2_fs_start` | `fs.c:1565` | ✅ |
| `open_with_backend` | `bch2_fs_open` | `fs.c:1580` | ✅ |
| `format` | `bch2_format`（CLI） | `format.c` | ✅ |
| `create` | —（subvolmount 特有：format + open_with_backend） | — | ➖ |
| `open_backend` | —（subvolmount 特有：启动时打开已有后端） | — | ➖ |
| `set_read_write` | `bch2_fs_read_write`（骨架，后台线程后续） | `fs.c:647` | ➖ | subvol 无 copygc/rebalance；已对齐 RW 生效顺序（先置 RW，再启动后台工作） |
| `set_read_only` | `bch2_fs_read_only`（骨架，GoingRo→drain→checkpoint 已对齐） | `fs.c:415` | ➖ | Phase 1-2 已完成：GoingRo 写引用追踪、write buffer flush；新增 bcachefs 非 RW 早退分支（仅停 journal reclaim） |
| `checkpoint` | `__bch2_fs_read_only`（flush 循环，2+ clean pass） | `fs.c:317` | ➖ | 已实现 flush 循环 + interior updates flush + write buffer flush（Phase 2） |
| `close` | `bch2_fs_stop` | `fs.c:738` | ✅ | 含 GoingRo→drain→flush_all_reads→checkpoint + journal 后台停止 + backend.flush 完整序列 |
| `delete` | —（subvolmount 特有：直接删卷目录，不再重开或 checkpoint） | — | ➖ |
| `create_backend` | —（subvolmount 特有：pub 后端工厂） | — | ➖ |

### 状态机（3）

| 函数 | bcachefs 对应 | 参考 | 状态 |
|------|---------------|------|------|
| `state` | — | 内联 atomic load | ➖ |
| `is_rw` | `test_bit(BCH_FS_rw, &c->flags)` | `fs.c:417` | ✅ |
| `set_error` | `set_bit(BCH_FS_error, &c->flags)` | `fs.c:785` | ✅ |

### 恢复跟踪（2）

| 函数 | bcachefs 对应 | 参考 | 状态 |
|------|---------------|------|------|
| `recovery_progress` | `c->recovery.pass_done` / `passes_complete` | `recovery.h` | ✅ |
| `set_recovery_progress` | 同上（写版本） | `recovery.h` | ✅ |

### 错误计数（4）

| 函数 | bcachefs 对应 | 参考 | 状态 |
|------|---------------|------|------|
| `record_error` | `bch2_fs_errors` 累积 | `fs.c:1358` | ✅ |
| `error_count` | — | getter | ➖ |
| `record_fsck_error` | `bch2_fsck_errs` | `fs.c:686` | ✅ |
| `fsck_error_count` | — | getter | ➖ |

### 快照操作（5）

| 函数 | bcachefs 对应 | 状态 |
|------|---------------|------|
| `create_snapshot` | 委托 `bch2_snapshot_node_create` | ✅ |
| `list_snapshots` | `bch2_snapshots_read` | ✅ |
| `rollback` | —（subvolmount 封装） | ➖ |
| `delete_snapshot` | 委托 `bch2_snapshot_node_set_deleted` | ✅ |
| `clone_snapshot` | 委托 `bch2_subvolume_snapshot` | ✅ |

### 快照运行时（1）

| 函数/方法 | bcachefs 对应 | 状态 |
|-----------|---------------|------|
| `install_snapshot_runtime` / `snapshot_runtime_*` | `c->snapshots.table` 视图更新 | ✅ |

> 说明：任何会改写 SnapshotT 可见字段的路径，除了写回 Snapshots btree，还必须同步刷新共享 `SnapshotRuntime`，否则后续 `bch2_snapshot_read_value()` 仍可能命中旧值。`bch2_subvolume_create()` 的 root snapshot 绑定和 `check_snapshots` 的修复回写都属于这一类。

### 子卷操作（3）

| 函数 | bcachefs 对应 | 状态 |
|------|---------------|------|
| `create_subvol` | 委托 `bch2_subvolume_create` | ✅ |
| `delete_subvol` | 委托 `bch2_subvolume_delete` + reparent | ✅ |
| `list_subvols` | 委托 `bch2_subvolume_list` | ✅ |

### Btree 元数据操作（3）

| 函数 | bcachefs 对应 | 状态 |
|------|---------------|------|
| `btree_insert` | `bch2_btree_insert` | ✅ |
| `btree_insert_with_journal` | `bch2_trans_commit`（带 journal） | ✅ |
| `btree_get` | `bch2_btree_iter_peek` | ✅ |

### Extent 读写/删除（4）

| 函数 | bcachefs 对应 | 参考 | 状态 |
|------|---------------|------|------|
| `bch2_read` | `bch2_read` | `fs/data/read.c:1691-1885` | ✅ | 公开入口，API 签名完全对齐：含 trans/rbio/iter/inum/failed/prev_read/flags 7 参数，对应 bcachefs `int bch2_read(struct btree_trans *, struct bch_read_bio *, struct bvec_iter, subvol_inum, struct bch_io_failures *, struct bkey_buf *, enum bch_read_flags)` |
| `bch2_write` | `CLOSURE_CALLBACK(bch2_write)` | `fs/data/write.c:2919-2988` | ✅ | 入口校验：对齐检查 + RO 检查 + write ref + dispatch → `__bch2_write` |
| `__bch2_write` | `__bch2_write` | `fs/data/write.c:2703-2838` | ✅ | 主写循环：分配扇区 → write extent → submit bio → 索引更新 → sync/async 完成 |
| `bch2_btree_delete_range` | `bch2_btree_delete_range` | `fs/btree/update.h:262` | ✅ | 新增，对应 bcachefs `int bch2_btree_delete_range(struct bch_fs *, enum btree_id, struct bpos, struct bpos, unsigned, struct bch_io_failures *)` |

### 2026-07-18 subvol → snapshot API 路由复核

- 本地 `fs/data/read.c:1733-1739` 的 `bch2_read()` 使用 `inum.subvol` 调用 `bch2_subvolume_get_snapshot()`；subvol 的 `BchVol::bch2_read()` 现在保持同一语义，不再把 `SubvolInum.subvol` 误当 snapshot ID。
- 本地 `fs/data/write.c:1122-1149`、`2476` 的写路径使用 `op->subvol` 解析目标 snapshot 后再构造 extent key；subvol 的 `BchVol::bch2_write()` 现在在入口完成同样的解析。
- FUSE 与 NBD root 路径传递 `BCACHEFS_ROOT_SUBVOL`；指定子卷 NBD 导出传递子卷 ID。`root_snapshot_id` 仅作为运行时当前 root snapshot 状态，不再作为底层 `SubvolInum.subvol` 或 `BchWriteOp.subvol`。
- FUSE/NBD 写请求的 `nr_replicas` 现在从持久化的 `data_replicas` 卷选项传递， 对齐本地 `fusemount.rs:815-816` 从 inode 选项取副本数、再进入 `write.c:2736-2747` 分配副本的控制流；不再在协议边界硬编码单副本。
- FUSE 会话配置保持本地 `fusemount.rs:1026-1027` 的 `Config::default()`，不擅自设置
  `n_threads`/`clone_fd`；FUSE worker 调度边界由 bcachefs 同一版本的 fuser 默认值决定，
  `VolFuseFs` 内部 Tokio runtime 仍负责每个请求的核心异步 I/O。
- FUSE 错误映射保留设备故障类别：`no writable extent device` 映射 `ENOSPC`，`no online extent replica` 映射 `EIO`，只有真正的路径/对象缺失才映射 `ENOENT`；这保持本地 FUSE `bch_err()` 直接传播 bcachefs 错误码的语义。
- `BchVol::flush()` 先调用本地对应的 `bch2_journal_flush()` (`fs/journal/journal.c:1255`)，再对每个在线成员执行 backend flush；FUSE `flush/fsync` 与 NBD `FLUSH/FUA` 因而同时覆盖 journal entry 和设备缓存。
- NBD TRIM 与读写一样先将请求携带的 subvol ID 解析为当前 snapshot ID，再调用 `bch2_btree_delete_range`；trim-hole 记录和后续读路径因此使用同一 snapshot 命名空间，删除后稳定返回零。
- `bch2_btree_delete_range` 的范围扫描现在先取得 volume write reference，再使用 intent
  iterator；对应本地 `fs/btree/update.c:782-830` 的事务入口顺序，避免只读切换在扫描后、
  提交前插入导致 trim 更新越过写生命周期。

### 2026-07-18 读 API 的 transaction/iterator 生命周期约束

- 本地 `fs/btree/iter.h:680-703` 的 `bch2_btree_iter_set_pos()` 不只是写入
  `iter->pos`：它先通过所属 `btree_trans` 释放 `update_path`，再按
  `BTREE_ITER_all_snapshots` 决定是否覆盖新位置的 snapshot，最后重置当前 key。
- subvol 的 `BtreeIter` 由 `BtreeTrans` 所有；`BtreeTrans::bch2_btree_iter_set_pos()`
  现在在 transaction 内执行 `update_path` 释放、snapshot 注入和 root → leaf
  重定位，`bch2_read` 每个 extent 都按本地顺序重新执行
  `bch2_trans_begin`、`bch2_subvolume_get_snapshot`、`bch2_btree_iter_set_pos`。
- 异步设备 IO 仍在 transaction 释放后执行；`failed` 现在按设备记录 I/O/
  checksum 失败，并在同一 extent 的后续 retry 中跳过失败副本，extent key
  变化时清空历史，保持本地 `bch2_mark_io_failure()` 与
  `bch2_bkey_pick_read_device()` 的控制流。
- `BkeyBuf` 现在同时保存完整 key/value；retry 路径按本地
  `bkey_and_val_eq()` 比较 inode、vaddr、size、snapshot、type、version 及
  value，而不是只比较逻辑位置字段。FUSE/NBD 的 `BkeyBuf` 初始化也统一使用
  该完整 API。
- 新写入 extent 已使用 CRC32C：checksum 保存在 btree value，写入时覆盖完整未压缩
  extent，读取时先读完整原始 extent、校验后再复制请求分片；CRC 失败按本地
  `fs/data/read.c:905-1065` 的副本重试路径记录并跳过失败设备。覆盖/trim 分片保留
  原始 extent 长度及分片偏移，符合本地 `fs/data/extents_format.h` 对 partial extent
  checksum 原始边界的要求。当前格式为新格式，不兼容旧数据。
- CRC presence 不由 checksum 数值是否为零推断；新格式使用原始 extent 长度元数据标记
  checksum 存在，因此合法的零值 CRC32C 也会进入完整 extent 校验路径。
- FUSE 读入口已透传 `RETRY_IF_STALE | MAY_PROMOTE | USER_MAPPED`；CRC 校验读取使用
  临时 extent buffer，等价覆盖 `MUST_BOUNCE` 的用户映射安全路径。poisoned extent
  持久化标记使用新 CRC 元数据的保留位：对齐本地 `fs/data/read.c:541-590`
  的当前 key/value 再检查后提交；后续读取在设备选择前按
  `fs/data/read.c:1369-1392` 返回 `extent_poisoned`，显式
  `NO_POISON_CHECK` 才绕过该检查。副本重试全部 checksum 失败是异步路径的 retry
  边界；读写、split、trim 均保留原始 extent 长度与分片偏移。文件系统侧 reflink/indirect
  extent 不属于当前 FUSE 块设备导出范围，外部 XFS 负责文件系统语义，subvol 只负责
  volume/subvol 的块读写与一致性。
- FUSE 导出现在支持显式 `subvol` ID，默认使用 `BCACHEFS_ROOT_SUBVOL`；导出的普通文件
  是给外部 XFS 格式化和挂载的块设备语义，读写路径始终使用同一个 subvol。写入前按本地
  `fs/snapshots/subvolume.c:323-329` 的 `bch2_subvol_is_ro()` 检查只读/已解除链接子卷，
  并向 FUSE 返回 `EROFS`。
- FUSE `fallocate(PUNCH_HOLE|KEEP_SIZE)` 现在按本地 `fs/data/io_misc.c:158-191`
  的 `bch2_fpunch()` 语义转换为指定 subvol/snapshot 的 extent 范围删除；因此 XFS/loop
  发出的 discard 不再退化为 `ENOSYS`，而是复用 NBD 与 core 的 trim 路径。
- FUSE punch-hole 的块范围必须是
  `round_up(offset, block_size)..round_down(offset + length, block_size)`，与本地
  `bchfs_fpunch()` 的 `block_start`/`block_end` 一致；没有完整块覆盖时成功返回，不删除
  边缘块。该导出没有 inode/page-cache 层，不能承担 bcachefs 对部分边缘页的处理。
- FUSE punch-hole 必须在长度/块范围提前返回前先检查 subvol 的 RO/UNLINKED 状态，保持
  本地 `bch2_fallocate_dispatch()` 先取得写生命周期、再进入 `bchfs_fpunch()` 的错误顺序；
  只读 subvol 不能因请求未覆盖完整块而伪装成成功。
- FUSE 指定非 root subvol 时，导出文件容量读取该子卷的 `size` 扩展字段；root 或旧记录
  的 `size=0` 回退到整卷容量，避免 XFS 访问超出目标子卷的逻辑范围。
- FUSE 导出的只读 volume/subvol 文件属性必须使用 `0444`；其 `open()` 对
  `O_WRONLY/O_RDWR` 提前返回 `EROFS`，并继续由写入路径按本地 `bch2_subvol_is_ro()`
  复核 RO/UNLINKED 状态。
- CLI 在创建 FUSE 导出前必须按本地 `bch2_subvolume_get(..., true)` 解析显式 subvol；
  不存在或损坏时挂载直接失败，不能以整卷容量回退后再延迟到首次 I/O 才报错。
- FUSE 非对齐读写向 block-aligned 范围扩展后，传入 core `BvecIter.bi_size` 前必须做
  checked `u32` 转换；不能让超过 bvec 宽度的请求截断回绕为零长度。
- FUSE `statfs` 的可用块数必须按本地 `fusemount.rs:924-944` 使用
  `usage.capacity - usage.used` 计算，不能改用不同语义的 `usage.free`；总量、可用量和
  reserved 可用量保持同一 bcachefs 返回顺序。
- NBD TRIM 的 block rounding 与 btree end position 必须使用 checked addition；极限范围
  溢出时返回 `NBD_EINVAL`，不能回绕后提交错误的 extent 删除事务。
- FUSE CLI 卸载探测必须按 `/proc/mounts` 的 `fs_spec == FSName` 与
  `fs_vfstype == fuse.subvol` 精确匹配，并解码 mountpoint 转义；不能用卷名 substring
  匹配导致误卸载相似名称的其他导出；`fs_spec` 本身的 `\\040/\\011/\\134` 转义也必须
  解码后再比较。
- NBD `new_with_subvol` 不再无条件设置只读；能力位和 WRITE/TRIM 拒绝逻辑统一依据本地
  `bch2_subvol_is_ro()` 的 RO/UNLINKED 状态，普通可写子卷保留 FUA/TRIM，快照子卷仍动态
  宣布为只读。
- NBD `new_with_subvol` 的 `size()` 必须与 FUSE 使用相同的 subvol `size` 扩展字段；
  `size=0` 的 root/旧记录回退整卷容量，不能让 NBD 客户端越过目标 subvol 的导出边界。
- 禁止新增只修改 `iter.pos` 的伪 `bch2_btree_iter_set_pos`；任何后续变更必须保留
  `update_path` 释放、snapshot 注入、path 重遍历和错误重试顺序。

### Trim-hole concurrency

`trim_holes` is runtime read-zero metadata used by NBD reads. Because NBD
allows concurrent READ requests while a WRITE/TRIM holds the mutating boundary,
the map must use an internal `RwLock`; an `UnsafeCell<HashMap>` would permit a
concurrent read/write data race even though bcachefs itself protects its
equivalent extent/discard state internally.

### 统计（1）

| 函数 | bcachefs 对应 | 状态 |
|------|---------------|------|
| `stats` | 间接：从各子系统聚合 | ✅ |

## 偏差说明

### Superblock backup selection (2026-07-17)

`BchSb::read_from_device()` now reads every current-format superblock slot and
selects the newest valid copy instead of returning the first valid slot. This
matches local bcachefs `read_backup_supers()` (`fs/sb/io.c:917-967`), which must
survive a stale-but-readable primary after a torn or reordered write. The
subvol format uses the primary member's `seq`, then `journal_last_seq` and
`journal_seq` as compatibility fallbacks because it has no separate top-level
superblock sequence field.

`BchSb::write_to_device()` likewise attempts every slot even after one write
fails, matching the per-slot loop in local `__bch2_write_super()`
(`fs/sb/io.c:1390-1430`). It returns the first error only after all backup
locations have had a chance to become durable.

| 函数 | 类型 | 说明 |
|------|------|------|
| `format` | ✅ 已对齐 | 写 superblock 到 backend，不启动；对应 bcachefs CLI `bch2_format` |
| `open_with_backend` | ✅ 已对齐 | 从 backend superblock 恢复全部 meta；对应 `bch2_fs_open` |
| `create` | ➖ subvolmount 特有 | bcachefs 无 `create`；subvol 中 = `create_backend` + `format` + `open_with_backend`，目录创建改为原子 `create_dir` |
| `open_backend` | ➖ subvolmount 特有 | 启动时打开已有后端，不做目录探测；`init_volume` 使用它直接按配置加载 |
| `init_volume` | ➖ subvolmount 特有 | 启动加载必须命中 `config.volumes`，不再对缺失配置做 backend 默认回退 |
| `load_or_default` / CLI config bootstrap | ➖ 控制面特有 | 启动时直接尝试读取配置文件；缺失时回退默认配置，其他加载错误直接失败，不先 `exists()` 探测 |
| `delete` | ➖ subvolmount 特有 | bcachefs 无 `delete`；subvol 仅做卷目录删除，不再重开 backend 或 checkpoint |
| `create_backend` | ➖ subvolmount 特有 | pub 后端工厂，供 daemon 使用 |
| `rollback` | ➖ subvolmount 特有 | bcachefs 无直接 `rollback` API |
| `set_read_write/set_read_only` | ➖ 骨架 | journal 后台线程已启停；copygc/rebalance/reconcile 为 bcachefs 多线程架构特有，subvolmount 模型无对应 |
| `checkpoint` | ➖ 简化 | 有 flush 循环（2+ clean pass）+ interior updates flush + write buffer flush 已接入；journal append 待完成 |
## bcachefs 对齐设计决策

### 状态模型差异：bcachefs 位标志 vs subvolmount 枚举（2026-07-05）

**bcachefs 使用正交位标志**（`BCH_FS_*` in `bcachefs.h:1022`）：

```c
enum bch_fs_flags {
    BCH_FS_STARTED,         // 文件系统已启动
    BCH_FS_RW,               // 可读写
    BCH_FS_WAS_RW,           // 曾为可读写（恢复用）
    BCH_FS_FSCK_DONE,        // fsck 完成
    BCH_FS_NEED_DELETE_PASS, // 需删除 pass
    BCH_FS_NEED_RECALC,       // 需重新计算
    BCH_FS_ERROR,             // 错误标志
    BCH_FS_FIXED_OFFLINE,    // 离线修复
    BCH_FS_STOPPING,         // 正在停止
    BCH_FS_EMERGENCY_RO,     // 紧急只读
    // ...
};
```

位标志设计特点：
- **可同时设置多个状态**：如 `BCH_FS_RW | BCH_FS_ERROR` 共存
- **set_bit/test_bit 原子操作**：无锁并发读/写状态
- **没有"生命周期顺序"强制**：全凭调用链编排

**subvol 使用互斥枚举**（`VolumeState` in `bch_vol.rs`）：

```rust
pub enum VolumeState {
    New = 0,
    Starting = 1,
    Rw = 2,
    ReadOnly = 3,
    Stopping = 4,
    Stopped = 5,
    FsError = 6,
    Recovery = 7,
    GoingRo = 8,      // Phase 1 新增，对应 BCH_FS_going_ro
    EmergencyRo = 9,  // Phase 1 新增，对应 BCH_FS_emergency_ro
}
```

枚举设计特点：
- **互斥状态**：任一时刻只有一个状态
- **`AtomicU8` 存储**：`load(Ordering::Acquire)` / `store(Ordering::Release)`
- **明确生命周期管线**：`New → Starting → Rw → Stopping → GoingRo → Stopped` 等
- **状态转换校验**：`try_transition()` 在非法转换时返回错误

**设计理由**：

| 差异 | bcachefs 位标志 | subvolmount 枚举 |
|------|---------------|--------------|
| 状态组合 | 支持多标志同时设置 | 单值互斥 |
| 检查方式 | `test_bit(flag, &c->flags)` | `match state.load() { ... }` |
| 转换控制 | 隐式：调用方自己编排原子操作 | 显式：`try_transition()` 校验合法路径 |
| 复杂度 | 灵活但需调用链保证一致性 | 简单，编译器检查分支覆盖 |
| 适用场景 | 内核多线程并行，各子系统独立控制 | async 运行时，上层需要明确生命周期可见性 |

**关键转换路径差异**：

| 转换 | bcachefs | subvolmount |
|------|----------|----------|
| 正常关闭 | `BCH_FS_STOPPING` → `bch2_fs_read_only()` → 停止 | `Stopping → GoingRo → drain → checkpoint → Stopped` |
| 只读切换 | `set_bit(BCH_FS_going_ro)` → `enumerated_ref_stop_async` → drain | `GoingRo → drain → checkpoint → ReadOnly` |
| 错误处理 | `set_bit(BCH_FS_ERROR)` — 叠加在其他标志之上 | `FsError` — 互斥，需显式转换 |

**影响**：subvolmount 枚举设计导致无法表达 `(Rw + Error)` 组合状态。当错误发生在 Rw 状态时，subvolmount 会直接切换到 `FsError` 并停止正常 I/O；bcachefs 则允许 `BCH_FS_RW | BCH_FS_ERROR` 共存，等待上层决定是否、何时停止。

**写引用门禁**：`try_begin_write()` 只允许 `Rw` / `RwWithPendingRecovery` 状态获取写引用；`ReadOnly`、`GoingRo`、`Stopping`、`Stopped`、`EmergencyRo` 与 `Error` 都必须拒绝。这个约束对齐 bcachefs `c->writes` 的可用期，不再只依赖 `GoingRo` 一项做拦截。

### 设计决策：共享快照运行时对齐 `c->snapshots.table`

**Context**: 快照读写如果只靠独立缓存或事务外查询，会和 bcachefs 的实时可见性脱节，尤其是在创建、删除和恢复后立即读取的路径上。

**Decision**: `BchVol` 持有单一共享 `SnapshotRuntime`，在恢复/启动时用 `bch2_snapshots_read()` 整体装载，在快照创建、删除、恢复 pass 后直接更新这份运行时视图。

**Why**: 这和 bcachefs 的 `c->snapshots.table` 语义一致。快照 ID 分配必须按“找空槽”而不是“猜下一个号”，否则一旦中间有空洞，就会出现可重复的偏差和性能退化。

**Example**:
```rust
let (table, tree_table) = bch2_snapshots_read(self);
self.install_snapshot_runtime(SnapshotRuntime::from_tables(table, tree_table));
```

**Extensibility**: 后续如果要做增量同步，也必须写回同一个 runtime 视图，不能并存第二套快照缓存或 next-id hint。

### 消除 `RwLock<BchVolInner>`（Phase 2, 2026-07-05）

**目标**：对齐 `struct bch_fs`（`bcachefs.h:766`）— 无外层锁，字段直接暴露。

**变更**：
- `BchVolInner` 结构体完全移除
- `background_tasks` 移除（死代码）
- 字段直接嵌入 `BchVol`，各有内部同步机制：

| 字段 | 类型 | 同步机制 | 依据 |
|------|------|---------|------|
| `engine` | `UnsafeCell<BtreeEngine>` | BtreeEngine 内部 `unsafe impl Sync` + UnsafeCell | 81 处外部调用需 `&mut BtreeEngine` |
| `journal` | `UnsafeCell<Arc<Journal>>` | UnsafeCell（`start()` 需替换 Journal 实例） | Journal 内部已有锁 |
| `allocator` | `UnsafeCell<BchAllocator>` | UnsafeCell + 内部 `Mutex<Group>` | `start()` 需替换 allocator |
| `root_snapshot_id` | `AtomicU32` | 原子操作（无锁） | 值类型 |
| `config` | `VolumeConfig` | 完全不可变 | 代码确认无写操作 |
| `background_tasks` | — | 已移除 | 死代码 |

**关键模式**：`unsafe impl Sync for BchVol` + UnsafeCell 访问器：

```rust
fn engine_mut(&self) -> &mut BtreeEngine {
    unsafe { &mut *self.engine.get() }
}
fn engine(&self) -> &BtreeEngine {
    unsafe { &*self.engine.get() }
}
```

**安全论证**：
- BtreeEngine 所有 `&self` 方法通过内部 UnsafeCell 管理可变性
- BchAllocator 分配方法均为 `&self`（内部 `Vec<Mutex<Group>>`）
- `Arc<Journal>` 跨线程共享安全
- **无外层锁**：`self.inner.write().await` / `self.inner.read().await` 全部消除
- 与 `struct bch_fs` 一致：不保护整个结构体，各子系统自管内部锁

### 主设备离线时的在线成员回退（2026-07-16）

`BchVol::primary_device_rcu_noerror()` 必须优先返回配置的主设备；若其已离线，按 `dev_idx` 选择在线成员，只有全部成员离线时才回退到原主设备。该顺序对应本地 bcachefs `for_each_online_member_rcu()`（`/home/black/Documents/bcachefs-tools/fs/sb/members.h:110-145`），保证主设备故障后 btree、journal、checkpoint 等元数据路径仍可继续访问可用副本。

`BchVol::checkpoint()`、启动恢复后的同步以及 `set_read_write()` 标记 dirty 时，必须遍历所有在线成员写 superblock；对应本地 `__bch2_write_super()`（`/home/black/Documents/bcachefs-tools/fs/sb/io.c:1390-1430`）的多设备写入循环。每个成员都尝试完成后才返回按成员顺序记录的首个错误。

`set_read_only()` 即使 checkpoint 返回错误，也必须先停止 journal 后台任务，再把状态置为 `Error` 并返回错误，不能遗留 `GoingRo` 状态；这是本地 `__bch2_fs_read_only()` 的错误/紧急只读收口要求。

NBD 导出必须声明并实现 FUA：请求 flags 中的 FUA 写入完成后，先执行后端 flush 再回复客户端。该语义对应本地 bcachefs `REQ_FUA`（`fs/data/write.c:1772-1773`、`fs/journal/write.c:548-550`）对稳定写入的要求。

NBD 新式握手必须发送完整的固定新式 greeting（`NBDMAGIC`、`IHAVEOPT`、global flags 和 124 字节 reserved），不能只发送前两个 magic；否则标准客户端会把后续 option header 错位读取。

NBD 握手返回的 transmission flags 必须反映握手时卷的只读状态；卷进入只读后，新连接应声明 `NBD_FLAG_READ_ONLY`，而不是继续报告可写。

NBD 连接关闭必须能打断写请求 payload 的读取；不能让客户端在已发送请求头后停顿而阻塞 daemon 的 shutdown drain。

固定新式 `NBD_OPT_GO` 完成 ACK 后，服务端必须先读取客户端 32-bit flags，再进入 transmission 请求循环；否则首个请求会发生 4 字节帧错位。

传输能力位必须保持协议位图：`SEND_FUA` 使用 bit 3，bit 4 保留为 rotational，`SEND_TRIM` 使用 bit 5；错误位会使客户端误判导出能力。

btree 新副本写入只允许在线 `Rw` 成员；`Ro`/`Evacuating` 成员仅保留读取用途，对应本地 `for_each_rw_member_rcu()`（`fs/sb/members.h:134-145`）。无可写成员时必须返回错误，不能提交空副本集合。

`BchVol::start()` 的任何恢复或首个设备 I/O 错误都必须把状态从 `Starting` 收口到 `Error`，对应本地 `bch2_fs_start()` 的失败路径，避免卷永久卡在不可重试的启动状态。

`BchVol::close()` 必须在 checkpoint 或最终 backend flush 失败后仍停止后台任务并发布 `Stopped` 终态；返回错误但不得遗留 `Stopping/GoingRo`。

`StorageService::close()` 同样必须在 superblock 写入失败后继续执行 backend flush，再返回首个错误，保证关闭路径不跳过最后的持久化屏障。

卷关闭的最终 flush 必须覆盖所有在线成员，不能只刷新主设备；多设备 journal/data 副本都需要经过同一持久化屏障。

`StorageService::create_on_sb()` 与 `open_on_sb()` 必须通过卷级在线设备选择解析 metadata backend；配置的 primary 离线时应回退到在线成员，不能直接失败。

allocator 的 `target_rw_devs()` 必须同时过滤设备 online 状态与 `Rw` member 状态；离线但残留 `Rw` 状态的设备不能进入 journal/btree 分配候选，避免后续分配阶段反复跳过失效成员。

用户 extent 分配必须额外遵守 member 的 `data_allowed` 位；仅允许 journal/btree 的设备不能接收 User 数据，防止元数据设备被错误消耗。

用户 extent 的有效副本数必须按设备 `durability` 累计，达到 `data_replicas` 后停止候选遍历；对应本地 `add_new_bucket()`（`fs/alloc/foreground.c:851-867`），硬件 RAID 设备不能按物理设备数量重复写入。

多副本读取应按设备读延迟 EWMA 优先选择更快副本，同时保留其他指针作为失败回退；对应本地 `bch2_bkey_pick_read_device()`（`fs/data/extents.c:202-310`）与 `bch2_latency_acct()`（`fs/data/write.c:837-863`）。

设备副本贡献必须从已转换的 member CPU 元数据读取，按本地 `BCH_MEMBER_DURABILITY` 的零值默认 1、非零值减 1 规则处理；对应 `bch2_mi_to_cpu()`（`fs/sb/members.h:416-439`）。

单副本 extent 只有在实际设备为主设备 `dev_idx == 0` 时才能使用兼容的 `BchVal` 形态；选中其他成员时必须保留 extent pointer 的设备索引，不能让后续读取回退到设备 0。该设备指针随副本提交路径保留，对应本地 `bch2_submit_wbio_replicas()`（`fs/data/write.c:1341-1478`）。

设备 IO ref 获取必须先递增 ref、再检查 online/member state；对应本地 `bch2_dev_get_ioref()`（`fs/sb/members.h:377-390`），避免 RO 切换竞态放行新的写 IO。

卷切换只读必须在排空卷级写引用并完成 checkpoint 后停止所有设备的 WRITE ref；重新 RW 时再恢复该闸门，对应本地 `__bch2_dev_read_only()`（`fs/init/dev.c:370-430`）。

快照子卷导出到 NBD 时必须保持只读：本地 `bch2_subvolume_snapshot()` 设置 `BCH_SUBVOLUME_RO`（`fs/snapshots/subvolume.c:644`），`bch2_subvol_is_ro()` 对该标志拒绝写入（`fs/snapshots/subvolume.c:323-329`）；NBD 导出需声明 `NBD_FLAG_READ_ONLY` 并以 `NBD_EPERM` 拒绝 WRITE/TRIM。

卷进入只读状态时，NBD `INFO_EXPORT`/oldstyle flags 还必须清除 `SEND_FUA` 与 `SEND_TRIM`，避免客户端根据能力位发送必然失败的变更请求；卷级只读切换对应本地 `__bch2_dev_read_only()`（`fs/init/dev.c:370-430`）的写引用闸门。

NBD WRITE 必须先完整消费 payload 再获取卷级写操作锁；这样单个客户端的 framing 阻塞不会占住 bcachefs 写路径的串行化边界，同时 WRITE/TRIM/FLUSH 在 payload 完整后仍按原顺序持有独占锁。

NBD 越界 READ/WRITE/TRIM 必须返回 `NBD_EINVAL`，而不是把请求范围错误伪装成 `NBD_EIO`；客户端才能区分请求错误与设备故障并避免无意义重试。

卷核心路径不能对主设备解析使用 `expect()`：`start`、`set_read_write`、`checkpoint`、extent 读写和 btree 分配在设备登记缺失时必须传播可恢复错误；主设备回退到在线成员仍遵循本地 `for_each_online_member_rcu()`（`fs/sb/members.h:110-145`）语义。

extent 写入必须先验证字节对齐，再获取卷级 write ref；无效请求不能在 `GoingRo` drain 计数中留下悬挂引用。有效写入仍按本地 `bch2_write_ref_tryget`/数据写入/`bch2_write_ref_put` 的成对生命周期执行（`fs/data/write.c:1341-1478`）。

extent 读写和 trim 的 key-space 端点必须使用 checked addition；本地 bcachefs 以 `POS_MAX`/`KEY_OFFSET_MAX` 限制 bkey 范围（`fs/bcachefs_format.h:187-188`），不能让范围回绕后继续进入 btree iterator。

COW 写入必须先完成新副本的分配与 IO，再删除/拆分旧 extent 并更新 Extents btree；本地 `__bch2_write_index()` 明确位于数据 IO 完成之后（`fs/data/write.c:1541-1588`），分配或设备失败时旧映射必须仍可读。

旧 extent 的删除与左右段插入必须在同一个 `BtreeTrans` journal commit 中完成；本地 `__bch2_trans_commit()` 对完整 update set 统一运行 triggers、写 journal 并处理错误（`fs/btree/commit.c:1381-1519`），不得用多个独立 commit 暴露半拆分状态。

多副本 extent 分配遇到单个候选成员的 `freelist_empty`、`insufficient_devices` 或其他可恢复分配错误时，必须记录首个错误并继续尝试后续在线成员；仅当所有候选都失败时才返回该错误。该候选遍历/重试顺序对应本地 `bch2_alloc_sectors_req()`（`fs/alloc/foreground.c:1498-1540`），允许在成员空间不均衡时保留可用的降级副本写入路径。

NBD 后端错误必须保留块设备 errno 语义：空间耗尽/配额超限返回 `NBD_ENOSPC`，对象不存在返回 `NBD_ENOENT`，只读拒绝返回 `NBD_EPERM`，范围错误返回 `NBD_EINVAL`；只有未分类 IO/后台错误才返回 `NBD_EIO`，避免客户端错误重试策略失真。

NBD 显式 FLUSH 与 FUA 写入的 flush 失败必须复用同一 errno 映射，不能单独压成 `NBD_EIO`；客户端据此区分只读、空间耗尽和真实持久化故障。
