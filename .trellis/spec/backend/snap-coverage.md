# Snap — 快照模块覆盖地图

> 生成日期: 2026-07-04
> 源文件: `crates/subvol-core/src/snap/` (snapshot.rs, meta.rs, table.rs)
> 参考实现: bcachefs `fs/snapshots/snapshot.c` + `fs/snapshots/snapshot.h` + `fs/snapshots/delete.c`

## 覆盖统计

| 状态 | 数量 | 说明 |
|------|------|------|
| ✅ | 35 | 完全对齐（含 4 项之前错标→修正） |
| ⚠️ | 0 | 已知偏差 |
| ❓ | 0 | 未验证 |
| ➖ | 10 | subvol 特有 |
| **总计** | **45** | |

## 函数状态表

### snapshot.rs — 顶层公有函数

| 函数 | bcachefs 对应 | 参考 | 状态 |
|------|---------------|------|------|
| `read_snapshot_value` | `bch2_snapshot_lookup` | `snapshot.h` + `SnapshotRuntime` | ✅ 先查 runtime，再回退 btree |
| `read_snapshot_value_allow_deleted` | — | — | ➖ |
| `list_snapshots_from_btree` | — | Volmount 扩展 | ➖ |
| `is_ancestor_from_btree` | `__bch2_snapshot_is_ancestor` | `snapshot.h` | ✅ skip[2→1→0]+bitmap+parent |
| `bch2_snapshot_is_ancestor_btree` | `bch2_snapshot_is_ancestor` | `snapshot.h` | ✅ 委托 is_ancestor_from_btree |
| `get_next_snapshot_id` | `create_snapids` / `bch2_bkey_get_empty_slot` | `snapshot.c` | ✅ 事务视图空槽分配（含 pending journal，直接扫描事务可见 btree） |
| `bch2_snapshot_skiplist_good` | — | Volmount 扩展，skip 合法性检查 | ➖ |
| `bch2_snapshot_skiplist_get` | `bch2_snapshot_skiplist_get` | `check_snapshots.c:211-221` | ✅ |
| `create_root_snapshot_btree` | — | Volmount 扩展，btree 创建根快照 | ➖ |
| `bch2_snapshot_node_create` | `bch2_snapshot_node_create` | `snapshot.c` | ✅ |
| `bch2_snapshot_node_set_deleted` | `bch2_snapshot_node_set_deleted` | `snapshot.c` | ✅ |
| `read_snapshot_tree_value` | `bch2_snapshot_tree_lookup` | `snapshot.h` | ✅ typed lookup + 缺失报错 |
| `write_snapshot_tree_value` | — | Volmount 扩展，btree 写入 SnapshotTreeT | ➖ |
| `bch2_snapshot_tree_master_subvol` | `bch2_snapshot_tree_set_master_subvol` | `snapshot.c` | ✅ |
| `dfs_descendants` | — | Volmount 扩展，递归后序 DFS | ➖ |
| `dfs_descendants_alive` | — | Volmount 扩展，过滤已删除 | ➖ |
| `bch2_snapshot_node_delete` | `bch2_snapshot_node_delete` | `delete.c:167-290` | ✅ |
| `bch2_fix_child_of_deleted_snapshot` | `bch2_fix_child_of_deleted_snapshot` | `delete.c:611-662` | ✅ depth 递减；仅替换命中 deleted 的 skip 槽位；逐层跳过 deleted 祖先后排序 |
| `bch2_check_snapshot_needs_deletion` | `bch2_check_snapshot_needs_deletion` | `delete.c:853-878` | ✅ |
| `check_should_delete_snapshot` | `check_should_delete_snapshot` | `delete.c:532-610` | ✅ |
| `bch2_delete_dead_snapshots` | `bch2_delete_dead_snapshots` | `delete.c` | ✅ |
| `bch2_delete_dead_interior_snapshots` | `bch2_delete_dead_interior_snapshots` | `delete.c:811-851` | ✅ |
| `bch2_check_key_has_snapshot` | `bch2_check_key_has_snapshot` | `check_snapshots.c` | ✅ |
| `bch2_reconstruct_snapshots` | `bch2_reconstruct_snapshots` | `check_snapshots.c` | ✅ |

### snapshot.rs — 公有类型方法

| 方法 | bcachefs 对应 | 状态 |
|------|---------------|------|
| `DfsIter::new` | — | ➖ |

### meta.rs — 公有类型与方法

| 函数/方法 | bcachefs 对应 | 参考 | 状态 |
|-----------|---------------|------|------|
| `SnapshotMeta::from_value` | — | Volmount 扩展 | ➖ |
| `SnapshotT::new_leaf` | `struct snapshot_t` 构造 | `snapshot.h` | ✅ |
| `SnapshotT::new_interior` | `struct snapshot_t` 构造 | `snapshot.h` | ✅ |
| `SnapshotT::mark_deleted` | `snapshot_t` WILL_DELETE | `snapshot.h` | ✅ |
| `SnapshotT::is_leaf` | leaf 判断 | `snapshot.h` | ✅ |
| `SnapshotT::is_interior` | interior 判断 | `snapshot.h` | ✅ |
| `SnapshotT::has_subvol` | subvol 存在判断 | `snapshot.h` | ✅ |
| `SnapshotT::deleted_placeholder` | — | 占位构造 | ➖ |
| `SnapshotTreeT::new` | `bch_snapshot_tree` 构造 | `snapshot.h` | ✅ 仅含 `master_subvol` + `root_snapshot` |
| `SnapshotIdState` | `enum snapshot_id_state` | `snapshot.h` | ✅ |
| `BchSnapshotFlags` | `BCH_SNAPSHOT_*` bitmask | `snapshot.h` | ✅ |

### table.rs — SnapshotTable 方法

| 方法 | bcachefs 对应 | 参考 | 状态 |
|------|---------------|------|------|
| `SnapshotTable::build` | snapshot_table 构建 | `snapshot.c` | ✅ |
| `SnapshotTable::get` | 表查找 | — | ✅ Vec 数组索引 O(1) |
| `SnapshotTable::parent` | `bch2_snapshot_parent` | `snapshot.h` | ✅ |
| `SnapshotTable::root` | `bch2_snapshot_root` | `snapshot.h` | ✅ |
| `SnapshotTable::children` | `snapshot_t.children` | 子节点数组 | ✅ |
| `SnapshotTable::depth` | `bch2_snapshot_depth` | `snapshot.h` | ✅ |
| `SnapshotTable::exists` | `bch2_snapshot_exists` | `snapshot.h` | ✅ |
| `SnapshotTable::id_state` | `bch2_snapshot_id_state` | `snapshot.h` | ✅ |
| `SnapshotTable::next_empty_id` | — | 事务分配内部 helper | ➖ |
| `SnapshotTable::is_ancestor` | `bch2_snapshot_is_ancestor` | `snapshot.h` (bitmap 版) | ✅ |

### table.rs — SnapshotTreeTable 方法

| 方法 | bcachefs 对应 | 状态 |
|------|---------------|------|
| `SnapshotTreeTable::build` | snapshot_tree 构建 | ✅ |
| `SnapshotTreeTable::get` | 表查找 | ➖ |
| `SnapshotTreeTable::root_snapshots` | — | ➖ |
| `SnapshotTreeTable::master_subvols` | — | ➖ |
| `SnapshotTreeTable::len` | — | ➖ |
| `SnapshotTreeTable::is_empty` | — | ➖ |

### table.rs — SnapshotRuntime 方法

| 方法 | bcachefs 对应 | 状态 |
|------|---------------|------|
| `SnapshotRuntime::from_tables` | `c->snapshots.table` + `c->snapshot_trees` 装载 | ✅ |
| `SnapshotRuntime::snapshot` | `bch2_snapshot_lookup` 的实时表视图 | ✅ |
| `SnapshotRuntime::snapshot_allow_deleted` | `bch2_snapshot_lookup` 的 whiteout 可见路径 | ✅ |
| `SnapshotRuntime::set_snapshot` | `bch2_snapshot_t_mut()` 后的内存更新 | ✅ |
| `SnapshotRuntime::install_tables` | `bch2_snapshots_read()` 后的整体装载 | ✅ |

### table.rs — 顶层函数

| 函数 | bcachefs 对应 | 参考 | 状态 |
|------|---------------|------|------|
| `bch2_snapshots_read` | `bch2_snapshots_read` | `snapshot.c` | ✅ |
| `bch2_fs_snapshots_init` | `bch2_fs_snapshots_init` | `snapshot.c` | ✅ |
| `bch2_fs_snapshots_exit` | `bch2_fs_snapshots_exit` | `snapshot.c` | ✅ |

## 偏差说明

| 函数 | 偏差类型 | 说明 |
|------|----------|------|
| `read_snapshot_value` | ✅ runtime-first | 先读共享 `SnapshotRuntime`，再回退 btree，避免绕开刚提交的可见状态 |
| `read_snapshot_tree_value` | ✅ typed lookup | 使用事务视图做 typed lookup；缺失、非 Raw、反序列化失败都返回错误，语义对齐 `bch2_snapshot_tree_lookup()` |
| `get_next_snapshot_id` | ✅ 事务视图空槽分配 | 必须以当前事务可见的 Snapshots 位置为准，不能用隐藏的单调 hint 或仅 runtime 缓存；pending journal 插入也要占位 |
| `bch2_snapshot_node_set_deleted` | ✅ live table 保留 | 仅设置 `WILL_DELETE` 并清空 `SUBVOL`，节点在物理删除前仍保留在 live table，可继续参与祖先和清理流程 |
| `SnapshotT::mark_deleted` | `bch2_snapshot_node_set_deleted()` | `delete.c:115-140` | ✅ 保留 parent / skip / is_ancestor，仅切换删除状态 |
| `bch2_snapshot_node_set_deleted` | `bch2_snapshot_node_set_deleted()` | `delete.c:115-140` | ✅ 清 `SUBVOL` / `subvol`，写 `WILL_DELETE` 白洞 |

## Volmount 特有函数 (➖)

所有 `➖` 标注的函数均为 subvolmount 扩展，无 bcachefs 直接对应。详见上方表格。
### 2026-07-17 API 可见性复核

- 本地 `fs/snapshots/` 没有 subvol 的 `bch2_snapshot_read_value/list/next_id/root_create` 等同名导出；这些 helper 仅供 crate 内 snapshot、recovery 和 volume 路径调用，已收敛为 crate 内部。
- 本地 `snapshot.h:275-277` 明确将 `bch2_snapshot_node_create()` 标记为仅测试导出；subvol 对应入口保持 `pub(crate)`，而 `bch2_snapshot_lookup/tree_lookup/node_set_deleted` 等头文件公开 API 才从 `snap` 根模块公开。

### 2026-07-18 snapshot_node_create 参数对齐

- 以本地 `snapshot.h:275-277` 和 `snapshot.c:715-785` 为唯一依据，
  `bch2_snapshot_node_create()` 采用 `parent, new_snapids,
  snapshot_subvols, nr_snapids` 形状；不再保留 subvol 自定义的
  `subvol + Option<extra_child_subvol>` 入口。
- 保留本地约束：`parent == 0` 时 `nr_snapids == 1` 创建新树 root；
  `parent != 0` 时 `nr_snapids == 2` 一次创建两个 children 并更新父节点。
- root tree ID 和 snapshot ID 分配都读取当前事务视图，避免同一事务中的
  pending journal key 被再次分配。
