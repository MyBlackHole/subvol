# bcachefs 对齐验证指南

> **目的**：防止"声称对齐但实际未验证"的问题，确保所有模块的 bcachefs 对齐声明可追溯、可验证。

---

## 问题

subvolmount 多个模块声明"对齐 bcachefs"（`n 对齐`、`对应 bcachefs`、`对齐 bcachefs`），但**声明不验证等于没对齐**。

SixLock 的教训：
- C1 竞态：push_waiter 外 trylock = 没看 bcachefs wait_lock 内重试协议
- C2 位置：should_sleep 在入队前调 = 没看 bcachefs 实际调用位置(line 637)

**根因**：写"对齐"时没有先读 bcachefs 源码确认。

---

## 验证清单

### 写前必做

- [ ] **找到对应源码** — 在 bcachefs 源码树的 `fs/` 子目录中找到对应函数/文件
- [ ] **理解完整上下文** — 不只读目标行，读前后 30 行确认调用链和并发约束
- [ ] **记录参考位置** — 在注释中写 `对应 bcachefs <file>:<line>`，不写模糊的"对齐"

### 写中必做

- [ ] **确认语义等价** — Rust 抽象是否改变了语义？（`Mutex` vs `raw_spinlock_t`、`Atomic` vs `atomic_t`）
- [ ] **确认函数选择正确** — 不能用 `try_lock_write()` 的地方用了 `try_lock_write_preset()`？
- [ ] **确认边界条件** — bcachefs 的死锁回滚、错误路径是否都有映射？

### 写后必做

- [ ] **测试通过** — `cargo test -p subvol-core` 通过
- [ ] **spec 更新** — 如果学到了新的约束，更新对应模块的 spec
- [ ] **函数覆盖地图更新** — 被修改模块的 spec 中 `bcachefs 函数覆盖地图` 对应条目同步更新

### 切换模块前必做

- [ ] **已修改的模块的覆盖地图已更新** — 每个修改过的函数标 ✅（已验证）或 ⚠️（已知偏差）
- [ ] **目标模块的覆盖地图已读取** — 确认 ❓ 数量，了解未验证风险

---

## 模块 → 参考源码映射

所有比较文档/注释中的 `n` 缩写均指 bcachefs。

源码根路径：`bcachefs 源码树`。以下路径均为相对此根路径下的 `fs/` 子目录。

| subvol 模块 | bcachefs 参考文件 | 说明 |
|---|---|---|---|
| `lock/six.rs` | `util/six.c` (1109 行) + `util/six.h` (536 行) | 核心六锁实现（atomic bitfield + percpu reader）。关键流：SET_WAITING(line 590)→TRYLOCK(line 591)→ENQUEUE(line 598) 全部在 wait_lock 内原子。should_sleep_fn 仅在 park 循环(line 637)调，不在入队前。**wakeup 路径**：`six_lock_wakeup`(line 412-424) + `__six_lock_wakeup`(line 316-410)。两个关键区别：(1) `six_lock_wakeup` 有 `write + held_read → skip` 检查 (2) `__six_lock_wakeup` handoff 失败 via `goto out` 不清 WAITING bit |
| `lock/deadlock.rs` | `btree/locking.c` + `btree/locking.h` | btree-level 锁排序 + 死锁检测。`bch2_six_check_for_deadlock`(locking.c:783) 是实际 should_sleep_fn 回调 |
| `lock/wait_fifo.rs` | `util/fifo.h`（通用 FIFO）+ `six.c`（six_lock_waiter） | 等待队列，six.c 中内嵌使用 |
| **btree 整体** | `btree/` 目录 | 全部 btree 模块。**注意命名差异**：bcachefs C 文件无 `btree_` 前缀（`iter.c`, `read.c`, `write.c`, `commit.c` 等），而 subvolmount Rust 文件有 `btree_` 前缀（`btree_iter.rs`, `btree_io.rs` 等） |
| `btree/btree.rs` | `btree/init.c` + `btree/types.h` | Btree 主结构（bch_fs 中的 btree 实例） |
| `btree/types.rs` | `btree/types.h` + `bkey_types.h` | 共享类型（bpos, bkey, btree_path_level） |
| `btree/key.rs` | `btree/bkey.h` + `btree/bkey.c` + `bkey_types.h` | bkey 打包/解包/bpos 操作 |
| `btree/node.rs` | `btree/bset.h` + `btree/bset.c` | BtreeNode + bset 布局 + 辅助搜索树 |
| `btree/iter.rs` | `btree/iter.c` + `btree/iter.h` | BtreeIter 遍历器 |
| `btree/transaction.rs` | `btree/commit.c` + `btree/update.h` | BtreeTrans 事务 |
| `btree/io.rs` | `btree/read.c` + `btree/write.c` + `btree/io.h` | btree 节点 I/O 读写 |
| `btree/cache.rs` | `btree/cache.c` + `btree/cache.h` | btree 节点缓存 + eviction |
| `btree/write_buffer.rs` | `btree/write_buffer.c` + `btree/write_buffer.h` | 写缓冲区 flush |
| `btree/key_cache.rs` | `btree/key_cache.c` + `btree/key_cache.h` | key cache（hash 表 + per-entry 锁） |
| `btree/interior.rs` | `btree/interior.c` + `btree/interior.h` | 内部节点操作（split/merge/rewrite/set_root） |
| `btree/update.rs` | `btree/update.c` + `btree/update.h` | btree interior update state machine |
| `btree/search.rs` | `btree/iter.c`（`bch2_btree_iter_traverse`） | 搜索优先级 + 路径下降 |
| `btree/transaction.rs` | `btree/commit.c`（triggers）+ 各 `*_trigger.c` | 按 bkey update 执行触发器 |
| `btree/gc.rs` | `btree/check.c` + `check.h`（无独立 gc.c） | GC 遍历 + 一致性检查 |
| `btree/node_scan.rs` | `btree/node_scan.c` + `btree/node_scan.h` | 设备 btree 节点扫描 |
| `btree/mod.rs` | `btree/types.h`（BTREE_ID 枚举） | BtreeId + subvol_ino_map |
| **alloc 整体** | `alloc/` 目录 | 全部 alloc 模块 |
| `alloc/mod.rs` | `alloc/foreground.c` + `alloc/background.h` | BchAllocator 主结构 + 分配入口 |
| `alloc/bucket.rs` | `alloc/background.c` + `alloc/types.h` + `alloc/format.h` | Bucket 状态管理 + bch_alloc_v4 |
| `alloc/foreground.rs` | `alloc/foreground.c` + `alloc/foreground.h` | 前台分配 + alloc_prio_hint |
| `alloc/background.rs` | `alloc/background.c` + `alloc/background.h` | 后台 GC 分配 |
| `alloc/btree.rs` | `alloc/background.c` + `buckets.c` | Alloc btree 操作 |
| `alloc/open_bucket.rs` | `alloc/foreground.h`（open_bucket 结构）+ `alloc/types.h` | 开放桶引用计数 |
| `alloc/reservation.rs` | `alloc/buckets.h`（`disk_reservation`） | 扇区预留系统 |
| `alloc/write_point.rs` | `alloc/foreground.c`（`write_point`）+ `alloc/types.h` | 写点管理 |
| **journal 整体** | `journal/` 目录 | 全部 journal 模块 |
| `journal/types.rs` | `journal/types.h` + `journal.h` | Journal 类型 + 状态 |
| `journal/mod.rs` | `journal/journal.c` + `journal/journal.h` | Journal 核心（buf/commit/flush） |
| `journal/reclaim.rs` | `journal/reclaim.c` + `journal/reclaim.h` | Journal 回收 |
| `journal/replay.rs` | `journal/read.c` + `init/recovery.c`（调用方） | Journal 回放 |
| `journal/jset.rs` | `journal/types.h`（jset 结构）+ `bcachefs_format.h` | Journal entry 格式 |
| `subvol/` | `snapshots/subvolume.c` + `snapshots/subvolume.h` | 子卷管理 |
| `snap/` | `snapshots/snapshot.c` + `snapshots/snapshot.h` + `snapshots/check_snapshots.c` | 快照 skip_list + 一致性检查 |
| `recovery/` | `init/recovery.c` + `init/passes.c` + `init/passes.h` | 崩溃恢复框架 + pass 调度 |
| `super_block/` | `sb/` 目录（`sb/members.c`, `sb/clean.c`, `sb/io.c` 等） | Superblock 管理 |
| `volume/` | `init/fs.c`（fs lifecycle, BCH_FS_* flags） | 卷生命周期管理 |

### 覆盖地图状态（2026-07-04 更新）

| 模块 | 覆盖地图文件 | ✅+⚠️ 覆盖率 | ❓ 未验证 |
|------|-------------|-------------|-----------|
| lock/six | `lock-concurrency.md` | 100% | 0 (0%) |
| btree/transaction | `btree-transaction.md` | 88.6% (62/70) | 0 (0%) |
| alloc | `alloc-coverage.md` | 64.6% | 0 |
| journal | `function-coverage.md` | 61.5% ✅ | 0 (0%) |
| snap | `snap-coverage.md` | 60% ✅ | 0 (0%) |
| subvol | `subvol-coverage.md` | 55% ✅ | 0 (0%) |
| recovery | `recovery-coverage.md` | 97.4% | — |
| volume | `volume-coverage.md` | 56.4% | 0 (0%) |
| btree/io | `btree-io-coverage.md` | 88% | 0 (0%) |
| btree/cache | `btree-cache-coverage.md` | 90% ✅ | 0 (0%) |
| `block_device/` | `data/checksum.h` + `data/io_misc.c` | 块设备 + 校验和 |

---

## 正确做法 vs 错误做法

### 正确

```rust
// 对应 bcachefs __six_lock_slowpath line 637
// 在 FIFO 入队（line 598）之后的 park 循环内调 should_sleep_fn
```

包含：文件名 + 行号 + 逻辑顺序描述

### 错误

```rust
// 对齐 bcachefs——入队前先调 should_sleep
```

不包含：无行号、无验证、顺序错误

---

## 发现不一致的修正流程

```
发现"对齐"声明 → 读 bcachefs 对应源码 → 确认是否一致
  一致 → 补上行号，确认 ✅
  不一致 → 修改实现对齐 bcachefs
          → 在 commit 中说明差异细节
          → 更新 spec（学到了什么）
          → 更新本清单的常见误区表
```

---

## 函数级覆盖地图

> 声明对齐的每个模块必须在自己的 spec 中维护一张函数级覆盖地图。
> **文件级映射只告诉你"去哪个文件找"，函数级映射才知道"哪些已经验过、哪些还没验"。**

### 模板

每个声明对齐的模块在其 spec 文件中（如 `lock-concurrency.md`）添加以下表格：

```markdown
### bcachefs 函数覆盖地图

| 我们的函数 | bcachefs 对应 | 行号 | 状态 |
|-----------|--------------|------|------|
| `pub fn lock_write(&self)` | `do_six_lock_ip` | `six.c:528` | ✅ |
| `fn lock_slowpath(...)` | `__six_lock_slowpath` | `six.c:543` | ✅ |
| `fn try_lock_read(&self)` | `__do_six_trylock(Read)` | `six.c:70` | ❓ |
```

### 覆盖状态说明

| 状态 | 含义 | 要求 |
|------|------|------|
| ✅ | 已验证对齐 bcachefs | 注释含 bcachefs 行号 |
| ⚠️ | 已知偏差（Rust 特有抽象导致） | 偏差原因必须说明 |
| ❓ | 未验证—没对照过 bcachefs | 下次改此模块时优先验证 |
| ➖ | 无 bcachefs 对应（纯 Rust 新增） | 简短说明为什么没有 |

### 治理规则

- 每个模块修改前必须读覆盖地图，**❓ 数量是技术债指标**
- 每次修改后更新对应条目的状态（❓ → ✅）
- 一个模块的 ❓ 清零后才能声称"此模块已完成 bcachefs 对齐"
- 覆盖地图维护在模块的 spec 中（如 `lock-concurrency.md`），而非 guide 中（避免过长）

---

## 常见误区（持续更新）

| 模块 | 错误假设 | 事实 | 证据 |
|---|---|---|---|
| lock | should_sleep 在入队前调 | 在 park 循环内调 | `six.c:637` |
| lock | push_waiter 外 trylock 就行 | 必须 wait_lock 内重试 | `six.c:584-611` |
| lock | `trylock_ip` → `try_lock_write()` 在 WRITE_BIT 预设后有效 | 无效—has_write_lock 始终返回 true | `six.c:573` 预设后 line 591 不检查 HELD_write |
| lock | WAITING bit 和入队之间无其他操作 | 在 WAITING bit 设置(line 590)和入队(line 598)之间有 __do_six_trylock(line 591) | `six.c:590-598` |
| lock | wakeup 路径也对齐了 bcachefs | wakeup_lock_type/__wakeup_lock_type 是从原实现继承的，从未对照 six_lock_wakeup/__six_lock_wakeup | `six.c:316-424` — 两个差异：(1)six_lock_wakeup 外层有 write+held_read 检查(line 416-417) (2)__six_lock_wakeup handoff 失败不走 WAITING bit 清除(line 380-383 vs 400-402) |
| btree | bcachefs 文件有 `btree_` 前缀 | 无前缀，`btree/iter.c` 而非 `btree_iter.c` | 实际 `ls fs/btree/` |
| btree | `btree_gc.c` 是独立文件 | GC 在 `btree/check.c` | 实际 `ls fs/btree/` |
| transaction | journal 写入在 btree 修改之前 | `bch2_bch2_trans_commit(): journal_res_get → btree modify → journal_add_entry → journal_res_put`（先保留→再修改→最后填充） | `journal.h:journal_res_get_fast` + `commit.c:bch2_bch2_trans_commit()` |
| transaction | 一次 trans_commit = 一次 journal_res_get | trans_commit 的 journal 条目（可能跨多个 btree 组）打包为一个 Jset 写入一次保留空间 | btree/commit.c 中 `journal_res_get` + `__bch2_trans_commit` 之间按需预留精确大小 |
| transaction | Volume 级 pin 是必需的 | 节点级 pin（在 `bch2_btree_node_write` 时注册）已覆盖全部语义，Volume 级 pin 是冗余（subvolmount 特有，bcachefs 无对应） | `reclaim.c:bch2_journal_pin_add()` 在 `bch2_btree_node_write` 路径调用 |
| transaction/bcachefs 对齐 | `__bch2_bch2_trans_commit()` 需要对所有路径按 `(btree_id, pos, -level)` 排序才能避免死锁（`bch2_trans_sort_locks`） | bcachefs 排序是因为早期路径预分配后顺序不确定；但写锁升级（`bch2_trans_lock_write_inlined`）实际按 `trans_for_each_update` 遍历 journal 条目，而非 sorted paths。我们的 `try_lock_all()` 直接按 journal 顺序，与 bcachefs 实际写锁路径一致 | `commit.c:141-159` — `bch2_trans_lock_write_inlined` 遍历 `trans_for_each_update` |
| ~~transaction~~ | ~~`try_lock_read()` try-fail 模式适合遍历路径~~ | ~~G1 已修复：`lock_read()` 阻塞版已经存在，iter.rs 已全部使用~~ | ~~`six.rs:698` — `lock_read()` try→spin→sleep~~ |
| transaction | `sort_locks()` 是 `__bch2_trans_commit` 的必要步骤 | bcachefs 的 `__bch2_bch2_trans_commit()` 入口处不调用 sort_locks，它由调用者在需要时（如跨事务锁获取）手动调用。锁升级路径走 `bch2_trans_lock_write_inlined` 无排序 | `commit.c:141-159` — 写锁升级函数体 |
| transaction | BtreeTrans 需要 sort_locks 来保证加锁顺序一致性 | bcachefs 的 `bch2_bch2_trans_relock()` 遍历 `trans_for_each_path()` 不排序；`bch2_trans_lock_write_inlined` 遍历 journal 不排序。排序仅在外部锁获取时按需调用 | `locking.c:1487-1517` — `bch2_trans_relock` + `locking.c:1059` — `bch2_trans_sort_locks` |
| transaction | BtreeTransEntry 必须包含完整 old_key/old_value | bcachefs 的 `verify_update_old_key()` 在 commit 流程中从 btree 实时查找 old_key，无需调用者提供 | `commit.c:56-130` — `verify_update_old_key` 查 `bch2_btree_path_peek_slot` |
| extent | vaddr=结束位置才是正确对齐 | vaddr=起始位置+size是可行的混合方案：Ord 按 start 排序+ `peek_visible_range` 应用层范围比较。纯迁移到 end-position 排序风险高、收益有限 | `prd.md` / `design.md` — extent-model 任务文档 |
| btree/key | `cut_back(vaddr=end)` 正确实现应为 vaddr=new_end, size=new_end-start | 在 subvolmount vaddr=start 模型中，`cut_back` 应保持 vaddr 不变，只缩小 size（`size = new_end - vaddr`）。同理 `cut_front` 应前移 vaddr 同时缩小 size（`vaddr = new_start; size = new_start - old_start`）。| `btree/key.rs:1154-1181` — `cut_front`/`cut_back` 实现 |
| alloc | `sectors_needed = count * BLOCKS_PER_BUCKET * SECTORS_PER_BLOCK` | 正确值 `count * SECTORS_PER_BLOCK`。原公式导致每次分配一整桶，范围分配无效。 | `alloc/mod.rs:854` — `bch2_alloc_sectors_start_trans` |
| alloc/write_point | try_reuse_current_wp 用 `load`+`fetch_sub` 有 TOCTOU | `fetch_sub` + old-value-check: `let old = fetch_sub(n); if old >= n { ok } else { fetch_add(n); None }` | `write_point.rs:346-374` — `try_reuse_current_wp` |
| btree/cache | `nr_in_flight_inner` 放在 `Mutex` 内部时永远是 0 | 应使用 `AtomicUsize` 以匹配 bcachefs `atomic_t`，配合 `tokio::sync::Notify` 实现 async 等待唤醒 | `cache.rs` — `nr_in_flight_inner: AtomicUsize`, `notify: Notify` |
| device/io | `WRITE io_ref` 只需要在线，不需要 member state 判断 | `bch2_dev_get_ioref()` 对 `WRITE` 还要求 `ca->mi.state == BCH_MEMBER_STATE_rw`；`READ` 才能只看在线状态 | `sb/members.h:377-390` + `init/dev.c:430-441` |
| transaction | foreground merge 必须在提交期间使用真实 path/level | 本地 `trans_commit_merge()` 先压实 updates，再调用 `__bch2_foreground_maybe_merge()`，并保留 path 表重分配后的 update 指针 | `fs/btree/commit.c:1006-1040` |
| transaction/path | C 指针 helper 返回 `NULL`/有效节点，因此 Rust 可用 `Option<Node>` | `btree_path_node()` 还可能返回 `ERR_PTR`；Rust 必须保留显式 error sentinel，只把越界映射为 `None` | 本地 `iter.h:183-187`、`errcode.h:237-247` |
| transaction/path | 同 leaf 的 iterator 可以覆盖并复用同一个 Rust iterator 对象 | bcachefs 共享的是相同 position 的 `btree_path` 引用；不同 position 需 `bch2_btree_path_set_pos()`，ref/preserve 条件下先 make-mut，旧 iterator 不能被覆盖 | 本地 `iter.c:2201-2278`、`types.h:602-630` |
| transaction/path | iterator 自带测试 path pool 不影响生产所有权 | 测试兼容 owner 会掩盖 transaction path 生命周期错误；生产和测试都必须从唯一 transaction pool 解析 path | 本地 `types.h:602-630,939-946` |
| transaction/path | iterator 初始化后可把 `iter->pos/path->pos` 改成第一个命中的 key | `pos` 是查询位置，节点 iterator 才指向当前命中 key；二者必须独立 | 本地 `types.h:602-630`、`iter.c:784-815` |
| transaction/path | `BtreePathNode::Error` 等价于 C `NULL`，up traversal 应停止 | C `btree_path_node()` 原样返回 `ERR_PTR`，它仍是非空；`btree_path_up_until_good_node()` 会 set-level-up 后继续向上，只有真正 `NULL`/越界才转 lock-root | 本地 `iter.h:183-187`、`iter.c:1360-1395` |
| transaction/locking | transaction 应另存一份 `trans_start_time` 供死锁检测 | 本地 `struct btree_trans` 只有内嵌 `locking_wait.trans_start_time`；`bch2_trans_begin()` 只在 `!restarted` 时用 `now` 刷新，restart 必须保留 waitlist 年龄 | 本地 `types.h:855`、`iter.c:3970-3971` |
| transaction/locking | 只要有 DFS 检测器就可以接入 `btree_node_lock` slowpath | 本地死锁图从 wait FIFO 中的内嵌 `six_lock_waiter *` 反查 transaction，再扫描其全部 path/level；复制元数据的 FIFO 与手工 `WaiterInfo` 不是等价实现 | 本地 `six.h:210-258`、`locking.c:189-310,783-857` |
| transaction/path | relock 失败后可以调用整树 iterator 初始化替代 traverse-one | `bch2_btree_path_traverse_one()` 必须复用可重锁 parent，并在 `btree_path_down()` 中先释放 read parent 再锁 child；整树重建会绕过逐层错误和锁状态 | 本地 `iter.c:1216-1260,1490-1590`、`cache.c:1289-1470` |
| transaction/async | raw path 指针要求给 `BtreeIter` 无条件实现 `Send + Sync` | transaction 独占整体移动只需要 `Send`；`Sync` 会允许共享并发访问可变 path pool，必须禁止 | `BtreeTrans.paths: Box<Vec<...>>` + `BtreeIter.paths_ptr` 生命周期约束 |
| transaction | journal_replay pre/post 协议需要移植才能算对齐 | 直接对齐本地 `journal_keys` 与 commit pre/post；禁止引入自有 overlay | 本地 `fs/btree/commit.c:718-766`、`fs/btree/commit.c:1280-1523` |
| btree/io | `bch2_sort_keys` 可直接复用 read overlap repair 的过滤逻辑 | 两者只共用 cursor-array 机制。read 的 `bch2_key_sort_fix_overlapping` 用指针顺序和 `should_drop_next_key()` 淘汰旧重叠键；write 的 `bch2_sort_keys` 不做指针 tie-break，只过滤当前 Deleted，并返回写入 u64 数。当前 `bch2_bset_insert` 已通过 iterator 位置 + memmove 保持 bset 有序。 | `sort.c:75-125,202-216`；`node.rs:bch2_bset_insert` |
| btree/cache | `evicted_sizes` 用 `HashMap<u64, u16>` 等价于 bcachefs | bcachefs 使用固定大小、无冲突链的 `btree_evicted_size` 表（`u64 *entries` + mask），插入会覆盖，冲突自然降级。HashMap 是无界的，会导致内存泄漏 | `cache.h:55-83` + `types.h:1041-1044` (2026-07-18 修复) |
| btree/cache | `bch2_recalc_btree_reserve` 与 bcachefs 同名函数语义相同 | bcachefs 计算 `nr_reserve`（预分配节点数 16+8*alive_roots），subvolmount 计算 `should_throttle`（布尔节流标志）。subvol 无内核 MM shrinker，不需要 nr_reserve，但函数名误导性声称对齐 | `cache.c:123-138` (2026-07-18 归档为架构差异) |
| btree/node | `NODE_DIRTY` 标志位对齐 `BTREE_NODE_dirty` | bcachefs 的 `BTREE_NODE_dirty` 是 bit 5（`1U << 5`，参见 bindgen 输出 `bcachefs.rs:37277`）。subvol 使用独立标志位编号，`NODE_DIRTY` 分配在 bit 6 (0x40)。语义一致：set_dirty 在 bkey 修改时设置，clear_dirty 在写入 CAS 成功时清除。⚠️ `node.rs:394` 注释曾误标为 bit 0，2026-07-20 已修正。 | `node.rs:395` + bcachefs `write.c:53-58` |
| btree/io | `bch2_btree_node_io_lock` 用 spin-loop 对齐 bcachefs | bcachefs 使用 `wait_on_bit_lock_io()` 基于 waitqueue 阻塞等待而非 spin。subvol 改用 `Condvar` + `Mutex` 实现等价阻塞等待，避免 CPU 空转。语义完全一致：等待 write_in_flight 清除 → CAS 设置标志。 | `io.rs:29-35` + bcachefs `read.c:70-73` |
| lock/six | park 循环中 handoff 检查在 should_sleep_fn 之后 | bcachefs `six.c:634` 先检查 `lock_acquired`，再调 `should_sleep_fn` (line 637)。subvol 先调 should_sleep_fn 再 park，醒来后检查 handoff。顺序差异在实践中很少出问题（should_sleep_fn 幂等），但应修复以精确对齐。 | `six.rs:1187-1255` + `six.c:626-667` |
| lock/six | `six_lock_readers_add` percpu 路径有 debug_assert 下溢检查 | bcachefs 在 percpu 路径（`lock->readers` 非空）无下溢检查，因为 `this_cpu_add` 不可失败。subvol 用 `fetch_sub` + `debug_assert` 替代，增加了防护。不是错误但非 bcachefs 行为。 | `six.rs:1595-1616` + `six.c:1039-1048` |
| lock/six | `six_lock_exit` 缺少 WARN_ON 调试检查 | bcachefs 在退出时用 `WARN_ON(pcpu_read_count(lock))` 和 `WARN_ON(state & HELD_read)` 检查锁是否仍在被使用。subvol 跳过这些检查。可添加 `debug_assert!` 增强。 | `six.rs:971-974` + `six.c:1058-1073` |
| journal | `bch2_journal_check_for_missing` 多过滤了 csum_good | bcachefs 的 `journal_replay_ignore` 不检查 checksum。subvol 额外过滤 `!r.csum_good` 的 entry，可能导致 false positive gap 报告。建议移除该条件。 | `types.rs:4101` + `read.c:1012-1057` |
| btree-interior | set_root_inmem 缺少 roots_b 数组更新 | docs 中未标注 roots_b 差异。subvol 使用 current_root_disk 替代 roots_b 概念。 | `interior.rs:259-271` + `interior.c:1606-1626` |
| btree/iter | `for_each_btree_key_entry` 硬编码 `BtreeId::Extents` | 函数在 `bch2_trans_get_iter` 中传了 `BtreeId::Extents`，而非 `self.btype`。导致 `bch2_subvolume_list` 迭代的是 Extents btree 而非 Subvolumes btree。 | `btree.rs:1282` — 已修复为 `self.btype` |

---

## 核心原则

> **"对齐"不是标签，是承诺。写之前读源码，写之后注行号。**

### 铁律：实现逻辑必须对齐 bcachefs

**Rust 语法可以现代化**（enum 替代 bitfield、Mutex 替代 spinlock、Atomic 替代 atomic_t、trait 替代 callback），但**实现逻辑必须与 bcachefs 完全一致**：

```
✅ 允许的 Rust 语法改进：
   HashMap → hlist              （数据结构不同但语义等价）
   Mutex / RwLock → spinlock    （锁语义一致，只是实现不同）
   AtomicU32::fetch_sub → load+sub （操作等价，并发语义相同）
   enum → bitfield+union        （类型安全，布局等价）

❌ 绝对禁止的逻辑偏差：
    函数调用顺序不同             （如 should_sleep 在入队前 vs 在 park 内）
    条件判断逻辑不同             （如 sectors_needed = count*BLOCKS_PER_BUCKET*SECTORS_PER_BLOCK）
    cut_front/cut_back 语义错误  （vaddr=start vs vaddr=end 混淆）
     并发安全假设不同             （load-then-sub 假设无竞争，bcachefs 也用原子操作）
    堆合并前置条件错误           （bset 未排序时使用 sort_iter 堆合并，输出乱序）
```

**验证方法：** 每个函数标注 bcachefs 参考行号后，对照源码确认以下三个方面完全一致：

1. **调用链** — 函数 A 先调 B 再调 C 还是先 C 再 B？
2. **边界条件** — 空值、零值、满值、溢出时的行为与 bcachefs 一致吗？
3. **错误路径** — 失败时的回滚步骤与 bcachefs 相同吗？

### 必须使用本地 bcachefs 源码

**禁止通过 webfetch/Context7/gh_grep 获取 bcachefs 源码进行对齐验证。** 必须使用本地已克隆的仓库：

- **本地参考仓库**：bcachefs 源码树的本地克隆版本 — 当前规范默认参考仓库；具体源码位于 `fs/` 子目录
- **查找方式不确定时**：先用 `find /home/black/Documents/bcachefs* -name "FILE.c"` 确认后再引用行号
- **文件映射**：见上方"模块 → 参考源码映射"表
- **原因**：
  - 网络来源可能不是最新或最权威的版本
  - 本地源码可配合 `grep`/`rg` 全文搜索
  - 本地 IDE 可跳转阅读完整上下文
  - 所有 `对应 bcachefs <file>:<line>` 注释引用必须基于本地源码验证
