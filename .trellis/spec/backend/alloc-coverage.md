# Alloc Coverage

> Alloc module bcachefs function-level coverage map.
> Updated: 2026-07-14 (disk reservation cache protocol aligned with bcachefs)

## Statistics

| Status | Count | % |
|--------|-------|---|
| ✅ 已验证对齐 | 77 | 62.6% |
| ⚠️ 已知偏差 | 0 | 0% |
| ❓ 未验证 | 0 | 0% |
| ➖ 无 bcachefs 对应 | 46 | 37.4% |
| **合计** | **~123** | **100%** |

## 已发现的问题

## Scenario: allocator does not consume transaction reservations

### 1. Scope / Trigger

- 修改 `bch2_alloc_sectors_start_trans()`、open-bucket reuse 或新 bucket 分配时适用。
- 唯一依据是本地 `fs/alloc/foreground.h:365-429` 与
  `fs/btree/commit.c:1125-1160`。

### 2. Contract

allocator 只负责选择 bucket、推进 write point/open bucket 状态并返回物理地址；它不直接
递减 `disk_reservation` 或 `online_reserved`。extent/btree transaction 的 usage delta
在 commit 的 accounting hook 中统一消费 reservation，并在 Atomic 错误路径恢复已发布的
usage/reservation 状态。这样一个 allocation 不会在 allocator 和 transaction 两处双消费。

### 3. Validation

- 分配与 open-bucket reuse 后 reservation 状态不被 allocator 直接改变。
- extent transaction commit 才递减 reservation/online_reserved；失败回滚恢复原值。
- production source 不得出现 `AllocRequest.reservation`、`with_reservation` 或 allocator
  内的 reservation 直扣路径。

## Scenario: backpointer updates share the extent transaction

Extent pointer mark/update must append the corresponding backpointer insert/delete to the same
`BtreeTrans` journal. The local `bch2_bucket_backpointer_mod()` operates on the transaction
iterator; direct immediate writes to the backpointer btree would expose alloc metadata without its
paired extent update and would bypass commit/restart ordering.

## Scenario: filesystem usage short and capacity lifecycle

### 1. Scope / Trigger

- 修改 filesystem usage、disk reservation free-space calculation、capacity per-context state 或
  volume constructor/exit 顺序时适用。
- 唯一依据是本地 `fs/alloc/buckets_types.h:78-90`、`fs/alloc/types.h:165-188`、
  `fs/alloc/buckets.c:65-98`、`fs/alloc/buckets.h:413-418` 与
  `fs/alloc/background.c:1736-1757`。

### 2. Signatures

- `bch2_fs_usage_read_short(c: &BchVol) -> BchFsUsageShort`。
- 内部 `__bch2_fs_usage_read_short()` 只在调用者已持有 `capacity.mark_lock` 时使用。
- `bch2_fs_capacity_init(c: &BchVol) -> Result<(), StorageError>`。
- `bch2_fs_capacity_exit(c: &BchVol)`。

### 3. Contracts

- 当前 runtime 没有 CPU pinning，`capacity.pcpu` 固定初始化为一个 zeroed slot；所有
  per-context usage、available cache 与 online reservation 均落在该 slot。
- usage short 在一次循环中累加完整 slot，包括随后丢弃的 `sectors_available`；不得按字段
  多次扫描或把 cached available 算入 free space。
- 计算顺序固定为 `capacity-hidden`、`data+btree`、`reserved+online_reserved`、
  `min(capacity, data+reserve_factor(reserved))`、`capacity-used`。
- `reserve_factor(r) = r + (round_up(r, 64) >> 6)`；
  `avail_factor(r) = (r << 6) / 65`，保留 unsigned arithmetic，不改成 saturating 公式。
- public usage read 必须先取得 `mark_lock` 读锁；不得在其中获取
  `sectors_available_lock`。
- capacity init 必须在 device bucket 建立和首次 recalc 之前执行；exit 检查所有 slot 的
  `online_reserved` 后再清空 slot。

### 4. Validation & Error Matrix

| 条件 | 结果 |
|---|---|
| local/global `sectors_available` 非零 | 两者均不参与 usage short 结果 |
| `hidden <= capacity` | `capacity = filesystem capacity - hidden` |
| `data + reserve_factor(reserved) > capacity` | `used` clamp 到 capacity，`free = 0` |
| exit 时 `online_reserved != 0` | 发出 shutdown warning，随后仍按本地顺序清空 slot |
| 构造后首次 capacity recalc | pcpu slot 已初始化，不读取未建立的 per-context state |

### 5. Good / Base / Bad Cases

- Good：data、btree、reserved 与 online reservation 同时非零，按本地顺序得到 used/free，
  改变 available cache 不改变结果。
- Base：全部 usage 为零时 `used = 0`、`free = capacity`，且恰有一个 zeroed slot。
- Bad：把 local/global `sectors_available` 加入 free，或在 device bucket/recalc 之后才执行
  capacity init，会让 refill cache 被重复计入或暴露未初始化状态。

### 6. Tests Required

- hidden/data/btree/reserved/online_reserved 组合值逐字段验证。
- 0、64 对齐、非对齐 reserve factor 和截断 avail factor 逐项验证。
- 同时改变 local/global `sectors_available` 不得改变 usage short。
- 两条 `BchVol` 构造路径均初始化恰好一个 slot；exit 后 slot 为空，再 init 后重新 zeroed。
- 定向与全量 lib 测试均必须在 60 秒内结束。

### 7. Wrong vs Correct

```rust
// Wrong: cached reservation refill state is counted as free filesystem space.
free = capacity - used + pcpu.sectors_available;

// Correct: sum the complete slot, then intentionally ignore cached available sectors.
let reserved = usage.reserved + pcpu.online_reserved;
let used = capacity.min(data + reserve_factor(reserved));
let free = capacity - used;
```

## Scenario: per-device filesystem capacity recalculation

### 1. Scope / Trigger

- 修改 member state、durability、bucket geometry、btree reserve、GC reserve 或 allocator
  wait path 时适用。
- 唯一依据是本地 `fs/alloc/background.c:1569-1648`、`fs/alloc/types.h:165-188`、
  `fs/alloc/foreground.c:79-91` 与 `fs/init/dev.c:393-442,867-938`。

### 2. Signatures

- `bch2_recalc_capacity(c: &BchVol)`：调用者必须持有 `state_lock`。
- `bch2_min_rw_member_capacity(c: &BchVol) -> u64`。
- `bch2_alloc_wake_all(c: &BchVol)`。
- filesystem-level member state 发布必须同步 superblock member、`BchDev.mi.state` 与
  IO atomic view；低层 device setter 不得只更新其中一个视图。

### 3. Contracts

- 内部 `BchFsCapacity.capacity/reserved` 单位为 512-byte sectors，不得复用或改变
  NBD/API 的逻辑字节容量。
- 容量遍历只计入 `mi.state == RW && mi.durability != 0` 的 member device；每设备
  reserve 严格按 btree reserve、copygc、三个 write point、bucket size 的原始顺序计算，
  累加时再乘二。
- 容量和最小 RW 成员容量遍历还必须过滤 online 设备；本地
  `for_each_member_device_rcu(..., &c->devs_online)`/`for_each_rw_member_rcu()` 不会把
  已离线但仍残留 `RW` 状态的设备计入可用容量。
- 每种分配请求还必须过滤 member 的 `data_allowed` 位；本地
  `bch2_bucket_alloc()` 在 `fs/alloc/foreground.c:565` 先做该检查。卷格式化路径默认允许
  journal/btree/user，显式限制后的设备不能接收不允许的数据类型。
- raw capacity 为 `(nbuckets - first_bucket) * bucket_size`；设备 reserve 与 GC-percent
  reserve 先取最大值，再 clamp 到 raw capacity，然后依次发布 reserved、可用 capacity、
  `bucket_size_max`。
- 发布结束后必须按 member-device 顺序递增每个 allocator wake counter，最后只 notify
  一次 filesystem-global freelist wait list。
- 构造时必须等全部 device buckets 建立后统一重算；live resize 只在 geometry/runtime
  成功发布后重算；前置错误不得改变已发布容量。

### 4. Validation & Error Matrix

| 条件 | 结果 |
|---|---|
| RO / evacuating / spare member | 不计入容量 |
| durability 为零 | 不计入容量 |
| device reserve 大于 raw capacity | reserved clamp 到 raw capacity，可用 capacity 为零 |
| 无 RW member | aggregate scalars 为零；minimum RW capacity 为 `u64::MAX` |
| live resize geometry 非法 | 返回原错误，容量状态不变 |

### 5. Good/Base/Bad Cases

- Good：两个 RW durable device 使用不同 bucket size，分别累加 raw/device reserve，
  `bucket_size_max` 取较大值，发布后所有 device counter 各加一。
- Base：单设备卷走相同聚合路径，内部 sector capacity 与对外逻辑 byte capacity 分离。
- Bad：直接用 `VolumeConfig.capacity` 作为内部可用 sectors，或在 geometry 发布前重算，
  会造成单位错误或发布半初始化容量。

### 6. Tests Required

- 单设备和不同 bucket geometry 的多设备逐项校验 raw/device/GC reserve、clamp 与最大
  bucket size。
- 覆盖所有非 RW state、零 durability、空集合、resize 成功/失败与 RW→非 RW→RW。
- 每次 recalc 断言所有 device wake counter 恰好加一且 global waiter 被唤醒。
- 同时断言 `BchVol::capacity()` 与 `BlockVolume::size()` 仍为配置的逻辑字节数；每条
  测试及全量 lib suite 必须在 60 秒内结束。

### 7. Wrong vs Correct

```rust
// Wrong: logical bytes overwrite bcachefs internal sector capacity.
c.capacity = c.logical_capacity;

// Correct: recalc from per-device runtime geometry under state_lock.
capacity += bucket_to_sector(ca, (mi.nbuckets - u64::from(mi.first_bucket)) as usize);
fs_capacity.capacity = capacity - reserved_sectors;
```

## Scenario: per-device metadata bucket marking

### 1. Scope / Trigger

- 修改 superblock layout、journal bucket ownership、device-add recovery、Alloc metadata
  标记或 GC metadata accounting 时适用。
- 唯一依据是本地 `fs/alloc/buckets.c:961-1181`、`fs/init/dev.c:1131-1161`、
  `fs/journal/init.c:18-299` 与 `fs/bcachefs_format.h:1129-1141`。

### 2. Signatures

- `bch2_trans_mark_metadata_bucket(c, ca, b, data_type, sectors, flags)`
- `bch2_trans_mark_dev_sb(c, ca, flags)` / `bch2_trans_mark_dev_sbs_flags(c, flags)`
- `bch2_is_superblock_bucket(ca, b) -> bool`
- `bch2_dev_add_initialize(c, ca)`

`BackupSbLayout` 必须包含 `magic/layout_type/sb_max_size_bits/nr_superblocks/pad` 与
固定 `[u64; 61]` sector offsets；`BchDev` 必须直接拥有 `disk_sb` 与
`journal_device` runtime。

### 3. Contracts

- layout offset、range length 与 `ca.mi.bucket_size` 均为 512-byte sectors；
  `nr_superblocks` 之外的 offset 不参与标记或副本 IO。
- subvol `Journal`/`BchSb.journal_buckets` 保存 block address；进入 per-device
  `journal_device.buckets` 时除以该设备 `bucket_size / SECTORS_PER_BLOCK`，写回
  superblock 时反向相乘。metadata marking 内部只接收 bucket number。
- transactional 分支读取并保留完整 alloc v4，只在 type/sectors 改变时更新；GC
  分支在 bucket lock 内执行 old/type/overflow/add/new，锁外更新 device counters。
- transactional metadata 更新还必须同步 `BchDev` runtime bucket、free count、allocated
  sectors 与 Freespace bit；否则 journal persistence rollback 只会改 alloc v4，allocator
  runtime 仍泄漏容量。
- `bch2_trans_mark_dev_sbs_flags()` 对每个 online device 先恢复
  `bch2_dev_add_initialize()`，再无条件重复 metadata 标记；错误立即返回并由 guard
  释放当前 READ IO ref。
- device-add 初始化阶段严格为 usage → state → mark-sb → state → freespace → state →
  journal allocation → initialized；每个 state 写入必须先持久化对应 device superblock。

### 4. Validation & Error Matrix

| 条件 | 结果 |
|---|---|
| metadata bucket `>= nbuckets` | 成功跳过，允许尾部 backup superblock |
| transactional 旧 type 与新 metadata type 冲突 | 记录 fsck error，返回 metadata inconsistency，原 alloc v4 不变 |
| GC runtime bucket 不存在 | 记录 filesystem error，返回 metadata inconsistency |
| GC type mismatch / sector overflow | 锁内检查点失败，不修改 bucket，不做 accounting |
| layout type 非 0、count 不在 1..=61、size bits > 16、range 重叠 | superblock 读取失败 |
| partially initialized device | 从持久化 initialized stage 继续，不重做之前阶段 |

### 5. Good/Base/Bad Cases

- Good：两个设备分别使用不同 layout/journal，生成 `(dev_idx, bucket)` alloc v4 key，
  online iterator 结束后所有 READ ref 为 0。
- Base：重复 transactional 标记保持非 metadata alloc v4 字段不变且不累计 sectors。
- Bad：把 Journal block address 直接写进 `journal_device.buckets`；例如 block 256
  应在 1 MiB bucket geometry 下转换为 bucket 1，否则可能错误覆盖 bucket 256。

### 6. Tests Required

- transactional idempotence/type conflict 必须断言原始 alloc v4 bytes 未覆盖。
- sector range 必须覆盖跨 bucket tail、`BCH_SB_SECTOR` 前缀与最终 flush。
- GC 必须覆盖正常 accounting、invalid runtime bucket、type mismatch 与 overflow。
- device initialization 必须覆盖 `PreDevUsage` 全链和 `PreJournalAlloc` 的 8-bucket
  最小 journal 分配；全量 lib 测试必须在 60 秒内通过。
- recovery 测试中的 Journal block addresses 必须 bucket 对齐，并断言产出的所有
  Alloc raw values 都能通过 alloc v4 decoder。

### 7. Wrong vs Correct

#### Wrong

```rust
ja.buckets = sb.journal_buckets.clone(); // block address 被误当 bucket number
futures::executor::block_on(sb.write_to_device(ca))?; // Tokio 后端可能无 reactor 进展
```

#### Correct

```rust
ja.buckets = sb.journal_buckets.iter().map(|addr| addr / bucket_blocks).collect();
// 同步 bch2_write_super 语义通过独立 runtime 线程桥接 async backend。
```

## Scenario: per-device bucket runtime ownership

### 1. Scope / Trigger

- 修改 alloc、freespace、BucketGens、GC、recovery 或 open-bucket 的设备路由时适用。
- 唯一依据是本地 `fs/bcachefs.h` 的 `struct bch_dev`、`fs/alloc/types.h`
  的 `struct bch_fs_allocator` 与 `fs/sb/members.h` 的 online-member 迭代。

### 2. Signatures

- `bch2_dev_buckets_alloc(c: &BchVol, ca: &BchDev) -> Result<(), StorageError>`
- `bch2_dev_buckets_resize(c: &BchVol, ca: &BchDev, nbuckets: u64) -> Result<(), StorageError>`
- allocator 的 bucket 操作必须显式接收 `ca: &BchDev`；不得从
  `BchAllocator` 或 block address 推断设备。
- `bch2_get_next_online_dev(previous, state_mask, rw)` 返回持有对应 IO ref 的 guard。

### 3. Contracts

- `BchVol` 只持有一份 filesystem-global `BchAllocator`；其中只有 global open
  buckets、write points 与 reservations。
- `BchDev` 直接持有 groups/buckets、GC buckets、gens、usage、alloc cursor、
  freespace initialized、open bucket count 与 btree reserve。
- Alloc/Freespace/BucketGens key 的 `Bpos.inode` 必须等于 `dev_idx`；extent
  trigger 与 GC 必须使用 `ExtentPtr.dev` 路由。
- `OpenBucket` 的唯一身份是 `(dev: u8, bucket: u64)`；pool 为 filesystem-global，
  两个设备的同号 bucket 必须能同时存在。
- online member 按 dev_idx 升序：先释放 previous ref，再检查 state mask，再
  tryget READ/WRITE ref；offline/tryget 失败继续下一设备。

### 4. Validation & Error Matrix

- key device 不存在 -> 跳过该 device key range，不回退到 device 0。
- bucket `< first_bucket` 或 `>= nbuckets` -> 不访问 runtime array。
- member geometry 缺失/无效 -> 构造或 resize 返回错误，不创建 fallback runtime。
- open bucket pool 耗尽 -> 保持原分配回滚顺序，只回滚目标 `BchDev`。
- online iteration 提前退出 -> guard drop 必须释放最后一个 IO ref。

### 5. Good/Base/Bad Cases

- Good: dev 0 与 dev 1 分别打开 bucket 3，Alloc key 为 `(0,3)` 与 `(1,3)`，
  两个设备 usage 独立增加。
- Base: 单设备卷仍显式解析 primary `BchDev` 后调用同一 API。
- Bad: `Bpos::new(0, bucket, 0)` 处理任意 `ExtentPtr.dev`，或维护
  `HashMap<u8, BchAllocator>`。

### 6. Tests Required

- 两设备同号 bucket allocation/open lookup 均成功且返回不同 open-bucket index。
- alloc-read、BucketGens、freespace rebuild 与 GC 按 key/pointer dev 更新对应 runtime。
- online READ/WRITE 迭代覆盖顺序、state-mask、offline skip、正常前进、结束与提前
  drop 后 refcount 为 0。
- 每条单元测试及全量 lib 测试必须在 60 秒内完成。

### 7. Wrong vs Correct

#### Wrong

```rust
let alloc_pos = Bpos::new(0, bucket, 0);
let allocator = allocators.get(&ptr.dev).unwrap();
```

#### Correct

```rust
let ca = vol.device_rcu_noerror(ptr.dev).ok_or(...)?;
let alloc_pos = Bpos::new(ca.dev_idx as u64, bucket, 0);
let allocator = unsafe { &*vol.allocator.get() };
```

## Scenario: per-device dynamic bucket geometry

### 1. Scope / Trigger

- 修改 allocation、open bucket、reservation、writepoint、free/trim、extent trigger、
  metadata marking、GC 或 recovery 的 bucket 地址/容量换算时适用。
- 唯一依据是本地 `fs/sb/members_types.h:5-29`、`fs/sb/members.h:416-439`、
  `fs/alloc/buckets.h:15-40,113-133`、`fs/alloc/foreground.c:303-317`、
  `fs/alloc/foreground.h:389-429` 与 `fs/init/dev.c:594-595`。

### 2. Signatures

```rust
sector_to_bucket(ca: &BchDev, s: u64) -> u64
bucket_to_sector(ca: &BchDev, b: usize) -> u64
bucket_remainder(ca: &BchDev, s: u64) -> u64
sector_to_bucket_and_offset(ca: &BchDev, s: u64, offset: &mut u32) -> u64
```

`BchDev.mi` 是 `BchSbMember` 经本地 `bch2_mi_to_cpu()` 语义转换后的
per-device runtime geometry。生产 bucket 换算不得回退到
`BLOCKS_PER_BUCKET`/`DEFAULT_BUCKET_SIZE`。

### 3. Contracts

- `BchSbMember.bucket_size`、`BchDev.mi.bucket_size`、open-bucket capacity 和
  reservation/writepoint 计数单位都是 512-byte sectors。
- Volsnap allocator 返回值和 `ExtentPtr.offset` 保持 block 单位；进入本地 helper
  边界时显式乘 `SECTORS_PER_BLOCK`，反向地址显式除 `SECTORS_PER_BLOCK`。
- alloc-btree 与 freespace-btree 分配、open identity/capacity、allocated/free counter、
  reservation commit、L1/partial/linear reuse、put/free/trim/nocow、allocator scans、
  extent trigger、metadata marking、GC 与 recovery 必须解析同一个 `BchDev.mi`。
- allocation groups 只能覆盖完整、不重叠的 device buckets；group start/count 在
  bucket 单位分区后再换算为 block。
- L1 writepoint reuse 必须在原子 `fetch_sub` 前过滤目标 `dev`，失败回滚与后续
  fallback 顺序保持不变。
- `nr_btree_reserve` 按本地 `DIV_ROUND_UP(BTREE_NODE_RESERVE,
  ca->mi.bucket_size / btree_sectors(c))` 的设备容量关系计算。

### 4. Validation & Error Matrix

| 条件 | 结果 |
|---|---|
| `bucket_size == 0` | resize 在发布 runtime 前返回 invalid argument |
| `bucket_size % SECTORS_PER_BLOCK != 0` | resize 在发布 runtime 前返回 invalid argument |
| `nbuckets < first_bucket` | resize 在发布 runtime 前返回 invalid argument |
| extent/GC pointer device 缺失 | 保持该调用点原有 error 或 skip 分支，不回退到主设备 |
| writepoint 中只有其他设备 bucket | 跳过且不消费其 `sectors_free`，继续原 fallback |

### 5. Good/Base/Bad Cases

- Good：512 KiB 与 2 MiB 设备上的同号 bucket 分别按 128/512 block stride
  返回地址，并以 `(dev, bucket)` 建立不同 open entries。
- Base：默认 1 MiB geometry 仍由相同 helper 链处理。
- Bad：`ptr.offset / BLOCKS_PER_BUCKET`、`bucket * BLOCKS_PER_BUCKET` 或用默认
  bucket sectors 初始化任意设备的 open bucket。

### 6. Tests Required

- 1024/4096-sector members 覆盖 helper、open capacity、连续复用 offset、reservation、
  allocated/free、free/trim、extent trigger 与 alloc-v4 bucket identity。
- 两设备不同 bucket size、同 bucket number 覆盖 open identity 和物理 block stride。
- GC pointer mapping 与 journal allocation 至少各有一个 non-default geometry 回归。
- zero/non-block-aligned geometry 必须失败；每条测试及全量 lib suite 必须在 60 秒内完成。

### 7. Wrong vs Correct

```rust
// Wrong: global default geometry leaks into a device-relative address.
let bucket = ptr.offset / BLOCKS_PER_BUCKET;

// Correct: the persisted pointer is blocks; the local helper consumes sectors.
let ca = vol.device_rcu_noerror(ptr.dev).ok_or(...)?;
let bucket = sector_to_bucket(&ca, ptr.offset * SECTORS_PER_BLOCK);
```

### 行号漂移 / 函数名不存在（已修复）

| 文件 | 原注释 | 修正后 |
|------|--------|--------|
| `reservation.rs:3` | `buckets.h:341-401`（错文件） | `buckets_types.h:98-102` |
| `reservation.rs:241` | `buckets.c:594-596`（函数体内部行） | `buckets.c:562`（声明行） |
| `foreground.rs:7` | `alloc_prio_hint`（不存在） | subvolmount 特有 |
| `foreground.rs:134` | `bch2_alloc_key_v2`（不存在） | subvolmount 特有序列化辅助 |
| `mod.rs:595` | `bch2_bucket_alloc_new_fs()`（不存在） | `__dev_alloc_bucket()` |
| `mod.rs:697` | `bch2_bucket_free()`（不存在） | 功能分布于 alloc→free 状态转换 |

### bch_alloc_v4 固定布局与边界（已精确对齐）

- 持久化值使用 `BchAllocV4`：`repr(C, align(8))`、固定基段 64 字节，字段
  offset 逐项锁定为本地 `fs/alloc/format.h:82-107` 的
  `journal_seq_nonempty(0)`、`flags(8)`、`gen(12)`、`oldest_gen(13)`、
  `data_type(14)`、`stripe_redundancy_obsolete(15)`、`dirty_sectors(16)`、
  `cached_sectors(20)`、`io_time[2](24)`、`stripe_refcount(40)`、
  `nr_external_backpointers(44)`、`journal_seq_empty(48)`、
  `stripe_sectors(56)`、`pad(60)`。
- `stripe_refcount`、`stripe_sectors` 均为 `u32`；`group` 不进入持久化值，
  只保留在运行时 `Bucket`/allocator group 上下文中。
- 读取只接受 alloc v4 raw value：48、56、64 字节以及 flags 指示的
  40 字节 inline backpointer 尾部；固定字段先零填充，再严格按本地
  `bch2_alloc_v4_validate()` 顺序校验。
- owned mutable 转换按本地 `__bch2_alloc_to_v4_mut()` v4 分支执行：
  backpointer start 迁到 8，必要时清零 gap，随后清空 inline count。
- 新写值固定为 64 字节、`BACKPOINTERS_START=8`、`NR_BACKPOINTERS=0`；
  编码不可失败，不再保留 bincode 编码、探测或回退，也不接受 alloc v1/v2/v3。
- `gen` 在运行时 allocator bucket 与 `bucket_gens` 状态中管理，持久化 v4
  仍保存原始 `u8 gen`。
- `journal_seq` 记录最后引用 seq，`journal_seq_empty` 记录空转移 seq；删除路径先写 `NeedDiscard` 并记录当前 seq，trim 到 `Free` 时清空两者；`may_alloc_bucket_journal_seq()` 只看 `journal_seq_empty`
- `may_alloc_bucket_journal_seq()` 的判定门槛必须取 journal 的已落盘序列（`last_seq_ondisk` / flushed seq），而不是分配请求里的序列；`journal_seq_empty` 只有在 <= flushed seq 时才允许复用
- freespace key 的 genbits 必须由 `version - oldest_gen` 计算，不能把 `oldest_gen` 固定成 0；分配、free/trim、恢复扫描都必须保留该字段
- `flags` 位域使用本地范围：NEED_DISCARD `0..1`、NEED_INC_GEN `1..2`、
  BACKPOINTERS_START `2..8`、NR_BACKPOINTERS `8..14`。

## Function Coverage Map

### filesystem capacity usage

| subvol 函数/类型 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|---|---|---|---|---|
| `BchFsUsageBase` / `BchFsCapacityPcpu` / `BchFsCapacity` | 同名结构 | `buckets_types.h:78-84`、`alloc/types.h:165-188` | ✅ | 字段语义及 sector 单位对齐；当前 runtime 固定单 slot。 |
| `BchFsUsageShort` | `struct bch_fs_usage_short` | `buckets_types.h:86-90` | ✅ | `capacity/used/free` 三字段。 |
| `reserve_factor()` | 同名函数 | `buckets.c:65-68` | ✅ | 1/64 round-up reserve。 |
| `avail_factor()` | 同名 inline | `buckets.h:413-418` | ✅ | 左移后除 65，保留截断。 |
| `__bch2_fs_usage_read_short()` | 同名函数 | `buckets.c:70-91` | ✅ | 完整 slot 单次累加，忽略 available cache。 |
| `bch2_fs_usage_read_short()` | 同名函数 | `buckets.c:93-98` | ✅ | `mark_lock` 读保护后调用内部 helper。 |
| `bch2_fs_capacity_init()` | 同名函数 | `background.c:1747-1757` | ✅ | 构造早期建立单一 zeroed slot。 |
| `bch2_fs_capacity_exit()` | 同名函数 | `background.c:1736-1745` | ✅ | 检查 online reservation 后清空 slot。 |

### mod.rs — BchAllocator 主结构 + 分配入口（~2400 行）

| 行号 | subvol 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| 62 | `struct AllocRequest` | `alloc_request` | `foreground.h:161-178` | ✅ | 精简版（无 `devs_requested` / `devs_have`） |
| 128-133 | 常量（DEFAULT_BUCKET_SIZE 等） | 同值常量 | `format.h:11-29` | ✅ | 对齐 |
| 140 | `struct AllocGroup` | `bch_dev` per-device bucket runtime | `bcachefs.h:479-575` | ➖ | Rust group 组织形式；唯一所有者是 `BchDev` |
| 192 | `struct BchAllocator` | `bch_fs_allocator` | `alloc/types.h:192-219` | ✅ | 只保留 filesystem-global open buckets/write points/reservations |
| 218 | `bch2_dev_buckets_alloc/resize()` | 同名函数 | `alloc/buckets.c:1277-1334` | ✅ | 按 member 几何初始化每个 `BchDev` runtime |
| 596 | `fn bch2_bucket_alloc_new_fs()` | `__dev_alloc_bucket()` | `foreground.c` 内部分配路径 | ➖ | bcachefs 中无独立函数名，subvolmount 封装为便利函数 |
| 664 | `fn open_bucket_free_unused()` | `open_bucket_free_unused()` | `foreground.h:184-216` | ✅ | 已对齐 |
| 672 | `fn bch2_open_bucket_put()` | `bch2_open_bucket_put()` | `foreground.h:252-264` | ✅ | 已对齐 |
| 698 | `fn bch2_bucket_free()` | 无独立函数 | — | ➖ | subvolmount 特有；功能分布于 `bch2_trans_mark_alloc()` 的 alloc→free 路径 |
| 768 | `fn bch2_bucket_do_trim()` | `bch2_discard_one_bucket()` | `discard.c:289` | ✅ | 函数名不同但语义等价，subvol 使用更清晰的命名 |
| 846 | `fn bch2_alloc_sectors_start_trans()` | `bch2_alloc_sectors_start_trans()` + `bch2_alloc_sectors_append_ptrs_inlined()` | `foreground.h:365-429` | ✅ | `sectors_needed = count * SECTORS_PER_BLOCK`；allocator 只选择/推进 open bucket，disk reservation 由 transaction usage accounting 消费。L1/L2 通过目标设备 helper 换算为 block。 |
| 938 | `fn bch2_alloc_buckets()` | — | — | ➖ | subvolmount 特有批量分配便利函数 |
| 953-968 | `total/allocated/free_blocks()` | — | — | ➖ | subvolmount 特有统计 |
| 975-992 | `for_each_bucket_mut/fw()` | — | — | ➖ | subvolmount 特有便利遍历 |
| 1009-1033 | `btree_bitmap_mark/clear/test()` | `bch2_btree_bitmap_mark/clear/test` | `buckets.c` | ✅ | 已对齐 |
| 1046-1071 | `bucket_nocow_is_locked/trylock/unlock()` | `bucket_nocow_is_locked/trylock/unlock` | `buckets.c` | ✅ | 已对齐 |
| 1096 | `fn bch2_alloc_read()` | `bch2_alloc_read()` | `background.c:xxx` | ✅ | 已对齐 |
| 1181 | `fn bch2_trigger_extent()` | Alloc extent trigger | `buckets.c` / alloc trigger 路径 | ✅ | 已对齐 |
| 1301 | `fn bch2_freespace_insert()` | Freespace 插入 | — | ✅ | 已对齐 |
| 1316 | `fn bch2_freespace_delete()` | Freespace 删除 | — | ✅ | 已对齐 |
| 1343 | `fn bch2_trigger_alloc()` | alloc→freespace 同步 | `background.c:1232-1364` | ✅ | transactional trigger 追加 Freespace 更新 |
| 1404 | `fn bch2_rebuild_freespace()` | `bch2_recalc_freespace()` | check.c | ✅ | 已对齐 |
| 178 | `fn may_alloc_bucket_journal_seq()` | `may_alloc_bucket_journal_seq()` | — | ✅ | 已对齐（检查 `journal_seq_empty`） |
| 1649 | `__bch2_trans_mark_metadata_bucket()` | 同名函数 | `buckets.c:961-1001` | ✅ | alloc v4 preserve、冲突与幂等更新顺序对齐 |
| 1695 | `bch2_mark_metadata_bucket()` | 同名函数 | `buckets.c:1003-1038` | ✅ | GC lock 内 old/check/add/new，锁外 accounting |
| 1768 | `bch2_trans_mark_metadata_bucket()` | 同名函数 | `buckets.c:1040-1062` | ✅ | 越界 skip 与 trigger 分支顺序对齐 |
| 1800 | `bch2_trans_mark_metadata_sectors()` | 同名函数 | `buckets.c:1064-1087` | ✅ | do/while 分段、切换 flush、尾部 flush 对齐 |
| 1839 | `__bch2_trans_mark_dev_sb()` | 同名函数 | `buckets.c:1089-1124` | ✅ | layout 后 journal 顺序对齐 |
| 1899 | `bch2_trans_mark_dev_sb()` | 同名函数 | `buckets.c:1126-1133` | ✅ | 单设备入口 |
| 2103 | `bch2_trans_mark_dev_sbs_flags()` | 同名函数 | `buckets.c:1135-1154` | ✅ | initialize 后无条件重标、online IO-ref 对齐 |
| 2127 | `bch2_trans_mark_dev_sbs()` | 同名函数 | `buckets.c:1156-1159` | ✅ | transactional flags wrapper |
| 2132 | `bch2_is_superblock_bucket()` | 同名函数 | `buckets.c:1161-1181` | ✅ | bucket 0/layout/journal/normal 顺序对齐 |

### device initialization/accounting

| subvol 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|---|---|---|---|---|
| `bch2_dev_usage_init()` | 同名函数 | `accounting.c:1257-1289` | ✅ | set-to-target，重复恢复幂等 |
| `bch2_dev_set_initialized()` | 同名函数 | `init/dev.c:1131-1138` | ✅ | device superblock 写成功后推进 runtime stage |
| `journal::bch2_dev_journal_alloc()` | 同名函数 | `journal/init.c:263-302` | ⚠️ | 实现移至 `journal/init.rs`；Rust 显式传 `c`，其余 gate/target/clamp/loop 顺序对齐 |
| `bch2_dev_add_initialize()` | 同名函数 | `init/dev.c:1140-1161` | ✅ | 五阶段 fallthrough 顺序 |

### bucket.rs — Bucket 状态管理（~323 行）

| 行号 | subvol 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| 15 | `enum BchDataType`（11 变体） | `enum bch_data_type`（11 变体） | `accounting_format.h:55-75` | ✅ | 有效值与本地 bcachefs 严格一致：Free(0) 到 Unstriped(10)，`BCH_DATA_NR=11` 仅作为数量边界。 |
| 45 | `struct Bucket` | 运行时 bucket/allocator 状态 | `buckets_types.h:37-45` | ➖ | subvol 运行时精简层；`group/nocow_locked/journal seq` 不作为 `bch_alloc_v4` 固定布局，持久化由 `BchAllocV4` 独立承担。 |
| 83 | `fn derive_data_type()` | `alloc_data_type()` | `background.h` | ✅ | 语义等价 |
| 92 | `fn alloc_state()` | `alloc_data_type()` | `background.h` | ✅ | wrapper |
| 99 | `fn data_type()` | `alloc_data_type()` | `background.h` | ➖ | subvolmount 特有封装 |
| 105 | `fn alloc_lru_idx_read()` | alloc read-LRU accessor | `background.h` / `lru.c` | ✅ | 已对齐 |
| 114 | `fn alloc_lru_idx_fragmentation()` | alloc fragmentation-LRU | `background.h` / `lru.c` | ✅ | 已对齐 |
| 121 | `fn alloc_nr_external_backpointers()` | backpointer count | `background.h` / `backpointers.c` | ✅ | 已对齐 |

### btree.rs — Alloc btree v4 原始值边界（~415 行）

| 行号 | subvol 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| 14 | `struct BchAllocV4` | `struct bch_alloc_v4` | `format.h:82-107` | ✅ | 64 字节、8 字节对齐，字段宽度与 offset 逐项测试锁定；磁盘 data_type 保留原始 u8。 |
| 240 | `fn serialize_alloc_entry()` | current alloc_v4 write boundary | `format.h:82-107`、`background.c:769-782` | ✅ | 不可失败地写 64 字节 current v4，start=8、nr=0；无 bincode 路径。 |
| 248 | `fn deserialize_alloc_entry()` | `bch2_alloc_v4_validate()` + `__bch2_alloc_to_v4_mut()` v4 branch | `background.c:698-767,868-895` | ✅ | 接受本地 v4 短值/inline tail；先校验，再规范化 owned mutable value。 |

### accounting.rs — Alloc device accounting

| 行号 | subvol 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| 127 | `fn bch2_bucket_sectors*()` | `bch2_bucket_sectors*()` | `background.h:79-111` | ✅ | 使用有符号 sector，保留 Cached、fragmented 与 Unstriped 分支。 |
| 198 | `fn bch2_dev_data_type_accounting_mod()` | `bch2_dev_data_type_accounting_mod()` | `background.c:1168-1180` | ✅ | 三个有符号 delta；仅传递 trigger GC 位。 |
| 225 | `fn bch2_alloc_key_to_dev_counters()` | `bch2_alloc_key_to_dev_counters()` | `background.c:1182-1213` | ✅ | new type 增加、old type 减少、same-type delta、Unstriped delta 的顺序一致。 |

### bucket_gens.rs — Bucket 代索引（~30 行）

| 行号 | subvol 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| 6 | `struct BchBucketGens` | `bch_bucket_gens` | `format.h` | ✅ | 对齐（u8[256] 代索引） |
| 15 | `fn set_gen()` | bucket_gens 写路径 | — | ✅ | 对齐 |
| 25 | `fn get_gen()` | bucket_gens 读路径 | — | ✅ | 对齐 |

### foreground.rs — 前台分配策略（~250 行）

| 行号 | subvol 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| 13 | `enum PrioHint` | — | — | ➖ | subvolmount 特有（bcachefs 无 `alloc_prio_hint` 符号） |
| 48 | `fn prio_hint_for_watermark()` | — | — | ➖ | subvolmount 特有 |
| 85 | `struct AllocTarget` | — | — | ➖ | subvolmount 特有 |
| 130 | `fn alloc_key_v2()` | — | — | ➖ | subvolmount 特有序列化辅助 |
| — | `fn write_watermark()` | — | — | ➖ | 函数不存在于 subvolmount 代码中（已移除/从未实现）|

### open_bucket.rs — 开放桶引用计数（~605 行）

| 行号 | subvol 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| 32 | `struct BchOpenBuckets` | open_bucket 管理 | `types.h:65-91,192-205` | ✅ | filesystem-global pool；identity 为 `(dev, bucket)`，count=4096 |
| 98 | `fn new()` | `open_bucket_init()` | — | ✅ | 对齐 |
| 145 | `fn get()` | `open_bucket_get()` | `foreground.h:184` | ✅ | 已对齐 |
| 175 | `fn put()` | `open_bucket_put()` | `foreground.h:252` | ✅ | 已对齐 |
| 210 | `fn try_get()` | — | — | ➖ | subvolmount 特有非阻塞版 |
| 240 | `fn free_unused()` | `open_bucket_free_unused()` | `foreground.h:184-216` | ✅ | 已对齐 |
| 275 | `fn flush()` | 写路径 flush | — | ✅ | 对齐，语义等价 |
| 310 | `fn nr_open()` | — | — | ➖ | subvolmount 特有统计 |
| 340 | `fn nr_available()` | — | — | ➖ | subvolmount 特有统计 |
| 375 | `fn is_full()` | — | — | ➖ | subvolmount 特有 |
| 410 | `fn clear()` | — | — | ➖ | subvolmount 特有 |
| 440 | `fn iter_open()` | — | — | ➖ | subvolmount 特有 |

### reservation.rs — 扇区预留系统（~1059 行）

| 行号 | subvol 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| 40 | `const SECTORS_CACHE` | `SECTORS_CACHE=1024` | `buckets.c:1188` | ✅ | 全局 refill 批量领取上限 |
| 94 | `struct BchReservationFlags` | `enum bch_reservation_flags` | `buckets.h:350-353` | ✅ | 显式 u8 bitmask，NOFAIL=1<<0、PARTIAL=1<<1，支持组合标志并保留 bit semantics |
| 114 | `struct DiskReservation` | `struct disk_reservation` | `buckets_types.h:98-102` | ✅ | 对齐（sectors: Cell\<u64\> + gen: u32 + nr_replicas: u32） |
| 337 | `fn flags_has()` | — | — | ➖ | 辅助函数：检查 BchReservationFlags 是否包含指定位 |
| 348 | `fn disk_reservation_recalc_sectors_available()` | `disk_reservation_recalc_sectors_available()` | `buckets.c:1190-1213` | ✅ | 加锁→清零pcpu→avail_factor(usage.free)→PARTIAL/NOFAIL/ENOSPC 三分支 |
| 407 | `fn __bch2_disk_reservation_add()` | `__bch2_disk_reservation_add()` | `buckets.c:1215-1240` | ✅ | 三级缓存协议：local shortage→global cmpxchg refill→full recalc；reservation 使用可变指针语义 |
| 468 | `fn bch2_disk_reservation_add()` | `bch2_disk_reservation_add()`（userspace） | `buckets.h:358-378 #else` | ✅ | 委托给 `__bch2_disk_reservation_add` |
| 480 | `fn bch2_disk_reservation_put()` | `bch2_disk_reservation_put()` | `buckets.h:341-348` | ✅ | 通过 `Cell::replace` 读 sectors 后清零，递减 pcpu online_reserved |
| 492 | `fn bch2_disk_reservation_init()` | `bch2_disk_reservation_init()` | `buckets.h:380-391` | ✅ | 返回 sectors=0 空预留；`_c: &BchVol` 保留供 future capacity_gen |
| 503 | `fn bch2_disk_reservation_get()` | `bch2_disk_reservation_get()` | `buckets.h:393-401` | ✅ | 接收 reservation 可变指针，先 init 再 add(sectors * nr_replicas)，返回错误码而非新建对象 |

### write_point.rs — 写点池（~894 行）

| 行号 | subvol 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| 29 | `WRITE_POINT_MAX = 32` | `WRITE_POINT_MAX = 32` | `types.h:58` | ✅ | 常量对齐 |
| 44 | `enum WritePointSpecifier` | `struct write_point_specifier` | `types.h:161-163` | ✅ | enum 替代 bit-0 编码 |
| 86 | `struct WritePoint` | `struct write_point` | `types.h:130-159` | ✅ | 精简版 |
| 123 | `fn reassign()` | `writepoint_find()` reuse 路径 | `foreground.c:1340-1342` | ✅ | 已对齐 |
| 134 | `fn done()` | `bch2_alloc_sectors_done_inlined()` | `foreground.h:230-250` | ✅ | 已对齐 |
| 184 | `struct WritePointPool` | `bch_fs_allocator` write_points 相关 | `types.h:208-215` | ✅ | 对齐 |
| 210 | `fn new()` | `bch2_fs_allocator_foreground_init()` | `foreground.c:1680-1714` | ➖ | Rust 封装差异，HashMap 替代 hlist |
| 241 | `fn resolve()` | `writepoint_find()` | `foreground.c:1291-1347` | ✅ | 已对齐 |
| 253 | `fn resolve_hint()` | `writepoint_find()` + hint | — | ✅ | 组合操作 |
| 292 | `fn resolve_direct()` | `writepoint_find()` direct 路径 | `foreground.c:1299-1303` | ✅ | 已对齐 |
| 300 | `fn find_lru()` | writepoint_find oldest 扫描 | `foreground.c:1317-1321` | ✅ | 线性扫描替代，逻辑一致 |
| 318 | `fn nr_active()` | `a->write_points_nr` | — | ✅ | 对齐，语义等价 |
| 333 | `fn too_many_writepoints()` | `too_many_writepoints(factor)` | `foreground.c:1241-1247` | ✅ | 已对齐。统计口径只包含活跃池写点，专用写点不计入 stranded space。 |
| 346 | `fn try_reuse_current_wp()` | `bch2_ob_ptr()` + `bch2_alloc_sectors_append_ptrs_inlined()` | `foreground.h:390-429` | ✅ | 在 `fetch_sub` 前过滤目标 device；old-value-check/rollback 顺序不变。容量取目标 `ca.mi.bucket_size`，返回 block offset 时除以 `SECTORS_PER_BLOCK`。 |
| 377 | `fn try_decrease()` | `try_decrease_writepoints()` | `foreground.c:1263-1289` | ✅ | 精简版 |

## 注释行号验证状态

| 文件 | 引用数 | 已验证数 | 已修正 |
|------|--------|---------|--------|
| foreground.rs | 0 | 0 | 0（无直接 bcachefs 行号引用）|
| open_bucket.rs | 4 | 4 ✅ | 0 |
| reservation.rs | 3 | 3 ✅ | 2 处修正 |
| write_point.rs | 3 | 3 ✅ | 0 |
| **合计** | **10** | **10** | **2** |

## 结构差异总结

| 维度 | bcachefs-tools | subvolmount |
|------|----------------|----------|
| 并发 | spinlock + per-CPU + hlist | Mutex + HashMap（封装度更高） |
| 写点分配 | `hlist` + `write_point` 指针 | HashMap `WritePointPool` |
| allocator ownership | `bch_fs_allocator` 全局一份，bucket runtime 在 `bch_dev` | `BchAllocator` 全局一份，groups/gens/GC state 在 `BchDev` |
| gen 管理 | `bch_alloc_v4.gen` + `bch_dev.bucket_gens` | 每个 `BchDev` 的 groups/gens，持久化仍为 alloc v4 `u8 gen` |
| Bucket 状态 | `bch_data_type` 11 个有效值 | `BchDataType` 11 个有效值（0-10） |
| 字段 | `bch_alloc_v4` 完整字段与固定宽度 | `BchAllocV4` 固定 64 字节完整对齐；运行时 `Bucket` 与持久化布局分离 |
| 写策略 | 每次 gen 变更更新 btree | 懒惰：积攒 64 脏桶批量写入 |

## Notes

- The alloc-info helper treats free buckets as requiring an exact matching
  freespace generation and treats any freespace entry for an allocated bucket as
  stale.
- `bucket_gens` keys are chunked in groups of 256 buckets. The value payload
  stores one `u8` generation per bucket slot, matching bcachefs
  `KEY_TYPE_BUCKET_GENS_BITS = 8`.
- `freespace` keys now encode the generation in the high 8 bits of `offset`
  and keep `snapshot = 0`, matching bcachefs `alloc_freespace_pos()` instead
  of using the `snapshot` field as a stand-in for generation.
- `alloc_gc_gen()` uses `version - oldest_gen`, so freespace generation bits
  are derived from the current bucket generation window rather than raw
  `version` alone.
- `BchAllocEntry` is an internal Rust alias for the exact `BchAllocV4` value;
  it derives `PartialEq/Eq` so normalized alloc snapshots can be compared.
- `deserialize_alloc_entry()` accepts only local alloc v4 raw values. It does
  not contain historical subvol bincode or alloc v1/v2/v3 compatibility.
- Alloc delete/trim lifecycle matches bcachefs: `bch2_bucket_free()` transitions
  buckets into `NeedDiscard`, `bch2_bucket_do_trim()` finalizes them as `Free`,
  and both journal seq fields are cleared only at trim time.
- `Bucket::mark_free()` now only clears state + journal bookkeeping; gen window
  advancement stays in the dedicated alloc trigger path, matching bcachefs
  `NEED_INC_GEN` handling.
- `bch2_bucket_do_trim()` must clear both `journal_seq` and
  `journal_seq_empty` while preserving stable accounting fields like
  `io_time[2]` and `nr_external_backpointers`.
- Free / trim paths preserve stable accounting fields such as `io_time[2]`
  and `nr_external_backpointers`; only the journal bookkeeping is reset when
  entering `Free`.
- Free may skip recording `journal_seq_empty` when the journal has already
  advanced past the current seq boundary; that keeps already-flushed frees from
  carrying stale discard bookkeeping.
- Invalidate work is intentionally allocator-state neutral until backpointer /
  LRU cleanup is modeled; it should not coerce `Cached` buckets into `Free`.
- Extent triggers must be idempotent for identical old/new payloads: if the
  serialized extent value is unchanged, the alloc side must not rewrite the
  bucket entry again.
- Trim/free paths must decode the complete extent pointer set and release each
  `(device, offset)` allocation; freeing only the primary device leaks degraded
  replicas and violates allocator accounting.
- `bch2_fpunch_snapshot()` (`fs/data/io_misc.c:146-166`) batches the extent
  delete transaction, while `bch2_extent_trim_atomic()` (`fs/data/extent_update.c:117`)
  supplies the trim update. The Rust trim wrapper must publish metadata first
  and release every decoded pointer afterward.
- Partial trim must cut the extent into left/right metadata ranges in the same
  transaction; only a fully covered extent may release its physical pointers.
  This follows `bch2_extent_trim_atomic()`'s `bch2_cut_back()` boundary logic
  and prevents data outside the discard range from being lost.
- `WritePointPool::stranded_space()` mirrors bcachefs `too_many_writepoints()` input: only active pooled write points count; dedicated btree/journal/GC write points are excluded from the stranded-space budget.
- `KeyValue::ExtentPtrs` 采用 struct 变体 `{ blocks: u32, ptrs: Vec<ExtentPtr> }`，`blocks` 字段内联携带 extent block 数，无需上游链追溯。序列化格式为 `blocks(4 LE) + count(4 LE) + entries(8B each)`。
- Extent atomic triggers must decode both legacy bincode `KeyValue` values and the compact `KeyValue::to_bytes()` representation; production writes use the latter, while older callers may still provide the former.
- GC 的 sector 计数策略：`bch2_gc_mark_key` 中 `extent_blocks()` × `for_each_ptr` 累加到 `gc_buckets.dirty_sectors`/`cached_sectors`；sweep 时必须处理 ALL buckets（含 gc.gen_valid=true 的被引用桶），不可跳过，否则 sector 计数丢失。
- Device accounting 的修改量必须使用 `[i64; 3]`，不能用 `u64` wrapping
  编码负数；alloc old/new 差量顺序固定为 new type、old type、Unstriped。
- `BchSbMember.bucket_size` 是 512-byte sector 数，`nbuckets` 是设备 bucket 数；
  fragmented accounting 必须按对应设备 member 的 bucket size 计算。
- `DevStripeState::sync()` 对新加入候选集的设备必须复制现有成员的最小
  `next_alloc` 虚拟时钟；设备移除才清零时钟。该顺序对应本地
  `dev_stripe_state_sync()`（`fs/alloc/foreground.c:740-765`），避免新成员长期获得
  非公平的分配优先级。
- `DevStripeState::rescale()` 必须从所有时钟计算共同 `scale=max(scale_min,scale_max)`
  后统一相减，对应本地 `bch2_stripe_state_rescale()`（`fs/alloc/foreground.c:790-807`）；
  逐项右移会改变设备间虚拟时钟距离和分配权重。
- journal stripe 增量的 free-space 输入必须使用 `dev_buckets_free(..., Normal)`，
  包含 watermark 预留和 open bucket 扣除，对应本地 `__dev_buckets_free()`
  （`fs/alloc/buckets.h:258-271`）与 `bch2_dev_stripe_increment()`
  （`fs/alloc/foreground.c:819-825`），不能用 raw blocks minus allocated 替代。
- `BchAllocator` production path 不再持有固定 5% margin，旧版 `ReservationTracker` 已删除；
  `AllocRequest` 的 reservation 必须由 `BchVol` capacity API 获取。分配成功后的
  reservation 扇区与 `online_reserved` 扣减保持本地
  `bch2_trans_account_disk_usage_change()` 的 `added > 0` 分支顺序；剩余预留仍由
  owner 在终止路径调用 `bch2_disk_reservation_put()` 释放；所有生产调用方均使用
  capacity-backed reservation primitives。
- `BchDev::nr_free_buckets` 是各 `AllocGroup::free_buckets` 的设备级原子聚合缓存；
  每个分配、trim、alloc-trigger 和 resize/rebuild 路径必须同步更新它。`dev_buckets_free()`
  读取聚合 free 计数，仅为各组精确计算 watermark reserve，避免热路径逐组读取
  free counter 的 mutex 竞争，同时保持 local `__dev_buckets_free()` 的结果。
- 事务磁盘用量 delta 现由 `BtreeTrans::fs_usage_delta` 累加，并按本地
  `bch2_trans_account_disk_usage_change()`（`fs/alloc/buckets.c:562-601`）顺序消费
  capacity-backed reservation；extent 插入/删除的 data/cached delta 在 trigger 阶段生成，
  并在 Atomic trigger 成功后发布，重试边界清零，避免重复消费；trigger 直接使用当前 transaction context，
  不通过 volume-only 兼容入口写入 alloc/backpointer 状态。
- capacity reservation 的 `bch2_disk_reservation_add()`/`put()` 必须在
  `BchFsCapacity.mark_lock` 写锁下修改 `pcpu[0]`，与
  `bch2_fs_usage_read_short()` 的读锁边界一致；分配成功后的 reservation consumption
  也必须使用同一写锁，避免 async 并发下 `online_reserved` 数据竞争。
- capacity lifecycle 的 `bch2_fs_capacity_init()`、`bch2_fs_capacity_exit()` 与
  `bch2_recalc_capacity()` 在替换 `pcpu` 或发布 `capacity/reserved/bucket_size_max`
  前必须持有同一写锁；这保持 usage snapshot 与设备上下线重算之间的发布原子性。
- 多副本写入中，任一 replica bio 失败后必须在发布 extent 前释放该副本对应的
  allocation/open bucket；成功副本继续按提交顺序发布。该清理顺序对应本地
  `bch2_write_done()` 的失败 completion 路径，避免暂时性设备故障永久泄漏 bucket。
- 新 extent 的所有副本 IO 成功后，overlap split 或 extent journal 提交若失败，
  仍必须回收全部新 allocation；只有元数据提交成功后才转移 allocation ownership。
- NBD trim 对同一请求发现的 extent 删除必须合并进单个 `BtreeTrans` 提交，提交成功后
  再按原顺序回收 allocation；不能为每个 extent 单独获取 write ref/journal reservation，
  也不能在事务失败前写入 trim-hole 运行时状态。
### 2026-07-17 API 可见性复核

- 本地 `fs/alloc/` 未提供 `bch2_bucket_alloc_new_fs`、`bch2_bucket_free`、`bch2_bucket_do_trim`、`bch2_alloc_buckets`、`bch2_btree_bitmap_test` 或 `bch2_rebuild_freespace` 同名导出。
- 这些接口仍供 subvol crate 内的分配、journal、storage 和测试路径调用，因此仅收敛为 `pub(crate)`，不改变分配/释放/trim 控制流。

## Scenario: quota helper visibility follows the local quota API

### 1. Scope / Trigger

- 修改 `alloc/quota/account.rs`、`alloc/quota/ops.rs` 或 quota 模块重导出时适用。
- 唯一依据是本地 `fs/fs/quota.h:38-48` 与 `fs/fs/quota.c:329-378,440-502,799-915`。

### 2. Signatures

- 本地公开记账入口为 `bch2_quota_acct(struct bch_fs *, struct bch_qid,
  enum quota_counters, s64, enum quota_acct_mode) -> int`。
- 本地 quota get/set 由静态 `bch2_get_quota()`、`bch2_get_next_quota()`、
  `bch2_set_quota_trans()`、`bch2_set_quota()` 和 `__bch2_quota_get/set()` 实现。
- subvol 的 `bch2_quota_account/cur_get/set/get/del/check` 没有同名本地公开 API，
  因而不得从 `alloc::quota` 公开重导出；仍有生产调用的辅助最多为 `pub(crate)`。

### 3. Contracts

- API 收口只改变 Rust 可见性，不改变现有 quota btree 读写、错误返回、journal sequence
  或子卷计数更新顺序。
- `bch2_quota_cur_get()` 与 `bch2_quota_check()` 仍供 allocator crate 内路径调用；
  set/get/del/account 当前仅供同模块单元测试使用。
- 后续实现本地 `bch2_quota_acct()` 时必须照搬本地的分配全部 memquota、按 qtype
  顺序加锁、全部检查、全部更新、按相同顺序解锁并刷新 warning 的控制流；不得把现有
  单 qid helper 误标为该 API 的等价实现。

### 4. Validation & Error Matrix

| 条件 | 结果 |
|---|---|
| crate 外导入六个 subvol helper | 编译期不可见 |
| allocator 内调用 `cur_get/check` | 保持可见且行为不变 |
| quota 单元测试调用 set/get/del/account | `cfg(test)` 私有导入后继续通过 |
| 本地源码出现同名静态 helper | 不得据此扩大 Rust 公共 API |

### 5. Good/Base/Bad Cases

- Good：只公开本地 `quota.h` 实际声明的对齐 API，内部过渡 helper 限制在 crate 内。
- Base：同模块测试可直接验证内部 CRUD，但下游 crate 无法依赖这些临时名称。
- Bad：因为函数名以 `bch2_` 开头就公开重导出，或声称 `bch2_quota_set/get/check`
  是本地同名 API，会固化不存在的接口和错误语义。

### 6. Tests Required

- `cargo fmt --check` 与 `git diff --check` 必须通过。
- `timeout 55s cargo check -p subvol-core` 必须通过。
- `timeout 55s cargo test -p subvol-core alloc::quota -- --nocapture` 必须在一分钟内通过，
  并覆盖 set/get/del/check/account 的既有控制流。

### 7. Wrong vs Correct

```rust
// Wrong: exposes names absent from local quota.h.
pub use ops::{bch2_quota_check, bch2_quota_get, bch2_quota_set};

// Correct: production-only helpers stay within the crate; test CRUD stays private.
pub(crate) use ops::bch2_quota_check;
#[cfg(test)]
use ops::{bch2_quota_del, bch2_quota_get, bch2_quota_set};
```
