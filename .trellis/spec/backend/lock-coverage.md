# Lock — 锁模块覆盖地图

> 生成日期: 2026-07-08
> 源文件: `crates/subvol-core/src/lock/` (six.rs, deadlock.rs, wait_fifo.rs)
> 参考实现: bcachefs `fs/util/six.h` + `fs/util/six.c`（本地 `/home/black/Documents/bcachefs-tools/`）
> 最后对比: 2026-07-08，基于本地 bcachefs v1.38.8 `six.c:1109行` / `six.h:536行`（家目录含中文注释版）

## 覆盖统计

| 状态 | 数量 | 说明 |
|------|------|------|
| ✅ | 46 | 完全对齐 |
| ⚠️ | 3 | 架构差异（非 bug，上层适配策略不同） |
| 🔴 | 0 | 确认的 bug |
| ❓ | 0 | 未验证 |
| ➖ | 10 | subvol 特有 |
| **总计** | **~59** | |

## SixLock 方法状态

### 生命周期

| subvolmount | bcachefs 对应 | 状态 |
|----------|---------------|------|
| `new()` | `__six_lock_init` | ✅ 语义等价 |
| `with_percpu()` | — | ➖ percpu 模式是 Rust 扩展 |
| `destroy()` | `six_lock_exit` | ✅ |
| `lock_seq()` / `six_lock_seq()` | `six_lock_seq` | ✅ |

### 核心锁操作

| subvolmount | bcachefs 对应 | 状态 |
|----------|---------------|------|
| `six_lock_read()` | `six_lock_type(lk, SIX_LOCK_read)` | ✅ |
| `six_lock_intent()` | `six_lock_type(lk, SIX_LOCK_intent)` | ✅ |
| `six_lock_write()` | `six_lock_type(lk, SIX_LOCK_write)` | ✅ |
| `six_trylock_read()` | `six_trylock_type(lk, SIX_LOCK_read)` | ✅ |
| `six_trylock_intent()` | `six_trylock_type(lk, SIX_LOCK_intent)` | ✅ |
| `six_trylock_write()` | `six_trylock_type(lk, SIX_LOCK_write)` | ⚠️ bcachefs 不排除自身读者（上层通过 `six_lock_readers_add(-N)` 实现）；subvol 内联排除（THREAD_READ_CNT/slot skip），语义等价但架构不同 |
| `try_lock_write_preset()` | `__do_six_trylock(type=write, try=false)` | 🔴 **BUG：跳过 my_slot** — 本地 bcachefs v1.38.8 six.c:103-111 `pcpu_read_count()` 遍历 ALL cpu 无排除；six.c:186-204 write `!try` 路径无读者排除。参见下方「确认的 bug」 |
| `try_lock_write_preset_for(tid)` | `__do_six_trylock(type=write, task=waiter->task, try=false)` | 🔴 同上 |
| `try_upgrade_read_to_intent()` | `six_lock_tryupgrade` | ✅ |
| `six_unlock_read()` | `six_unlock_ip(lk, SIX_LOCK_read, ip)` | ✅ |
| `six_unlock_intent()` | `six_unlock_ip(lk, SIX_LOCK_intent, ip)` | ✅ |
| `six_unlock_write()` | `six_unlock_ip(lk, SIX_LOCK_write, ip)` | ✅ |
| `six_relock_read(seq)` | `six_relock_ip(lk, SIX_LOCK_read, seq, ip)` | ✅ |
| `six_relock_intent(seq)` | `six_relock_ip(lk, SIX_LOCK_intent, seq, ip)` | ✅ |
| `six_relock_write(seq)` | `six_relock_ip(lk, SIX_LOCK_write, seq, ip)` | ✅ |

### 锁转换

| subvolmount bcachefs 名 | bcachefs 对应 | 状态 |
|----------------------|---------------|------|
| `lock_downgrade()` / `six_lock_downgrade()` | `six_lock_downgrade` | ✅ |
| `lock_tryupgrade()` / `six_lock_tryupgrade()` | `six_lock_tryupgrade` | ✅ |
| `lock_increment()` / `six_lock_increment()` | `six_lock_increment` | ✅ write 分支与本地 `six.c:948-953` 一致：`write_lock_recurse++` 后 fall through 到 `intent_lock_recurse++` |
| `lock_ip_waiter()` | `six_lock_ip_waiter` | ✅ |
| `lock_contended()` | `six_lock_contended` | ✅ |

### 通用 dispatch

| subvolmount | bcachefs 对应 | 状态 |
|----------|---------------|------|
| `trylock_ip(type, ip)` / `six_trylock_type(type)` | `six_trylock_ip` | ✅ |
| `unlock_ip(type, ip)` / `six_unlock_type(type, ip)` | `six_unlock_ip` | ✅ |
| `relock_ip(type, seq, ip)` / `six_relock_type(type, seq, ip)` | `six_relock_ip` | ✅ |
| `six_lock_type(type)` | `six_lock_type` | ✅ |

### 其他保留方法

| subvolmount | bcachefs 对应 | 状态 |
|----------|---------------|------|
| `unlock_ip_read(ip)` | `six_unlock_ip(lk, SIX_LOCK_read, ip)` | ✅ 保留（携带 return_ip） |
| `unlock_ip_intent(ip)` | `six_unlock_ip(lk, SIX_LOCK_intent, ip)` | ✅ 同上 |
| `unlock_ip_write(ip)` | `six_unlock_ip(lk, SIX_LOCK_write, ip)` | ✅ 同上 |
| `upgrade_read_to_intent()` | `six_lock_tryupgrade` 重试版 | ✅ |

### 查询

| subvolmount | bcachefs 对应 | 状态 |
|----------|---------------|------|
| `is_write_locked()` | `six_lock_is_write_locked` | ✅ |
| `is_intent_locked()` | `six_lock_is_intent_locked` | ✅ |
| `is_write_locked_by_current()` | — | ➖ per-thread 查询 |
| `is_intent_locked_by_current()` | — | ➖ per-thread 查询 |
| `reader_count()` | — | ➖ debug 辅助 |
| `lock_counts()` | — | ➖ debug 辅助 |
| `is_nospin()` | — | ➖ debug 辅助 |

### 等待/唤醒

| subvolmount | bcachefs 对应 | 状态 |
|----------|---------------|------|
| `lock_wakeup_all()` | `six_lock_wakeup_all` | ✅ |

### WaitFifo

| subvolmount | bcachefs 对应 | 状态 |
|----------|---------------|------|
| `push()` | `six_lock_wait_fifo_push` | ✅ |
| `remove_by_thread()` | `six_lock_wait_fifo_remove` | ✅ |
| `remove()` | `six_lock_wait_fifo_remove` | ✅ |
| `remove_by_index()` | `six_lock_wait_fifo_remove` | ✅ O(1) 槽位清理 |
| `len()` / `is_empty()` | FIFO 观察辅助 | ➖ 调试/守护辅助 |

## DeadlockDetector

| 函数 | 状态 |
|------|------|
| `detect()` | ✅ 对齐 DFS 死锁检测 |
| `with_detector_mut()` | ➖ subvolmount 特有 |

### BtreeTrans 集成

| 函数 | bcachefs 对应 | 状态 |
|------|---------------|------|
| `lock_must_abort` | `trans->lock_must_abort` (locking.c:14-17) | ✅ |
| `lock_may_not_fail` | `trans->lock_may_not_fail` (locking.c:47-51) | ✅ |
| `bch2_check_for_deadlock` | `bch2_check_for_deadlock` (locking.c:189-310) | ✅ |
| `bch2_btree_trans_lock_fn` | `bch2_btree_trans_lock_fn` (locking.c:14-37) | ✅ |

## 确认的 bug

| # | 位置 | 问题 | bcachefs 行为 | 严重度 |
|---|------|------|--------------|--------|
| 1 | `six.rs:534-561` percpu 路径 (`try_lock_write_preset_for`) | 跳过 `my_slot` — 当前线程的 percpu reader 不计入检查 | `pcpu_read_count()`（`six.c:103-111`）遍历 ALL possible CPUs，**无排除**。`__do_six_trylock(write, !try)`（`six.c:186-198`）的 percpu 分支做 `smp_mb(); ret = !pcpu_read_count(lock)` — 无自身排除 | 🔴 HIGH |
| 2 | `six.rs:534-561` non-percpu 路径 (`try_lock_write_preset_for`) | `total_reads.saturating_sub(my_reads)` 先减自身再判零 | `__do_six_trylock(write, !try)` 的 non-percpu 分支（`six.c:159-167`）做 `ret = !(old & l[type].lock_fail)` — `lock_fail` = `SIX_LOCK_HELD_read`，直接读 raw state reader bits，**无减法** | 🔴 HIGH |
| 3 | `six.rs:1094-1095` (`lock_slowpath`) | `self.state.fetch_or(WRITE_BIT\|WAITING_WRITE_BIT, Ordering::SeqCst)` 后紧跟 `fence(Ordering::SeqCst)` — `fetch_or(SeqCst)` 已是 RMW + SeqCst 全屏障，`fence` 冗余 | bcachefs `__six_lock_slowpath`（`six.c:571-575`）做 `atomic_add(SIX_LOCK_HELD_write, &lock->state)` + `smp_mb__after_atomic()` — 一次屏障。WAITING_WRITE_BIT 在 `wait_lock` 内 `six_set_bitmask`（`six.c:590`）设置，spin_lock 提供排序 | 🟡 MEDIUM |

## 已验证的架构差异

| # | 差异 | 说明 |
|---|------|------|
| 1 | 写锁 trylock 排除自身读者 | bcachefs 依赖上层 `six_lock_readers_add(-N)`；subvol 内联排除。`try=true` 路径可接受，`try=false` 路径是 bug |
| 2 | 无 `EBUG_ON(intent_held)` 在 write unlock | bcachefs `six_unlock_ip`（`six.c:814-815`）有 `EBUG_ON(type == SIX_LOCK_write && !(state & SIX_LOCK_HELD_intent))`；subvol 无此断言。非安全关键 |
| 3 | WAITING bit 清理策略 | C（`six.c:402`）：仅 `__six_lock_wakeup` 中 lazy 清理（n_matches==0 时逐个类型清）。`six_lock_wait_fifo_remove`/`six_lock_wait_fifo_shrink` 不碰 WAITING bit。subvol 的 `remove_self_from_fifo`（`six.rs:1642-1656`）在 FIFO 空时 eager 清理所有 WAITING bit。两者语义等价（`wait_lock` 保护），但策略不同 |

## 已完成修复

| # | 项目 | 状态 |
|---|------|------|
| 1 | 删除 `downgrade_intent_to_read`（已用 `lock_downgrade` 替代） | ✅ |
| 2 | `try_upgrade_read_to_intent` 改为内部 `fn`（外部用 `lock_tryupgrade`） | ✅ |
| 3 | `seq()` → `lock_seq()` + 更新全部调用方 | ✅ |
| 4 | 新增 bcachefs 风格别名 | ✅ |
| 5 | 消除 Rust 风格命名：12 个主方法体重命名为 `six_*`，删除 12 个旧别名 + 9 个冗余 `*_type_*` 别名 | ✅ |
| 6 | `try_lock_write_preset_for` 修复（跳 my_slot bug） | ✅ |
| 7 | `try_lock_write_preset` 修复（减 THREAD_READ_CNT bug） | ✅ |
| 8 | `lock_slowpath` 冗余 `fence(SeqCst)` 去除 | ✅ |
| 9 | `six_lock_increment(SIX_LOCK_write)` 级联增加 write + intent recurse | ✅ |
| 10 | intent/write unlock 与 held bit 同时清除 `SIX_LOCK_NOSPIN` | ✅ |

## 强制审查流程

对 lock 模块的所有 bcachefs 对齐审查，**必须按以下步骤进行**，不得仅凭推理或注释比对：

### 步骤 1：确认本地 bcachefs 源码路径

```bash
ls /home/black/Documents/bcachefs-tools/fs/util/six.{c,h}
```

如路径不存在，先定位本地安装的 bcachefs 源码（`which bcachefs`）。

### 步骤 2：确定对比目标函数

通过 grep 定位 bcachefs 中的对应函数：

```bash
grep -n "函数名" /home/black/Documents/bcachefs-tools/fs/util/six.c
```

### 步骤 3：逐行语义对比

对比维度：
- **读者排除**：`pcpu_read_count()`（six.c:103-111）是否遍历 ALL cpu？subvol 对应处是否匹配？
- **位操作**：state 的 `WAITING_*` 位定义（six.c:26-32）、`lock_fail` 表（six.c:48-67）是否一致？
- **内存序**：bcachefs 的 `smp_mb()` / `smp_mb__after_atomic()` 是否被正确翻译为 Rust 的 `Ordering`？
- **错误路径**：trylock 失败后是否需要 `__six_lock_wakeup`（six.c:453-456）？subvol 是否等价处理？
- **慢路径协议**：`six_set_bitmask(lock, SIX_LOCK_WAITING_read << type)` → `__do_six_trylock(..false)` → 入队顺序（six.c:612-621）是否一致？

### 步骤 4：记录差异

| 结果 | 行动 |
|------|------|
| ✅ 完全对齐 | 更新覆盖统计的 ✅ 计数 |
| ⚠️ 架构差异 | 记录到「已验证的架构差异」表，说明理由 |
| 🔴 确认 bug | 记录到「确认的 bug」表，标注严重度 |
| ❓ 未验证 | 记录到覆盖统计，标记下次审查 |

### 步骤 5：更新覆盖统计行

更新文件开头的统计表：
```
| ✅ | N | 完全对齐 |
| ⚠️ | N | 架构差异 |
| 🔴 | N | 确认的 bug |
| ❓ | N | 未验证 |
| ➖ | N | subvol 特有 |
```
