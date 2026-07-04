# Lock 并发模型

> Btree 并发模型设计，对齐 bcachefs six lock。

---

## 核心选择

### WaitFifo URCU 化

使用 `urcu` crate（liburcu Rust safe wrapper）做无锁遍历 + 内存安全回收。

- Rust crate: `urcu = "0.0.4"`（系统需 `apt install liburcu-dev`）
- Variant: `memb`（Linux `membarrier()` 系统调用，性能最优）
- 方式: `RcuBox<Option<Box<WaiterBox>>>` per-slot 管理
  
**关键 Know-how**：
- `RcuBox::Drop` 在内核 `call_rcu` 中回收（非阻塞），在 `rscs` 内 drop 不会死锁
- `compare_and_update` 返回的旧值 `RcuBox` 必须在 `rscs` 闭包外 drop（否则死锁）
- 线程注册通过 `thread_local! { OnceCell<(Rcu, RcuThread)> }` 管理
- `WaitFifo::push()` 的候选槽位扫描必须包裹在单次 RCU read-side section 内，避免为每个槽位重复进入 `rscs` 造成慢路径放大

### DeadlockDetector per-thread 瞬态化

移除持久化 `Arc<Mutex<HashMap>>` LockGraph，改为 per-thread DFS 栈。

```rust
thread_local! {
    pub(crate) static DEADLOCK_DETECTOR: UnsafeCell<DeadlockDetector> =
        const { UnsafeCell::new(DeadlockDetector::new()) };
}
```

- 8 帧深度（`[DetectorNode; 8]`），对应 `BTREE_MAX_DEPTH`
- `UnsafeCell` + thread_local 保证线程独占访问（等效 bcachefs `preempt_disable`）
- DeadlockDetector 的 `detect()` 通过 WaitFifo RCU 遍历收集等待链

### SixLock should_sleep_fn 回调

- 字段类型: `Option<Arc<dyn Fn() -> bool + Send + Sync>>`
- 无参回调（不传递 trans_id），死锁检测由下游在回调中进行
- 默认 None 时 `should_sleep()` 返回 true（兼容原行为）
- `lock_slowpath` 在 park 循环内调用 should_sleep_fn（所有 sleep 路径最终都汇聚于此：`lock_read`/`lock_intent`/`lock_write`/`lock_ip_waiter`/`lock_contended` 均委托 `lock_slowpath`）

### WRITE_BIT preset

写锁进入 sleep 路径时预设 `WRITE_BIT` 阻塞后续读者，防止写锁饥饿：
1. `fetch_or(WRITE_BIT)` — 先预设
2. `push_waiter_with_recheck()` — 再入队（内置 wait_lock 内 trylock 重试）
3. `unlock_write` 同时清除 `WRITE_BIT | WRITE_LOCK_BIT`
4. 死锁回滚时 `fetch_and(!WRITE_BIT)` + `remove_self`

补充：`try_lock_write()` 的 percpu 路径不应先做一次“是否存在其他读者”的预扫描；应像 bcachefs `__do_six_trylock(Write)` 一样先预设写位，再沿同一轮扫描等待其他 slot 归零。这样可以避免双重遍历把 write fast path 放大成两次 O(n) 读者扫描。
再补充：`try_lock_write()` 只把 `read` 视为直接冲突位，不把调用者自己持有的 `intent` 当作 trylock 冲突条件；但它的硬前置条件是当前线程已经是 intent owner，和 bcachefs `__do_six_trylock(Write)` 的 `EBUG_ON(lock->owner != task)` 一致。
补充：`try_lock_write_preset()` / `try_lock_write_preset_for()` 的 percpu 路径也应保持单次 readers 扫描，不应先 `any()` 预检再二次等待；写锁慢路径和唤醒 handoff 都只需要一轮从当前 slot 之外的所有 reader 归零的遍历。
补充：`try_lock_read()` 的 fast path 只和“write lock 已持有”冲突，`WAITING_WRITE_BIT` 只是 writer 慢路径状态，不应单独阻断新读者。

### Write 必须嵌套在 intent 生命周期内

**契约**：所有 write fast path、slowpath、handoff 和 unlock 都要求同一线程仍持有 intent。路径状态中的 `Write` 隐含 `Intent + Write` 两层锁；完整释放顺序固定为 `unlock_write()` 后 `unlock_intent()`。

```rust
// 错误：裸 write 违反 six.c __do_six_trylock()/six_unlock_ip() 的 owner 检查
node.lock.six_lock_write();
node.lock.six_unlock_write();

// 正确：intent → write → unlock write → unlock intent
node.lock.six_lock_intent();
node.lock.six_lock_write();
node.lock.six_unlock_write();
node.lock.six_unlock_intent();
```

**边界行为**：
- `None` 或仅 `Read` 的路径不得直接升级为 `Write`；先取得/升级为 `Intent`。
- write relock/restart 必须重建 `Intent` 后再获取 `Write`；write 获取失败时释放刚取得的 intent。
- write 路径的通用 unlock/restart 必须先释放 write，再释放仍隐含持有的 intent。
- `six_trylock_write()`、preset handoff 和 `six_unlock_write()` 用 owner 断言阻止裸 write 或跨线程释放。

**测试断言**：write 获取前 intent owner 是当前线程；write unlock 后 intent 仍持有；完整释放后 write/intent 均未持有；所有并发测试在 60 秒超时内结束。

### push_waiter_with_recheck — wait_lock 内 trylock 重试协议（C1 fix）

**问题**：trylock 失败 → 入队之间存在竞态窗口：

```
Thread B: try_lock() 失败         Thread A: unlock()
    │                                  │
    │ (not in FIFO yet)                │ 释放锁
    │                                  │ 检查 WAITING bit → 未设 → 不唤醒
    │ push_waiter() 设 WAITING bit     │
    │ FIFO 入队                        │
    │ park() 永久睡眠 ─────────────────→ (无人唤醒)
```

**解法**（对应 bcachefs `__six_lock_slowpath` wait_lock 内重试）：

```rust
fn push_waiter_with_recheck(&self, waiter: &WaiterBox) -> bool {
    let _lock = self.wait_lock.lock();
    // Step 1: 持 wait_lock 设 WAITING bit
    self.set_waiting_bit(waiter.lock_type);
    // Step 2: wait_lock 内 trylock（unlock 若发生，已设 WAITING bit → 触发唤醒）
    let acquired = match waiter.lock_type {
        Read    => self.try_lock_read(),
        Intent  => self.try_lock_intent(),
        Write   => self.try_lock_write_preset(), // 非 try_lock_write()（后者检查 WRITE_BIT）
    };
    if acquired {
        self.clear_waiting_bit(waiter.lock_type);
        return true; // 锁已获取，调用者不应 park
    }
    // Step 4: 入 FIFO（WAITING bit 保持设置）
    self.wait_fifo.push(...);
    false
}
```

**关键约束**：
- `lock_slowpath(Write)` 的 double-check trylock 必须用 `try_lock_write_preset()`，**不能用** `trylock_ip()` → `try_lock_write()`。因为 WRITE_BIT 已预设，`try_lock_write()` 中的 `has_write_lock(state)` 检查会误以为写锁已被其他线程持有，始终返回 false。
- `lock_slowpath` should_sleep 错误路径清除 `WRITE_BIT` 时必须同步清除 `WAITING_WRITE_BIT`，否则残留的 WAITING bit 会阻塞后续读者。
- 读锁 recheck 成功路径（`lock_slowpath` 中 `push_waiter_with_recheck` 成功返回后 或 非 handoff trylock 成功后）需同步 `THREAD_READ_CNT`（非 percpu 模式）。
- 所有 sleep 路径（`lock_read`、`lock_intent`、`lock_write`、`lock_ip_waiter`、`lock_contended`）统一通过 `lock_slowpath` 进入，不可保留分离的 trylock + push_waiter 两步调用。
- **should_sleep_fn 必须在入队之后、park 循环内调用**（对齐 bcachefs `__six_lock_slowpath` line 637）。在入队前调用会破坏死锁检测的等待链（waiter 不可见），导致假阳性检测结果。

### wakeup_lock_type — 写锁唤醒时检查 readers 活跃度（BC3 fix）

**问题**：`stress_deadlock_burst_wake` 概率性死锁，GDB 显示 Writer 在 `lock_slowpath(Write)` park，WAITING_WRITE_BIT 已清。

**根因**：`__wakeup_lock_type(Write)` 在 handoff 失败（`try_lock_write_preset_for` 发现其他读者仍持锁）后错误地清除了 `WAITING_WRITE_BIT`，导致后续 unlock_read 提前返回，Writer 永远无人唤醒。

**时序**：
```
1. Writer slowpath: WRITE_BIT | WAITING_WRITE_BIT 预设 → 入 FIFO → park
2. Reader 1 unlock_read → wakeup_lock_type(Write):
   wait_lock 内: 找到 Writer (n_matches=1)
   try_lock_write_preset_for → 其他 7 读者仍持锁 → 失败！
   n_matches <= 1 → 清除 WAITING_WRITE_BIT  ← BUG
3. Reader 2~8: unlock_read → WAITING_WRITE_BIT=0 → 直接 return
4. Writer 永久 park
```

**解法**（对应 bcachefs `six.c:412-423` + `six.c:357-409`）双重防护：

```rust
// 防护 1：外层 six_lock_wakeup six.c:416-417 — 读者活跃时直接跳过
// 不进入 wait_lock，WAITING_WRITE_BIT 保持设置
if lock_type == SixLockType::Write && (state & READ_COUNT_MASK) != 0 {
    return;
}

// 防护 2：内层 __six_lock_wakeup six.c:380-402 — handoff 失败不清 WAITING bit
if acquired {
    // ... remove, unpark ...
    n_matches -= 1;  // 成功移除，递减计数
}
// handoff 失败：n_matches 不变 → 不清理 WAITING bit
// 对应 bcachefs: ret <= 0 goto out 跳过 six_clear_bitmask
if n_matches == 0 {
    // 只有 FIFO 中确实无剩余 waiter 时才清 WAITING bit
    self.state.fetch_and(!WAITING_WRITE_BIT, Ordering::Release);
}
```

**关键约束**：
- `wakeup_lock_type(Write)` 必须先检查 `state & READ_COUNT_MASK != 0`（bcachefs `six_lock_wakeup` line 416）。无此检查时每次 unlock_read 都会进 wait_lock，而 handoff 必然因读者活跃而失败。
- `__wakeup_lock_type(Write/Intent)` 只在 `n_matches == 0`（waiter 已全部移除或从未存在）时清除 WAITING bit。handoff 失败时必须保持 WAITING bit 供后续唤醒。
- 这两层防护不是冗余——防护 1 在最外层避免 wait_lock 争用（高频路径优化），防护 2 在 wait_lock 内兜底（防止竞态）。

**验证**：`cargo test -p subvol-core -- "stress::" --ignored` = 8 passed（含 `stress_deadlock_burst_wake`）

## bcachefs 函数覆盖地图 — `lock/six.rs`

> 六锁模块的函数级 bcachefs 对齐状态。→ `util/six.c` (1109 行) + `util/six.h` (536 行)

### 加锁路径

| 我们的函数 | bcachefs 对应 | 行号 | 状态 | 备注 |
|-----------|--------------|------|------|------|
| `lock_write()` | `do_six_lock_ip` / `__six_lock_slowpath` | `six.c:528` | ✅ | 委托 lock_slowpath |
| `lock_read()` | `do_six_lock_ip` / `__six_lock_slowpath` | `six.c:528` | ✅ | 委托 lock_slowpath |
| `lock_intent()` | `do_six_lock_ip` / `__six_lock_slowpath` | `six.c:528` | ✅ | 委托 lock_slowpath |
| `lock_slowpath()` | `__six_lock_slowpath` | `six.c:543` | ✅ | C1+C2+park 统一 |
| `push_waiter_with_recheck()` | `__six_lock_slowpath` wait_lock 段 | `six.c:584` | ✅ | WAITING→TRYLOCK→ENQUEUE |
| `try_lock_write()` | `__do_six_trylock(Write)` | `six.c:186` | ✅ | 断言当前线程是 intent owner；PERCPU 用 atomic_add 预设→读 readers |
| `try_lock_read()` | `__do_six_trylock(Read)` | `six.c:159` | ✅ | 只检查持有中的 write lock；waiting bit 不单独阻断 |
| `try_lock_intent()` | `__do_six_trylock(Intent)` | `six.c:122` | ✅ | CAS 循环 + 仅 intent 冲突检查；write 由升级路径约束 |
| `try_upgrade_read_to_intent()` | `six_lock_tryupgrade` | `six.c:655` | ✅ | 只检查 `INTENT_BIT`；`WRITE_BIT` 不参与直接失败条件 |
| `try_lock_write_preset()` | `__do_six_trylock(Write, try=false)` | `six.c:591` | ✅ | 不检查 HELD_write |
| `try_lock_write_preset_for()` | `__do_six_trylock(Write, false)` | `six.c:163` | ✅ | handoff 路径，断言 waiter 是 intent owner 后替其声明 |
| `lock_ip_waiter()` / `lock_contended()` | `__six_lock_slowpath` | `six.c:543` | ✅ | 委托 lock_slowpath |
| `relock_read()` / `relock_ip()` | `six_relock_ip` | `six.c:470` | ✅ | seq 双检模式 |

### 解锁路径

| 我们的函数 | bcachefs 对应 | 行号 | 状态 | 备注 |
|-----------|--------------|------|------|------|
| `unlock_write()` | `six_unlock_ip` + `do_six_unlock_type(Write)` | `six.c:812, 771` | ✅ | 断言当前线程拥有 intent+write；seq++ → clear WRITE+NOSPIN → wakeup Read |
| `unlock_read()` | `do_six_unlock_type(Read)` | `six.c:778` | ✅ | PERCPU: Release dec; 标准: sub |
| `unlock_intent()` | `do_six_unlock_type(Intent)` | `six.c:775` | ✅ | clear INTENT+NOSPIN → wakeup Intent |
| `unlock_ip()` | `six_unlock_ip` | `six.c:812` | ✅ | 通用释放分派 |

### 唤醒路径

| 我们的函数 | bcachefs 对应 | 行号 | 状态 | 备注 |
|-----------|--------------|------|------|------|
| `wakeup_lock_type()` | `six_lock_wakeup` | `six.c:412` | ✅ | BC3 修复后对齐 |
| `__wakeup_lock_type()` | `__six_lock_wakeup` | `six.c:316` | ✅ | BC3 修复后对齐 |
| `lock_wakeup_all()` | `six_lock_wakeup_all` | `six.c:969` | ✅ | 三类型唤醒 + 剩余 waiter unpark |

### 升级/重入

| 我们的函数 | bcachefs 对应 | 行号 | 状态 | 备注 |
|-----------|--------------|------|------|------|
| `try_lock_write()`（持 intent 时） | `six_trylock_write()`(持 intent 时) | `six.c:186` | ✅ | 与 raw write trylock 共享读者冲突规则；intent 由上层路径约束 |
| `six_lock_increment()` | `six_lock_increment()` | `six.c:932-957` | ✅ | write 分支先增 write recurse，再 fall through 增 intent recurse |
| `lock_readers_add()` | `six_lock_readers_add` | `six.c:1039` | ✅ | PERCPU: this_cpu_add; 标准: atomic_add |
| `lock_counts()` | `six_lock_counts` | `six.c:1004` | ✅ | 读=state/PERCPU, intent=bit+recurse, write=bit |

### 工具函数

| 我们的函数 | bcachefs 对应 | 行号 | 状态 | 备注 |
|-----------|--------------|------|------|------|
| `set_waiting_bit()` | `set_bit` / `atomic_or` | — | ✅ | 简单原子操作 |
| `clear_waiting_bit()` | `clear_bit` / `atomic_and` | — | ✅ | 简单原子操作 |
| `read_state()` / `read_count()` | `atomic_read(&state)` + mask | — | ✅ | 简单原子操作 |
| `has_write_lock()` / `has_intent_lock()` | state mask check | — | ✅ | 简单原子操作 |

### 统计数据

| 度量 | 值 |
|------|-----|
| 函数总数（六锁路径） | ~29 |
| ✅ 已对齐 | 29 (100%) |
| ❓ 未验证 | 0 (0%) |
| ➖ 额外差异 | 0 |
| ➖ 无对应 | 0 |

> **目标**：所有 ✅ 已达 100%。未来新增函数需同步对齐并记录。

## 依赖链

```
D5 (WaitFifo URCU) → D6 (DeadlockDetector) → D7 (should_sleep_fn), D4 (WRITE_BIT)
D1 (bch2_trans_get_iter → get_path) → D3 (restart_with_relock)
```

## bcachefs 对齐验证规范

SixLock 的修改必须严格对齐 bcachefs `fs/util/six.c` + `six.h`。违反此规则会导致竞态（如 C1 丢失唤醒）或行为偏差（如 C2 should_sleep 位置错误）。

### 对齐修改的必做清单

每次修改 SixLock 并声称"对齐 bcachefs"时，必须满足以下所有条件：

1. **读源码确认** — 找到 bcachefs 中对应的逻辑片段，确认完整的上下文
   - **错误做法**：凭记忆或推测写代码 → 写完加注释"对齐 bcachefs"
   - **正确做法**：先读 `six.c`/`six.h` 找到对应函数 → 理解完整上下文 → 再写代码
2. **记录参考位置** — 在注释或 commit 中写明 bcachefs 的精确行号：
   ```rust
   // 对齐 bcachefs __six_lock_slowpath line 637
   ```
3. **验证关键区别** — 确认 subvol 的 Rust 特有抽象是否改变了语义：
   - `wait_lock` 在 bcachefs 中是 `raw_spinlock_t`，subvolmount 是 `Mutex`（语义等价但注意重入）
   - `try_lock_write_preset()` vs `try_lock_write()` — WRITE_BIT 预设后的 trylock 必须用前者
   - percpu reader 实现不同（subvol 用 `AtomicArray`，bcachefs 用 per-CPU 变量）
4. **运行测试** — `cargo test -p subvol-core -- six` = 46 passed

### 常见误区和纠正

| 错误假设 | 事实 | bcachefs 证据 |
|---------|------|---------------|
| "should_sleep 在入队前调" | 在 park 循环内调 | `six.c:637` — `ret = should_sleep_fn(lock, wait)` 在 `FIFO insert`（line 598）之后 |
| "trylock_ip 分发到正确的变体" | `try_lock_write()` 检查 WRITE_BIT，预设后始终 false | `six.c:573` preset → line 591 `__do_six_trylock(lock, type, ...)` 不检查 HELD_write |
| "push_waiter 外双检 trylock 就够了" | 必须 wait_lock 内重试闭合竞态 | `six.c:584-611` — set_bit + trylock + insert 三者都在 wait_lock 临界区内 |

### 修改流程

```
读 bcachefs 源码 → 写 spec 记录对齐点 → 实现 → 写测试 → trellis-check → commit
   ↑                                       ↓
   └── 发现不一致 ← 再确认 bcachefs ←─────────┘
```

## 测试

- `cargo test -p subvol-core -- "lock::" "btree::transaction::"` = 120 passed
- 锁相关测试位于 `lock::six::tests`、`lock::deadlock::tests`、`lock::wait_fifo::tests`
- 事务相关测试位于 `btree::transaction::tests`
