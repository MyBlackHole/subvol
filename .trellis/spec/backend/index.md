# Backend Development Guidelines

> Best practices for backend development in this project.

---

## Overview

This directory contains guidelines for backend development. The project is a Rust reimplementation of bcachefs's COW BTree storage engine.

**关键原则**: 所有核心模块的 API 命名和语义必须与 bcachefs C 源码 (`fs/` 目录) 保持 100% 一致。主参考源码仓库使用本地 bcachefs 源码树，实际对照文件位于其 `fs/` 子目录。

---

## Guidelines Index

| Guide | Description | Status |
|-------|-------------|--------|
| [Directory Structure](./directory-structure.md) | Crate 架构 + 模块 bcachefs 映射 + 子模块职责 bcachefs 语义对齐 | ✅ Updated (2026-06-28) |
| [Database Guidelines](./database-guidelines.md) | ORM patterns, queries, migrations | N/A (no DB) |
| [Error Handling](./error-handling.md) | Error types, handling strategies | To fill |
| [Quality Guidelines](./quality-guidelines.md) | bcachefs API 对齐约定 + 设计决策 + Batch A-D 验证 | ✅ Updated (2026-06-28) |
| [Logging Guidelines](./logging-guidelines.md) | Structured logging, log levels | To fill |
| [Btree Transaction](./btree-transaction.md) | BtreeTrans 函数覆盖地图 + 设计决策 + 待办 | ✅ Updated (2026-07-05) 80% ✅, 0% ⚠️, 0% ❓, 20% ➖ |
| [Lock Concurrency](./lock-concurrency.md) | SixLock 函数覆盖地图 + WaitFifo URCU + DeadlockDetector | ✅ Created (2026-06-29) 100% ✅, 0% ❓ |
| [Alloc Coverage](./alloc-coverage.md) | Alloc 模块函数级覆盖地图（99 pub fn） | ✅ Updated (2026-07-14)；bch_alloc_v4 固定布局与 per-device dynamic bucket geometry 已对齐 |
| [Journal Coverage](../journal/function-coverage.md) | Journal 模块函数级覆盖地图（135 fn） | ✅ Updated (2026-07-04) 61.5% ✅, 0% ⚠️, 0% ❓, 51 ➖ |
| [Snap Coverage](./snap-coverage.md) | Snap 模块函数级覆盖地图 + bcachefs 未实现项 | ✅ Updated (2026-07-05) 65.4% ✅, 0 ⚠️, 0 ❓, 18 ➖（4 ➖ → ✅ 已对齐修正） |
| [Subvol Coverage](./subvol-coverage.md) | Subvol 模块函数级覆盖地图 + 偏差说明 | ✅ Updated (2026-07-05) 57.1% ✅, 0 ⚠️, 0 ❓, 9 ➖（1 ⚠️ → ✅ 计数修正） |
| [Recovery Coverage](./recovery-coverage.md) | Recovery 模块函数级覆盖地图 | ✅ Updated (2026-07-05) 54.5% ✅, 0% ⚠️, 0% ❓, 45.5% ➖；flags 对齐 + `bch2_reconstruct_alloc` 补齐 |
| [Volume Coverage](./volume-coverage.md) | Volume 模块函数级覆盖地图（35 fn） | ✅ Updated (2026-07-05) 54.3% ✅, 0 ⚠️, 0 ❓, 16 ➖（3 ⚠️ → ➖ 架构差异） |
| [Btree IO Coverage](./btree-io-coverage.md) | Btree IO 模块函数级覆盖地图（26 fn） | ✅ Updated (2026-07-05) 88.5% ✅, 0 ⚠️, 0 ❓, 4 ➖（1 ⚠️ → ➖ 架构差异） |
| [Btree Interior Coverage](./btree-interior-coverage.md) | Btree internal-node helper 覆盖地图 | Partial (2026-07-17)：4 个 static-inline helper 已逐行核对，其余路径待审计 |
| [Btree Cache Coverage](./btree-cache-coverage.md) | BtreeCache 模块函数级覆盖地图 | ✅ Updated (2026-07-05) 59.6% ✅, 0 ⚠️, 0 ❓, 23 ➖ |

---

## Key Context

### 参考源码

```
bcachefs 源码树
├── six.c/h              # six_lock 读写意向锁
├── btree/               # btree 全部子模块
├── alloc_background.c/h  # 分配器后端
├── alloc_foreground.c/h  # 分配器前台
├── buckets.c/h           # bucket 管理
├── journal*.c/h          # WAL 流水线
├── snapshots.c/h         # 快照 skip_list
├── subvolume.c/h         # 子卷管理
└── recovery.c/h          # 恢复 pass
```

### 对齐策略

1. 按批执行：lock+volume → btree(5 子批) → alloc+journal → snap+subvol+recovery
2. 每批只做 API 命名和字段对齐，不改动未对齐模块
3. **每次"对齐"修改都必须先读 bcachefs 源码验证** — 见 [bcachefs 对齐验证指南](../guides/bcachefs-alignment-guide.md)
4. 验证：`cargo build` + `cargo test -p subvol-core --lib` + `cargo clippy --all-targets`
5. subvolmountd 集成测试 (`test_create_and_init_nfs_volume` 等) 已知预存失败 (`AddressSpaceExhausted`)
6. `btree/update.rs` 的 `BtreeInteriorUpdate` 只在 `mark_done()` 时清理 `old_nodes`；`btree_id`、`mode`、node span、level span、进度计数和 `nodes_written` 需要保留到完成后，便于后续检查和文本输出对齐。

### 核心模块依赖关系

```
recovery → journal → btree → alloc → lock
        ↘         ↘         ↘
         subvol → snap       volume → storage
                              ↕
                            subvolmountd (HTTP/NBD)
```
