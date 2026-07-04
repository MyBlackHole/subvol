# BtreeTransaction — 事务模块覆盖地图

## Depth-zero root update serialization (2026-07-18)

- `Btree::bch2_btree_bset_insert_key_wrapper()` 与
  `bch2_btree_bset_insert_key_wrapper_skip_cache()` 对应本地
  `fs/btree/commit.c:326-353` 的 leaf update；本地调用方在 node/path write
  lock 下修改 leaf。subvol 的 depth-zero wrapper 没有持有 transaction path，
    因此必须在修改 `BtreeRoot` 前取得 `root_lock`。
- wrapper 必须在该锁的保护范围内完成 root mutation，并在释放锁前 clone
  `root.node`；journal pin 使用这份 clone，不能在解锁后重新从
  `UnsafeCell<BtreeRoot>` 读取 root。
- 该锁只保护 root mutation 与 pin 注册，不改变 skip-cache wrapper 的
  skip-cache 语义，也不改变正常 wrapper 的 cache invalidation 顺序。

### Validation

- 并发 depth-zero wrapper 更新 8 个不同 key 后，8 个插入都成功且 root
  key count 为 8。
- 依据：本地 `fs/btree/commit.c:326-353` 的 node write-lock 调用上下文；
  不得移除 wrapper 的 journal pin 或 cache invalidation 分支。

## Journal replay commit boundary (2026-07-18)

- 本地 `fs/btree/commit.c:718-766,1291-1320` 的 replay overwrite 语义属于
  `struct journal_keys`，不是独立的 btree overlay。
- subvol 不创建额外的 `JournalKeys` 或读穿透层；提交只使用本地 transaction
  lock、journal reservation、btree materialization 顺序。
- replay keys 由 journal replay 直接写入 btree；普通读路径直接读取 btree path。

## Transaction trigger order (2026-07-18)

- `btree_trigger_order()` 必须对齐本地 `fs/btree/types.h:1363-1373`：
  `BTREE_ID_alloc` 使用 `U8_MAX`，`BTREE_ID_stripes` 使用 `U8_MAX - 1`，
  其它 btree 使用自身持久化 ID。
- `alloc` 和 `stripes` 的特殊顺序是 transaction update 的排序/锁顺序，不能由
  当前是否实现 fs 业务 trigger 来决定；通用 btree 容器也必须保留它。
- 回归测试必须同时断言 `alloc=255`、`stripes=254` 和普通类型的 identity order。

## Deleted update materialization (2026-07-17)

- `bch2_btree_delete_at()` (`/home/black/Documents/bcachefs-tools/fs/btree/update.c:725-743`)
  constructs a deleted update at the iterator position; it does not call a
  separate node-level delete using a synthetic `Deleted` key for lookup.
- Every subvol journal materialization path must append a
  `BtreeEntry::Deleted` at `Bpos::from_key(&entry.key)`. Keeping the tombstone
  in journal/trigger data and materializing it by position preserves the local
  update/trigger ordering.
- Do not route transaction deletes through the Rust `Btree::delete()` helper:
  its shared-node `Arc::get_mut` precondition is not part of the local
  bcachefs operation and can turn a valid delete into a silent no-op.

> **2026-07-10 复审中**：任务 `07-10-bcachefs-core-alignment` 已从本地源码确认 path sentinel、深度常量、位图容量、path 双副本、begin/traverse/relock 等多项偏差。下表历史 ✅ 在本次函数级账本复核完成前均视为暂定，不能作为已对齐证据。

> **2026-07-13 检查点**：单一 transaction path pool、固定 level 槽、path ref、基础 unlock/downgrade/traverse-all 外层，以及 ordinary path 的 up/root/down 单路径状态机已落地并通过 1163 个库测试。`trans_start_time` 已只保留在内嵌 `locking_wait` 中，并按 `iter.c:3970-3971` 只在非 restart begin 刷新。cached traversal、transaction-aware `btree_node_lock` slowpath/自动 wait graph 收集、`bch2_trans_begin()` 的内存/计时/journal-replay 分支仍是 ⚠️，不得声称 transaction 核心已经完整对齐。

> 生成日期: 2026-07-08（历史状态；2026-07-10 正在重审）
> 源文件: `crates/subvol-core/src/btree/transaction.rs` (4300 行)
> 参考实现: bcachefs `fs/btree/commit.c` (1524 行) + `fs/btree/locking.c` (1645 行)

## Scenario: update/trigger flags 与 iterator flags 分离（2026-07-13）

### 1. Scope / Trigger

- 新增或修改 btree update/trigger、metadata marking、transaction path traverse 时适用。
- 唯一依据是本地 `fs/btree/types.h:448-525` 与 `fs/btree/iter.rs:514-535`。

### 2. Signatures

```rust
pub struct UpdateTriggerFlags: u32;

pub fn bch2_btree_path_traverse_one(
    &mut self,
    path_idx: PathIdx,
    flags: IterFlags,
) -> Result<(), BtreePathTraverseError>;
```

### 3. Contracts

- `UpdateTriggerFlags` 使用本地统一 C enum 的 bit 18..28：3 个 `BTREE_UPDATE_*`
  位后紧接 8 个 `BTREE_TRIGGER_*` 位，名称与本地 Rust API 完全一致。
- update/trigger flags 使用 `bitflags` 的 `u32` 组合语义；空值 bits 为 0。
- `IterFlags` 只控制 subvol 当前 iterator/path 的 intent、方向和 journal 数据；
  path traverse 不得接收 `UpdateTriggerFlags`。
- 不保留 `BtreeIterUpdateTriggerFlags = IterFlags` 兼容别名。

### 4. Validation & Error Matrix

| 条件 | 结果 |
|---|---|
| update/trigger 常量位于 bit 18..28 | 接受，`bits()` 与本地枚举一致 |
| `TRANSACTIONAL | INSERT` 或 `GC | INSERT` | 可组合并分别由 `contains()` 观察 |
| path traverse 使用 `IterFlags::default()` | 保持既有遍历控制流 |
| 重新引入 iterator/update flags 类型别名 | 拒绝，两个语义域不可混用 |

### 5. Good / Base / Bad Cases

- Good：metadata caller 组合 `UpdateTriggerFlags::TRANSACTIONAL | INSERT`。
- Base：`UpdateTriggerFlags::empty().bits() == 0`。
- Bad：把 bool 字段组成的 `IterFlags` 当成 transactional/GC 位传递。

### 6. Tests Required

- 逐项断言 11 个常量的 `u32` 位值和类型大小。
- 断言 transactional/GC 与 insert 的组合、包含和互斥观察。
- path traverse 回归测试必须继续传 `IterFlags::default()`。
- `timeout 60s cargo test -p subvol-core --lib` 必须通过。

### 7. Wrong vs Correct

```rust
// Wrong: 遍历配置冒充 update/trigger 位集合。
pub type BtreeIterUpdateTriggerFlags = IterFlags;

// Correct: 两个语义域使用不同类型。
pub struct UpdateTriggerFlags: u32;
bch2_btree_path_traverse_one(path_idx, IterFlags::default());
```

## Path node sentinel contract（2026-07-10）

### 1. Scope / Trigger

修改 `BtreePath`、traverse、relock、SRCU reset 或 level-up 路径时适用。依据仅为本地 `fs/btree/iter.h:183-187`、`fs/errcode.h:237-247` 及实际 `ERR_PTR` 写入点。

### 2. Signatures

- `BtreePath::btree_path_node(level: usize) -> Option<&BtreePathNode>`
- `BtreeTrans::bch2_btree_path_traverse_all() -> Result<(), RestartReason>`

### 3. Contracts

- 四个 level 槽位必须区分 `BtreePathNode::None`、`Node(_)`、`Error(BtreePathError)`。
- `btree_path_node()` 只把越界映射为 `None`；槽内 `Error` 必须原样返回，等价于 C 返回 `ERR_PTR`。
- 新路径进入遍历前的未填充槽位是 `Error(Init)`；cached path 的长解锁重置是 `Error(SrcuReset)`。
- traverse-all 临时引用必须调用 `__btree_path_get/__btree_path_put`，不得调用会释放路径的公开 put 阶段。

### 4. Validation & Error Matrix

| 条件 | 结果 |
|---|---|
| `level >= BTREE_MAX_DEPTH` | `btree_path_node()` 返回 `None` |
| 槽位为 `Error(Relock/Upgrade/...)` | 返回该 `Error`，调用方走错误/重遍历分支 |
| traverse-all 已在执行 | `Err(RestartReason::InTraverseAll)` |
| ref/intent ref 溢出或下溢 | 硬失败，不得饱和运算 |

### 5. Good / Base / Bad Cases

- Good：有效 leaf 为 `Node(level)`，slot 0 可直接参与锁序列检查。
- Base：越界 level 返回 `None`。
- Bad：把 `Error(Init)` 转成 `None`，会把“必须重遍历”错误误判为“路径到此结束”。

### 6. Tests Required

- `btree_path_node()` 必须分别断言有效 error sentinel 可见和越界为 `None`。
- 注册多层路径必须断言 leaf=slot 0，未使用槽为 `Error(Init)`。
- traverse-all 必须覆盖递归 guard 清理、动态 sorted 遍历和临时 ref 不释放路径。
- SRCU reset 必须断言保留 `SrcuReset` 错误身份。

### 7. Wrong vs Correct

```rust
// Wrong: ERR_PTR 被折叠，调用方无法区分错误与 NULL
BtreePathNode::Error(_) => None

// Correct: 只对数组越界返回 None，槽位状态原样可见
self.levels.get(level)
```

## Scenario: transaction-owned iterator paths（2026-07-13）

### 1. Scope / Trigger

修改 iterator 构造、path 复用、restart、unlock、downgrade 或直接遍历 btree 的调用方时适用。唯一依据是本地 `fs/btree/types.h:602-630`、`iter.c:1264-1340,1490-1590,1748-1839,2201-2278,3657-3672` 与 `locking.c:1386-1570`。

### 2. Signatures

- `BtreeTrans::bch2_trans_get_iter(&mut self, root, target, intent, btree_type) -> &mut BtreeIter`
- `BtreeTrans::bch2_btree_iter_set_pos(&mut self, iter_idx, new_pos: Bpos)`
- `BtreeTrans::bch2_btree_path_traverse_one(&mut self, path_idx, flags: IterFlags) -> Result<(), BtreePathTraverseError>`
- `BtreeTrans::bch2_trans_unlock(&mut self)`
- `BtreeTrans::bch2_trans_downgrade(&mut self)`

### 3. Contracts

- `BtreeTrans.paths` 是唯一 `BtreePath` owner；`BtreeIter` 只保存 `path/update_path/key_cache_path` 索引和指向该 pool 的 transaction 关联。
- 禁止 iterator 自持有 path pool，也禁止 clone iterator 来复制 path 状态。
- 相同 `(btree_id, level, pos)` 的 iterator 可共享 path，并分别增加 `ref/intent_ref`；同 leaf 但不同 position 不得直接共享可变 path。
- 生产遍历必须经 `BtreeTrans::bch2_trans_get_iter()`，transaction drop 负责最终 unlock。
- `BtreeIter` 仅实现 `Send` 以允许 transaction 整体跨 async await 移动；不得实现 `Sync`。
- `bch2_btree_iter_set_pos()` 必须由 `BtreeTrans` 执行：先对 iterator 的
  `update_path` 调用 transaction-owned `path_put()`，再按 iterator snapshot
  重建查询 key 和 root→leaf path；禁止只写 `iter.pos`。
- ordinary path 必须按本地 `iter.c:1490-1590` 顺序执行：restart/SRCU → relock → `should_be_locked` → cached 分流 → up-until-good → root/down 循环 → linked upper-level copy。
- `btree_path_up_until_good_node()` 遇到 Rust `BtreePathNode::Error` 时必须等价于 C 的非空 `ERR_PTR`：调用 `__btree_path_set_level_up()` 后继续向上；只有 `BtreePathNode::None`/越界才等价于 `NULL`。
- `btree_path_down()` 必须先从已初始化的 parent node iterator 取得 child pointer；parent 只持 read lock 时，在获取 child lock 前释放 parent read lock。不得用整树 `BtreeIter::init_with_path()` 替换该状态机。

### 4. Validation & Error Matrix

| 条件 | 结果 |
|---|---|
| path ref 达到 `u8::MAX` | 硬失败，不得饱和 |
| 同 position 复用 | 新 iterator，共享 path，`ref += 1` |
| 同 leaf、不同 position | 独立 path（后续应继续收口到本地 make-mut/set-pos 流程） |
| relock 失败且 `should_be_locked` | 保留锁失败状态并返回 `Restart(RelockPath)` |
| traverse-all restart/ENOMEM | 从 `retry_all` 重新排序、解锁、遍历 |
| terminal storage error | 清 cannibalize guard 与 `in_traverse_all` 后返回 |
| ordinary parent iterator 无 child pointer / pointer 不覆盖 lookup pos | 解锁整条 path、恢复 `path.level = depth_want`、在目标层留下错误 sentinel，再返回 terminal error |
| cached path 尚无 `btree_bkey_cached` 表示 | 返回 terminal error 并保留 `Error(Cached)`；禁止返回 `Ok(())` 造成 traverse-all 假成功/自旋 |

### 5. Good / Base / Bad Cases

- Good：两个同 position iterator 的 path index 相同，ref 为 2，锁状态只有一份。
- Base：读 transaction drop 后 leaf 的 read/intent 锁均已释放。
- Bad：复用已有 iterator 对象并覆盖其 `pos/flags`；这会让旧调用方观察到被静默修改的 iterator，同时造成 ref 泄漏。

### 6. Tests Required

- 同 position 两 iterator 必须断言 iterator 数量为 2、path index 相同、ref 为 2。
- 同 leaf 不同 position 必须断言 path index 不同。
- write unlock 必须断言所有同 level linked path 的 `locked_seq` 同步。
- Drop、traverse guard、relock error identity、SRCU reset、multi-level downgrade 均需回归测试；每条命令 60 秒超时。
- parent 复用下降测试必须断言 leaf slot 0 指向选中的 child、child 持 read/intent 锁、仅 read 的 parent 已解锁。
- cached path 测试必须断言当前架构缺口是显式 terminal error，而不是 uptodate。

### 7. Wrong vs Correct

```rust
// Wrong: 覆盖旧 iterator，并把同 leaf 当作相同 position
self.iters[old_idx].pos = target;
return old_idx;

// Correct: 相同 position 新建 iterator handle，共享唯一 path
self.__btree_path_get(path_idx, intent);
self.iters.push(BtreeIter::from_existing(..., path_idx, &mut self.paths));
```

```rust
// Wrong: Error 被当成 NULL，或直接重建整棵 iterator path
if matches!(path.levels[level], BtreePathNode::Error(_)) {
    return Ok(());
}

// Correct: ERR_PTR 非空，逐层 set-level-up；有 parent 时逐层 down
path.level = self.btree_path_up_until_good_node(path_idx, 0);
while path.level > depth_want {
    // node -> btree_path_down, NULL/error gap -> btree_path_lock_root
}
```

## 覆盖统计

| 状态 | 数量 | 说明 |
|------|------|------|
| ✅ | 77 | 完全对齐（新增 15：R1 锁 API 3 个 + R2 commit 出口 1 个 + 锁相位方法 5 个 + 字段 4 个 + debug 验证 1 个 + 修正 1 个） |
| ⚠️ | 0 | 已知偏差 |
| ❓ | 0 | 未验证 |
| ➖ | 21 | subvol 特有（含 7 项架构差异标注，新增 6 个占位字段+方法） |
| **总计** | **98** | |

## 函数状态表

### Ⅰ. 构造函数 & 生命周期（5）

| 函数 | bcachefs 对应 | 参考 | 状态 |
|------|---------------|------|------|
| `new` | — | Rust 构造 | ➖ |
| `set_watermark` | — | Rust Builder 模式 | ➖ |
| `begin` | `bch2_trans_begin` | `iter.c:3887-3946` | ✅ |

### Ⅱ. Iter 路径管理（5）

| 函数 | bcachefs 对应 | 参考 | 状态 |
|------|---------------|------|------|
| `bch2_trans_get_iter` | `bch2_trans_get_iter` | `iter.c` | ✅ |
| `get_path` | — | 路径缓存复用 | ➖ |
| `iter_mut` | — | 访问器 | ➖ |
| `iter` | — | 访问器 | ➖ |
| `iter_type` | — | 访问器 | ➖ |

### Ⅲ. 提交核心（3）

| 函数 | bcachefs 对应 | 参考 | 状态 |
|------|---------------|------|------|
| `commit` | `__bch2_trans_commit` | `commit.c:1381-1523` | ✅ |
| `rollback` | `bch2_trans_reset_updates` | `update.h:557-571` | ✅ |

事务磁盘使用量记账在本地 `fs/alloc/buckets.c:562-601` 的基础上保留可逆账本：
若 Atomic 触发器失败或显式回滚，恢复 `usage`、`sectors_available`、
`online_reserved` 及事务 reservation；成功重置/开始事务时清除该账本。
异步 journal 事务只有在完整 jset 发布并释放 journal reservation 后才清除账本，
因此 journal 写入失败仍可回滚已发布的 capacity accounting。

### Ⅳ. 锁管理 & 重启（21）

| 函数 | bcachefs 对应 | 参考 | 状态 |
|------|---------------|------|------|
| `collect_paths` | — | 调试辅助 | ➖ |
| `restart_optimized` | — | R2 优化，无 bcachefs 直接对应 | ➖ |
| `unlock_all` | `bch2_trans_unlock` | `locking.c:1478-1490` | ✅ |
| `unlock_write` | `bch2_trans_unlock_write` | `locking.c:1572-1581` | ✅ |
| `bch2_trans_locked` | `bch2_trans_locked` | `locking.c:1622-1631` | ✅ |
| `bch2_trans_downgrade` | `bch2_trans_downgrade` + `__bch2_btree_path_downgrade` | `locking.c:1386-1438` | ✅ |
| `bch2_trans_unlock_long` | `bch2_trans_unlock_long` | `locking.c:1543-1570` | ✅ |
| `request_restart` | — | 内部标志设置 | ➖ |
| `restart` | `btree_trans_restart` | `iter.h:613` | ✅ |
| `restart_with_relock` | — | subvolmount 特有 | ➖ |
| `needs_restart` | — | 标志检查 | ➖ |
| `restart_count` | — | 计数器 | ➖ |
| `restart_reason` | — | 查看器 | ➖ |
| `lock_must_abort` | `trans->lock_must_abort` | `locking.c:14-17` | ✅ |
| `lock_may_not_fail` | `trans->lock_may_not_fail` | `locking.c:47-51` | ✅ |
| `bch2_check_for_deadlock` | `bch2_check_for_deadlock` | `locking.c:189-310` | ⚠️ 当前只接收预收集 `WaiterInfo`，未扫描 path/WaitFifo |
| `bch2_btree_trans_lock_fn` | `bch2_six_check_for_deadlock` | `locking.c:783-857` | ⚠️ 尚未连接 node reuse 与自动 lock graph |
| `add_commit_hook` | `bch2_trans_commit_hook` | `commit.c:198-230` | ✅ |
| `run_commit_hooks` | `run_hooks` | `commit.c:210-222` | ✅ |
| `bch2_trans_journal_res_get` | `bch2_trans_journal_res_get` | `commit.c:49-70` | ✅ |

### Ⅴ. 重启触发辅助（16）

所有 `trigger_*` 函数对应 bcachefs `BCH_ERR_transaction_restart_*` 错误码。

| 函数 | bcachefs 对应 | 状态 |
|------|---------------|------|
| `trigger_node_split` | `BCH_ERR_transaction_restart_btree_node_split` | ✅ |
| `trigger_key_cache_miss` | `BCH_ERR_transaction_restart_key_cache_raced` | ✅ |
| `trigger_node_read_required` | `BCH_ERR_transaction_restart_lock_node_reused` (errcode.h:145) | ✅ |
| `trigger_needs_lock` | `BCH_ERR_transaction_restart_upgrade` / `BCH_ERR_transaction_restart_relock` (errcode.h:153/141) | ✅ |
| `trigger_would_deadlock` | `BCH_ERR_transaction_restart_would_deadlock_write` | ✅ |
| `trigger_write_overflow` | `BCH_ERR_transaction_restart_write_overflow` | ✅ |
| `trigger_split_with_interior_updates` | `BCH_ERR_transaction_restart_split_with_interior_updates` | ✅ |
| `trigger_traverse_all` | `BCH_ERR_transaction_restart_traverse_all` | ✅ |
| `trigger_relock` | `BCH_ERR_transaction_restart_relock` | ✅ |
| `trigger_relock_path` | `BCH_ERR_transaction_restart_relock_path` | ✅ |
| `trigger_upgrade` | `BCH_ERR_transaction_restart_upgrade` | ✅ |
| `trigger_fault_inject` | `BCH_ERR_transaction_restart_fault_inject` | ✅ |
| `trigger_nested` | `BCH_ERR_transaction_restart_nested` | ✅ |
| `trigger_lock_waitlist_alloc` | `BCH_ERR_transaction_restart_lock_waitlist_alloc` | ✅ |
| `trigger_mem_realloced` | `BCH_ERR_transaction_restart_mem_realloced` | ✅ |
| `check_path_integrity` | `__bch2_btree_path_verify` (`iter.c:378-396`) | ✅ |
| `detect_iter_restart_needed` | `trans->restarted` 标志 | ✅ |

### Ⅵ. 基础查询（5）

| 函数 | bcachefs 对应 | 状态 |
|------|---------------|------|
| `iter_count` | — | ➖ |
| `is_committed` | — | ➖ |
| `set_journal_seq` | — | ➖ |
| `journal_seq` | — | ➖ |
| `snapshot_alloc_ids` | `create_snapids` / `bch2_bkey_get_empty_slot` | ✅ 事务视图空槽分配（含 pending journal） |

### Ⅶ. WAL Pin 集成（3）

| 函数 | bcachefs 对应 | 状态 |
|------|---------------|------|
| `set_wal_pin` | —（subvolmount Phase B2） | ➖ |
| `clear_wal_pin` | —（subvolmount Phase B2） | ➖ |
| `wal_pin_id` | —（subvolmount Phase B2） | ➖ |

### Ⅷ. Journal 条目管理（6）

| 函数 | bcachefs 对应 | 参考 | 状态 |
|------|---------------|------|------|
| `journal_insert` | `bch2_trans_update` / `bch2_btree_insert` | — | ✅ |
| `journal_delete` | `bch2_trans_update` + KEY_TYPE_deleted | `commit.c:297` | ✅ KEY_TYPE_deleted 统一存储 |
| `journal_whiteout` | — | subvolmount 特有 | ➖ |
| `drain_journal` | — | subvolmount 特有 | ➖ |
| `journal_is_empty` | — | 检查器 | ➖ |
| `journal_len` | — | 检查器 | ➖ |

### Ⅸ. 旧 API / 弃用（1）

| 函数 | bcachefs 对应 | 状态 |
|------|---------------|------|
### Ⅹ. bcachefs 对齐 API（9）

| 函数 | bcachefs 对应 | 参考 | 状态 |
|------|---------------|------|------|
| `path_put` | `bch2_path_put` | — | ✅ |
| `trans_unlock` | `bch2_trans_unlock` | `locking.c:1524` | ✅ |
| `trans_relock` | `bch2_trans_relock` | `locking.c:1487-1517` | ✅ |
| `trans_commit` | `__bch2_trans_commit` | `commit.c:1381-1523` | ✅ |
| `trans_begin` | `bch2_trans_begin` | `iter.c` | ✅ |

`trans_iter_put`、`trans_iter_mut`、`trans_iter` 这类仅用于 Rust 侧访问器的 helper 已移除，不再伪装成 bcachefs API。

### Ⅺ. 其他 impl（2）

| 函数 | bcachefs 对应 | 状态 |
|------|---------------|------|
| `Default::default` | — | ➖ |
| `Debug::fmt` | — | ➖ |

### Ⅻ. BtreePath 方法（1）

| 函数 | bcachefs 对应 | 参考 | 状态 |
|------|---------------|------|------|
| `sort_key` | `__btree_path_cmp` | — | ✅ |

## 偏差说明

1 项已知偏差：`trans_commit_to_journal_replay_pre/post()` 不移植（架构差异）。详见上方说明。

其余 4 项 bcachefs 独有函数已全部完成对齐移植（✅）。

### 提交路径（2026-07-18）

不保留 subvol 自有的第二套提交入口或额外 journal 条目写入包装。提交入口和 journal reservation、写入、materialize、错误重试的顺序以本地 bcachefs `bch2_trans_commit()` / `do_bch2_trans_commit()` 为唯一依据。

## 新增 bcachefs 对齐函数（本次对齐新增）

### 第一轮（2026-07-06）：SixLock 写锁慢路径修复

| 函数/概念 | 参考 | 状态 | subvol 对应 |
|-----------|------|------|-------------|
| `try_lock_write_preset_for` percpu skip-my-slot | `six.c:122-214` | ✅ | 修复：不再跳过当前 slot，不再对非 percpu 路径减 THREAD_READ_CNT |
| `smp_mb()` after `fetch_or(SeqCst)` | `six.c:299` | ✅ | 移除冗余 `fence(SeqCst)` |

### 第二轮（2026-07-08）：Btree Transaction Engine 对齐

| 函数 | bcachefs 参考 | 状态 | subvol 对应 |
|------|--------------|------|-------------|
| `bch2_trans_locked` | `locking.c:1622-1631` | ✅ | `BtreeTrans::bch2_trans_locked` — 检查任意 path level 是否持有锁 |
| `bch2_trans_downgrade` | `locking.c:1427-1438`, `locking.c:1386-1423`, `iter.h:635-641` | ✅ | `BtreeTrans::bch2_trans_downgrade` — 非 leaf 解锁 + leaf intent→read |
| `bch2_trans_unlock_long` | `locking.c:1543-1570` | ✅ | `BtreeTrans::bch2_trans_unlock_long` — 3 相位完整对齐 |
| `trans_set_locked` | `locking.h:115-127` | ✅ | `BtreeTrans::trans_set_locked` — locked flag + dep_map/PF_MEMALLOC_NOFS 占位 |
| `trans_set_unlocked` | `locking.h:129-139` | ✅ | `BtreeTrans::trans_set_unlocked` — locked flag + lockdep/PF_MEMALLOC_NOFS 释放占位 |
| `trans_maybe_disable_migrate` | `locking.h:88-104` | ✅ | `BtreeTrans::trans_maybe_disable_migrate` — 含 `shard_cpu` 条件（no-op） |
| `trans_enable_migrate` | `locking.h:107-113` | ✅ | `BtreeTrans::trans_enable_migrate` — 
| `bch2_btree_path_verify_locks` | `locking.c:1415` | ✅ | `BtreeTrans::bch2_btree_path_verify_locks` — `#[cfg(debug_assertions)]` |
| `__bch2_trans_commit` 出口 `bch2_trans_downgrade` | `commit.c:1513-1514` | ✅ | 成功路径调用 `bch2_trans_downgrade()` |
| `trans->migrate_disabled` | `locking.h:99` | ✅ | 字段对齐，subvol 中仅作相位标记 |
| `trans->shard_cpu` | `locking.h:99` | ✅ | 字段对齐，默认 -1（不启用） |
| `trans->locked` / `trans->last_unlock_ip` | `locking.h:115-139` | ✅ | 字段对齐 |
| `trans->srcu_held` | `locking.c:1548` | ✅ | 字段对齐，Phase 3 块完整实现（no-op） |

### ➖ 架构差异（不移植操作）

| 操作 | bcachefs 对应 | 原因 |
|------|--------------|------|
| `lock_acquire_exclusive` + `lock_release` | `locking.h:120,134` | lockdep 框架，subvol 无等效 |
| `PF_MEMALLOC_NOFS` flag 设置/恢复 | `locking.h:122-123,136-138` | Linux 内核 PF_ 标志，subvol 无等效 |
| `migrate_disable()` / `migrate_enable()` | `locking.h:103,111` | Rust async 运行时不控制 CPU 迁移 |
| `btree_cache_cannibalize_lock`/`unlock` | `locking.c:1539-1540` | subvol 无 btree cache cannibalize 机制 |
| `srcu_read_lock` / `srcu_read_unlock` | `locking.c:1548-1569` | subvol 使用 Arc 管理节点生命周期 |
| `trans_commit_to_journal_replay_pre/post()` | `commit.c:746-766` | recovery 阶段无并发写入 |
| `bch2_trans_reset_updates` 中的 `path_put` | `update.h:557-571` | subvol 路径管理方式不同 |

### R2 变更详情：`__bch2_trans_commit` 成功出口（2026-07-08）

原 `__bch2_trans_commit` 成功路径仅 `restart_count = saved_restart_count` 后立即 `return Ok(())`。

**修复**：添加 `bch2_trans_downgrade()` 调用，对齐 bcachefs `out_reset:` 标签中的 `if (!ret) bch2_trans_downgrade(trans);` (commit.c:1513-1514)。

`bch2_trans_reset_updates(trans)` 在成功路径未移植：subvol 的 `begin()` 会清除 `committed` 标志，与测试期望冲突。由 caller 在下一次操作前显式调用 `begin()`。

## 无 bcachefs 对应的 subvol 独有函数 (➖)

所有 `➖` 标注的函数均为 Rust 特有的访问器/构造器/subvol 扩展。详见上方表格。

### 事务磁盘用量记账（2026-07-17）

已把本地 `fs/alloc/buckets.c:562-601` 的事务字段和记账顺序接入
`BtreeTrans`：`fs_usage_delta`、唯一所有权的 `disk_res`、`extra_disk_res`，以及
`bch2_trans_account_disk_usage_change()`。提交前先按
`added = btree + data + reserved` 计算超 reservation 部分并只扣一次
`capacity.sectors_available`，再消费 `res.sectors/online_reserved`，最后累加
`capacity.pcpu[0].usage`；`bch2_trans_begin`、`reset_updates`、`rollback` 均清除局部
delta。Extent 更新在 transactional 相位累加 data/cached delta。

参考：本地 `fs/btree/types.h:877-884`、`fs/btree/commit.c:1125-1160,1475-1510`。

触发器按本地 bcachefs commit 的 transaction context 执行：事务性更新在提交前处理，
atomic 更新在 reservation 与写锁建立后处理；不维护额外的 volume 级 trigger registry。
### 2026-07-17 commit 顺序复核

- 对照本地 `fs/btree/commit.c:1200-1267`：事务提交必须先把更新填入已保留的 journal entry，再以同一 `journal_res.seq` 修改 btree 节点，最后 `journal_res_put()`。
- subvol `BtreeTrans::bch2_trans_commit()` 已按该顺序调整；journal reservation 失败仍在任何 btree 修改前返回并回滚 accounting。

### 2026-07-18 write-lock 生命周期复核

对照本地 `fs/btree/commit.c:1280-1320`，async 提交的 write lock 现在覆盖
journal reservation、dirty key flush、journal 写入和 btree materialize；顺序固定为
`lock_write → journal_res_get → commit_write/materialize → journal_res_put → unlock_updates_write`。
atomic trigger 失败路径先释放 write lock，再向调用者返回错误。
