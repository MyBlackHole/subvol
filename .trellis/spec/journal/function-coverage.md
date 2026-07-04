# Journal 模块 bcachefs 函数覆盖地图

> 状态：❓ 全部清除 — 160+ 个函数全部已验证
> 更新：2026-07-15（runtime allocation + write completion/submit + multi-device journal read）
> 2026-07-11 补充：journal / btree I/O 入口统一通过 `BchVol.device_registry` 和 `BchSb.primary_dev_idx` 解析主设备，避免继续在 API 层直接传裸 backend。
> 2026-07-11 补充 2：`btree/bucket_io` 也已改为 `BchDev` 入口，节点读写不再直接吃 `&dyn BlockDevice`。
> 2026-07-11 补充 3：移除 `Btree`/`BtreeNode` 的 `set_test_backend()` 方法（接受 `Arc<dyn BlockDevice>` 并包装为 `BchDev::new(backend, 0)` 的兼容入口），btree 生产路径统一通过 `BchVol.device_registry` 和 `Btree.dev_idx()` 解析设备。`BtreeWriter._backend` 死字段已移除。`BtreePtrV2.dev_idx` 不再硬编码为 0，改为从 `Btree.dev_idx()` 获取（当前始终为 primary_dev_idx，为多设备预留）。
> 对应 bcachefs 路径：`bcachefs 源码树/fs/journal/`

---

## 统计总览

| 状态 | 数量 | 占比 |
|------|------|------|
| ✅ 已验证对齐 | 128 | 75.3% |
| ⚠️ 已知偏差 | 2 | 1.2% |
| ❓ 未验证 | 0 | 0% |
| ➖ 无 bcachefs 对应 | 40 | 23.5% |
| **合计** | **170** | **100%** |

## Scenario: per-device journal bucket allocation

### 1. Scope / Trigger

- 新设备初始化或 recovery 遇到 `journal.nr == 0` 时适用。
- 唯一依据是本地 `fs/journal/init.c:19-180,263-320` 与
  `fs/journal/sb.c:176-216`。

### 2. Signatures

- private `bch2_set_nr_journal_buckets_iter(c, ca, nr, new_fs, watermark)`
- private `bch2_set_nr_journal_buckets_loop(c, ca, nr, new_fs)`
- public `bch2_dev_journal_alloc(c, ca, new_fs)`
- public `bch2_fs_journal_alloc(c)`

`BchDev` 没有可安全保存的 movable `BchVol` 裸反向指针，因此 Rust 的 iter/device
入口显式接收 `c: &BchVol`，并省略 C closure 参数；这是 API 表示偏差，分配控制流不变。

### 3. Contracts

- `new_fs` 使用 Btree watermark，否则使用 Normal watermark 并按缺少的整桶 sector
  数获取 reservation；不实现缩容。
- 每次 allocation 后立即按整桶 sector 数 transactionally 标记 Journal。零进展返回
  原错误；已有进展时吞掉后续 allocation error，先持久化部分结果，outer loop 再重试。
- candidate 在 `discard_idx ?: nr` 插入 bucket number 与零 seq；先写 per-device
  superblock 的 block address，再交换 runtime 数组，并按本地顺序旋转四个 index。
- persistence failure 将所有新 bucket 标 Free，再释放全部 open bucket；mark failure
  释放当前 open bucket，并补偿 Rust allocator 的即时提交，使其等价于 C transaction abort。
- recovery pass 成功后必须把 primary `BchDev.disk_sb` 复制回
  `RecoveryState.superblock`；否则随后的 progress write 会用旧副本擦除刚写入的 buckets。
- `Journal::new([])` 在打开首个空 entry 后将 ondisk 水位设为当前 seq，表示没有可刷写
  bucket 的 journal 已 quiesced；否则 allocation 中的 `bch2_journal_block()` 会永久等待。

### 4. Validation & Error Matrix

| 条件 | 结果 |
|---|---|
| device 不允许 Journal data | 成功跳过 |
| small-image feature | 在目标计算前返回 filesystem-full 对应错误 |
| 第一次 allocation/mark 失败 | 返回原错误，runtime/superblock 不变 |
| 部分 allocation 后耗尽 | 提交已得到 buckets，iter 返回成功 |
| superblock write 失败 | 新 metadata 全部回滚为 Free，open refs 全部释放 |
| device 已有 journal | filesystem-level pass 跳过 |

### 5. Good/Base/Bad Cases

- Good：多 online device 分别得到目标数量，READ refs 最终都为 0。
- Base：`discard_idx == 0` 追加到末尾；nonzero 时在 discard 前插入并旋转 indices。
- Bad：只更新 `BchDev.disk_sb` 而不更新 recovery 的持久化副本；下一 pass 会把 bucket
  列表写回为空。

### 6. Tests Required

- 覆盖 end/discard 插入、四 index 旋转、partial/zero progress、persistence rollback。
- 覆盖 existing/offline/multi-device iteration、READ ref 与每桶 reservation 归零。
- recovery 集成测试必须在 progress persistence 后重新读取 superblock，断言 8 个 bucket
  与对应零 seq 仍存在；所有 test command 必须在 60 秒内完成。

### 7. Wrong vs Correct

```rust
// Wrong: runtime 先可见，write 失败时外部已经观察到新 buckets。
ca.journal.lock().unwrap().buckets.extend(new_buckets);
disk_sb.write_to_device(ca).await?;

// Correct: candidate 先落盘，成功后才交换 runtime arrays。
disk_sb.write_to_device(ca).await?;
ca.journal.lock().unwrap().buckets = candidate_buckets;
```

### init.rs allocation coverage

| subvol 函数 | 本地 bcachefs 对应 | 状态 | 说明 |
|---|---|---|---|
| `bch2_set_nr_journal_buckets_iter` | `init.c:19-142` | ⚠️ | `c` 显式传入并省略 closure；分支、提交、回滚顺序对齐 |
| `bch2_set_nr_journal_buckets_loop` | `init.c:144-180` | ✅ | watermark、no-shrink、每桶 reservation 与 retry 对齐 |
| `bch2_dev_journal_alloc` | `init.c:263-302` | ⚠️ | `c` 显式传入；gate、target、clamp 与错误顺序对齐 |
| `bch2_fs_journal_alloc` | `init.c:305-320` | ✅ | online READ iteration、skip、first-error release 对齐 |

## Scenario: runtime journal write allocation

### 1. Scope / Trigger

- journal entry 完成 prep、尚未 checksum/submit 时触发。
- 唯一依据：本地 `journal/write.c:29-159`、`alloc/disk_groups.h:61-71`、`alloc/background.c:1667-1684`。

### 2. Signatures

- `journal_advance_devs_to_next_bucket(&self, devs, sectors, seq)`
- `__journal_write_alloc(&self, w, devs, sectors, replicas, replicas_want)`
- `journal_write_alloc(&self, w, replicas) -> Result<(), JournalError>`
- `target_rw_devs(c, data_type, target) -> BchDevsMask`

### 3. Contracts

- target 取 `metadata_target ?: foreground_target`；replica 目标取 `metadata_replicas`。
- journal RW mask 必须同时满足 member state=RW、`data_allowed & BIT(Journal)`、durability 非零；不能只遍历 online/RW 设备。
- 第一次 allocation 不足时，每个候选设备最多推进一次 bucket 并重试；指定 target 仍不足时 target 清零，从全部 journal RW 设备重试。
- extent ptr 与 IO ref guard 同序追加；跳过设备时立即释放 guard，成功路径延迟到 submit/no-IO completion 释放。
- 达不到 `replicas_want` 但至少得到一个 durability 时属于 degraded success；零 replica 才返回 insufficient-journal-devices。

### 4. Validation & Error Matrix

| 条件 | 结果 |
|---|---|
| member 非 RW / 不允许 Journal / durability=0 | 不进入候选 mask |
| 设备重复、无 journal bucket 或当前 bucket 空间不足 | 释放本次 WRITE ref 并继续 |
| 当前 bucket 不足但 discarded bucket 可用 | 推进一次并重试 |
| target 内副本不足 | 清 target，从全部 journal RW 设备重试 |
| 最终 replica durability > 0 但小于目标 | 成功（degraded） |
| 最终 replica durability = 0 | `JournalError::Full` |

### 5. Good/Base/Bad Cases

- Good：target 设备先分配，副本不足后从全设备补齐，ptr/cas 数量和顺序一致。
- Base：默认 target=0、metadata_replicas=1，单设备写入并持有一个 WRITE ref。
- Bad：用 `devices_by_state(Rw)` 直接作为 journal mask，会把禁止 Journal 或 durability=0 的设备写入 extent。

### 6. Tests Required

- 单设备空间不足时只推进一次，断言 `cur_idx/sectors_free/bucket_seq`。
- 指定 target + 多 replica fallback，断言设备顺序、degraded success 和所有 IO ref 最终归零。
- 补充 disk-group parent mask、零 replica error 和无可用 bucket 分支。
- 每条测试命令必须在 60 秒内结束。

### 7. Wrong vs Correct

```rust
// Wrong: RW 状态不等于可写 journal metadata。
let devs = registry.devices_by_state(BchMemberState::Rw);

// Correct: 还必须应用 data_allowed 与 metadata durability gate。
let devs = target_rw_devs(c, BchDataType::Journal, target);
```

## Scenario: journal write submit and no-I/O completion

### 1. Scope / Trigger

- closed entry 已被 `bch2_journal_do_writes_locked` 选中、且 reservation count 归零后触发。
- 唯一依据：本地 `journal/write.c:234-617,819-946` 与 `opts.h` 的 `nochanges`。

### 2. Signatures

- `bch2_journal_write(&self) -> Result<(), JournalError>`
- `journal_write_preflush(&self, buf_idx, journal_devs, first_err, io_failures)`
- `journal_write_submit(&self, buf_idx, writes, flush, separate_flush, first_err, io_failures)`
- `journal_write_endio(first_err, io_failures, buf_idx, dev_idx, result)`
- `VolumeConfig.nochanges: bool` / `BchOpts.nochanges: bool`
- `StorageConfig.journal_flush_delay_ms: u32` / `BchOpts.journal_flush_delay: u32`
- `VolumeConfig.journal_rewind_discard_buffer_percent: u8` /
  `BchOpts.journal_rewind_discard_buffer_percent: u8`（本地默认值 4）
- `write_work_deadline_ms: AtomicU64` / `write_work_notify: Notify` /
  `write_work_running: AtomicBool`，共同承载本地 `struct delayed_work write_work`
  的 pending deadline、workqueue 生命周期与重置/取消唤醒。
- `seq_write_started: AtomicU64`、`nr_flush_writes: AtomicU64`、
  `nr_noflush_writes: AtomicU64`，对应本地 `struct journal` 同名字段。
- `entry_bytes_written: AtomicU64`，对应本地 `struct journal`
  同名字段。

### 3. Contracts

- 主链顺序固定为 `realloc -> prep -> allocation -> checksum -> publish
  write_allocated -> do_writes -> devs_written/replicas -> nochanges gate -> preflush -> submit -> done`。
- `write_allocated` 只能在 checksum 成功后发布；发布前必须断言当前 seq 仍是
  oldest unallocated entry，发布后立即调度下一 entry。
- checksum 成功且 oldest-unallocated 不变式成立后，必须保持本地
  `w->sectors = 0 -> w->write_allocated = true -> entry_bytes_written +=
  vstruct_bytes(w->data)` 的顺序；记账完成后才可设置 `separate_flush`、
  更新 space 并调度下一 entry。同一 entry 只能记账一次。
- 非 `FLUSH_NO_WAIT` entry 在任何 bio 前先标记 replicas 和 pin devs；
  `FLUSH_NO_WAIT` 延迟到 completion。
- 多 RW member 的 flush write 先对所有 RW member 并发零数据 PREFLUSH，全部完成后
  才提交 extent data；RO member 不能进入 preflush。
- `nochanges=true` 仍执行 prep/allocation/checksum/replica mark，然后跳过所有 bio，释放
  allocation WRITE refs 并进入 `journal_write_done`。
- prep/allocation/checksum 任一失败都执行 emergency halt，再走同一
  no-I/O ref release 和 FIFO completion 清理链。
- `__should_flush()` 挂载到 `BchVol` 后必须直接读取 `c.opts.journal_flush_delay`；
  `Journal::journal_flush_delay_ms` 只允许作为无 `BchVol` 的 standalone fallback。
  `BchVol::alloc_with_registry()` 从持久化 `StorageConfig` 初始化该 option，缺字段时
  使用本地 `opts.h` 默认值 1000ms。
- flush 被选中后，仅当 `journal_rewind_discard_buffer_percent == 0` 时才把
  `rewind_seq` 设置为 `seq + 1`；默认值 4 必须保留既有 rewind 边界。随后无论
  option 值为何，都以 `min(rewind_seq, seq + 1)` 写入 `RewindLimit` entry。
- 每次 open entry 都在 workqueue running 且 work 尚未 pending 时按
  `journal_flush_delay` 排队；构造期 open 不得提前触发。flush write
  选中后，若 `seq != journal_cur_seq()`，必须在递增 `flushes_outstanding` 之后、
  更新 `rewind_seq` 之前把 deadline 重置为 `now + journal_flush_delay`；否则取消。
  deadline 到期只调用 `bch2_journal_write_work()`，其 callback 只调用
  `bch2_journal_flush_async()`。
- final buf ref 由 reservation put 或 entry-close 内部 put 释放时，都必须在进入
  `__bch2_journal_buf_put_final()` 后先发布 Rust 状态 `Closing -> WriteSubmitted`，
  再保持本地 `pin_put -> update_last_seq -> do_writes_locked -> wake` 顺序。
- noflush 分支在清除 header/buf `last_seq` 后递增 `nr_noflush_writes`；flush
  分支在更新 `last_flush_write` 后、清 `JOURNAL_need_flush_write` 前递增
  `nr_flush_writes`。通过 flush ordering gate 后，必须先发布
  `seq_write_started = seq`，再发布 `w.write_started = true`。

### 4. Validation & Error Matrix

| 条件 | 结果 |
|---|---|
| separate flush + RW member | 先 PREFLUSH，后 data + FUA |
| separate flush + RO member | 不提交 PREFLUSH |
| 单设备 bio 失败 | 记录该 dev failure，从 `devs_written` 删除，允许 degraded completion |
| 所有 replica 失败 | 设置 `err_seq`，不推进 `flushed_seq_ondisk` |
| `nochanges=true` | write/flush 调用数为 0，refs 归零，仍推进 completion FIFO |
| allocation 零 replica | 返回 `JournalError::Full`，halt，释放 refs，移除 in-flight entry |
| 持久化 flush delay=60000ms，Journal fallback=0 | timeout 未到时 `__should_flush()==0`，不得立即 flush |
| rewind discard buffer percent=0 | flush 选择把 `rewind_seq` 推进到 `seq+1` |
| rewind discard buffer percent=4（默认） | flush 选择不修改 `rewind_seq`，RewindLimit 使用既有边界 |
| flush old entry（`seq != cur_seq`） | delayed work deadline 重置为当前时间加配置 delay |
| flush current entry（`seq == cur_seq`） | delayed work deadline 清零，已排队 work 被取消 |
| entry-close 内部 final-put | `Closing -> WriteSubmitted` 后 worker 可完成写入，不得同步递归卡死 |
| noflush/flush 被选中一次 | 只递增对应计数器，另一个保持不变 |
| flush ordering gate 未通过 | 不更新 `seq_write_started`，也不发布 `write_started` |
| prep/allocation/checksum 失败 | `entry_bytes_written` 不变 |
| checksum 成功并发布 allocation | 精确加上当前 Jset 序列化对齐后字节数 |
| worker 再次看到已 `write_allocated` entry | 不得重复记账 |

### 5. Good/Base/Bad Cases

- Good：两个 closed entry 只先启动 oldest；第一个 allocation 发布后才启动第二个。
- Base：单设备 flush entry 用 PREFLUSH+data+FUA，completion 后 WRITE ref 为 0。
- Bad：在 checksum 前设 `write_allocated=true`，会让下一 worker 看到尚未完整的 entry。
- Bad：在 checksum 成功和 `write_allocated` 发布前累加
  `entry_bytes_written`；失败或尚未发布的 entry 会被错误统计。
- Bad：无条件执行 `rewind_seq = seq + 1`；这会把本地默认的 4% rewind discard
  buffer 当成关闭状态，过早宣告旧 journal bucket 可安全 discard。
- Bad：只在公开 reservation put 发布 `WriteSubmitted`；无 reservation 的 entry-close
  走内部 final-put 后会留下 `Closing + write_started`，worker 永远无法消费并无限递归。
- Bad：先设 `write_started` 再更新 `seq_write_started`；观察者可能看到 worker 已启动，
  但全局启动边界仍落后于该 entry。

### 6. Tests Required

- oldest-unallocated 启动顺序与 FIFO ondisk 顺序。
- PREFLUSH 先于 data，FUA completion 后才返回；RW-only member 筛选。
- all-replica failure、`nochanges`、allocation error cleanup，断言 `err_seq`/ondisk 边界/refcount。
- `test_should_flush_uses_volume_journal_flush_delay`：断言 volume option 覆盖
  standalone fallback，且超时前不选择 flush。
- `test_flush_selection_respects_rewind_discard_buffer_option`：分别断言 option=0
  时推进到 `seq+1`，option=4 时保持原 `rewind_seq`。
- `test_flush_selection_rearms_or_cancels_write_work`：分别断言 old/current entry 的
  delayed-work 重置与取消。
- `test_auto_commit_write_work_fires_and_stops_cleanly`：deadline 到期执行 callback，
  entry-close 内部 final-put 可完成，worker 可同步停止且不栈溢出。
- `test_bch2_journal_do_writes_counts_noflush_and_started_seq` 与 zero-delay flush
  用例分别断言两个计数器及 `seq_write_started -> write_started` 最终状态。
- `test_journal_write_pipeline_advances_oldest_unallocated_entry_first`：在验证
  oldest-unallocated 顺序同时，断言 `entry_bytes_written` 等于两个
  已发布 Jset 的序列化对齐后字节数之和。
- 每条测试命令必须在 60 秒内完成。

### 7. Wrong vs Correct

```rust
// Wrong: allocation 后立即发布，checksum 失败时下一 entry 已可启动。
journal_write_alloc(w)?;
w.write_allocated = true;
bch2_journal_write_checksum(w)?;

// Correct: checksum 先成功，再在 oldest-unallocated 不变式下发布。
journal_write_alloc(w)?;
bch2_journal_write_checksum(w)?;
assert_eq!(journal_last_unallocated_seq(), w.seq);
w.write_allocated = true;

// Wrong: checksum/发布前记账，失败 entry 也会进入统计。
entry_bytes_written += serialized_bytes(w);
bch2_journal_write_checksum(w)?;

// Correct: 对齐本地 write.c:872-878 的发布与记账顺序。
bch2_journal_write_checksum(w)?;
w.sectors = 0;
w.write_allocated = true;
entry_bytes_written += serialized_bytes(w);

// Wrong: production timeout 读取 Journal 构造时的 0 fallback，导致每次立即 flush。
let delay = self.journal_flush_delay_ms.load(Ordering::Acquire);

// Correct: 与本地 write.c 一致，从挂载卷 c->opts 读取；仅 standalone 回退 atomic。
let delay = self.vol.upgrade().map_or(fallback, |c| c.opts.journal_flush_delay);

// Wrong: 忽略 rewind discard buffer option，无条件缩短可回滚范围。
self.rewind_seq.store(seq + 1, Ordering::Release);

// Correct: 与本地 write.c:1140-1141 一致，仅 option=0 时推进。
if c.opts.journal_rewind_discard_buffer_percent == 0 {
    self.rewind_seq.store(seq + 1, Ordering::Release);
}

// Wrong: flush write 后保留旧 deadline，auto-commit 可能过早触发。
// Correct: old entry 重置 deadline，current entry 取消；顺序位于 outstanding++ 与 rewind 之间。
if seq != journal_cur_seq {
    write_work_deadline = now + c.opts.journal_flush_delay;
} else {
    write_work_deadline = 0;
}

// Wrong: per-buffer started 先于全局启动序列发布。
w.write_started = true;
seq_write_started = seq;

// Correct: 与本地 write.c:1157-1158 一致。
seq_write_started = seq;
w.write_started = true;
```

## Scenario: journal buffer growth

### 1. Scope / Trigger

- write worker 已取得待写 `JournalBuf`，且 `buf_size < journal.buf_size_want` 时触发。
- 唯一依据：本地 `journal/write.c:161-189,819-839` 与
  `btree/write_buffer.c:285-303,1345-1353`。

### 2. Signatures

- private `journal_buf_realloc(&self, buf: &mut JournalBuf)`
- public `bch2_btree_write_buffer_resize(c: &BchVol, new_size: usize) -> i32`
- private `wb_keys_resize(wb: &mut BtreeWriteBufferKeys, new_size: usize) -> i32`

### 3. Contracts

- `buf_size >= buf_size_want` 必须立即返回，不触碰 btree write buffers。
- journal buffer 扩容前必须先把全部 `BCH_WB_BTREE_NR` 的 `flushing`、`inc`
  依次扩到 `new_size / 64`；任何一次失败都立即返回并保留旧 journal buffer。
- 新 journal buffer 分配成功后复制恰好 `old buf_size` 字节；只在 `journal.lock`
  内交换 `data` 和 `buf_size`，退出锁区后释放旧 allocation。
- write path 持有 `journal.buf_lock` 执行 realloc 和 prep，顺序固定为
  `journal_buf_realloc -> bch2_journal_write_prep`。

### 4. Validation & Error Matrix

| 条件 | 结果 |
|---|---|
| 旧 buffer 已足够大 | 立即返回，data/size/capacity 不变 |
| 任一 write-buffer mutex try-lock 失败 | 返回 `-EINTR`，journal buffer 不变 |
| 任一 write-buffer allocation 失败 | 返回 `-ENOMEM`，journal buffer 不变 |
| journal buffer allocation 失败 | 静默返回，已扩大的 write buffers 保留，journal buffer 不变 |
| 全部成功 | 原数据前缀保持，data/size 在 lock 内交换，旧 allocation 在解锁后释放 |

### 5. Good/Base/Bad Cases

- Good：`buf_size_want=2*BUF_SIZE`，全部 11 组 inc/flushing capacity 至少为
  `buf_size_want/64`，journal data 前缀逐字节不变。
- Base：`buf_size_want==buf_size`，零 allocation、零 swap。
- Bad：先扩大 journal buffer，后扩大 btree buffers；中途失败会让 journal 接受
  write-buffer 无法承载的 entry 大小。

### 6. Tests Required

- 断言增长后 journal `buf_size/data.len`、原数据前缀与 22 个 write-buffer capacity。
- 后续补充 try-lock failure 注入，断言 journal data/size 未交换。
- 所有测试必须在 60 秒内完成。

### 7. Wrong vs Correct

```rust
// Wrong: journal 先增长，破坏上游的失败原子性与依赖顺序。
buf.data.resize(new_size, 0);
bch2_btree_write_buffer_resize(c, new_size / 64);

// Correct: write buffers 全部成功后才分配、复制并在 journal.lock 内交换。
if bch2_btree_write_buffer_resize(c, new_size / 64) != 0 {
    return;
}
// allocate/copy; lock; swap(data); swap(buf_size); unlock; free old
```

## Scenario: journal multi-device search/read path

### 1. Scope / Trigger

- replay 读取所有 RW/RO member journal；当单设备 bucket 数大于 32
  且未强制 full read 时触发搜索路径。
- 唯一依据：本地 `journal/read.c:331-827,917-1011,1156-1414`。

### 2. Signatures

- `journal_read_bucket(&self, journal_dev: Arc<BchDev>, bucket_idx: u32, journal_list: Arc<Mutex<JournalList>>) -> Result<(), JournalError>`
- `journal_peek_bucket(&self, journal_dev: Arc<BchDev>, bucket: usize) -> Result<u64, JournalError>`
- `journal_anchor_bucket(...)-> Result<Option<usize>, JournalError>`
- `journal_bsearch_head(..., anchor: usize) -> Result<usize, JournalError>`
- `journal_walk_inuse(..., head: usize, order: &mut Vec<(usize, u64)>) -> Result<bool, JournalError>`
- `journal_bsearch_collect(&self, journal_dev: Arc<BchDev>) -> Result<Vec<(usize, u64)>, JournalError>`
- `bch2_journal_read_device(&self, journal_dev: Arc<BchDev>, journal_list: Arc<Mutex<JournalList>>) -> Result<(), JournalError>`
- `bch2_journal_read(&self, info: &mut JournalStartInfo) -> Result<Vec<(u32, Jset)>, JournalError>`
- `bch2_journal_seq_blacklist_add(&self, c: &BchVol, start: u64, end: u64) -> Result<(), JournalError>`
- `bch2_journal_entry_missing_range(&self, start: u64, end: u64) -> U64Range`
- `journal_has_any_missing(&self, journal_list, start_seq, end_seq) -> bool`
- `journal_retry_full_read(&self, journal_list: Arc<Mutex<JournalList>>) -> Result<(), JournalError>`
- `JournalList.full_read: bool`

`JournalStartInfo` 必须保持本地 `journal/types.h:502-507` 的字段顺序：
`last_seq: u64`、`replay_end: u64`、`cur_seq: u64`、`clean: bool`。

### 3. Contracts

- peek 只读 bucket 首 block，且每桶最多 peek 一次。
- anchor 先查 0，再从 `rounddown_pow_of_two(nr - 1)` 开始按奇数倍 stride 查找。
- head 搜索使用环形索引和 high-biased midpoint；反向 walk 要求 seq 严格递减。
- walk 失败必须清空 order，peek 剩余全桶并 rebuild。
- order 按 seq 降序全读；已确定 `last_seq` 且当前桶 seq 更旧时停止。
- 搜索返回空 order 不代表结束，必须进入 full-bucket-read 处理空 journal。
- 设备入口按稳定 `dev_idx` 顺序遍历 member，仅 RW/RO 并且成功取得
  READ ref 的设备启动 read future；全部 future 完成后才统一释放 READ refs。
- 有效 early header 先更新对应设备的 `highest_seq_found/cur_idx/sectors_free/
  bucket_seq`，再进行 checksum 处理；设备读完后以 `(cur_idx + 1) % nr`
  同时设置 `discard_idx/dirty_idx_ondisk/dirty_idx`。
- 所有设备必须写入同一个按 seq 排序的 `JournalList`；checksum-bad entry
  不能在 bucket 层提前丢弃，因为后续设备可能提供同 seq 的 good replica。
- 同 seq 副本合并顺序固定：先拒绝同设备不同位置；两个 checksum-good
  且内容不同则报错；内容相同或新副本 checksum-bad 时保留已有内容；新副本
  checksum-good 且已有副本 checksum-bad 时替换内容，同时保留全部物理 ptr。
- recovery 只读 journal list 一次；btree-root 提取与 accounting/data replay
  必须复用已读取 Jset，不能在 `bucket_seq` 已更新后二次读盘。
- 多设备读取完成后必须按 seq 逆序确定 start-info：`cur_seq` 来自最高的任意
  on-disk entry（包括 NO_FLUSH 和 checksum-bad）加一；NO_FLUSH 先标记忽略；
  首个 checksum-bad flush 作为最新 torn write 标记忽略；再由首个可用 flush
  entry 设置 `last_seq/replay_end`。该顺序不能改成先过滤再计算 `cur_seq`。
- `clean` 必须与本地 `journal_entry_empty()` 一致：`seq == last_seq`，且不存在
  payload 非空的 `BtreeKeys` entry。
- `last_seq > seq` 当前已按本地修复为 `last_seq = seq`，但 Rust 尚未表达对应
  fsck policy；不得把这一步改成无条件报错。
- `drop_before = info.last_seq` 必须在 missing/full-read retry 前执行。普通 leaf
  insert/delete 成功后必须立即调用 `bch2_btree_add_journal_pin()`，使 write header
  的 `last_seq` 受真实 dirty-node pin 约束；否则会过早删除仍需 replay 的旧 entry。
- blacklist 的唯一持久来源是 `BchSb.journal_seq_blacklist`。add 在 `sb_lock` 下按
  严格 `<`/`>` 比较合并重叠或相邻区间，设置 feature bit，写全部 member superblock，
  成功后替换 runtime table；不得新写 Blacklist Jset。
- drop 阶段先跳过已 ignore entry，再删除 `seq < drop_before`；命中 blacklist 时，
  flush entry 记录 fsck error，随后只设置 `ignore_blacklisted`。该顺序不得移到
  start-info 之前或 missing retry 之后。
- missing-range 计算必须先用 `next_nonblacklisted(start)` 跳过覆盖区；若仍在
  `end` 前，再用 `min(end, next_blacklisted(start))` 截出第一段缺口。
- fast path 完成后只允许针对所有设备共享 union 检查 `[last_seq, replay_end]`
  的缺口；单设备 bucket gap 不能直接判错。发现缺口后创建 full-read round，
  仅对 `nr > 32` 的 RW/RO member 重新获取 READ ref，并令 `full_read=true` 跳过
  bsearch；所有设备完成并释放 READ ref 后才继续。
- missing/retry 阶段之后，按 seq 升序扫描所有未 ignore 且 checksum/validate 成功的
  Jset entries：每个 `RewindLimit` 依次覆盖 `rewind_seq` 与
  `rewind_seq_ondisk`，每个 `Rewind` 按出现顺序追加 `(from, to)` 到
  `rewind_ranges`。该恢复必须发生在返回 replay list 之前。

### 4. Validation & Error Matrix

| 条件 | 结果 |
|---|---|
| peek IO 错/首 block 不是 jset | 该桶 seq 为 0，不中止 recovery |
| 无任何 anchor | order 为空，设备路径回退 full read |
| 反向 seq 非严格递减 | 废弃 fast order，全 peek rebuild |
| 全桶读遇 checksum 错 | 设 `saw_bad`，后续从下一 block 边界继续 |
| 单设备全桶 IO 错 | 返回空结果，不中止整体 recovery |
| member 非 RW/RO 或 READ ref 失败 | 不启动该设备读取 |
| 任一设备读取 future 返回错误 | 等待已启动设备全部完成，释放 refs 后传播错误 |
| 同物理位置重复读同 seq | 保持幂等，不重复加入 ptr |
| 同设备不同位置出现同 seq | 返回 `InvalidData`；当前尚未表达本地 fsck policy |
| 两个 checksum-good 同 seq 但内容不同 | 返回 `InvalidData`；当前尚未表达本地 fsck policy |
| 已有 bad、新到 good 同 seq | good 内容替换 bad，bad/good 两个物理 ptr 均保留 |
| 已有 good、新到 bad 同 seq | 保留 good 内容，同时保留新 bad ptr |
| 最高 flush 只有 checksum-bad 副本 | 作为最新 torn write 忽略；`cur_seq` 仍取其 seq+1，再从更旧有效 flush 得出 replay 边界 |
| 只有 NO_FLUSH entry | `cur_seq` 取最高 seq+1，`replay_end/last_seq` 保持 0，不输出 replay entry |
| 最高 torn flush、其次 NO_FLUSH、再其次有效 flush | 两个较新 entry 均忽略，`cur_seq` 取 torn seq+1，边界来自有效 flush |
| `start == end` 或跳过 blacklist 后 `start >= end` | 返回 `{ start: 0, end: 0 }` |
| 区间跨越下一 blacklist | 只返回 blacklist 之前的第一段 missing range |
| fast-path union 在 `[last_seq, replay_end]` 有缺口 | 对所有 `nr > 32` RW/RO 设备启动 full-read retry |
| retry 遇 `nr <= 32`、Spare 或 READ ref 失败 | 跳过该设备，继续等待其余已启动读取 |
| 有效 RewindLimit payload | 同时恢复内存与 ondisk rewind limit |
| 有效 Rewind payload | 按 journal 顺序追加 `[from, to)`；不得加入 early write queue |

### 5. Good/Base/Bad Cases

- Good：40 桶中尾部两桶 seq=100/101，搜索后只恢复这两个活跃桶。
- Good：共享 union 只有 seq=100/102 时识别 101 缺口，full-read retry 从未读桶补入
  seq=101，最终 READ ref 为 0。
- Base：RW 设备含 checksum-bad seq=77、RO 设备含内容不同的 checksum-good
  seq=77、Spare 含 seq=999，结果只有 RO 的 good seq=77，三者 READ ref 最终均为 0。
- Bad：accounting 阶段后重新读盘；第一次读取已提升 `bucket_seq`，
  二次遇到桶内较旧 seq 会按本地代码立即停止。
- Bad：仅凭 flush header 的 `last_seq` 删除更旧 entry；当前测试中的直接
  `append()` 没有 btree pins，曾导致 7 个 replay 用例丢失仍需重放的 entry。
- Bad：先过滤 NO_FLUSH/checksum-bad 再取 `cur_seq`；这会把下一次写入序号回退到
  replay 边界，而不是最高 on-disk 序号之后。
- Bad：在每个设备 fast path 结束后分别检查缺口；多设备副本布局本来就允许单设备
  seq 不连续，必须等共享 union 构建完成后检查。
- Bad：只把 Rewind range 恢复到 `early_journal_entries`；那会在 recovery 后把历史
  rewind 控制 entry 当成待写数据重复提交。

### 6. Tests Required

- `test_journal_read_device_bsearch_finds_live_tail`：断言 40 桶环形布局恢复
  seq `[100, 101]`，并断言 per-device head/free/index 状态。
- `test_journal_entries_read_scans_rw_ro_devices_and_releases_read_refs`：断言 RW/RO
  筛选、Spare 排除、bad→good 副本择优、good 内容标记与 READ ref 归零。
- `test_journal_entry_add_prefers_good_replica_and_tracks_ptrs`：断言 good 内容
  替换 bad，同时两个设备的 bucket/offset/sector ptr 全部保留。
- `test_journal_entry_add_keeps_good_for_reread_and_bad_replica`：断言同物理位置
  reread 幂等，且 good 之后到达的 bad 只追加 ptr、不替换内容。
- `test_journal_entry_add_rejects_duplicate_conflicts`：断言同设备异位置和两个
  good 内容不一致均返回 `InvalidData`。
- `test_journal_entry_missing_range_skips_blacklisted_sequences`：断言空区间、
  blacklist 内、从 blacklist 起点开始及跨下一 blacklist 的边界结果。
- `test_bch2_journal_read_computes_start_info_after_noflush_and_torn_write`：断言
  torn seq=12、NO_FLUSH seq=11、有效 flush seq=10 时，`cur_seq=13`，
  `replay_end=10`、`last_seq=5`，且只输出 seq=10。
- `test_journal_retry_full_read_fills_union_gap`：断言 100/102 union 检出 101 缺口，
  40-bucket full-read round 补齐 101、设置 `full_read` 并释放 READ ref。
- `test_bch2_journal_read_restores_rewind_state`：断言 RewindLimit=7 同时恢复两个
  rewind seq，Rewind=(8,10) 只进入 runtime ranges。
- recovery/replay 测试必须复用一次读取结果，断言 btree root、blacklist
  与 accounting/data 两阶段不二次读盘。
- journal 定向套件要覆盖全读、replay、btree roots 和 blacklist，每条命令必须小于 60 秒。
- 后续必须增加：一个副本 IO 错、bad-only 的 fsck policy、retry 后仍 missing 的
  `bch2_journal_check_for_missing` policy 和 rewind reread。

### 7. Wrong vs Correct

```rust
// Wrong: accounting/data 阶段分别重新读盘。
let accounting = journal.bch2_journal_read(&mut accounting_info).await?;
let data = journal.bch2_journal_read(&mut data_info).await?;

// Correct: 只读一次，两阶段复用同一 journal list。
let jsets = journal.bch2_journal_read(&mut journal_start).await?;
let mut replayer = JournalReplayer::from_jsets(journal, jsets);

// Wrong: bucket 层直接丢弃 checksum-bad，后续无法合并同 seq good replica。
if !jset.verify() { continue; }

// Correct: bad/good 都进入共享 JournalList，最终只输出选中的 good replica。
journal_entry_add(dev, ptr, &mut journal_list, jset, raw)?;

// Wrong: 在单设备 fast read 后立即把 bucket gap 当成 journal 缺失。
if device_has_missing { return Err(...); }

// Correct: 所有设备共享 union 完成后检查，缺口先强制 full-read retry。
if journal_has_any_missing(&journal_list, last_seq, replay_end) {
    journal_retry_full_read(&journal_list).await?;
}

// Wrong: recovery 出来的 rewind range 被再次排入 journal write。
slowpath.early_journal_entries.push((from, to));

// Correct: read.c 只恢复 runtime rewind_ranges。
slowpath.rewind_ranges.push((from, to));

```

### Round 3 新增函数 (journal.c 对齐, 2026-07-08)

| 函数 | bcachefs 对应 | 状态 |
|------|--------------|------|
| `__bch2_journal_buf_put` | `__bch2_journal_buf_put` (journal.h:395) | ✅ |
| `__bch2_journal_buf_put_final` | `__bch2_journal_buf_put_final` (journal.c:240) | ✅ |
| `bch2_journal_quiesced` | `journal_quiesced` (journal.c:692) | ✅ |
| `bch2_journal_quiesce` | `bch2_journal_quiesce` (journal.c:703) | ✅ |
| `bch2_journal_shutdown_quiesced` | `journal_shutdown_quiesced` (journal.c:722) | ✅ |
| `bch2_journal_shutdown_quiesce` | `bch2_journal_shutdown_quiesce` (journal.c:737) | ✅ |
| `bch2_journal_halt_locked` | `bch2_journal_halt_locked` (journal.c:666) | ✅ |
| `bch2_journal_halt` | `bch2_journal_halt` (journal.c:686) | ✅ |
| `__bch2_journal_meta` | `__bch2_journal_meta` (journal.c:1316) | ✅ |
| `bch2_journal_meta` | `bch2_journal_meta` (journal.c:1330) | ✅ |
| `__bch2_journal_block` | `__bch2_journal_block` (journal.c:1365) | ✅ |
| `bch2_journal_block` | `bch2_journal_block` (journal.c:1386) | ✅ |
| `JournalBlockGuard` | RAII guard, bcachefs 用显式 block/unblock | ✅ |
| `bch2_journal_unblock` | `bch2_journal_unblock` (journal.c:1344) | ✅ |
| `bch2_journal_flush_seq` | `bch2_journal_flush_seq` (journal.c:1207) | ✅ |
| `bch2_journal_flush_seq_async` | `bch2_journal_flush_seq_async` (journal.c:1157) | ✅ |
| `bch2_journal_flush_async` | `bch2_journal_flush_async` (journal.c:1243) | ✅ |
| `bch2_journal_entry_res_resize` | `bch2_journal_entry_res_resize` (journal.c:988) | ✅ |
| `bch2_journal_noflush_seq` | `bch2_journal_noflush_seq` (journal.c:1265) | ✅ |
| `bch2_journal_advance_rewind_seq` | `bch2_journal_advance_rewind_seq` (journal.c:1288) | ✅ |
| `bch2_journal_add_rewind_range` | `bch2_journal_add_rewind_range` (journal.c:1294) | ✅ |
| `bch2_journal_do_writes_locked` | `bch2_journal_do_writes_locked` (write.c:1087) | ✅ |
| `bch2_journal_do_writes` | `bch2_journal_do_writes` (write.c:1164) | ✅ |
| `bch2_journal_write_work` | `bch2_journal_write_work` (journal.c:748) | ✅ |

### Round 3 Flush / Quiesce Notes

| 函数 | bcachefs 关键条件 | 状态 | 说明 |
|------|------------------|------|------|
| `bch2_journal_quiesced` | `seq == seq_ondisk` (journal.c:696) | ✅ | 不能用 `flushed_seq_marker` 代替；quiesce 关注真正落盘完成 |
| `bch2_journal_shutdown_quiesced` | `seq == seq_ondisk` when errored, else `seq == flushed_seq_ondisk && !flush_wait` (journal.c:727-730) | ✅ | `flush_wait` 非空时不算 shutdown quiesced |
| `bch2_journal_flush_seq_async` | `seq <= flushed_seq_ondisk` early return; `seq > cur_seq` short-circuit; `flushing_seq` max; `err_seq` gate (journal.c:1165-1193) | ✅ | 已刷过的 seq 直接返回，越界 seq 不进入等待链，flush 水位保持单调 |
| `bch2_journal_flush_seq` | `closure_sync_timeout` equivalent via async+wait bridge, with runtime-safe blocking (`block_on_safe`) (journal.c:1207-1230) | ✅ | 同步入口必须等到 `flushed_seq_ondisk`，thin wrapper 不再单独做前置分支判断 |

### Round 3 Noflush / Rewind Notes

| 函数 | bcachefs 关键条件 | 状态 | 说明 |
|------|------------------|------|------|
| `bch2_journal_noflush_seq` | `flushed_seq_ondisk >= start` early return; then `[start, end)` walk of in-flight seqs (journal.c:1265-1283) | ✅ | 以真实落盘边界判断，不能用 `flushed_seq_marker` 替代 |
| `bch2_journal_add_rewind_range` | `rewind_ranges` darray push + `early_journal_entries` append (journal.c:1294-1312) | ✅ | pending rewind range 会先 materialize 为 `Rewind` entry；`RewindLimit` 仍由 flush 路径单独追加，和 bcachefs 的分层一致 |

### bcachefs 独有函数（subvol 无对应）

| 模块 | 数量 | 说明 |
|------|------|------|
| validate.c 余量 | ~32 | 不适用于 subvol 的 entry types（prio_ptrs/usage/clock/log/datetime/rewind 等）+ to_text 链 + static helpers |
| init.c 生命周期 | 10 | `_start/_exit/_init_early/_dev_*` 等（`set_replay_done` ✅，`_stop` 已对齐 → ✅） |
| write.c 写入链 | 21 | 全部 21 个函数已由 `07-15-journal-write-alignment` 对齐为 ✅ |
| sb.c | 2 | superblock 字段 ops |
| read.c 扫描/搜索 | 12 | `journal_peek_bucket`/`bsearch_head`/`walk_inuse` 等 |
| **总计** | ~**52** | |

---

## types.rs（65 个非测试函数）

### Part 1：自由函数

| 行号 | subvol 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| 184 | `journal_error_check_stuck()` | `journal_error_check_stuck` | journal.c:209 | ✅ | 简化版（无 flags/ERO），语义等价 |
| 2419 | `extract_blacklist_entries()` | — | — | ➖ | subvolmount 特有工具函数 |

### Part 2：impl JournalBuf

| 行号 | subvol 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| 296 | `fn free()` | — | — | ➖ | Rust 析构构造，bcachefs 中 bufs 是静态数组始终分配 |
| 310 | `fn reset_for_accepting()` | `__journal_entry_open_one` buf init | journal.c:391 | ✅ | 重置 buf 状态为 Accepting，语义等价 |
| 331 | `fn journal_buf_try_noflush()` | `journal_buf_try_noflush` | journal.h:191 | ✅ | 仅允许 `NULL -> NOFLUSH`；`NOFLUSH` 幂等，`FLUSH_NO_WAIT` / waiters 保持可 flush，已对齐上游 sentinel 语义 |

### Part 3：impl JournalResState

| 行号 | subvol 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| 394 | `fn new()` | `JOURNAL_ENTRY_CLOSED_VAL` | journal.h | ✅ | 初始化为关闭状态 |
| 401 | `fn read()` | `smp_load_acquire(&j->reservations.v)` | journal.c | ✅ | Acquire load — 语义等价 |
| 406 | `fn cur_entry_offset(v)` | `union journal_res_state.cur_entry_offset:22` | types.h:159 | ✅ | bits 0-21 提取 — 位布局一致 |
| 411 | `fn idx(v)` | `union journal_res_state.idx:2` | types.h:160 | ✅ | bits 22-23 提取 — 位布局一致 |
| 416 | `fn buf_count(v, idx)` | `journal_state_count` | journal.h:243 | ✅ | shift 公式一致（BUF0_COUNT_SHIFT=24, BUF_COUNT_BITS=10） |
| 426 | `fn try_reserve()` | `journal_res_get_fast` | journal.h:475 | ✅ | 核心 CAS 保留逻辑 |
| 465 | `fn release()` | `__bch2_journal_buf_put` | journal.h:395 | ✅ | atomic_sub 释放 |
| 480 | `fn close_entry()` | `__journal_entry_close_one` | journal.c:276 | ✅ | loop CAS + 已关闭状态早退；语义对齐 bcachefs 的幂等 close |
| 499 | `fn open_entry()` | `__journal_entry_open_one` | journal.c:391 | ✅ | CAS open + 格式转换 |
| 528 | `fn align_idx_to_seq()` | 不变量 `idx ≡ (seq-1) & BUF_MASK` | journal.c | ✅ | bcachefs 不变量 |
| 537 | `fn is_closed()` | `__journal_entry_is_open` (相反) | journal.c:137 | ✅ | `offset >= CLOSED_VAL` = `!is_open()`，逻辑互补 |

### Part 4：impl BufArray

| 行号 | subvol 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| 561 | `fn new()` | — | — | ➖ | Rust Vec 封装 |
| 569 | `fn get()` | — | — | ➖ | dead_code |
| 575 | `fn get_mut()` | — | — | ➖ | 内部访问器 |
| 581 | `fn get_all_mut()` | — | — | ➖ | dead_code |

### Part 5：impl JournalSlowpath

| 行号 | subvol 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| 668 | `fn new()` | `bch2_fs_journal_alloc` slowpath 部分 | init.c:305 | ✅ | 初始化 bucket_seq/buckets 等慢路径字段 |
| 689 | `fn from_superblock()` | `bch2_fs_journal_init` 部分 | init.c:802 | ✅ | 从 sb 状态恢复慢路径字段 |

### Part 6：impl Journal — 构造函数 & 错误处理

| 行号 | subvol 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| 863 | `fn new()` | `bch2_fs_journal_alloc` | init.c:305 | ✅ | 基础构造（Phase 4 已验证） |
| 897 | `fn create()` | `bch2_fs_journal_alloc` + `bch2_dev_journal_alloc` | init.c:305/263 | ✅ | 含 allocator 动态分配 |
| 915 | `fn from_superblock()` | `bch2_fs_journal_init` + `bch2_fs_journal_init_rw` | init.c:802/758 | ✅ | 从 sb 恢复（Phase 4 已验证） |
| 951 | `fn to_superblock_state()` | `bch2_journal_buckets_to_sb` | sb.c:176 | ✅ | 导出状态到 sb |
| 967 | `fn journal_error_set()` | `bch2_journal_error_set` | journal.c | ✅ | 已对齐 |
| 990 | `fn journal_error_check()` | `bch2_journal_error` | journal.h:365 | ✅ | 已对齐 |
| 1101 | `fn bch2_journal_error_set()` | `bch2_journal_set_error` | journal.c | ✅ | journal_error_set 的別名 |
| 1120 | `fn bch2_journal_error_check()` | `bch2_journal_error` | journal.h:365 | ✅ | 已对齐 |

### Part 6b：核心 Fastpath API

| 行号 | subvol 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| 1039 | `fn journal_res_get_fast()` | `journal_res_get_fast` | journal.h:475 | ✅ | 核心 CAS fastpath |
| 1083 | `fn bch2_journal_set_watermark()` | `bch2_journal_set_watermark` | reclaim.c:69 | ✅ | 已对齐 |
| 1091 | `fn watermark()` | — | — | ➖ | Rust 枚举 getter |
| 1140 | `fn journal_cur_seq()` | `journal_cur_seq` | journal.h:137 | ✅ | inline getter |
| 1149 | `fn add_entry()` | `bch2_journal_add_entry` | journal.h:338 | ✅ | 对齐 |
| 1170 | `fn journal_res_put()` | `bch2_journal_res_put` | journal.h:423 | ✅ | 对齐 |
| 1189 | `fn bch2_journal_set_commit_callback()` | — | — | ➖ | subvolmount 特有（tokio 异步回调） |
| 1199 | `fn bch2_journal_wake_up()` | `journal_wake` | journal.h:118 | ✅ | 语义等价：仅唤醒等待者；buf 状态推进已在 `journal_res_put()` 中完成 |

### Part 6c：Flush flag 辅助

| 行号 | subvol 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| 1214 | `fn journal_set_needs_flush_write()` | `set_bit(JOURNAL_need_flush_write)` | init.c:628 | ✅ | AtomicBool:store(true, Release) 等价 set_bit |
| 1219 | `fn journal_clear_needs_flush_write()` | `clear_bit(JOURNAL_need_flush_write)` | write.c:1126 | ✅ | AtomicBool:store(false, Release) 等价 clear_bit |
| 1224 | `fn journal_needs_flush_write()` | `test_bit(JOURNAL_need_flush_write)` | write.c:848 | ✅ | AtomicBool:load(Acquire) 等价 test_bit |
| 1229 | `fn journal_update_flush_jiffies()` | `j->last_flush_write = jiffies` | init.c:610 | ✅ | 时间戳用 ms 记录，bcachefs 用 jiffies，均用于相对比较 |
| 1238 | `fn journal_last_flush_jiffies()` | `j->last_flush_write` (读) | — | ✅ | getter，语义等价 |
| 1248 | `fn bch2_journal_set_replay_done()` | `bch2_journal_set_replay_done` | init.c:619 | ✅ | 恢复→正常过渡（无 flags，设置 needs_flush_write） |
| 1260 | `fn bch2_fs_journal_stop()` | `bch2_fs_journal_stop` | init.c:438 | ✅ | 关闭 journal（flush + meta entry） |

### Part 6d：内部方法（private）

| 行号 | subvol 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| 1244 | `fn bch2_journal_update_last_seq()` | `bch2_journal_update_last_seq` | reclaim.c:422 | ✅ | 对齐 |
| 1262 | `fn journal_entry_open()` | `__journal_entry_open_one` | journal.c:391 | ✅ | R2 增强：添加 reclaim + space_available 调用 |
| 1300 | `fn journal_entry_close()` | `__journal_entry_close_one` | journal.c:276 | ✅ | R2 增强：添加 last_seq、sectors、buf_put、space_available |
| 1314 | `fn wait_for_pending_drain()` | `journal_buf_wait` | journal.c:1034 | ✅ | watch 等待 refcount→0，语义已从自旋近似收敛为事件驱动 |
| 1334 | `fn find_free_buf()` | `__journal_entry_open_one` idx++ 模式 | journal.c:391 | ✅ | idx 推进 |
| 1357 | `fn __bch2_journal_buf_put_final()` | `__bch2_journal_buf_put_final` | journal.c:240 | ✅ | R1: pin_put + update_last_seq + wake_up |
| 1394 | `fn __bch2_journal_buf_put()` | `__bch2_journal_buf_put` | journal.h:395 | ✅ | R1: release(idx) + final 检查 |

### Part 6e：Convenience API

| 行号 | subvol 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| 1373 | `fn append()` | — | — | ➖ | subvolmount 异步便利包装 |
| 1428 | `fn append_btree_root()` | — | — | ➖ | subvolmount 异步便利包装 |

### Part 6e'：Journal Safety Net (Phase 2)

| 行号 | subvol 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| ~1060 | `vol: OnceLock<Weak<BchVol>>` 字段 | — | — | ➖ | subvol 特有：Phase 2 journal safety net 用，OnceLock+Weak 避免循环引用 |
| ~1305 | `fn set_vol_ref()` | — | — | ➖ | subvol 特有：设置 BchVol 弱引用，在 `open_with_backend()` 中调用 |

### Part 6f：Bucket write / flush

| 行号 | subvol 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| ~3540 | `fn bch2_journal_write_single()` + `fn bch2_journal_write()` | `bch2_journal_write` | write.c:819 | ✅ | 已拆分为单 entry 异步函数 + wrapper；prep/error_check/alloc/checksum/write_allocated/do_writes/devs_written/replicas/preflush/submit/done 完整链；free_buf 回收已加入 |
| ~2444 | `fn journal_write_done()` + `fn journal_write_done_flush()` + `fn journal_write_endio()` + `fn journal_write_submit()` + `fn journal_write_preflush()` | write.c:234-617 | write.c:234-617 | ✅ | done_flush 早 waiter 唤醒、endio per-device 失败记录、submit 每设备 FUA/PREFLUSH、preflush 多 RW member 遍历、done 的 replica refs/FIFO 循环/wake/reclaim/cycle 顺序全部对齐 |
| 1699 | `fn bch2_journal_flush()` | `bch2_journal_flush` | journal.c:1255 | ✅ | 含 J2 flush data race fix |
| 1801 | `fn bch2_journal_flush_all()` | `bch2_journal_flush` | journal.c:1255 | ✅ | 委托 bch2_journal_flush，等价 bcachefs bch2_journal_flush |

### Part 6g：Read / Utilization / Bucket mgmt

| 行号 | subvol 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| 1726 | `fn utilization()` | — | — | ➖ | subvolmount 特有统计 |
| ~3838 | `fn journal_read_bucket()` | `journal_read_bucket` | read.c:331 | ⚠️ | 全桶读取、early validate 顺序、checksum 失败后按 block 继续、单设备 IO 错降级及共享 list 的 bad/good 副本择优已恢复；fsck policy 仍缺 |
| ~4070 | `fn bch2_journal_read_device()` | `bch2_journal_read_device` | read.c:724 | ⚠️ | `nr > 32` 搜索/空日志全读/降序读取/`last_seq` 早停、per-device head/free/index 与 `full_read` bypass 已恢复；options gate 仍缺 |
| ~4120 | `fn bch2_journal_read_reverse()` | — | — | ➖ | subvol R6 扩展，本地 read.c 无同名/同签名函数 |
| ~4170 | `fn bch2_journal_read(&mut JournalStartInfo)` | `bch2_journal_read` | read.c:1156 | ⚠️ | API/start-info、RW/RO READ ref、shared list、NO_FLUSH/torn、replay 边界、clean、union retry 与 rewind control-entry 恢复已完成；返回 Vec 是 Rust 表示差异；device/options gate、drop/blacklist、最终 missing fsck policy 仍缺 |
| 1937 | `fn update_bucket_seq()` | `ja->bucket_seq[cur_idx] = max(...)` | write.c:54,103,574 | ✅ | max() 确保多 entry 同 bucket 时取最高 seq |
| 1946 | `fn advance_dirty_idx()` | `bch2_journal_space_available` dirty_idx 推进 | reclaim.c:262,293-295 | ✅ | 使用回收完成后的 last_seq_ondisk 边界，避免过早回收 bucket |
| 1972 | `fn advance_dirty_idx_ondisk()` | `bch2_journal_update_last_seq_ondisk` | reclaim.c:453,297-299 | ✅ | 使用 last_seq_ondisk，对齐 |

### Part 6h：Reclaim + Slowpath

| 行号 | subvol 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| 1918 | `fn journal_seq_to_flush()` | `journal_seq_to_flush` | reclaim.c:861 | ✅ | 对齐 |
| 1933 | `fn journal_reclaim_needed()` | `__bch2_journal_reclaim` 触发条件 | reclaim.c:1047 | ✅ | 接受 `ReclaimParams`（含 `kicked`/btree dirty/key cache 外部参数） |
| 1967 | `fn __bch2_journal_reclaim()` | `__bch2_journal_reclaim` | reclaim.c:1047 | ✅ | 对齐（async 简化版，`kicked` 通过 `ReclaimParams` 传入） |
| 2056 | `fn bch2_journal_reclaim()` | `bch2_journal_reclaim` | reclaim.c:1184 | ✅ | 前台入口 |
| 2074 | `fn bch2_journal_flush_pins()` | `bch2_journal_flush_pins` | reclaim.c:1399 | ✅ | 对齐 |
| 2087 | `fn bch2_journal_rotate_or_reclaim()` | `bch2_journal_rotate_or_reclaim` | — | ✅ | 对齐 |
| 2131 | `fn bch2_journal_seq_blacklist_add()` | `bch2_journal_seq_blacklist_add` | seq_blacklist.c:49 | ⚠️ | 当前 Rust 写入 Blacklist Jset 并 flush；本地基准合并并持久化 superblock blacklist，语义未对齐 |
| 2182 | `fn bch2_journal_space_available()` | `bch2_journal_space_available` | reclaim.c:262 | ✅ | 对齐 |
| 2260 | `fn journal_has_flush_waiters()` | `journal_has_flush_waiters` | journal.c:581 | ✅ | flush waiter 判定拆分后仍与 bcachefs 一致 |
| 2275 | `fn journal_should_cycle_for_flush_waiters()` | `journal_should_cycle_for_flush_waiters` | journal.c:588 | ✅ | open/closed 分支与 in-flight 阈值对齐 |
| 2283 | `fn journal_should_open()` | `journal_should_open` | journal.c:611 | ✅ | MUST_OPEN 优先，其次 flush waiter |
| 2287 | `fn journal_cycle_locked()` | `bch2_journal_cycle_locked` | journal.c:636 | ✅ | R2 增强后循环体与 bcachefs 一致；flush_wait 差异已登记架构差异 |
| 2232 | `fn journal_res_get_slowpath()` | `bch2_journal_res_get_slowpath` | journal.c:958 | ✅ | 三级 fallback；新增 `journal_res_get_nonblocking()` 复刻 `JOURNAL_RES_GET_NONBLOCK` 语义 |
| 2233 | `fn journal_res_get_nonblocking()` | `JOURNAL_RES_GET_NONBLOCK` 分支 | journal.h:446-536 / journal.c:958-982 | ✅ | fastpath 成功即返回；slowpath 只做一次 cycle + recheck，不阻塞等待 |
| 2384 | `fn journal_res_get()` | `bch2_journal_res_get` | journal.h:521 | ✅ | fast→slow 路径，结构一致 |
| 2309 | `fn set_auto_flush_interval()` | — | — | ➖ | subvolmount 特有 |
| 2314 | `fn auto_flush_interval()` | — | — | ➖ | subvolmount 特有 |
| 2328 | `fn spawn_auto_flush_task()` | — | — | ➖ | subvolmount 特有（tokio） |
| 2469 | `fn spawn_background_reclaim_task()` | `bch2_journal_reclaim_thread` | reclaim.c:1216 | ✅ | tokio::spawn 版 kthread，核心逻辑一致 |

### Part 6i：外部触发参数

| 行号 | subvolmount 类型 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| 906 | `struct ReclaimParams` | `kicked` 参数 | reclaim.c:682 | ✅ | 封装 `kicked` + btree dirty pct + key cache flush cnt |
| 2795 | `fn start_background_reclaim()` / `fn stop_background_reclaim()` / `fn stop_auto_flush()` | `bch2_journal_reclaim_thread` / `kthread_stop()` | reclaim.c:763 / reclaim.c | ✅ | 任务循环接入取消标志，停止调用可正常收敛 |

### Part 6j：Round 3 新增函数 (journal.c 对齐)

| 行号 | subvol 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| ~880 | `pub err_seq: AtomicU64` | `journal->err_seq` | journal.c:667 | ✅ | R3 新增字段 |
| ~902 | `cur_entry_offset_if_blocked: AtomicU32` | `journal->cur_entry_offset_if_blocked` | journal.c:1371 | ✅ | R5 新增字段 |
| ~1014 | `pub struct JournalBlockGuard` | RAII guard | — | ✅ | R5: block → quiesce → drop → unblock |
| ~3288 | `bch2_journal_quiesced` | `journal_quiesced` | journal.c:692 | ✅ | R3 |
| ~3305 | `bch2_journal_quiesce` | `bch2_journal_quiesce` | journal.c:703 | ✅ | R3 |
| ~3317 | `bch2_journal_shutdown_quiesced` | `journal_shutdown_quiesced` | journal.c:722 | ✅ | R3 |
| ~3339 | `bch2_journal_shutdown_quiesce` | `bch2_journal_shutdown_quiesce` | journal.c:737 | ✅ | R3 |
| ~3355 | `bch2_journal_halt_locked` | `bch2_journal_halt_locked` | journal.c:666 | ✅ | R3 |
| ~3369 | `bch2_journal_halt` | `bch2_journal_halt` | journal.c:686 | ✅ | R3 |
| ~3383 | `__bch2_journal_meta` | `__bch2_journal_meta` | journal.c:1316 | ✅ | R4 |
| ~3401 | `bch2_journal_meta` | `bch2_journal_meta` | journal.c:1330 | ✅ | R4 |
| ~3457 | `bch2_journal_flush_seq` | `bch2_journal_flush_seq` | journal.c:1207 | ✅ | R7: sync 等待 |
| ~3539 | `bch2_journal_flush_seq_async` | `bch2_journal_flush_seq_async` | journal.c:1157 | ✅ | R6: async 触发 |
| ~3566 | `bch2_journal_flush_async` | `bch2_journal_flush_async` | journal.c:1243 | ✅ | R6 |
| ~3597 | `__bch2_journal_block` | `__bch2_journal_block` | journal.c:1365 | ✅ | R5 |
| ~3643 | `bch2_journal_block` | `bch2_journal_block` | journal.c:1386 | ✅ | R5 |
| ~3665 | `bch2_journal_unblock` | `bch2_journal_unblock` | journal.c:1344 | ✅ | R5 |
| ~3694 | `bch2_journal_entry_res_resize` | `bch2_journal_entry_res_resize` | journal.c:988 | ✅ | R8 |
| ~3755 | `bch2_journal_noflush_seq` | `bch2_journal_noflush_seq` | journal.c:1265 | ✅ | R9 |
| ~3792 | `bch2_journal_advance_rewind_seq` | `bch2_journal_advance_rewind_seq` | journal.c:1288 | ✅ | R10 |
| ~3810 | `bch2_journal_add_rewind_range` | `bch2_journal_add_rewind_range` | journal.c:1294 | ✅ | R10 (简化版) |
| ~5720 | `bch2_journal_do_writes_locked` | `bch2_journal_do_writes_locked` | write.c:1087 | ⚠️ | oldest-unallocated/refcount/flush_picked/defer、flush/noflush counters、seq_write_started、auto-commit delayed-work 与 rewind-discard option 已恢复；Rust slowpath/completion lock 仍分离 |
| ~5780 | `bch2_journal_do_writes` | `bch2_journal_do_writes` | write.c:1164 | ⚠️ | 单次锁包装存在，Rust slowpath/completion lock 仍分离 |
| ~3884 | `bch2_journal_write_work` | `bch2_journal_write_work` | journal.c:748 | ✅ | R11 |

---

## reclaim.rs（38 个函数）

### 自由函数

| 行号 | subvol 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| 64 | `journal_pin_type()` | `journal_pin_type` | reclaim.c:564 | ✅ | 对齐 |
| 70 | `btree_level_pin_type()` | `journal_pin_type` 的 level 分类来源 | reclaim.c:564-577 | ✅ | leaf(level=0)→Btree0，level>=3→Btree3 |
| 72 | `usize_to_pin_type()` | — | — | ➖ | 内部转换 |
| 669 | `journal_pin_active()` | `journal_pin_active` | reclaim.h:67 | ✅ | 对齐 |

### impl Link（侵入式链表节点）

| 行号 | subvol 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| 101 | `fn new()` | `list_head` 初始化 | — | ➖ | Rust 侵入式链表 |
| 109 | `fn read_prev()` | `le_prev` 读取 | — | ➖ | 链表访问 |
| 114 | `fn read_next()` | `le_next` 读取 | — | ➖ | 链表访问 |
| 119 | `fn write_prev()` | 指针赋值 | — | ➖ | 链表操作 |
| 126 | `fn write_next()` | 指针赋值 | — | ➖ | 链表操作 |
| 133 | `fn remove()` | `list_del_init` | — | ✅ | 对齐 |
| 147 | `fn insert_after()` | `list_add` | — | ✅ | 对齐 |
| 160 | `fn append_to_tail()` | `list_add_tail` | — | ✅ | 对齐 |
| 173 | `fn is_linked()` | `!hlist_unhashed` | — | ➖ | 检查 |

### impl PinPtrExt + Iterator

| 行号 | subvol 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| 184 | `fn link()` | — | — | ➖ | Rust 安全抽象 |
| 201-289 | `impl LinkedListHead`（8 个方法） | `list_head` 操作 | — | ➖ | Rust 链表封装 |
| 281 | `impl Iterator for LinkedListIter` | — | — | ➖ | Rust 迭代器 |

### impl JournalEntryPin / JournalEntryPinList

| 行号 | subvol 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| 320 | `fn new()` | `journal_entry_pin` 初始化 | — | ➖ | 构造 |
| 329 | `fn is_active()` | `journal_pin_active` | reclaim.h:67 | ✅ | 对齐 |
| 390 | `fn new(count)` | `journal_pin_list_init` | reclaim.h:25 | ✅ | 对齐 |
| 412-431 | unflushed_ref/mut/flushed | `pin_list->unflushed[]/flushed` | — | ➖ | Rust 访问器 |

### impl PinListFifo

| 行号 | subvol 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| 455 | `fn new()` | — | — | ➖ | Rust VecDeque 包装 |
| 464-516 | `len/is_empty/is_full/front/push_back/pop_front` | — | — | ➖ | FIFO 基本操作 |
| 521 | `fn entry_for_seq()` | `journal_seq_pin` | reclaim.h:72 | ✅ | 固定容量 FIFO 模索引等价 bcachefs fifo_entry |
| 533 | `fn entry_for_seq_mut()` | `journal_seq_pin` (mut) | reclaim.h:72 | ✅ | 可变版 |
| 548-633 | 过渡兼容 API（retain/drainable_*/drain_front/find_rev_idx/iter_all） | — | — | ➖ | 过渡期 API，待删除 |

### impl Journal（reclaim 方法）

| 行号 | subvol 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| 688 | `fn bch2_journal_maybe_update_last_seq()` | `bch2_journal_maybe_update_last_seq` | reclaim.c:443 | ✅ | 对齐 |
| 705 | `fn pin_fifo_ref()` | — | — | ➖ | UnsafeCell 包装 |
| 710 | `fn journal_seq_pin()` | `journal_seq_pin` | reclaim.h:72 | ✅ | unwrap_or_else(panic) 等价 EBUG_ON |
| 717 | `fn maybe_seq_pin()` | `maybe_seq_pin` | reclaim.c:610 | ✅ | seq=0->None 等价 NULL |
| 737 | `fn journal_pin_drop_locked()` | `journal_pin_drop_locked` | reclaim.c:512 | ✅ | 对齐 |
| 782 | `fn bch2_journal_pin_drop()` | `bch2_journal_pin_drop` | reclaim.c:538 | ✅ | 对齐 |
| 820 | `fn journal_pin_set_locked()` | `bch2_journal_pin_set_locked` | reclaim.c:579 | ✅ | 对齐 |
| 856 | `fn bch2_journal_pin_set()` | `bch2_journal_pin_set` | reclaim.c:664 | ✅ | 对齐 |
| 910 | `fn bch2_journal_pin_copy()` | `bch2_journal_pin_copy` | reclaim.c:615 | ✅ | 对齐 |
| 967 | `fn bch2_journal_pin_add()` | `bch2_journal_pin_add` | reclaim.h:106 | ✅ | 对齐 |
| 985 | `fn bch2_journal_pin_update()` | `bch2_journal_pin_update` | reclaim.h:119 | ✅ | 对齐 |
| 1004 | `fn __bch2_journal_pin_put()` | `__bch2_journal_pin_put` | reclaim.h:93 | ✅ | 对齐 |
| 1023 | `fn journal_get_next_pin()` | `journal_get_next_pin` | reclaim.c:729 | ✅ | 对齐 |
| 1094 | `fn journal_flush_pins()` | `journal_flush_pins` | reclaim.c:774 | ✅ | 对齐（精简版） |
| 1147 | `fn bch2_journal_pin_flush()` | `bch2_journal_pin_flush` | reclaim.c:713 | ✅ | 对齐 |
| 1168 | `fn journal_reclaim_kick()` | `journal_reclaim_kick` | reclaim.h:10 | ✅ | 对齐 |

---

## replay.rs（17 个函数）

### impl JournalReplayer

| 行号 | subvol 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| 58 | `fn new()` | — | — | ➖ | Rust 构造 |
| 69 | `fn from_jsets()` | — | — | ➖ | 测试用构造 |
| 84 | `fn get_jsets()` | — | — | ➖ | 内部获取 |
| 95 | `fn replayed_seqs()` | — | — | ➖ | getter |
| 105 | `fn replay_from()` | — | — | ➖ | 便利接口 |
| 121 | `fn replay_all()` | — | — | ➖ | 便利接口 |
| 129 | `fn replay_all_to_vol()` | — | — | ➖ | 便利接口 |
| 144 | `fn replay_accounting_to_vol()` | `bch2_journal_replay` Phase 1 | read.c:1156 | ✅ | 两阶段重放第一阶段 |
| 172 | `fn replay_data_to_vol()` | `bch2_journal_replay` Phase 2 | read.c:1156 | ✅ | 两阶段重放第二阶段；保留 raw extent 的完整副本指针 |
| 199 | `fn apply_accounting_entries()` | —（recovery.c `bch2_journal_replay` 一阶段） | — | ➖ | subvolmount 层 replay wrapper，直接调 BtreeEngine |
| 227 | `fn apply_data_entries()` | —（recovery.c `bch2_journal_replay` 二阶段） | — | ➖ | subvolmount 层 replay wrapper |
| 263 | `fn apply_jset_to_engine()` | — | — | ➖ | 旧接口，保留兼容 |
| 280 | `fn read_btree_roots()` | — | — | ➖ | subvolmount 特有：replay 前需先提取 roots |
| 304 | `fn parse_jset()` | — | — | ➖ | 内部数据转换到 ReplayedEntry |

---

## validate.rs（8 个函数）

### 自由函数

| 行号 | subvol 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| 39 | `jset_validate()` | `bch2_jset_validate` | validate.c:694 | ✅ | 完整 Jset 校验（version/csum/seq/entry 循环） |
| 73 | `journal_entry_validate()` | `bch2_journal_entry_validate` | validate.c:639 | ✅ | 逐 entry dispatch |
| 87 | `btree_keys_validate()` | `journal_entry_btree_keys_validate` | validate.c:115 | ✅ | btree_keys 可反序列化 |
| 95 | `btree_root_validate()` | `journal_entry_btree_root_validate` | validate.c:168 | ✅ | 恰好 1 个 BtreeEntry |
| 107 | `blacklist_validate()` | `journal_entry_blacklist_validate` | validate.c:225 | ✅ | start_seq < end_seq |
| 118 | `overwrite_validate()` | `journal_entry_overwrite_validate` | validate.c:483 | ✅ | payload 非空 |
| 124 | `btree_node_rewrite_validate()` | `journal_entry_write_buffer_keys_validate` (适配) | validate.c:517 | ✅ | payload 非空 |

## jset.rs（9 个函数）

### impl Jset

| 行号 | subvol 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| 176 | `fn new()` | — | — | ➖ | Rust struct 构造，bcachefs 中 jset 在预分配 buffer 中隐式创建 |
| 191 | `fn new_volatile()` | — | — | ➖ | subvolmount 特有魔数 |
| 210 | `fn verify()` | `bch2_jset_validate_early` | validate.c:748 | ➖ | 仅 CRC32C 校验；逐 entry type validate 链已在 validate.rs 中实现 |
| 242 | `fn serialize_padded()` | `bch2_journal_write_checksum` + padding | write.c:736 | ➖ | CRC 计算等价，padding 为 Rust 序列化对齐（非 bcachefs 直接对应） |
| 265 | `fn deserialize()` | — | — | ➖ | bincode 反序列化，bcachefs 用 struct 指针直接读 buffer |

### impl Crc32CHasher

| 行号 | subvol 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| 135 | `fn new()` | — | — | ➖ | Rust 哈希包装 |
| 142 | `fn update()` | `crc32c_le_bch` | — | ➖ | 增量计算 |
| 147 | `fn finalize()` | — | — | ➖ | 完成计算 |
| 152 | `fn hash()` | `crc32c_le_bch` | — | ➖ | 单次计算 |

---

## bcachefs 独有函数（subvol 无直接对应）

### validate.c — 全量 entry type validate 链（Phase 3 完成部分覆盖）

subvolmount 新增 `validate.rs`（对应 bcachefs validate.c），实现了核心校验函数：

| bcachefs 函数 | 行号 | subvol 对应 | 状态 | 说明 |
|--------------|------|--------------|------|------|
| `bch2_jset_validate_early` | validate.c:748 | `Jset::verify()` | ✅ | Phase 1 已对齐（CRC + magic） |
| `bch2_jset_validate` | validate.c:694 | `jset_validate()` | ✅ | Phase 3 新增（完整校验） |
| `jset_validate_entries` | validate.c:662 | `jset_validate()` 内联循环 | ✅ | Phase 3 新增 |
| `bch2_journal_entry_validate` | validate.c:639 | `journal_entry_validate()` | ✅ | Phase 3 新增 dispatch |
| `journal_entry_btree_keys_validate` | validate.c:115 | `btree_keys_validate()` | ✅ | Phase 3 新增 |
| `journal_entry_btree_root_validate` | validate.c:168 | `btree_root_validate()` | ✅ | Phase 3 新增 |
| `journal_entry_blacklist_validate` | validate.c:225 | `blacklist_validate()` | ✅ | Phase 3 新增 |
| `journal_entry_overwrite_validate` | validate.c:483 | `overwrite_validate()` | ✅ | Phase 3 新增 |

余下 ~32 个 validate.c 函数（to_text 链 + 不适用于 subvol 的 entry types）仍无对应：

| bcachefs 函数 | 行号 | 说明 |
|--------------|------|------|
| `journal_entry_prio_ptrs_validate` / `_to_text` | validate.c:210/220 | 优先级 ptrs — 无 allocator replicas |
| `journal_entry_usage_validate` | validate.c:294 | usage entry — 无计数统计 |
| `journal_entry_data_usage_validate` | validate.c:328 | data usage — 无统计 |
| `journal_entry_clock_validate` | validate.c:370 | clock entry — 无时钟同步 |
| `journal_entry_dev_usage_validate` | validate.c:410 | dev usage — 无统计 |
| `journal_entry_log_validate` | validate.c:466 | log entry — 无日志系统 |
| `journal_entry_write_buffer_keys_validate` | validate.c:517 | write_buffer_keys — 无 write buffer |
| `journal_entry_datetime_validate` | validate.c:533 | datetime entry — 无写时 datetime |
| `journal_entry_rewind_limit_validate` | validate.c:564 | rewind limit — 未实现 rewind |
| `journal_entry_rewind_validate` | validate.c:593 | rewind entry — 未实现 rewind |
| `bch2_journal_entry_to_text` | validate.c:651 | to_text 格式化 |
| 另含 ~17 个 `static` helpers | validate.c | 内部辅助 |

**总计：~32 个函数无 subvol 对应（不适用）—— 不再计划对齐**

| bcachefs 函数 | 行号 | 说明 |
|--------------|------|------|
| `journal_entry_btree_keys_validate` | validate.c:115 | 验证 btree_keys entry |
| `journal_entry_btree_keys_to_text` | validate.c:139 | 格式化输出 |
| `journal_entry_btree_root_validate` | validate.c:168 | 验证 btree_root entry |
| `journal_entry_btree_root_to_text` | validate.c:204 | 格式化输出 |
| `journal_entry_prio_ptrs_validate` | validate.c:210 | 验证 priority ptrs |
| `journal_entry_prio_ptrs_to_text` | validate.c:220 | 格式化输出 |
| `journal_entry_blacklist_validate` | validate.c:225 | 验证 blacklist entry |
| `journal_entry_blacklist_to_text` | validate.c:243 | 格式化输出 |
| `journal_entry_usage_validate` | validate.c:294 | 验证 usage entry |
| `journal_entry_data_usage_validate` | validate.c:328 | 验证 data_usage entry |
| `journal_entry_clock_validate` | validate.c:370 | 验证 clock entry |
| `journal_entry_dev_usage_validate` | validate.c:410 | 验证 dev_usage entry |
| `journal_entry_log_validate` | validate.c:466 | 验证 log entry |
| `journal_entry_overwrite_validate` | validate.c:483 | 验证 overwrite entry |
| `journal_entry_write_buffer_keys_validate` | validate.c:517 | 验证 write_buffer_keys |
| `journal_entry_datetime_validate` | validate.c:533 | 验证 datetime entry |
| `journal_entry_rewind_limit_validate` | validate.c:564 | 验证 rewind_limit |
| `journal_entry_rewind_validate` | validate.c:593 | 验证 rewind entry |
| `bch2_journal_entry_validate` | validate.c:639 | 全量调度的入口 |
| `bch2_journal_entry_to_text` | validate.c:651 | 通用 to_text 入口 |
| `bch2_jset_validate` | validate.c:694 | jset 整体验证 |
| `bch2_jset_validate_early` | validate.c:748 | 早期验证（CRC + magic） |
| 另加 ~17 个 `static` helpers | validate.c | 内部辅助 |

**总计：41 个函数。subvol 中无对应。——→ 计划 Phase 3 处理**

### init.c — 生命周期函数

| bcachefs 函数 | 行号 | 说明 |
|--------------|------|------|
| `bch2_set_nr_journal_buckets` | init.c:188 | 设置 journal 桶数 |
| `bch2_dev_journal_bucket_delete` | init.c:201 | 删除 journal 桶 |
| `bch2_journal_pin_fifo_resize` | init.c:344 | 调整 pin fifo |
| `bch2_dev_journal_stop` | init.c:432 | 停止设备 journal |
| `bch2_fs_journal_stop` | init.c:438 | 停止 FS journal |
| `bch2_fs_journal_start` | init.c:487 | ❌ 尚未实现；本地按 `cur_seq-last_seq` 动态扩容 pin FIFO（至少 `JOURNAL_PIN=32768`）并初始化 replay/ondisk/flushed/rewind/pin/replica 状态，Rust 当前固定 128 槽，禁止只设置 `seq` 的局部移植 |
| `bch2_journal_set_replay_done` | init.c:619 | 标记重放完成 |
| `bch2_dev_journal_exit` | init.c:635 | 设备 journal 退出 |
| `bch2_fs_journal_exit` | init.c:708 | FS journal 退出 |
| `bch2_fs_journal_init_early/rw/init` | init.c:738/758/802 | 初始化三阶段 |

`bch2_set_nr_journal_buckets_iter/loop`、`bch2_dev_journal_alloc` 与
`bch2_fs_journal_alloc` 已映射到 `journal/init.rs`；runtime resize/delete 仍未实现。

### write.c — 写入与 completion 链（重新审计中）

本地 `write.c` 有 21 个函数。现有 Rust 将其聚合进少量 async 函数，改变了多设备 allocation、waitlist、flush selection、逐 entry completion 和错误清理顺序；按本项目强制约束，这些不能标为架构不适用。

| bcachefs 函数 | 行号 | subvolmount 映射 | 状态 | 说明 |
|--------------|------|--------------|------|------|
| `journal_advance_devs_to_next_bucket` | write.c:29 | `journal_advance_devs_to_next_bucket` | ✅ | 逐设备 sectors/bucket-available 条件、cur_idx 推进、sectors_free 与 bucket_seq 更新顺序一致 |
| `__journal_write_alloc` | write.c:59 | `__journal_write_alloc` | ⚠️ | WRITE io_ref、重复设备/空间 gate、ptr/cas、sectors/bucket_seq/durability 与 break 顺序已恢复；stripe free-space 输入及 enumerated ref tag 仍有表示差异 |
| `journal_write_alloc` | write.c:112 | `journal_write_alloc` | ⚠️ | opts、target→all fallback、advance-once 与 degraded success 已恢复；disk-group target 目前只覆盖直接成员，尚缺父组传递 |
| `journal_buf_realloc` | write.c:161 | `journal_buf_realloc` | ✅ | `buf_size_want` 读取、write-buffer resize、journal buffer allocation/copy、`lock` 内 swap 与解锁后释放旧 buffer 顺序一致；外层 `buf_lock` 覆盖 realloc+prep |
| `replicas_refs_put` | write.c:191 | `replicas_refs_put` | ✅ | 遍历预分配 refs、逐 entry `put_many(nr_refs)`，最后清空 refs；相同 replica key 在 ondisk 推进阶段合并计数 |
| `last_uncompleted_write_seq` | write.c:224 | `last_uncompleted_write_seq` | ✅ | FIFO front 与 `write_done || seq == seq_completing` 判定一致 |
| `journal_write_done` | write.c:234 | `journal_write_done` | ⚠️ | replica 修正、err_seq、ondisk 边界、refs put、FIFO 前推和 waiter 唤醒顺序已恢复；Rust callback 存储与 lock-drop 表示仍有差异 |
| `journal_write_done_flush` | write.c:468 | `journal_write_done_flush` | ✅ | 无设备错误时 xchg 为 empty 并早唤醒真实 flush waiters，不提前关闭新 waiter |
| `journal_write_endio` | write.c:490 | `journal_write_endio` | ⚠️ | 逐设备错误与 `devs_written` drop 已恢复；通用 bio 层额外持有 per-block WRITE ref，未完全等价于 allocated ref 转交 |
| `journal_write_submit` | write.c:513 | `journal_write_submit` | ⚠️ | extent 多设备并发、PREFLUSH/FUA 选择与完成 continuation 已恢复；大 buffer 仍是 per-block request，未表达单设备 chained bio endio |
| `journal_write_preflush` | write.c:585 | `journal_write_preflush` | ✅ | separate flush 对所有且仅 RW member 并发零数据 PREFLUSH，全部完成后转 data submit |
| `bch2_journal_write_prep` | write.c:621 | `bch2_journal_write_prep` | ⚠️ | 空 entry、root、WB、datetime/common、overrun 顺序已恢复；WB start/end 的跨插入锁区仍缺失。subvol 使用 `Jset::deserialize` 逐块解析，而 bcachefs 用 `vstruct_for_each` 逐 entry 遍历。当预留但未写入的 gap 跨越 JSET_BLOCK_SIZE 边界时，subvol 需要显式跳过全零块（见 2026-07-19 修复） |
| `bch2_journal_write_checksum` | write.c:736 | `bch2_journal_write_checksum` | ⚠️ | magic/version/flags、CRC32C、后置 validate、padding 已恢复；encryption 分支尚未表达 |
| `bch2_journal_write` | write.c:819 | `bch2_journal_write` | ⚠️ | prep→alloc→checksum→publish→`entry_bytes_written`→replicas→no_io/preflush/submit→done 与 error cleanup 已恢复；后续 noflush worker 仍由 async 递归串行 drain |
| `journal_waitlist_add_batch` | write.c:948 | `journal_waitlist_add_batch` | ✅ | batch 转移与 noflush/flush-no-wait sentinel gate 已恢复 |
| `journal_waitlist_splice` | write.c:964 | `journal_waitlist_splice` | ✅ | waiter 级联及目标拒绝时原链还原已恢复 |
| `flush_would_free_space` | write.c:983 | `flush_would_free_space` | ✅ | 遍历 journal RW mask，逐设备检查 dirty index 与 bucket seq |
| `__should_flush` | write.c:999 | `__should_flush` | ✅ | error/first-flush/must-not-flush/reclaim/waiter demotion/must-flush/timeout 顺序一致；生产路径从 `c.opts.journal_flush_delay` 读取持久化配置，delay=0 standalone 测试仍立即 flush |
| `should_flush` | write.c:1079 | `should_flush` | ✅ | `__should_flush` 后仅在 false 时尝试 noflush，失败即升级 flush |
| `bch2_journal_do_writes_locked` | write.c:1087 | `bch2_journal_do_writes_locked` | ⚠️ | oldest-unallocated/refcount/flush_picked/flush ordering gate、flush/noflush counters、seq_write_started、auto-commit delayed-work 重置/取消与 rewind-discard 条件已恢复；Rust slowpath/completion lock 仍分离 |
| `bch2_journal_do_writes` | write.c:1164 | `bch2_journal_do_writes` | ⚠️ | 锁包装与单次调用存在，但 Rust slowpath lock 与 completion lock 仍分离 |

**小计：11 ✅ / 10 ⚠️。全部 21 个函数已按当前本地源码复核。**

### sb.c — Superblock 交互

| bcachefs 函数 | 行号 | 说明 |
|--------------|------|------|
| `bch2_journal_buckets_to_sb` | sb.c:176 | journal 桶→sb 序列化 |
| `bch2_sb_journal_sort` | sb.c:227 | sb 桶排序 |
| 另含 5 个 static 验证函数 | sb.c:22-171 | sb 字段验证 |

### seq_blacklist.c — 黑名单辅助

全部 7 个函数已完成（2026-07-04）：

| bcachefs 函数 | 行号 | 说明 | 状态 | subvolmount 位置 |
|--------------|------|------|------|--------------|
| `bch2_journal_seq_blacklist_add` | seq_blacklist.c:49 | superblock blacklist 合并/持久化 | ⚠️ | types.rs 当前仍写 Blacklist Jset；这是启用 read `drop_before` 的阻塞项 |
| `bch2_journal_seq_next_blacklisted` | seq_blacklist.c:114 | 查找下一个黑名单 | ✅ | jset.rs:BlacklistTable::next_blacklisted |
| `bch2_journal_seq_next_nonblacklisted` | seq_blacklist.c:132 | 查找下一个非黑名单 | ✅ | jset.rs:BlacklistTable::next_nonblacklisted |
| `bch2_journal_seq_is_blacklisted` | seq_blacklist.c:152 | 检查是否被黑名单 | ✅ | jset.rs:BlacklistTable::is_blacklisted |
| `bch2_journal_last_blacklisted_seq` | seq_blacklist.c:179 | 获取最后一个黑名单 | ✅ | jset.rs:BlacklistTable::last_blacklisted_seq |
| `bch2_blacklist_table_initialize` | seq_blacklist.c:189 | 初始化黑名单表 | ✅ | jset.rs:BlacklistTable::from_entries |
| `bch2_blacklist_entries_gc` | seq_blacklist.c:276 | 黑名单 GC | ✅ | jset.rs:BlacklistTable::gc |

### read.c — 设备扫描/搜索

| bcachefs 函数 | 行号 | 状态 | 说明 |
|--------------|------|------|------|
| `journal_read_bucket` | read.c:331 | ⚠️ | 全桶读、校验后续扫描、读 IO 降级及 bad/good 副本元数据/择优已恢复；fsck policy 未恢复 |
| `journal_peek_bucket` | read.c:473 | ✅ | 只读首 block，magic/version 可解析则取 seq，IO 错按空桶处理 |
| `journal_peek_once` | read.c:499 | ✅ | 每桶最多 peek 一次 |
| `journal_anchor_bucket` | read.c:523 | ✅ | bucket 0 后按最大 2 的幂 stride 逐层减半 |
| `journal_bsearch_head` | read.c:561 | ✅ | 以 anchor 为原点的环形二分和 high bias |
| `journal_walk_inuse` | read.c:609 | ✅ | 从 head 反向遍历，seq 必须严格递减，遇空停止 |
| `journal_bsearch_collect` | read.c:663 | ✅ | fast walk 失败时清空 order 并 peek 全桶 rebuild |
| `bch2_journal_read_device` | read.c:724 | ⚠️ | `nr > 32` fast path、full-read fallback/bypass 和 per-device 读后状态已恢复；options gate 仍缺 |
| `bch2_journal_entry_missing_range` | read.c:917 | ✅ | `start<=end`、跳过当前 blacklist、下一 blacklist 截断及空区间归零顺序一致 |
| `journal_retry_full_read` | read.c:973 | ✅ | union 序列缺口后仅对 `nr > 32` RW/RO member 重新取 READ ref 并强制全读 |
| `bch2_journal_reread_for_rewind` | read.c:1067 | ❌ | rewind 按 `need_from` 重读未实现 |
| `bch2_journal_read` | read.c:1156 | ⚠️ | API/start-info、RW/RO READ-ref、shared union、NO_FLUSH/torn、replay 边界、clean、missing/full-read retry 与 RewindLimit/Rewind 恢复已接入；device/options gate、drop/blacklist、最终 missing fsck policy 未完成 |

---

## 阶段计划

| 阶段 | 聚焦 | ❓ 变化 |
|------|------|--------|
| Phase 1 | 覆盖地图 + 注释修复 + 首次小修复 | ❓ 47 → 47（基线） |
| Phase 2 | 写路径深对齐（write.c） — **重新实施中** | 21 个当前偏差函数纳入 `07-15-journal-write-alignment` |
| Phase 3 | 校验路径对齐（validate.c） — **已完成** | ❓ 47 → 38（+9 个新对齐函数） |
| Phase 4 | 初始化/生命周期对齐（init.c） — **已完成** | ❓ 38 → ~26 |
| Phase 5 | Superblock 交互对齐（sb.c） — **已完成** | ❓ ~26 → ~24 |
| Phase 6 | ❓ 全量验证 — **已完成** | ❓ ~24 → 0 ✅ |

✅ **❓ 全部清除。所有 128 个函数已验证完毕。**

---

## 更新日志

| 日期 | 变更 | 原因 |
|------|------|------|
| 2026-06-30 | 初始创建 | Phase 1 基线 |
| 2026-06-30 | 写路径注释修正 + validate.rs 创建 | Phase 2（write.c 注释修正）+ Phase 3（validate.c 校验链）：新增 validate.rs（8 函数），接入读路径；coverage +9 ✅，❓ 47→38 |
| 2026-07-01 | write.c closure 链映射回补 + Part 6f 行号修正 | write.c 16 个函数全部映射：5 ✅ / 11 ➖；行号调整（Phase 4 漂移）；Part 6f 行号 1500→1568 等 4 处修正 |
| 2026-07-01 | ❓ 全量清除 | 全部 ~33 ❓ 验证完成：28 ✅ / ⚠️ 5 / ➖ 5 → ❓ 0 ✅ |
| 2026-07-05 | ⚠️ 简化项修复 | `journal_entry_open` 前置检查（blocked/cur_entry_error/seq/blacklist/in_flight）+ `journal_res_put` 立即 pin_put + `find_free_buf` count 检查。⚠️ 2→0, ✅ 81→83 |
| 2026-07-11 | btree 多设备对齐 | 移除 `Btree`/`BtreeNode.set_test_backend()`。移除 `BtreeWriter._backend` 死字段。`BtreePtrV2.dev_idx` 从硬编码 0 改为 `Btree.dev_idx()`。`Btree` 新增 `dev_idx()` 方法。|
| 2026-07-16 | 多副本 journal replay | `replay_data_to_vol()` 回放 `KeyValue::Raw`，新增双设备 extent pointer 保真回归测试。|
| 2026-07-17 | preflush WRITE ref 生命周期 | `journal_write_preflush()` 只提交 RW 成员；底层 `submit_bio_write()` 在 PREFLUSH 提交前获取 WRITE ref，并在 completion 后释放，对应本地 `journal_write_preflush()`（`fs/journal/write.c:585-617`）的 ref 边界，避免只读切换竞态。|
| 2026-07-17 | journal init capacity reservation | `bch2_set_nr_journal_buckets_loop()` 的生产获取/释放改用 `BchVol` capacity-backed `bch2_disk_reservation_get/put`，不再把 journal 初始化空间记账到旧 `ReservationTracker`。|
| 2026-07-17 | journal init transaction accounting | 每个新 journal bucket 独立获取 reservation；metadata mark 成功后由 `BtreeTrans` 按 `bch2_trans_account_disk_usage_change()` 消费，失败立即 put，保持本地 `init.c:19-142` 的 transaction 生命周期。|
| 2026-07-17 | journal init allocation failure cleanup | reservation 在 bucket allocation 失败分支也立即 put，对齐本地 `CLASS(disk_reservation, ...)` 的析构释放顺序。|
| 2026-07-17 | transaction-trigger journal ordering | 异步事务先完成 transactional/atomic trigger pipeline，再按完整 journal（含 alloc/backpointer 派生条目）构造单个 jset，避免派生更新落在已构造 payload 之外。|
### 2026-07-17 API 可见性复核

- 本地 `fs/journal/journal.h` 中 `journal_buf_try_noflush()`、`journal_res_get_fast()` 为静态内联实现；subvol 对应 wrapper 已限制为 crate 内部。
- 错误状态、当前序列、原始 entry 写入、唤醒和 flush 标志 helper 在本地没有同名 `bch2_*` 导出，已限制为 crate 内部，调用顺序未改变。
