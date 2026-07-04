# Directory Structure

> How backend code is organized in this project.

---

## Overview

Volmount-core 有 8 个核心存储引擎模块，全部对齐 bcachefs C 源码的实现：
alloc / btree / journal / lock / volume / snap / subvol / recovery

每个模块对应 bcachefs `fs/` 下的一组 `.c/.h` 文件。上层（subvolmountd、subvolmount、subvol-nbd）作为独立 crate 通过 HTTP/协议接口消费 core。

---

## Crate 架构

```
subvol-core/          核心引擎（8 模块 + storage/meta 等辅模块）
├── src/
│   ├── alloc/          bucket 分配器 + 后台 GC + freespace
│   ├── btree/          COW BTree（6 子模块：iter/trans/cache/io/interior/gc）
│   ├── journal/        WAL 流水线 + reclaim + blacklist
│   ├── lock/           six_lock 读写意向锁
│   ├── volume/         卷生命周期 + 状态机
│   ├── snap/           快照 skip_list + 祖先缓存
│   ├── subvol/         子卷创建/删除/快照
│   ├── recovery/       有序恢复 pass + journal replay
│   ├── storage/        后端存储抽象（block device / superblock / nfs）
│   └── meta/           卷元数据
├── subvolmountd/          HTTP 守护进程（axum）+ NBD 导出 + CLI 子命令
├── subvolmount/           CLI 客户端（ureq 调用 subvolmountd API）
└── subvol-nbd/       NBD 协议实现（TCP + io_uring）
```

---

## 模块 bcachefs 映射

| subvol 模块 | bcachefs 参考文件 | 关键函数/类型前缀 |
|---------------|-------------------|-----------------|
| `alloc/` | `alloc_background.c/h`, `buckets.c/h`, `open_buckets.c/h` | `bch2_alloc_*`, `bch2_bucket_*`, `bch2_open_bucket_*` |
| `btree/` | `btree/` 下全部子文件 | `bch2_btree_*`, `bch2_trans_*`, `bch2_bset_*` |
| `journal/` | `journal.c/h`, `journal_reclaim.c`, `journal_io.c`, `journal_seq_blacklist.c` | `bch2_journal_*` |
| `lock/` | `six.c/h` | `six_lock_*` |
| `volume/` | `fs.c/h`, `recovery.c` (部分) | `bch2_fs_*` |
| `snap/` | `snapshots.c/h`, `snapshot_types.h` | `bch2_snapshot_*` |
| `subvol/` | `subvolume.c/h`, `subvolume_types.h` | `bch2_subvolume_*` |
| `recovery/` | `recovery.c/h`, `recovery_passes.h` | `bch2_fs_recovery`, `BchRecoveryPass` |

---

## BTree 内部子模块（bcachefs 语义对齐）

每个子模块的职责描述包含：Rust 功能 + bcachefs C 源文件参考 + 函数前缀约定。

```
btree/
├── mod.rs           # BtreeEngine + BtreeId + pub 导出
│                    # bcachefs: btree.h, init.c — bch2_btree_id_*, BtreeEngine::recover
│
├── btree.rs         # Btree 主结构: insert/delete/get/查路/split/compact/merge
│                    # bcachefs: btree.h — bch2_btree_insert/delete/get/split_race/compact
│                    # 新增: insert_entry_skip_cache() (flush 路径, 不 invalidation key cache)
│                    #       insert_entry_into_node() (提取共用节点插入逻辑)
│
├── bucket_io.rs     # bucket 级 I/O: 为 btree 节点分配/释放 bucket
│                    # bcachefs: io.c/h — bch2_btree_io_*, btree_node_read/write_all
│
├── cache.rs         # 节点缓存: LRU + 两阶段时钟 shrink + cannibalize + throttle
│                    # bcachefs: cache.c/h — bch2_btree_cache_*, struct btree_cache
│
├── gc.rs            # GC: mark-and-sweep + 拓扑检查 + bucket 引用计数重建
│                    # bcachefs: gc.c/h — bch2_gc_btrees/gc_mark_key/gc_alloc_start_done
│
├── interior.rs      # 内部节点: split/merge/increase_depth/set_root/increase_depth
│                    # bcachefs: interior.c/h — bch2_btree_interior_*, struct btree_update
│
├── io.rs            # 节点磁盘 I/O: read_block/write_block/flush/validate/checksum
│                    # bcachefs: write.c/h, read.c/h — bch2_btree_node_read/write
│
├── iter.rs          # btree 迭代器: peek/next/prev/skip_to_next_leaf
│                    # bcachefs: iter.c/h — bch2_btree_iter_*, struct btree_iter
│
├── key.rs           # Bpos/BtreeKey/BtreeValue 类型 + 比较 + 排序 + pack/unpack
│                    # bcachefs: bkey.c/h, bkey_types.h — bch2_bkey_*, struct bpos/bkey
│
├── key_cache.rs     # Key Cache: find/drop/insert_key_cached(脏存储)/flush_dirty/collect_dirty/mark_clean
│                    # bcachefs: key_cache.c/h — bch2_btree_key_cache_*, struct bkey_cached
│                    # 语义: slot 复用(valid) + dirty 追踪(nr_dirty) + journal pin + 两阶段 flush
│                    # Phase 1-4 已完成, Batch D 已验证 (23 tests)
│
├── node.rs          # BtreeNode + bset 操作: 插入/删除/合并/分裂/排序/节点遍历
│                    # bcachefs: bset.c/h — bch2_bset_*, struct btree_node, bset_tree
│
├── node_scan.rs     # 节点扫描: 恢复/校验时遍历 btree 节点
│                    # bcachefs: node_scan.c/h — bch2_btree_node_scan_*
│
├── op.rs            # 操作类型/标志定义
│                    # bcachefs: btree.h — btree_update_flags, BTREE_INSERT_*
│
├── search.rs        # btree 路径搜索 + 栈式路径缓存
│                    # bcachefs: btree.h — bch2_btree_path_*, struct btree_path
│
├── snapshot.rs      # 快照树类型定义 (Snapshots btree key/value)
│                    # bcachefs: snapshots.h — bch2_snapshot_tree_*
│
├── transaction.rs   # BtreeTransaction: 重组 + lockrestart + bch2_trans_commit(含三阶段 trigger)
│                    # bcachefs: commit.c/h — bch2_trans_*, struct btree_trans
│
│
├── types.rs         # BtreePathLevel / BtreeRoot / NodePtr 等辅助类型
│                    # bcachefs: types.h — struct btree_root, btree_path_level
│
├── update.rs        # btree 内部更新: 写状态机 + merge/split/compact 后的节点更新
│                    # bcachefs: update.c/h — bch2_btree_update_*, struct btree_update
│
└── write_buffer.rs  # Write Buffer: journal keys 批处理 + 后台 flush + 排序合并
                     # bcachefs: write_buffer.c/h — bch2_btree_write_buffer_*
```

---

## Naming Conventions

### 函数命名

对齐 bcachefs C API 命名，使用 `bch2_` 前缀 + 子系统 + 操作：

```rust
// bcachefs: bch2_trans_get_iter(path, ...)
pub fn bch2_trans_get_iter(trans: &mut BtreeTrans, ...)

// bcachefs: bch2_btree_node_write(b, ...)
pub fn bch2_btree_node_write(b: &BtreeNode, ...)

// bcachefs: bch2_snapshot_is_ancestor(c, id, ancestor)
pub fn bch2_snapshot_is_ancestor(...)
```

### 类型命名

- 公开类型使用 bcachefs 原名（`Bpos`, `BtreeNode`, `BtreeTrans`, `JournalRes`）
- 模块内部类型使用 `pub(crate)`（`BsetTree`, `BtreePathLevel`）
- 枚举保持 bcachefs 变体名（`Watermark::InteriorUpdate`, `KeyType::Normal`）

### 字段命名

与 bcachefs `struct` 字段名对齐：
```
Bpos { inode, offset, snapshot }     // bcachefs: struct bpos
JournalEntry { seq, last_seq, ... }  // bcachefs: struct journal_entry
BchSubvolume { flags, snapshot, creation_parent, ... }  // bcachefs: struct bch_subvolume
```
