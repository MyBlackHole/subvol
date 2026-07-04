# Recovery Coverage

> Recovery 模块函数级覆盖地图

## 覆盖统计

| 状态 | 数量 | 说明 |
|------|------|------|
| ✅ | 12 | 完全对齐 |
| ⚠️ | 0 | 已知偏差 |
| ❓ | 0 | 未验证 |
| ➖ | 10 | subvolmount 特有 |
| **总计** | **22** (subvolmount 侧) | |

> 源文件: `crates/subvol-core/src/recovery/` (mod.rs, passes/*.rs)
> 参考实现: bcachefs `fs/init/recovery.c` + `fs/init/passes.c` + `fs/init/passes_format.h`

## Journal replay boundary (2026-07-18)

- 本地 `journal_keys` 是 bcachefs journal replay 的内部状态，不是独立的文件系统层。
- 不维护额外的 overlay、读穿透或 drain API；replay 直接按 bcachefs
  的 journal/btree 顺序 materialize，普通读直接走 btree。

## 函数状态表

### 恢复驱动（9）

| 函数 | bcachefs 对应 | 参考 | 状态 |
|------|---------------|------|------|
| `bch2_fs_recovery` | `bch2_fs_recovery` | `recovery.c:608` | ✅ |
| `bch2_fs_initialize` | `bch2_fs_initialize` | `recovery.c:953` | ✅ |
| `run_recovery` | — | subvolmount 封装 | ➖ |
| `bch2_run_recovery_passes` | `bch2_run_recovery_passes` | `passes.c:602` | ✅ |
| `bch2_run_recovery_passes_startup` | — | subvolmount 拆分 | ➖ |
| `bch2_restart_recovery` | 内联逻辑 | `recovery.c` | ✅ |
| `bch2_rewind_recovery` | 内联逻辑 | `recovery.c` | ✅ |
| `compute_passes_to_run` | `bch2_run_recovery_passes` mask assembly | `passes.c:604-609` | ✅ |
| `compute_passes_with_flag` | — | 辅助函数 | ➖ |

### Pass 表管理（4）

| 函数 | bcachefs 对应 | 参考 | 状态 |
|------|---------------|------|------|
| `bch2_recovery_pass_to_stable` | `bch2_recovery_pass_to_stable` | `passes_format.h` | ✅ |
| `bch2_recovery_pass_from_stable` | `bch2_recovery_pass_from_stable` | `passes_format.h` | ✅ |
| `bch2_recovery_pass_done` | `bch2_recovery_pass_done` | `recovery.c` | ✅ |
| `bch2_run_recovery_pass` dispatch | `bch2_run_recovery_pass` | `passes.c` | ✅ |

### Recovery Pass 实现（16 pass 文件）

| Pass 文件 | bcachefs pass | 参考 | 状态 |
|-----------|---------------|------|------|
| `journal_read` | journal read (pre-pass) | `recovery.c` | ✅ |
| `accounting_read` | `accounting_read` (stable=39) | `passes_format.h:27` | ✅ |
| `alloc_read` | `alloc_read` (stable=0) | `passes_format.h:28` | ✅ |
| `btree_roots` | `read_btree_roots` | `recovery.c:567` | ✅ |
| `snapshots_read` | `snapshots_read` (stable=3) | `passes_format.h:31` | ✅ |
| `check_topology` | `check_topology` (stable=4, flags=0) | `passes_format.h:26` | ✅ |
| `check_allocations` | `check_allocations` (stable=5, FSCK\|ALLOC) | `passes_format.h:33` | ✅ |
| `trans_mark_dev_sbs` | `trans_mark_dev_sbs` (stable=6, ALWAYS\|SILENT\|ALLOC) | `passes_format.h:34` | ✅ |
| `fs_journal_alloc` | `fs_journal_alloc` (stable=7, ALWAYS\|SILENT\|ALLOC) | `passes_format.h:35` | ✅ |
| `set_may_go_rw` | `set_may_go_rw` (stable=8, ALWAYS\|SILENT) | `passes_format.h:36` | ✅ |
| `journal_replay` | `journal_replay` (stable=9, ALWAYS) | `passes_format.h:37` | ✅ |
| `presplit_shard_boundaries` | — (stable=48, subvolmount 特有) | `passes_format.h` 无此 pass | ➖ |
| `check_alloc_info` | `check_alloc_info` (stable=10, ONLINE\|FSCK\|ALLOC) | `passes_format.h:38` | ✅ |
| `fs_freespace_init` | `fs_freespace_init` (stable=16, ALWAYS\|SILENT) | `passes_format.h:44` | ✅ |
| `bucket_gens_init` | `bucket_gens_init` (stable=17, flags=0) | `passes_format.h:45` | ✅ |
| `check_snapshots` | `check_snapshots` (stable=19, ONLINE\|FSCK) | `passes_format.h:48` | ✅ |

### Pass Flags 对照（已验证，2026-07-05）

| 运行时 Pass | Bcachefs flags | Volmount flags | 一致 |
|-------------|----------------|----------------|------|
| check_topology | — | — | ✅ 均为 0 |
| accounting_read | ALWAYS | ALWAYS | ✅ |
| alloc_read | ALWAYS | ALWAYS | ✅ |
| snapshots_read | ALWAYS | ALWAYS | ✅ |
| check_allocations | FSCK\|ALLOC | FSCK\|ALLOC | ✅ |
| trans_mark_dev_sbs | ALWAYS\|SILENT\|ALLOC | ALWAYS\|SILENT\|ALLOC | ✅ |
| fs_journal_alloc | ALWAYS\|SILENT\|ALLOC | ALWAYS\|SILENT\|ALLOC | ✅ |
| set_may_go_rw | ALWAYS\|SILENT | ALWAYS\|SILENT | ✅ |
| journal_replay | ALWAYS | ALWAYS | ✅ |
| presplit_shard_boundaries | — (subvolmount 特有) | ALWAYS | ➖ |
| check_alloc_info | ONLINE\|FSCK\|ALLOC | ONLINE\|FSCK\|ALLOC | ✅ |
| fs_freespace_init | ALWAYS\|SILENT | ALWAYS\|SILENT | ✅ |
| bucket_gens_init | — | — | ✅ 均为 0 |
| check_snapshots | ONLINE\|FSCK | ONLINE\|FSCK | ✅ |
| lookup_root_inode | ALWAYS\|SILENT | ALWAYS\|SILENT | ✅ |

### Bcachefs 兼容层（3）

| 函数 | bcachefs 对应 | 参考 | 状态 |
|------|---------------|------|------|
| `bch2_reconstruct_alloc` | `bch2_reconstruct_alloc` | `recovery.c:138` | ✅ |
| `RecoveryState` | `struct bch_fs_recovery` | `recovery.c` | ✅ |
| `BchSb.recovery_passes_required` | `bch_sb_field_ext.recovery_passes_required` | `passes_format.h:93` | ✅ |

### Volmount 特有函数（➖）

| 函数 | 说明 |
|------|------|
| `run_recovery` | 简洁恢复入口，供 daemon 层使用 |
| `bch2_run_recovery_passes_startup` | 启动 recovery 的 pass 运行包装 |
| `bch2_restart_recovery` | 重启 recovery 流程 |
| `bch2_rewind_recovery` | 回退到指定 pass |
| `compute_passes_with_flag` | 辅助函数，计算具有指定标志的 pass 位掩码 |
| `presplit_shard_boundaries` pass | subvolmount 特有 pass |
| `lookup_root_inode` pass | subvolmount 特有 pass |
| `RecoveryState::new` | Rust 构造器 |
| `RecoveryState::restore_progress` | crash 恢复进度还原 |
| `RecoveryState::sync_to_superblock` | recovery 完成后同步回 superblock |

## 偏差说明

无已知偏差 — 所有 15 个运行时 pass 的 flags 已与 bcachefs `passes_format.h` 逐对验证一致。

### 运行时一致性

- `check_snapshots` 的修复结果在写回 Snapshots btree 后，会同步刷新共享 `SnapshotRuntime`，避免恢复 pass 结束后继续读取到旧的 `SUBVOL` / `depth` / `skip` 值。
- 这条规则与 `subvolume_create` 的 root snapshot 绑定逻辑保持一致：任何影响 SnapshotT 可见字段的恢复/创建路径都要同时更新运行时视图。

## Bcachefs 独有函数（不实现）

以下 bcachefs recovery.c 函数为多设备/FSCK 场景，subvolmount 不涉及：

| 函数 | 文件:行 | 不实现原因 |
|------|---------|-----------|
| `kill_btree` | recovery.c:131 | btree 损坏修复，subvolmount 通过 engine 自身校验 |
| `bch2_btree_lost_data` | recovery.c:46 | 多设备数据降级，subvol 单设备 |
| `zero_out_btree_mem_ptr` | recovery.c:195 | kernel 内存管理（subvol 无 kernel 层） |
| `journal_sort_seq_cmp` / `replay_now_at` | recovery.c | journal 模块已有等价逻辑 |
| `journal_replay_entry_early` | recovery.c:471 | journal 模块已处理 |
| 33 个 FSCK/repair passes | passes_format.h | subvol 无文件系统，无 inode/extent/dirent 等概念 |

## 关键差异

- **Pass 数量**: bcachefs 有 ~48 个稳定 pass ID，subvol 实现了 15 个。其余 33 个为 FSCK/repair 类，subvol 无文件系统无需实现。
- **Journal replay**: bcachefs 的完整 journal replay 路径（排序、early entry 处理、key replay 事务提交）在 subvol 中通过 `Journal` 模块独立处理，recovery 层不直接参与。
- **Pass dispatch**: subvol 使用 `trailing_zeros()` (对应 `__ffs64`) 迭代，与 bcachefs `passes.c` 一致。额外增加了 fail-retry 循环和 RewindRecovery 信号。
- **Root level forwarding**: `journal_read` / `btree_roots` 会把恢复阶段合并出来的 root level 显式传给 `load_root()`，避免把 journal/superblock 的层级信息白白丢掉。
### 2026-07-17 API 可见性复核

- 本地 `fs/init/passes.c` 中 `bch2_recovery_pass_to_stable`、`bch2_recovery_pass_from_stable` 和 `fs/init/recovery.c` 中 `bch2_reconstruct_alloc` 为静态函数；subvol 对应 helper 已限制为 crate 内部，保持 recovery pass 映射与 alloc 重建流程不变。
