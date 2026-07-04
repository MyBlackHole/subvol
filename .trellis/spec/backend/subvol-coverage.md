# Subvol — 子卷模块覆盖地图

> 生成日期: 2026-07-04
> 源文件: `crates/subvol-core/src/subvol/` (ops.rs, types.rs)
> 参考实现: bcachefs `fs/snapshots/subvolume.c` + `fs/snapshots/subvolume.h`

## 覆盖统计

| 状态 | 数量 | 说明 |
|------|------|------|
| ✅ | 12 | 完全对齐 |
| ⚠️ | 0 | 已知偏差 |
| ❓ | 0 | 未验证 |
| ➖ | 9 | subvolmount 特有 |
| **总计** | **21** | |

## 函数状态表

### ops.rs — 顶层公有函数

| 函数 | bcachefs 对应 | 参考 | 状态 |
|------|---------------|------|------|
| `bch2_subvolume_create` | `bch2_subvolume_create` | `subvolume.c` | ✅ |
| `bch2_subvolume_snapshot` | `bch2_subvolume_create` (snapshot 模式) | `subvolume.c` | ✅ |
| `bch2_subvolume_get` | `bch2_subvolume_get` | `subvolume.c` | ✅ |
| `bch2_subvolume_get_snapshot` | `bch2_subvolume_get_snapshot` | `subvolume.c` | ✅ |
| `bch2_subvolume_trigger` | `bch2_subvolume_trigger` | `subvolume.c` (read-only 校验) | ✅ |
| `bch2_subvol_is_ro` | `bch2_subvol_is_ro` | `subvolume.c` | ✅ |
| `bch2_snapshot_get_subvol` | `bch2_snapshot_get_subvol` | `subvolume.c` | ✅ |

`bch2_snapshot_get_subvol` 使用本地 bcachefs 的输出参数式契约：调用方传入
`struct bch_subvolume *` 对应的 Rust `&mut BchSubvolume`，查找失败通过
`Result<(), StorageError>` 返回；不得恢复为 `Option<BchSubvolume>`，以免丢失
snapshot/subvolume lookup 的错误分支。
| `bch2_subvolume_delete` | `bch2_subvolume_unlink` + `bch2_subvolume_delete` | `subvolume.c` | ➖ 合并（保留 bcachefs 的 reparent + master_subvol 清理语义）|
| `bch2_subvolume_list` | — | Volmount 扩展：遍历 btree 列出全部子卷；bcachefs 无直接 API（通过 VFS 遍历） | ➖ |
| `bch2_subvolume_count` | — | Volmount 扩展 | ➖ |
| `bch2_subvolumes_reparent` | `bch2_subvolumes_reparent` | `subvolume.c` | ✅ |
| `bch2_initialize_subvolumes` | `bch2_initialize_subvolumes` | `subvolume.c:653-681` | ✅ |

### 运行时同步注意

- `bch2_subvolume_create()` 在把 root snapshot 绑定到真实 `tree` / `subvol` 后，必须刷新共享 `SnapshotRuntime`，否则后续读路径会继续看到 `tree=0` 的旧值。
- `bch2_subvolume_create()` 的 `snapshot_tree` ID 分配必须取最小空槽，而不是 `max+1`，否则删后重建会偏离 bcachefs 的空槽复用语义。
- `bch2_subvolume_delete()` 及其 reparent/master_subvol 清理路径仍以 btree 为准；若后续补充更多 runtime 缓存字段，也要保持同一写回顺序。

### types.rs — 常量

| 常量 | bcachefs 对应 | 参考 | 状态 |
|------|---------------|------|------|
| `BCACHEFS_ROOT_INO` | `BCACHEFS_ROOT_INO` | `subvolume.h` | ✅ |
| `BCACHEFS_ROOT_SUBVOL` | `BCACHEFS_ROOT_SUBVOL` | `subvolume.h` | ✅ |

### types.rs — BchSubvolumeFlags 方法

| 方法 | bcachefs 对应 | 状态 |
|------|---------------|------|
| `empty` | Bitmask 构造 | ✅ |
| `contains` | Bitmask 检查 | ✅ |
| `insert` | Bitmask 设置 | ✅ |
| `remove` | Bitmask 清除 | ✅ |

### types.rs — BchSubvolume 方法

| 方法 | bcachefs 对应 | 参考 | 状态 |
|------|---------------|------|------|
| `new` | — | 构造器 | ➖ |
| `new_snapshot` | — | 快照子卷构造器 | ➖ |
| `mark_unlinked` | `SET_BCH_SUBVOLUME_UNLINKED` | `subvolume.h` | ✅ |
| `is_read_only` | `BCH_SUBVOLUME_RO` 检查 | `subvolume.h` | ✅ |
| `is_snapshot` | `BCH_SUBVOLUME_SNAP` 检查 | `subvolume.h` | ✅ |
| `is_unlinked` | `BCH_SUBVOLUME_UNLINKED` 检查 | `subvolume.h` | ✅ |
| `set_read_only` | `SET_BCH_SUBVOLUME_RO` | `subvolume.h` | ✅ |
| `set_snapshot` | `SET_BCH_SUBVOLUME_SNAP` | `subvolume.h` | ✅ |
| `to_bytes` | — | 序列化 | ➖ |
| `from_bytes` | — | 反序列化 | ➖ |

## 偏差说明

| 函数 | 偏差类型 | 说明 |
|------|----------|------|
| `bch2_subvolume_delete` | ➖ 合并操作（见代码注释） | bcachefs 将 unlink 和 delete 分为两步（`bch2_subvolume_unlink` + `bch2_subvolume_delete`），subvolmount 仍合并为一步 `bch2_subvolume_delete`，但已补齐 `bch2_subvolumes_reparent()` 与 snapshot tree `master_subvol` 清理。同步上下文中不影响正确性。 |

## Volmount 特有函数 (➖)

| 函数 | 说明 |
|------|------|
| `bch2_subvolume_count` | Volmount 扩展，计数子卷 |
| `BchSubvolume::new` | Rust 构造器，无 bcachefs 直接对应 |
| `BchSubvolume::new_snapshot` | Rust 构造器，快照子卷专用 |
| `BchSubvolume::to_bytes` | Rust 序列化 |
| `BchSubvolume::from_bytes` | Rust 反序列化 |

## bcachefs 未实现

以下 bcachefs subvolume API 在 subvol 中无对应：
- `bch2_subvolume_unlink` — 被合并到 `bch2_subvolume_delete`
### 2026-07-17 API 可见性复核

- 本地 bcachefs 没有 `bch2_subvolume_snapshot/list/count` 的同名导出；subvol 保留 crate 内调用并收敛 re-export 可见性，行为与快照/子卷流程不变。

### 2026-07-18 API 返回语义复核

- 本地 `bch2_subvol_is_ro()` (`fs/snapshots/subvolume.c:323-329`) 是写入准入检查：可写返回 0，只读或 `UNLINKED` 返回 `-EROFS`。subvol 的对应 Rust API 使用 `Result<(), StorageError>`，只读映射为 `PermissionDenied`，不再以 `Ok(true)` 把拒绝状态伪装成成功。
- 本地 `bch2_subvolume_get_snapshot()` (`fs/snapshots/subvolume.c:353-370`) 通过返回码报告缺失 subvol；subvol 对应 API 使用 `Result<u32, StorageError>`，不再用 `Option<u32>` 静默吞掉缺失/损坏状态。
- 本地 `bch2_subvolume_get()` (`fs/snapshots/subvolume.c:298-313`) 带有 `inconsistent_if_not_found` 参数并以返回码报告缺失；subvol 对应 API 已采用同样的显式布尔参数与 `Result<BchSubvolume, StorageError>` 返回形态。
- `bch2_subvolume_create()` 的空槽搜索必须读取事务可见视图；subvol 通过 `BtreeTrans::get_entry()` 同时覆盖已落盘 Subvolumes B-tree 与当前事务 pending journal，避免同一事务重复分配 subvol ID。

### 2026-07-18 创建 API 签名与 flag 语义

- `bch2_subvolume_create()` 已对齐本地 `fs/snapshots/subvolume.h:136` 的参数顺序与输出参数：`inode`、`parent_subvolid`、`src_subvolid`、`new_subvolid`、`new_snapshotid`、`new_subvol_out`、`ro`。
- 本地 `fs/snapshots/subvolume.c:621-622` 只根据 `ro` 设置 RO；`src_subvolid != 0` 只设置 SNAP。Rust 回归测试固定这一语义，避免把所有 snapshot 子卷隐式变成 RO。
- `size` 与 `otime_lo` 是 subvol 的序列化扩展字段，不属于本地 bcachefs 创建 API；调用者通过 `new_subvol_out` 预置这两个扩展值，创建逻辑仍按 bcachefs 的字段初始化顺序写回输出对象。
