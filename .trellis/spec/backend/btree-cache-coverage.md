# BtreeCache — 缓存模块覆盖地图

> 生成日期: 2026-06-30 (P2 更新)
> 最后更新: 2026-07-18 (P1 evicted_size 修复 + recalc_btree_reserve 偏差文档)
> 源文件: `crates/subvol-core/src/btree/cache.rs` (~2500 行)
> 参考实现: bcachefs `fs/btree/cache.c` + `fs/btree/cache.h`

## 覆盖统计

| 状态 | 数量 | 说明 |
|------|------|------|
| ✅ | 34 | 完全对齐 |
| ⚠️ | 0 | 部分对齐 |
| ❓ | 0 | bcachefs 有但 subvol 无 |
| ➖ | 23 | subvolmount 特有 |
| **总计** | **56 (subvolmount)** | + 0 bcachefs 未实现 |

#### bcachefs 函数在 subvol 中由 Rust 架构等效替代（3 项）

| bcachefs 函数 | Rust 替代 | 说明 |
|--------------|-----------|------|
| `bch2_btree_node_data_free` | `Arc::drop` | 节点数据随 Arc 引用计数自动释放 |
| `bch2_btree_cache_cannibalize_to_text` | `bch2_btree_cache_to_text` | to_text 已包含 cannibalize 状态输出 |
| `bch2_fs_btree_cache_exit` | `Arc::drop` | 缓存生命周期由 Arc 引用管理，无需显式 exit |

## 函数状态表

### 生命周期
| 函数 | bcachefs 对应 | 状态 |
|------|---------------|------|
| `new()` | `bch2_fs_btree_cache_init_early` + `bch2_fs_btree_cache_init` | ➖ 无 kernel 生命周期约束，early/init 两阶段合并 |
| `with_journal()` | — | ✅ subvolmount 扩展 |
| `bch2_btree_node_mem_free()` | `bch2_btree_node_mem_free()` | ✅ Arc 生命周期收口 |
| `bch2_btree_node_transition_state()` | `bch2_btree_node_transition_state()` | ✅ cache-side thin wrapper |
| `bch2_btree_node_transition_state_locked()` | `bch2_btree_node_transition_state_locked()` | ✅ cache-side thin wrapper |
| `bch2_btree_node_write_done_clean()` | `bch2_btree_node_write_done_clean()` | ✅ write 完成收口 |
| `bch2_fs_btree_evicted_size_init()` | `bch2_fs_btree_evicted_size_init()` | ✅ `EvictedSizeTable` 固定大小 AtomU64 数组（128K 条目 min），对齐 `cache.h:60-83` + `types.h:1041-1044` |
| `bch2_fs_btree_evicted_size_exit()` | `bch2_fs_btree_evicted_size_exit()` | ✅ 清空 evicted size 生命周期 |
| `len()` / `is_empty()` | `btree_cache_list_nr` | ✅ |

### 节点查找/加载
| 函数 | bcachefs 对应 | 状态 |
|------|---------------|------|
| `get_or_load()` | `bch2_btree_node_get` (部分) | ➖ 缺 trans/iter（Rust 闭包替代）; ✅ 支持 InFlight 等待与 accessed 刷新 |
| `bch2_btree_node_get()` | `bch2_btree_node_get` | ➖ 缺 mem_ptr（subvol 无打包格式）; 命中时刷新 accessed |
| `bch2_btree_node_evict()` | `bch2_btree_node_evict` | ✅ 先等待 read/write in-flight 再移除 |
| `get()` | `btree_cache_find` | ✅ |

### 脏节点管理
| 函数 | bcachefs 对应 | 状态 |
|------|---------------|------|
| `mark_dirty()` | — | ✅ auto-flush 不丢数据 |
| `bch2_btree_node_set_dirty()` | `bch2_btree_node_set_dirty` | ✅ 使用 `NODE_NEED_REWRITE` 表示需要写回，写完成入口清理 |
| `flush_dirty()` | — | ✅ 拓扑排序 P0-2 |
| `insert_dirty()` / `insert()` | — | ✅ |

### Shrinker
| 函数 | bcachefs 对应 | 状态 |
|------|---------------|------|
| `shrink()` | `bch2_btree_cache_scan` | ➖ 两阶段 clock ✅，MM 集成为 kernel 特有（OOM notifier），subvol 无 MM 层 |
| `shrink_one()` | — | ✅ |

### Debug / text
| 函数 | bcachefs 对应 | 状态 |
|------|---------------|------|
| `bch2_btree_cache_to_text()` | `bch2_btree_cache_to_text` | ➖ subvol 使用可得统计段，覆盖核心指标 |

### Cannibalize
| 函数 | bcachefs 对应 | 状态 |
|------|---------------|------|
| `bch2_btree_cache_cannibalize_lock()` | 同名 | ✅ Atomic CAS vs cmpxchg |
| `bch2_btree_cache_cannibalize_unlock()` | 同名 | ✅ Atomic store |
| `try_cannibalize_phase1/phase2()` | `btree_node_cannibalize` | ✅ |

### 节流控制
| 函数 | bcachefs 对应 | 状态 |
|------|---------------|------|
| `bch2_recalc_btree_reserve()` | `bch2_recalc_btree_reserve()` (cache.c:123-138) | ⚠️ 架构偏差 — bcachefs 计算 `nr_reserve`（预分配节点数），subvol 无内核 MM shrinker，本函数计算 `should_throttle`（写节流标志）。语义不同但 subvolmount 不需要 nr_reserve。 |
| `bch2_btree_cache_should_throttle()` | 同名 | ✅ |
| `bch2_btree_cache_update_throttle()` | 同名 | ✅ |

## P1/P2 差距（已关闭）
- P1: 5 态状态机 — ✅ 已实现 (`bch2_btree_node_transition_state`)
- P1: pinned 节点保护 — ✅ 已实现 (`bch2_node_pin` / `bch2_btree_cache_unpin`)
- P1: transition_state 原语 — ✅ 已实现 (NODE_ACCESSED 等位标志)
- P1: reclaim 锁协议 — ✅ 已实现 (`btree_node_reclaim`)
- P2: prefetch — ✅ 已实现 (`bch2_btree_node_prefetch` + iter 集成)；
  `bch2_btree_node_prefetch_id` 现在先发布 InFlight 节点，再后台读取并收口，
  对应本地 `fs/btree/cache.c:1585-1598` 的 `bch2_btree_node_fill(..., false)`
  fire-and-forget 顺序，不在迭代下降路径同步等待后端 I/O。
- P2: 异步 fill — ✅ 已实现 (`bch2_btree_node_fill`, sync=true/false)
- P2: eviction 等待 IO — ✅ 已实现 (`bch2_btree_node_evict` 先等 read/write in-flight)

## Volmount 特有 (➖)
- `get_or_load()` / `bch2_btree_node_get()` 的预清理扫描 — miss 且 clean cache 接近满或系统内存压力升高时，先扫一轮 LRU，再进入硬驱逐
- `nr_live` 在所有节点装载路径上的同步维护 — 直接驱动节流与系统压力近似判定
- `shrink_one()` — 简化版 shrinker
- `try_cannibalize_phase1/phase2()` — 两阶段 cannibalize
- `insert_dirty()` — 直接插入 dirty 列表
- `evict_one_leaf()` — leaf 优先驱逐辅助
- `alloc_node_for_key()` — 基于 BtreeKey 分配新节点
- `read_node_data()` — 后端读取节点数据，供 sync/async fill 复用
- `bch2_btree_node_prefetch_id()` — 基于 node_id 的预取变体
- `prefetch_node()` — NodeCache 预取委托

<!-- `src/cache/mod.rs` 遗留 stub 已于 2026-07-05 清理，所有 cache 实现在 `btree/cache.rs` 中 -->

## P2 实现总结 (2026-06-30)

### 新增公开 API

| 函数 | 签名 | 用途 |
|------|------|------|
| `bch2_btree_node_prefetch_id` | `(node_id: u64, level: u8, _btree_id: BtreeId) -> bool` | 基于 node_id 的预取，用于 BtreeIter 下降路径 |
| `prefetch_node` | `(block_addr: u64, level: u8, btree_id: BtreeId) -> bool` | NodeCache 预取委托 |

### BtreeIter 集成

- `BtreeIter::init()` descent 循环：加载子节点后，预取下一个兄弟节点
- `back_up_and_advance()`：预取再下一个兄弟节点（readahead 风格）
- `rewind_impl()` / `prev_slot()`：跨兄弟节点回退时，复用同一套 sibling lookup + prefetch 逻辑

### 🔥 关键学习：竞态条件与修复

**竞态 1：`bch2_btree_node_fill` InFlight 状态设置顺序**

```rust
// 🚫 错误：先 insert，再设置 InFlight（另一个线程可能获取到空节点）
let node = self.alloc_node_for_key(key, level, btree_id);
// 节点已在缓存中（state=Alive），下一个线程的 get_or_load 可见
node.transition_state(InFlight);   // ⚠️ 太晚了
node.set_read_in_flight();

// ✅ 正确：先设置 InFlight，再 insert（节点可见时即处于保护状态）
let node = Arc::new(BtreeNode::new(level));
node.transition_state(InFlight);
node.set_read_in_flight();
self.insert(node_id, node.clone());
```

**竞态 2：`get_or_load` 持有 inner.lock 期间等待**

```rust
// 🚫 错误：持有缓存全局锁（inner）时等待 Condvar
let mut inner = self.inner.lock().unwrap();
inner.dirty.get(&node_id).unwrap().wait_on_read(None);
// 整个缓存被锁住，其他线程无法操作

// ✅ 正确：先克隆 Arc 节点，释放 inner，再等待
let mut inner = self.inner.lock().unwrap();
let node = inner.dirty.get(&node_id).unwrap().clone();
drop(inner);
node.wait_on_read(None);
```

**一致性：`bch2_btree_node_get` 必须与 `get_or_load` 行为一致**

两个方法都返回缓存节点，都必须在命中路径中检查 `read_in_flight` 标志。`bch2_btree_node_get` 的三个命中路径（dirty/pending_flush/clean）已同步添加 `wait_on_read`。

## P1 偏差修复（2026-07-18）

### 1. evicted_size 追踪 — 固定大小表

**偏差**：subvolmount 之前使用 `HashMap<u64, u16>` 追踪被驱逐节点的 live_u64s，这是一个**无界**哈希映射，随缓存操作持续增长，与 bcachefs 的固定大小、有界表不一致。

**bcachefs 机制** (`cache.h:60-83`, `types.h:1041-1044`):
- `struct btree_evicted_size`: 固定大小 `u64 *entries` 数组 + `u64 mask`（2 的幂）
- `bch2_btree_evicted_size_init()`: 基于文件系统容量计算表大小，最小 128K 条目（1 MiB）
- `bch2_btree_evicted_size_record(c, hash, live_u64s)`: 使用 `WRITE_ONCE` 将 `(hash << 16 | live_u64s)` 写入 `entries[hash & mask]`
- `bch2_btree_evicted_size_lookup(c, hash, &out)`: 使用 `READ_ONCE` 读取并验证哈希高位匹配

**修复**：
- 新增 `EvictedSizeTable` 结构体：`Box<[AtomicU64]>` 固定大小数组，128K 条目起步
- 替换 `BtreeCacheInner` 中的 `evicted_sizes: HashMap<u64, u16>` 为 `evicted_sizes: EvictedSizeTable`
- `record()` / `lookup()` 方法与 bcachefs 语义一致（packed entry、hash-verify on lookup）
- `lookup_evicted_size()` 保持锁内调用（与 `BtreeCacheInner` 一致），但表本身为无锁 `AtomicU64` 操作
- 参考行号：`cache.h:55-83` + `types.h:1041-1047` + `cache.c:1831-1853`

### 2. bch2_recalc_btree_reserve — 架构偏差归档

**偏差**：subvol 的 `bch2_recalc_btree_reserve` 计算 `should_throttle`（布尔节流标志），而 bcachefs 同名函数计算 `nr_reserve`（预分配节点数）。

**bcachefs 语义** (`cache.c:123-138`):
- `nr_reserve = 16 + (no roots? 8) + Σ(min(1, level) * 8)` — 为 cannibalization 预分配的可回收节点数
- 在 `bch2_fs_btree_cache_init` (line 1669) 中用于预分配 `nr_reserve` 个 freeable 节点
- 在 `bch2_btree_node_update_start`/split/merge 后重新计算

**subvolmount 差异原因**：
- subvol 无内核 MM shrinker
- 不需要 cannibalization 预分配（无内存压力回收场景）
- 节流标志基于 in-flight I/O 和 dirty 占比，用于通知调用者等待写入完成
- **结论**：架构差异，无需修改。已在函数注释中记录 bcachefs 原始语义。

### 3. alloc btree_reserve shrinker — 无需修复

**bcachefs 机制** (`background.c:1603`):
- `dev_reserve += ca->nr_btree_reserve * 2` — 在 `__bch2_fs_capacity()` 计算中用于判断是否等待分配
- `nr_btree_reserve` 由 `calc_btree_reserve_buckets()` 设置，基于 bucket_size 和 btree_node_size
- 不是 shrinker 回调，而是容量计算中的预留项

**subvolmount 状态** (`alloc/background.rs:69` + `alloc/mod.rs:227`):
- `dev_reserve += ca.nr_btree_reserve * 2` — 已对齐
- `calc_btree_reserve_buckets()` — 已实现
- 无 MM shrinker 依赖，subvolmount 不需要自动调整
- **结论**：已对齐，无需修复。
