# Journal 层规范

## 文件

| 文件 | 内容 | 适用场景 |
|------|------|---------|
| `function-coverage.md` | Journal 函数覆盖地图、write/read/blacklist 可执行契约 | 修改 journal 主路径、superblock blacklist 或 recovery 时 |
| `reclaim.md` | Pin API 模式（UnsafeCell 内变、_seq 过渡、方法命名、测试） | 修改 reclaim.rs、types.rs 中 pin 相关代码时 |

## 设计决策

### Phase 2: Journal Safety Net (root-pointer-journal)

**背景**：crash 时可能出现 data entry 已落盘但 btree root 指针未落盘的窗口。需在每次 journal write 时将根指针写入同一 buf。

**方案**：在 `write_bufs_to_bucket` Phase 1a 中，关闭 entry 后序列化前，调用 `bch2_inject_btree_roots_into_buf` 将 pending 的 btree_root jset 追加到 buf.data 末尾（与 data entries 写入同一 journal entry）。

**vs bcachefs 差异**：
- bcachefs（`bch2_btree_roots_to_journal_entries`，interior.c:3770）：每次写入**所有** alive btree roots
- subvol：只写 **pending-changed** roots（有 `pending_root_journal` 设置的 root）
- 理由：subvol 中 roots 变更频率低，每次变更至少被一次 journal entry 覆盖，crash safety 等价

**循环引用处理**：Journal 使用 `OnceLock<Weak<BchVol>>` 引用 BchVol，避免 Arc 循环阻止析构。`set_vol_ref()` 在 `open_with_backend()` 中调用。

**静态方法模式**：`bch2_inject_btree_roots_into_buf` 为 static fn（取 `&BchVol` 而非 `&self`），避免 `&self` 到 `vol` 的借用冲突。

**两个字段协同**：
- `pending_root_journal`（Btree 上的一次性 `UnsafeCell<Option<...>>`）：标记"根刚变更，还没写入 journal"——trans_commit 消费此信号
- `current_root_disk`（Btree 上的持久化 `UnsafeCell<Option<(u64, u8)>>`）：每次写入根变更时同步更新，后续 journal write 通过此字段获取根信息

## 关键约定

1. **所有 root 变更路径**（`bch2_btree_set_root`、`bch2_btree_increase_depth`、`split_root`、`collapse_root`、`insert_routing_entry_at`）必须同时设置 `pending_root_journal` 和 `current_root_disk`
2. `current_root_disk` 的 getter 是 `current_root_disk_info()`，返回 `Option<(u64, u8)>`
3. safety net 先消费 `pending_root_journal`（同步到 `current_root_disk`），再从 `current_root_disk` 读取写入 buf

## 质量检查

修改 journal/ 代码后，确认：

- [ ] `cargo test -p subvol-core --lib` 通过（1039 passed, 0 failed, 0 ignored）
- [ ] `cargo clippy -p subvol-core` 无新 error/warning
- [ ] `cargo fmt --check` clean
