# Btree IO — 节点 I/O 模块覆盖地图

> 生成日期: 2026-07-12
> 源文件: `crates/subvol-core/src/btree/io.rs` (~1880 行)
> 参考实现: bcachefs `fs/btree/read.c` + `fs/btree/write.c` + `fs/btree/commit.c`

## 覆盖统计

| 状态 | 数量 | 说明 |
|------|------|------|
| ✅ | 25 | 完全对齐 |
| ⚠️ | 0 | 无已知语义偏差 |
| ❓ | 0 | 未验证 |
| ➖ | 5 | subvol 特有（含 2 架构差异） |
| **总计** | **30** | |

## 函数状态表

### Read Path（14）

| 函数 | bcachefs 对应 | 参考 | 状态 |
|------|---------------|------|------|
| `bch2_btree_node_io_lock` | `bch2_btree_node_io_lock` | `read.c:70` | ✅ |
| `bch2_btree_node_io_unlock` | `bch2_btree_node_io_unlock` | `read.c:60` | ✅ |
| `bch2_btree_node_io_try_lock` | `mutex_trylock(&b->io_lock)` | `commit.c:254` | ✅ |
| `bch2_btree_node_wait_on_read` | `bch2_btree_node_wait_on_read` | `read.c:76` | ✅ |
| `bch2_btree_node_wait_on_write` | `bch2_btree_node_wait_on_write` | `read.c:84` | ✅ |
| `bch2_btree_node_read` | `bch2_btree_node_read` | `read.c:1025` (trans 驱动的 async 读入口) | ✅ |
| `bch2_btree_root_read` | `bch2_btree_root_read` | `read.c:1151` | ✅ |
| `bch2_validate_bset` | `bch2_validate_bset` | `read.c:245` | ✅ |
| `bch2_validate_bset_keys` | `bch2_validate_bset_keys` | `read.c:449` | ✅ |
| `bch2_btree_node_read_done` | `bch2_btree_node_read_done` | `read.c:574` | ✅ |
| `bch2_read_done_sort` | sort_iter 模式（内联于 read_done） | `read.c:574` | ✅ |
| `bch2_btree_node_sort_keys` | `bch2_btree_node_sort` | `sort.c:450` | ✅ |
| `bch2_sort_keys` | `bch2_sort_keys` | `sort.c:202` | ✅ 独立 packed-position comparator、`sort_iter_next` 顺序、仅过滤 Deleted、返回 u64 数 |
| `bch2_btree_node_drop_keys_outside_node` | `bch2_btree_node_drop_keys_outside_node` | `read.c:199` | ✅ |
| `bch2_btree_node_header_to_text` | `bch2_btree_node_header_to_text` | `read.c:49` | ✅ |
| `bch2_btree_flush_all_reads` | `bch2_btree_flush_all_reads` | `read.c` (简化版) | ➖ 同步 I/O 模型下为无操作 |

### Write Path（12）

| 函数 | bcachefs 对应 | 参考 | 状态 |
|------|---------------|------|------|
| `bch2_btree_node_write` | `__bch2_btree_node_write` | `write.c:336` | ✅ |
| `bch2_btree_node_write_mut` | `__bch2_btree_node_write`（写入前排序） | `write.c:336` | ✅ |
| `__bch2_btree_node_write` | `__bch2_btree_node_write` | `write.c:336` | ✅ |
| `__bch2_btree_node_write_locked` | `__bch2_btree_node_write`（已持锁版） | `write.c:336` | ➖ 拆分自 `__bch2_btree_node_write`，为 flush 回调提供非阻塞入口 |
| `bch2_btree_node_write_trans` | `bch2_btree_node_write_trans` | `write.c:717` | ✅ |
| `bch2_btree_post_write_cleanup` | `bch2_btree_post_write_cleanup` | `write.c:667` | ✅ |
| `bch2_btree_init_next` | `bch2_btree_init_next` | `write.c:754` | ✅ |
| `bch2_btree_flush_all_writes` | `bch2_btree_flush_all_writes` | `write.c:865` | ✅ |
| `bch2_btree_cancel_all_writes` | `bch2_btree_cancel_all_writes` | `write.c:895` | ✅ |
| `btree_node_write_if_need` | `__bch2_btree_node_write` (only_if_need 标志) | `write.c:375` | ✅ |

### Compat（2）

| 函数 | bcachefs 对应 | 参考 | 状态 |
|------|---------------|------|------|
| `compat_bformat` | `bch2_bkey_compat`（格式路径） | `read.c` | ✅ |
| `compat_bpos` | `bch2_bkey_compat`（位置路径） | `read.c` | ✅ |

## 偏差说明

| 函数 | 类型 | 说明 |
|------|------|------|
| `bch2_btree_node_io_lock` | ⚠️ 实现差异 | bcachefs 使用 `wait_on_bit_lock_io()` (`read.c:70-73`) 基于 waitqueue 等待；subvol 使用 `Condvar` + `Mutex` 等价实现。语义完全一致：先等待 write_in_flight 清除，再 CAS 设置标志。 |
| `bch2_btree_flush_all_reads` | ➖ 架构差异 | 同步 I/O 模型下为无操作，bcachefs 的异步 closure 循环不适用 |
| `bch2_btree_node_read` | ➖ 架构差异 | 已引入 `BtreeTrans` 作为上下文输入，但仍保留 Rust 异步返回值与简化错误类型；bcachefs 使用 `struct printbuf` 传错误信息 |
| `__bch2_btree_node_write_locked` | ➖ 架构差异 | subvol 特有拆分；bcachefs 使用同一函数，但 bcachefs 调用方保证锁已持。subvol 显式区分 `_locked` 以支持 flush 回调的非阻塞调用 |

## 关键差异

- **线程模型**: bcachefs 使用 bio/closure 异步回调；subvol 使用 async/await
- **事务上下文**: bcachefs 所有 I/O 函数需要 `btree_trans` 参数；subvol 无此要求
- **多设备**: bcachefs 支持多设备 replicas；subvol 单设备（所有 `bch2_btree_flush_all_*` 和 `cancel_all_*` 简化为近乎空操作）
- **NODE_DIRTY 标志位**: subvol 使用 bit 6 (0x40) 而非 bcachefs 的 bit 0；因 bit 0 已被 subvol 专用 `NODE_ACCESSED` 占用（`node.rs:395`）。语义与 bcachefs `BTREE_NODE_dirty` 一致：在 bkey 插入/删除/修改时设置，写入 CAS 中原子清除。

## 新增模式

### Flush 回调模式（Journal Reclaim → Btree Write）

对应 bcachefs `bch2_btree_node_flush0/1` (`commit.c:254-297`)，subvol 通过 `JournalPinFlushFn` 闭包实现。

**触发路径**: Journal reclaim → 遍历 pin_list → 调用 flush callback

**关键约束**:
1. flush 回调不可持阻塞锁（在 journal reclaim 路径中调用，持有 journal 内部锁）
2. 必须使用 `bch2_btree_node_io_try_lock` + `__bch2_btree_node_write_locked`（非阻塞 CAS）
3. 使用 `Weak<BtreeNode>` 避免节点被意外延长生命周期
4. 如果 try_lock 失败（节点正被其他线程写），直接跳过（bcachefs 的 best-effort 语义）

**代码模式**:
```rust
let weak: Weak<BtreeNode> = Arc::downgrade(&node);
let flush_cb: JournalPinFlushFn = Box::new(move |_j, _pin, _seq| {
    let n = weak.upgrade().ok_or_else(|| {
        StorageError::NotFound("btree node gone".into())
    })?;
    let addr = n.block_addr();
    if addr == 0 { return Ok(()); }
    // bcachefs mutex_trylock(&b->io_lock) — 非阻塞
    if bch2_btree_node_io_try_lock(&n) {
        __bch2_btree_node_write_locked(n, addr).ok();
    }
    Ok(())
});
// pass Some(flush_cb) to JournalEntryPin::new
```

**设置时机**: 在 `BtreeWriter::write_btree_node` 和 `bch2_btree_node_write_mut` 中创建 pin 时传入

### 固定地址 in-place 写盘

**原理**: bcachefs 中每个 node 的磁盘地址 (`b->key` 中的 extent pointer) 在 `__bch2_btree_node_alloc` 时分配，后续写盘直接原地覆盖同一地址。

subvol 对应:
- `BtreeNode.block_addr: AtomicU64` — 首次写分配，终身不变
- `try_set_block_addr()` CAS — 首次写设值，后续写无操作
- `BtreeWriter::write_btree_node` 先检查 `node.block_addr()`；如已分配直接复用

**约束**:
- 节点分裂（split）创建新节点，新节点需要分配新地址
- collapse 中 child 提升为新 root，复用 root 地址
- 快照隔离在 key 层面 (`Bpos.snapshot`)，in-place 覆盖不影响快照一致性
# 2026-07-17 对齐记录

- 对照本地 `fs/btree/read.c:861-868`：read-done 中 sibling u64 估计必须在
  aux-tree/排序重建完成后、range drop 前刷新；刷新值来自 live u64 数，不能使用
  包含旧 bset Deleted key 的 total bytes。
- 对照本地 `fs/btree/read.c:60-68`：`bch2_btree_node_io_unlock` 只清除
  `write_in_flight_inner` 和 `write_in_flight`，cache 状态收口必须在写完成的非重武装路径单独执行。
- 对照本地 `fs/btree/write.c:25-80`：写完成前后顺序为
  `will_make_reachable` 清理 → journal pin drop → flags/CAS 清理 →
  `write_done_clean`；I/O 错误路径也必须释放 journal pin。
- 对照本地 `fs/btree/commit.c:289-307`：journal pin flush 回调应设置重写需求并触发节点写入，不能依赖 `will_make_reachable` 才执行；重写标志在提交写入前消费。
