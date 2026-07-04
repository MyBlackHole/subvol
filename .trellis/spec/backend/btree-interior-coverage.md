# Btree Interior — 内部节点操作覆盖地图

> 更新日期: 2026-07-18
> Rust 源文件: `crates/subvol-core/src/btree/interior.rs`、
> `crates/subvol-core/src/btree/node.rs`、`crates/subvol-core/src/btree/btree.rs`
> 唯一参考: 本地 bcachefs-tools `fs/btree/interior.h`、`fs/btree/read.c`、
> `fs/btree/cache.h`、`fs/btree/init.c` 与 `include/linux/static_key.h`

## 本批覆盖范围

本页当前只声明下列四个 static-inline helper 已逐行核对；`interior.rs` 的 split、merge、
rewrite、collapse 等其余路径仍需后续分批审计，不得据此宣称整个模块已经完成对齐。

| Rust helper | 本地 bcachefs 对应 | 参考位置 | 状态 |
|---|---|---|---|
| `btree_node_needs_merge` | `btree_node_needs_merge` | `interior.h:194-201` | ✅ |
| `btree_update_reserve_required` | `btree_update_reserve_required` | `interior.h:251-265` | ✅ |
| `btree_node_reset_sib_u64s` | `btree_node_reset_sib_u64s` | `interior.h:267-271` | ✅ |
| `BtreeNode::bch2_btree_node_compact_fits` | `bch2_btree_node_compact_fits` | `interior.h:379-403`、`interior.c:1985-2001` | ✅ |

## Leaf insert accounting (2026-07-18)

- `Btree::bch2_btree_insert_key_leaf()` must preserve the local
  `fs/btree/commit.c:361-368` post-insert accounting: when `live_u64s_added < 0`,
  update each non-boundary `sib_u64s` estimate with the resulting live size as the
  lower bound; when `u64s_added > live_u64s_added`, apply the lazy whiteout compact
  gate from `fs/btree/sort.h:76-86` (`dead_u64s > 64 && dead_u64s * 3 > total_u64s`).
- This is generic node-space accounting only; it does not add inode, directory, or
  other filesystem payload semantics to `BtreeId`.

## Leaf delete accounting (2026-07-18)

- The Rust `delete_key` path must perform the same post-update accounting as the
  local `bch2_btree_insert_node()` whiteout path in `fs/btree/interior.c:2199-2248`:
  decrement non-boundary `sib_u64s` estimates by negative `live_u64s_added`, clamp
  them at zero, then apply the same lazy whiteout thresholds before foreground merge.
- The accounting occurs before the delete path invokes foreground merge/root
  collapse; it remains generic node-space bookkeeping and contains no fs payload logic.

## Merge boundary poisoning (2026-07-18)

- Before sibling candidate lookup, a node whose range begins at `POS_MIN` must set
  `sib_u64s[btree_prev_sib] = U16_MAX`; a node whose range ends at `SPOS_MAX` must set
  `sib_u64s[btree_next_sib] = U16_MAX`, matching local `interior.c:2945-2955`.
- The sentinel is a cached negative result for that side, not an early-return condition
  in `btree_node_needs_merge`; subsequent candidate scans must observe it.

## Merge failure backoff (2026-07-18)

- When three source nodes do not fit in fewer destinations, follow local
  `compute_merge()` order: remove the larger outer sibling candidate once, recompute
  destination count using half-node capacity, then either continue or reset the
  remaining pivot estimates and return.
- `merge_fail_reset_sib_u64s()` computes `pivot_live + sibling_live`, moves values
  halfway toward `BTREE_FOREGROUND_MERGE_HYSTERESIS`, and caps at `U16_MAX - 1`.
  This is a generic merge retry backoff; it does not depend on fs payload types.

## Merge candidate cheap gate (2026-07-18)

- Before reading either sibling, compare the cached `sib_u64s` estimate with the
  btree foreground merge threshold; estimates strictly greater than the threshold
  skip candidate loading, matching `btree_merge_push_pos()` at `interior.c:2465`.
- Boundary poisoning happens first, so `U16_MAX` boundary estimates naturally take
  the same cheap skip path without changing `btree_node_needs_merge` semantics.

## Merge entry gate (2026-07-18)

- The public merge entry must check `btree_node_needs_merge(c, b, d)` before
  entering candidate collection, using `min(sib_u64s) + d <= foreground threshold`.
- The Rust delete path uses `d = 0`; empty nodes retain the existing cleanup path so
  they can still be removed. Non-empty nodes no longer enter merge solely because
  their own live size is below one-third of the node buffer.

## Root publication lock boundary (2026-07-18)

- Local `fs/btree/interior.c:1606-1626` publishes a new root under the cache/root
  lock pair. Rust `bch2_btree_set_root_inmem()` now acquires the `root_lock`
  internally, so recovery, split/collapse, and direct callers share one root
  publication boundary.
- Depth-0 insert/transaction-insert/delete paths also take `root_lock` around
  the root depth decision and direct node mutation; they must not mutate the
  `UnsafeCell` root outside that boundary.
- Callers must not hold `root_lock` across this API; the lock belongs to the
  root publication operation itself. This is generic btree topology metadata only;
  it does not add btree-ID filesystem payload behavior.

## Root split visibility and generic btree ID (2026-07-18)

- Local `fs/btree/interior.c:1962-2174` constructs destination nodes and only
  publishes the replacement root after the update path succeeds. Rust
  `split_root()` now clones the current root under `root_lock`, performs the
  split and asynchronous writes on that private working node, then calls
  `bch2_btree_set_root_inmem()` only after the new root is ready. The published
  old root is not left half-split during an await.
- `init_interior_update()` uses the owning `Btree` instance's generic `btype`,
  matching the local transaction path's btree ID. It no longer hardcodes
  `BtreeId::Extents`; btree IDs carry topology identity only and no filesystem
  payload behavior.

## Split update ownership (2026-07-18)

- Local `bch2_btree_split_leaf()` creates one `btree_update` at
  `interior.c:2288-2309`; the public Rust wrapper must not pre-acquire a second
  update guard before delegating to the insertion path.
- The Rust wrapper now delegates directly, leaving the actual leaf split path as
  the single owner of the current update lifecycle. This prevents an update
  from rejecting itself as a concurrent `write_blocked` operation.
- Rust now stores `write_blocked` on each `BtreeNode`, so unrelated nodes are no
  longer serialized by one btree-wide flag. `Btree` also owns an
  `interior_updates` registry matching the local
  `bch_fs_btree_interior_updates` lists and waitlist: update registration,
  per-node blocker membership, reparent on root replacement, and wake-before-
  recheck waits are implemented. The blocker guard is retained until the
  newly written node's `write_in_flight` clears, matching the local
  `btree_update_done()` → `btree_update_set_nodes_written()` handoff. The
  remaining representation difference is that Rust uses mutex-protected
  bounded vectors instead of Linux intrusive `list_head` nodes; full
  closure/workqueue, reservation, and journal-pin transfer are still separate
  alignment work.

## Interior update flush boundary (2026-07-19)

- Rust now exposes `Btree::bch2_btree_interior_updates_flush()`, matching
  local `bch2_btree_interior_updates_flush()` at `interior.c:3740-3748`.
- Checkpoint and device flush paths wait for all registered interior updates
  before draining dirty btree nodes or issuing the journal/device flush.
- The current waitlist only observes the Rust update registry; it does not yet
  perform bcachefs's `btree_update_nodes_written_trans()` commit, disk
  reservation release, or journal-pin transfer.

## Parent route dirty tracking (2026-07-19)

- After a leaf/internal split inserts a new routing pointer, the non-root
  parent is inserted into the dirty cache, matching
  `bch2_btree_node_set_dirty()` and `btree_update_updated_node()` in local
  `interior.c:1189-1206` and `cache.c:540-553`.
- The parent remains blocked by the interior update, but is no longer lost as
  a clean cache entry before checkpoint/flush drains dirty nodes.

## Split destination dirty tracking (2026-07-19)

- In the Rust leaf split representation the left destination keeps the old
  cache address, while local bcachefs allocates a replacement object for
  every destination and calls `bch2_btree_update_write_new_node()`; therefore
  the retained left node must enter the dirty cache before parent propagation.
- This preserves the local `bch2_btree_node_set_dirty()` handoff and prevents
  the modified left half from being treated as clean after the split.

## Nodes-written completion boundary (2026-07-19)

- Local `bch2_btree_update_done()` (`interior.c:1374-1390`) only queues
  `btree_update_set_nodes_written()`; it does not set `nodes_written` during
  the foreground parent update.
- Rust `BtreeInteriorUpdate::mark_done()` therefore leaves the shared
  `nodes_written` state false. The write-completion guard sets it only after
  the protected node's `write_in_flight` has cleared, preserving the local
  foreground/completion phase boundary.
- Error-path guard destruction does not report a successful node write.
- Root split completion waits for all destination nodes, not only the new
  root, matching one closure completion reference per
  `bch2_btree_update_write_new_node()` submission.

## API 可见性（2026-07-17）

- 本地 `bch2_btree_set_root()`（`interior.c:1628-1647`）与
  `bch2_btree_node_rewrite()`（`interior.c:3276-3343`）均为 `static`，Rust 对应 helper
  必须保持模块私有。
- `bch2_btree_node_rewrite_key()`、`bch2_btree_node_rewrite_pos()` 等由
  `interior.h` 声明的入口才属于本地公开 API。可见性收口不代表上述两个 static helper
  的完整控制流已经完成审计。

## Scenario: sibling estimates and foreground merge decisions follow local bcachefs

### 1. Scope / Trigger

- 修改 sibling u64 估计、foreground merge 判定、interior update reserve 计算，或 read-done
  中 sibling 估计刷新位置时适用。
- 唯一依据是本地 `interior.h:194-201,251-271`、`read.c:861-868`、
  `cache.h:155-158,189-195`、`init.c:313-315` 和
  `include/linux/static_key.h:5-20`。

### 2. Signatures / Representation Mapping

- 本地三个 helper 都是 `static inline`，且名称没有 `bch2_` 前缀；Rust 对应 helper
  必须保持 crate 内部可见，不得建立不存在的公共 `bch2_*` API。
- 本地 reserve helper 接收 `struct bch_fs *c, struct btree *b`。Rust `BtreeNode`
  对应本地节点，但不内嵌 btree id；Rust 因而接收 `&Btree` 取得 root，再接收
  `&BtreeNode` 取得当前 level。这是表示层映射，不得改成由调用方传入预计算 depth。
- merge helper 接收 `&BchVol, &BtreeNode, i32`；filesystem 参数提供运行时初始化的
  `foreground_merge_threshold`，不能在 helper 内重新按节点大小推算。
- reset helper 接收 `&mut BtreeNode`，只改写 `sib_u64s[0..2]`。

### 3. Contracts / Call Order

- reserve 先读取 `root.level + 1`，再按 `depth < BTREE_MAX_DEPTH` 选择
  `(depth - node_level) * 2 + 1` 或 `(depth - node_level) * 2 - 1`；分支和运算顺序
  必须保持本地 `interior.h:254-264`。
- merge 先检查 merging-disabled 状态，再计算
  `min(sib_u64s[0], sib_u64s[1]) + d <= foreground_merge_threshold`。不得添加
  `U16_MAX` 早退、saturating add 或 node-local threshold。
- 本地 tools 兼容层的 `static_key_enable/disable()` 为空操作，
  `static_key_enabled()` 固定 false；Rust 当前以私有 `const false` 映射该本地实际行为，
  依据仅来自 `include/linux/static_key.h:5-20`。
- `foreground_merge_threshold` 在 volume 初始化时只计算一次，公式对应
  `BTREE_FOREGROUND_MERGE_THRESHOLD(c) = btree_max_u64s(c) / 3`。
- reset 使用 live u64 数，不使用 total bytes；左边界为 `POS_MIN` 时左值写 `U16_MAX`，
  右边界为 `SPOS_MAX` 时右值写 `U16_MAX`，其余位置写 live u64 数。
- read-done 的相对顺序必须保持：排序/aux-tree 重建完成 → reset sibling 估计 →
  range drop。对应本地 `read.c:861-868`；本批不扩大为整个 read-done 路径的重新审计。

### 4. Validation & Boundary Matrix

| 条件 | 结果 |
|---|---|
| merging-disabled 为 true | 在读取 sibling/threshold 前返回 false |
| `min(sib) + d <= filesystem threshold` | 需要 foreground merge |
| `min(sib) + d > filesystem threshold` | 不需要 foreground merge |
| sibling 为 `U16_MAX` 且 `d = 0` | 由普通比较自然得到 false，不得提前返回 |
| sibling 为 `U16_MAX` 且 `d` 足够负 | 仍执行加法与比较，可得到 true |
| depth 小于 `BTREE_MAX_DEPTH` | reserve 使用 `* 2 + 1` 分支 |
| depth 等于 `BTREE_MAX_DEPTH` | reserve 使用 `* 2 - 1` 分支 |
| 节点含旧 bset Deleted key | reset 只计 live key，不计 total bytes |
| `min_key == POS_MIN` / `max_key == SPOS_MAX` | 对应方向写 `U16_MAX` |

### 5. Good / Base / Bad Cases

- Good：volume 初始化一次 merge threshold，transaction 把同一 volume 和当前节点传给
  `btree_node_needs_merge()`。
- Base：普通内部节点把 live u64 数同时写入两个 sibling estimate；边界方向单独改为
  `U16_MAX`。
- Bad：把 `U16_MAX` 当作独立 sentinel 立即返回，或对 `min(sib) + d` 使用饱和加法；
  两者都会改变本地负 delta 的分支结果。
- Bad：reset 使用 `total_data_bytes()`；旧 bset whiteout/Deleted key 会虚增 sibling estimate。

### 6. Tests Required

- `timeout 55s cargo test -p subvol-core btree::interior::tests -- --nocapture`
  必须在一分钟内通过，并覆盖 reserve 两个深度分支、正负 delta、`U16_MAX` 负 delta、
  live-vs-total 以及左右边界。
- `timeout 55s cargo test -p subvol-core btree::io::tests::test_read_done_validates -- --nocapture`
  必须验证 read-done 刷新 sibling estimate。
- `timeout 55s cargo test -p subvol-core --lib`、`timeout 55s cargo check -p subvol-core`、
  `cargo fmt --check` 与 `git diff --check` 必须通过。

### 7. Wrong vs Correct

```rust
// Wrong: local helper has no U16_MAX fast exit and uses a filesystem threshold.
if min_sib == u16::MAX {
    return false;
}
min_sib.saturating_add(delta) <= btree_max_u64s(node.node_size) / 3

// Correct: preserve local branch and arithmetic order.
if BCH2_BTREE_NODE_MERGING_DISABLED {
    return false;
}
i32::from(node.sib_u64s[0].min(node.sib_u64s[1])) + delta
    <= i32::from(vol.btree_foreground_merge_threshold)
```

## Scenario: compact-fit uses the post-write sector budget

### 1. Scope / Trigger

- 修改 compact 后是否重试 insert、split 前空间判定、btree node record 头布局或 block
  对齐规则时适用。
- 唯一依据是本地 `interior.h:379-403` 与 `interior.c:1985-2001`；Rust 磁盘头映射
  只能依据当前 `BtreeNodeHeader`、`BtreeNodeDiskEntry` 和 `BsetHeader` 实际布局。

### 2. Signatures / Representation Mapping

- 本地 helper 是 `static inline bool bch2_btree_node_compact_fits(struct bch_fs *,
  struct btree *, unsigned)`；Rust 对应方法必须使用同名并保持 crate 内部可见。
- 本地 `sizeof(struct btree_node)` 映射为 Rust 完整初始 record 的
  `size_of::<BtreeNodeHeader>() + size_of::<BsetHeader>()`。
- 本地 `sizeof(struct btree_node_entry)` 映射为 Rust follow-on record 的
  `size_of::<BtreeNodeDiskEntry>() + size_of::<BsetHeader>()`。
- 已绑定 volume 的节点从 `BchVol::block_size()` 取得 block bytes；脱离 volume 的测试节点
  使用 Rust 磁盘格式固定的 `BLOCK_SIZE`。node sector 预算来自该节点的 `node_size`。

### 3. Contracts / Call Order

- `initial_bytes = initial_header_bytes + live_u64s * 8 + 8`。
- `followon_bytes = followon_header_bytes + new_key_u64s * 8 + 8`。
- 两项必须各自先按 block bytes 向上取整，再各自换算为 512-byte sectors；不得先相加
  再统一 round-up。
- 返回条件只能是 `initial_sectors + followon_sectors <= btree_sectors`。
- `live_u64s` 排除 Deleted/whiteout 历史数据，不能使用 total bytes、buffer end 或
  `whiteout_u64s` 替代。
- 调用方的 `new_key_u64s` 必须来自完整 packed `BtreeEntry`，包含 key header 与 value；
  不能只传 16-byte extent value 的 `2`。这对应本地 `as->new_key_u64s`。

### 4. Validation & Boundary Matrix

| 条件 | 结果 |
|---|---|
| node 只有一个 block，initial/follow-on 都为空 payload | 两个 record 各占一 block，返回 false |
| node 有两个 block，initial/follow-on 都为空 payload | 总计恰好两个 block，返回 true |
| follow-on key 恰好仍 round 到一个 block | 返回值不因该 key 增加第二个 follow-on block |
| follow-on key 多一个 u64 后跨 block 边界 | follow-on round 到两个 block，重新比较总预算 |
| initial live data 跨过一个 block 边界 | initial round-up 增加整 block，可能转为 false |
| total bytes 很大但全部为 Deleted | 只按 live u64s 计算，不得虚增 initial record |

### 5. Good / Base / Bad Cases

- Good：用实际磁盘 record 头、live packed bytes 和实际完整新 key 大小计算两个独立 record。
- Base：空 initial record 与空 follow-on record 在 8 KiB node 中各占一个 4 KiB block。
- Bad：用 `last.end_offset + key + whiteout <= node_size / 8`；该检查没有模拟写路径的
  两次 block round-up，会在 born-exhausted 节点上错误地重试 insert。
- Bad：调用方固定传 `2`，只计算 extent value，忽略 packed key header。

### 6. Tests Required

- `timeout 55s cargo test -p subvol-core btree::node::tests::test_btree_node_compact_fits -- --nocapture`
  必须覆盖双 record、follow-on block 边界、initial live 边界和 live-vs-total。
- `timeout 55s cargo test -p subvol-core --lib` 必须在一分钟内通过，覆盖 root leaf、
  multi-level leaf 与 routing parent 的 compact/split 调用点。
- `timeout 55s cargo check -p subvol-core`、`cargo fmt --check` 与 `git diff --check`
  必须通过。

### 7. Wrong vs Correct

```rust
// Wrong: buffer-space approximation and value-only input.
node.last_end_u64s() + 2 <= node.node_size / 8

// Correct: callers pass the complete packed key; the helper rounds both records separately.
let new_key_u64s = entry_packed_size(&BtreeEntry::from((key, value))) as u32 / 8;
let initial_sectors = initial_bytes.div_ceil(block_bytes) * block_bytes / SECTOR_SIZE;
let followon_sectors = followon_bytes.div_ceil(block_bytes) * block_bytes / SECTOR_SIZE;
initial_sectors + followon_sectors <= node.node_size as usize / SECTOR_SIZE
```

## Scenario: merge must preflight the locked parent route

### 1. Scope / Trigger

- 修改 interior merge、父节点路由更新、source 节点回收或 cache restore 时适用。
- 唯一依据是本地 `fs/btree/interior.c:2191-2265` 的
  `bch2_btree_insert_node()` 与 `:3084-3203` 的 merge update 路径：bcachefs 在
  destructive merge 前持有并验证 parent path；parent 不可用或身份不符时必须终止本次
  更新并保留可恢复状态。
- 该约束只描述通用 btree 拓扑和 cache 所有权，不引入 inode、extent 或其他 fs 语义。

### 2. Contracts / Call Order

- source ownership 校验完成后、任何 source 节点被 destructive merge 前，必须确认：root
  parent 可独占更新，或非 root parent 已从 cache 取出并保持独占到 route commit 完成。
- parent 不可用时，所有已取出的 source 必须逐一放回 cache，随后返回失败；不得先丢弃
  survivor 之外的节点再报告路由失败。
- route commit 前要在同一 parent 状态的私有副本上预检所有旧 child route 与新 route 的
  删除/插入；旧 route 不完整或新 route 不可容纳时，必须在 source 未修改前返回失败。
- 实际 routing update 仍必须检查返回值；不得像此前实现一样忽略失败结果。该前置检查
  不能替代本地 bcachefs 的 parent identity / route error 分支。
- 本检查只保护 Rust 当前 merge 的不可回滚窗口，不改变 btree ID 的含义，也不实现 fs
  相关 payload。

### 3. Validation

- `timeout 60s cargo check -p subvol-core --lib`
- `timeout 60s cargo test -q -p subvol-core btree::btree::tests::test_leaf_merge_after_delete --lib -- --test-threads=1`
- `timeout 60s cargo test -q -p subvol-core btree::btree::tests::test_btree_multi_level_delete --lib -- --test-threads=1`
- `timeout 60s cargo test -q -p subvol-core btree::btree::tests::test_btree_multi_level_insert_after_delete --lib -- --test-threads=1`
- `git diff --check`
