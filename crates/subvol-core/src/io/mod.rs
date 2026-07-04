//! Async IO 回调层 — bcachefs 对齐
//!
//! 提供 bcachefs 风格的异步 IO 回调原语：
//! - `Closure` — 引用计数异步完成跟踪（对齐 `struct closure`）
//! - `BioRequest` — IO 请求（对齐 `struct bio` + `bi_end_io`）
//! - `submit_bio_read` / `submit_bio_write` — 提交 IO 请求（对齐 `submit_bio`）
//!
//! # bcachefs 异步 IO 模型
//!
//! bcachefs 的写入管线完全基于回调驱动，函数**不等待** IO：
//!
//! ```c
//! CLOSURE_CALLBACK(bch2_write) {
//!     // 对每个副本：
//!     closure_get(&op->cl);              // 引用计数 +1
//!     bio->bi_end_io = bch2_write_endio; // 注册完成回调
//!     submit_bio(bio);                   // 提交 IO，立即返回
//!
//!     // 所有副本提交后，注册下一步：
//!     continue_at(&op->cl, bch2_write_index, wq);
//!     // ^^ "返回"当前函数，引用归零时执行 bch2_write_index
//! }
//!
//! void bch2_write_endio(struct bio *bio) {
//!     closure_put(&op->cl);  // 一个副本完成 → 引用计数 -1
//! }
//!
//! CLOSURE_CALLBACK(bch2_write_index) {
//!     // 全部副本完成，处理结果
//!     continue_at(&op->cl, bch2_write_done, wq);
//! }
//! ```
//!
//! 关键点：没有函数阻塞等待 IO —— 每个函数提交 IO 后立即 `continue_at` 返回，
//! 下一个阶段由 closure 引用归零时自动调度。这就是"异步回调"的核心。

use crate::block_device::{BchDev, BchDevIoRefKind};
use crate::btree::Bpos;
use crate::btree::key::KeyValue;
use crate::types::{AtomicCell, AtomicFirstError, BlockAddr, StorageError};
use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

/// IO 操作结果
pub type IoResult = std::result::Result<(), StorageError>;

// ═══════════════════════════════════════════════════════════════
// Closure — 对齐 bcachefs `struct closure`
// ═══════════════════════════════════════════════════════════════

/// 引用计数异步完成跟踪 — 对齐 bcachefs `struct closure`
///
/// bcachefs closure（closure.h:144-169）：
/// ```c
/// struct closure {
///     struct workqueue_struct *wq;       // 回调执行的工作队列
///     struct task_struct      *sleeper;  // 同步等待的线程
///     closure_fn              *fn;       // 引用归零时的回调
///     struct closure          *parent;   // 父 closure
///     atomic_t                remaining; // 引用计数（低24位）
/// };
/// ```
///
/// # 安全契约
///
/// `callback` 使用 `UnsafeCell` 而非 `Mutex`，对齐 bcachefs 中的裸 `fn` 指针。
/// bcachefs 中 `continue_at` 总是在最后一个 `closure_put` 之前设置回调，
/// 不存在竞态。subvol 严格遵循同一模式：`continue_at` 后紧跟 `put()`。
///
/// # 用法（与 bcachefs 一一对应）
///
/// | Rust | C |
/// |------|---|
/// | `Closure::new()` | `closure_init(cl, NULL)` |
/// | `cl.get()` | `closure_get(cl)` |
/// | `cl.put()` | `closure_put(cl)` |
/// | `cl.continue_at(cb)` | `continue_at(cl, fn, wq)` |
/// | `cl.set_parent(parent)` | `closure_init(cl, parent)` |
/// | `cl.sync()` | `closure_sync(cl)` |
pub struct Closure {
    remaining: AtomicU32,
    callback: UnsafeCell<Option<Box<dyn FnOnce() + Send>>>,
    parent: Option<Arc<Closure>>,
}

// Sync: callback 的写入（continue_at）与读取（fire）通过 remaining
// 计数器的 happens-before 关系保护，与 bcachefs 的 closure 语义一致。
unsafe impl Sync for Closure {}

impl Closure {
    /// 创建新 closure，引用计数初始为 1。
    /// 对应 `closure_init(cl, NULL)`。
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            remaining: AtomicU32::new(1),
            callback: UnsafeCell::new(None),
            parent: None,
        })
    }

    /// 创建有父 closure 的新子 closure。
    /// 对应 `closure_init(cl, parent)`。
    /// 子 closure 完成时，自动 `put` 父 closure。
    pub fn new_child(parent: &Arc<Self>) -> Arc<Self> {
        parent.get();
        Arc::new(Self {
            remaining: AtomicU32::new(1),
            callback: UnsafeCell::new(None),
            parent: Some(parent.clone()),
        })
    }

    /// 增加引用计数。
    /// 对应 `closure_get(cl)`。
    /// 每次提交 IO 前调用，确保 closure 在 IO 完成前存活。
    #[inline]
    pub fn get(&self) {
        let prev = self.remaining.fetch_add(1, Ordering::Release);
        debug_assert!(prev > 0, "closure_get on zero-ref closure");
    }

    /// 减少引用计数。归零时触发回调。
    /// 对应 `closure_put(cl)`。
    /// 每次 IO 完成后在 endio 中调用。
    #[inline]
    pub fn put(&self) {
        let prev = self.remaining.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(prev > 0, "closure_put on zero-ref closure");
        if prev == 1 {
            self.fire();
        }
    }

    /// 注册完成回调。对应 `continue_at(cl, fn, wq)`。
    ///
    /// 引用归零时执行 `cb`。
    /// 如果引用已归零，立即执行。
    /// 必须在对 closure 调用最终的 `put()` 之前调用。
    pub fn continue_at(self: &Arc<Self>, cb: Box<dyn FnOnce() + Send>) {
        if self.remaining.load(Ordering::Acquire) == 0 {
            cb();
            return;
        }
        // bcachefs 约定：continue_at 在最后一个 put 之前设置回调，
        // 不存在回调尚未设置就被 fire 的竞态。
        // UnsafeCell 的写在此处安全：remaining>0 保证 fire 尚未被调用。
        unsafe { *self.callback.get() = Some(cb) };
    }

    /// 设置父 closure。
    pub fn set_parent(&self, parent: &Arc<Self>) {
        parent.get();
        // 注意：这里需要用内部可变性，但 self.parent 不是 Mutex 保护的
        // 所以在创建时通过 new_child 设置父 closure
        // 运行时不应修改 parent
    }

    /// 同步等待引用归零。对应 `closure_sync(cl)`。
    pub fn sync(&self) {
        while self.remaining.load(Ordering::Acquire) > 0 {
            std::thread::yield_now();
        }
    }

    /// 异步等待引用归零。
    /// 内部释放初始引用（`put`），等待回调触发。
    pub async fn wait_async(self: &Arc<Self>) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let cl = self.clone();
        self.continue_at(Box::new(move || {
            let _ = tx.send(());
        }));
        cl.put();
        let _ = rx.await;
    }

    /// 引用归零时触发回调链
    fn fire(&self) {
        let cb = unsafe { (*self.callback.get()).take() };
        if let Some(cb) = cb {
            cb();
        }
        if let Some(ref parent) = self.parent {
            parent.put();
        }
    }

    /// 完成当前 closure，跳过回调，仅 signal 父 closure。
    /// 对应 bcachefs `closure_return(cl)`。
    ///
    /// 与 `put()` 的区别：不执行 `continue_at` 注册的回调，
    /// 直接放行父 closure。用于子操作完成但不应触发当前 phase 回调的场景。
    #[inline]
    pub fn closure_return(&self) {
        unsafe { *self.callback.get() = None };
        self.put();
    }
}

/// bcachefs 对齐: `struct closure_waitlist` — 等待列表
///
/// 允许多个 closure 等待同一个条件（如 IO 完成、flush 完成）。
/// 对应 bcachefs `closure_wait(cl, w, fn)` / `closure_wake_up(w)` 模式。
///
/// # 用法
///
/// ```rust,ignore
/// use std::sync::Arc;
/// use subvol_core::io::{Closure, ClosureWaitlist};
///
/// let parent = Closure::new();
/// let wl = Arc::new(ClosureWaitlist::new());
///
/// // waiter 侧：
/// let cl = Closure::new_child(&parent);
/// cl.continue_at(Box::new(|| { /* 等待完成后的处理 */ }));
/// wl.wait(&cl);
/// cl.put();  // 释放初始引用，closure 在 wl 上等待唤醒
///
/// // 唤醒侧：
/// wl.wake_up();  // 对所有 waiter 调用 closure_put，触发它们的回调
/// ```
pub struct ClosureWaitlist {
    waiters: Mutex<Vec<Arc<Closure>>>,
}

impl ClosureWaitlist {
    pub fn new() -> Self {
        Self {
            waiters: Mutex::new(Vec::new()),
        }
    }

    /// 将 closure 加入等待列表。
    /// 对应 bcachefs `closure_wait(w, cl)`。
    ///
    /// 调用方应确保 `cl` 已被 `get()`（持有引用）。
    /// 当 `wake_up()` 被调用时，`closure_put` 会触发 `cl` 的回调链。
    pub fn wait(&self, cl: &Arc<Closure>) {
        self.waiters.lock().unwrap().push(cl.clone());
    }

    /// 唤醒所有等待的 closure。
    /// 对应 bcachefs `closure_wake_up(w)`。
    pub fn wake_up(&self) {
        let waiters = self.waiters.lock().unwrap().drain(..).collect::<Vec<_>>();
        for cl in waiters {
            cl.put();
        }
    }
}

// ═══════════════════════════════════════════════════════════════
// BioRequest — 对齐 bcachefs `struct bio`
// ═══════════════════════════════════════════════════════════════

/// IO 操作类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BioOp {
    Read,
    Write,
}

/// IO 请求 — 对齐 bcachefs `struct bio`
///
/// bcachefs bio（blk_types.h:115-146）：
/// ```c
/// struct bio {
///     struct bio           *bi_next;     // 请求队列链接
///     struct block_device  *bi_bdev;     // 块设备
///     blk_status_t         bi_status;    // 完成状态
///     bio_end_io_t         *bi_end_io;   // 完成回调（函数指针）
///     void                 *bi_private;  // 回调私有数据
///     struct bio_vec       *bi_io_vec;   // 数据向量
/// };
/// ```
///
/// bcachefs 扩展（`struct bch_write_bio` / `struct bch_read_bio`）：
/// - `struct bch_dev *ca` — 提交时 stashed 的设备指针（`write_types.h:53`）
/// - 完成回调中通过 `wbio->ca` / `rbio->ca` 取回设备引用
///
/// 此结构对应：
/// - `dev` ↔ `ca` — 目标设备
/// - `end_io` ↔ `bi_end_io` — 完成回调
/// - `private` ↔ `bi_private` — 私有数据
/// - `data` ↔ `bi_io_vec` — 数据连续存储（简化）
pub struct BioRequest {
    dev: Arc<BchDev>,
    addr: BlockAddr,
    data: Vec<u8>,
    op: BioOp,
    preflush: bool,
    fua: bool,
    end_io: Option<Box<dyn FnOnce(IoResult) + Send>>,
    private: Option<Box<dyn std::any::Any + Send>>,
}

impl BioRequest {
    /// 创建写请求 — 对齐 bcachefs 中设置 `bio->ca = ca` 后 submit
    pub(crate) fn write(dev: Arc<BchDev>, addr: BlockAddr, data: Vec<u8>) -> Self {
        Self {
            dev,
            addr,
            data,
            op: BioOp::Write,
            preflush: false,
            fua: false,
            end_io: None,
            private: None,
        }
    }

    /// 创建零数据 write/PREFLUSH bio；对应 journal_write_preflush() 中
    /// `bio_alloc_bioset(..., 0, REQ_OP_WRITE|REQ_PREFLUSH, ...)`。
    pub(crate) fn preflush(dev: Arc<BchDev>) -> Self {
        Self {
            dev,
            addr: BlockAddr::new(0),
            data: Vec::new(),
            op: BioOp::Write,
            preflush: true,
            fua: false,
            end_io: None,
            private: None,
        }
    }

    /// 创建读请求
    pub(crate) fn read(dev: Arc<BchDev>, addr: BlockAddr, buf: Vec<u8>) -> Self {
        Self {
            dev,
            addr,
            data: buf,
            op: BioOp::Read,
            preflush: false,
            fua: false,
            end_io: None,
            private: None,
        }
    }

    /// 设置完成回调 — 对应 `bio->bi_end_io = fn`
    pub(crate) fn set_end_io(mut self, cb: impl FnOnce(IoResult) + Send + 'static) -> Self {
        self.end_io = Some(Box::new(cb));
        self
    }

    /// 对应 write bio 的 `REQ_PREFLUSH`。
    pub(crate) fn set_preflush(mut self, preflush: bool) -> Self {
        self.preflush = preflush;
        self
    }

    /// 对应 write bio 的 `REQ_FUA`。
    pub(crate) fn set_fua(mut self, fua: bool) -> Self {
        self.fua = fua;
        self
    }

    /// 设置读取私有数据（用于将读取结果回传给调用方）
    /// 对应 bcachefs `rbio->bio.bi_private`。
    pub(crate) fn into_read_private(mut self, sink: Arc<AtomicCell<Vec<u8>>>) -> Self {
        self.private = Some(Box::new(sink));
        self
    }
}

// ═══════════════════════════════════════════════════════════════
// submit_bio — 对齐 bcachefs `submit_bio(bio)` → `generic_make_request(bio)`
// ═══════════════════════════════════════════════════════════════

/// 提交写 IO 请求 — 对齐 `submit_bio(bio)`（写操作）
///
/// bcachefs 中 `submit_bio` 调用链：
/// ```c
/// submit_bio(bio) → generic_make_request(bio) → fops->write()
///     → aio_write() → io_submit(ctx, 1, &iocb)  // AIO: 立即返回
///     → sync_write() → pwritev2()                // 同步回退
/// ```
/// 无论 AIO 还是同步回退，`submit_bio` 都遵循"提交-回调"契约。
///
/// `req.dev` 对齐 bcachefs `wbio->ca` / `rbio->ca`，
/// 在 bio 创建时由调用方设置，submit 时直接使用。
pub fn submit_bio_write(mut req: BioRequest) {
    if !req.dev.try_get_io_ref(BchDevIoRefKind::Write) {
        if let Some(cb) = req.end_io.take() {
            cb(Err(StorageError::NotFound("device offline".into())));
        }
        return;
    }

    let backend = req.dev.bdev().clone();
    let dev = req.dev.clone();
    tokio::spawn(async move {
        let result: IoResult = async {
            if req.preflush {
                backend.flush().await?;
            }
            if !req.data.is_empty() {
                backend.write_block(req.addr, &req.data).await?;
            }
            if req.fua {
                backend.flush().await?;
            }
            Ok(())
        }
        .await;
        dev.put_io_ref(BchDevIoRefKind::Write);
        if let Some(cb) = req.end_io.take() {
            cb(result.map_err(Into::into));
        }
    });
}

/// 提交读 IO 请求 — 对齐 `submit_bio(bio)`（读操作）
///
/// 读取完成后 data 会被填充至 `req.data`，并通过 `req.private` 中可选的
/// `Arc<AtomicCell<Vec<u8>>>` 回传给调用方。
/// `req.dev` 对齐 bcachefs `rbio->ca`。
pub fn submit_bio_read(mut req: BioRequest) {
    if !req.dev.try_get_io_ref(BchDevIoRefKind::Read) {
        if let Some(cb) = req.end_io.take() {
            cb(Err(StorageError::NotFound("device offline".into())));
        }
        return;
    }

    let backend = req.dev.bdev().clone();
    let dev = req.dev.clone();
    tokio::spawn(async move {
        let result = backend.read_block(req.addr, &mut req.data).await;
        let filled = req.data;
        dev.put_io_ref(BchDevIoRefKind::Read);
        if let Some(p) = req.private.take() {
            if let Ok(sink) = p.downcast::<Arc<AtomicCell<Vec<u8>>>>() {
                sink.store(filled);
            }
        }
        if let Some(cb) = req.end_io.take() {
            cb(result.map_err(Into::into));
        }
    });
}

/// 提交多块并发读请求 — 对齐 bcachefs extent 读取模式
///
/// 将 `[start_addr, start_addr + block_count)` 拆分为独立 `submit_bio_read`，
/// 全部完成后结果按顺序拼入 `result_cell`，然后自动通过 `completion` 的
/// `continue_at` 通知调用方。
///
/// 内部使用 `Closure::new_child(completion)` 做数据组装，因此调用方只需
/// 在 `completion` 上注册 `continue_at` 感知读取完成，并从 `result_cell` 取结果。
///
/// 数据收集使用 `AtomicCell` 而非 `Mutex`：每个 IO slot 有唯一写入者，
/// 数据在 block device 回调写入 → end_io 读取之间存在 happens-before。
pub(crate) fn submit_bio_all_blocks_read(
    dev: Arc<BchDev>,
    start_addr: BlockAddr,
    block_count: usize,
    completion: &Arc<Closure>,
    result_cell: Arc<AtomicCell<Vec<u8>>>,
    first_err: &Arc<AtomicFirstError>,
) {
    let block_size = 4096usize;
    let first_err = first_err.clone();

    if block_count == 0 {
        result_cell.store(Vec::new());
        completion.get();
        completion.put();
        return;
    }

    let cl_io = Closure::new_child(completion);
    let assembled: Arc<Vec<AtomicCell<Vec<u8>>>> =
        Arc::new((0..block_count).map(|_| AtomicCell::new()).collect());

    for i in 0..block_count {
        cl_io.get();
        let assembled = assembled.clone();
        let cl = cl_io.clone();
        let sink: Arc<AtomicCell<Vec<u8>>> = Arc::new(AtomicCell::new());
        let sink_private = sink.clone();
        let first_err = first_err.clone();
        let req = BioRequest {
            dev: dev.clone(),
            addr: BlockAddr::new(start_addr.raw + i as u64),
            data: vec![0u8; block_size],
            op: BioOp::Read,
            preflush: false,
            fua: false,
            end_io: Some(Box::new(move |r| {
                if let Err(e) = r {
                    first_err.set_first(e);
                }
                if let Some(buf) = sink.take() {
                    assembled[i].store(buf);
                }
                cl.put();
            })),
            private: Some(Box::new(sink_private)),
        };
        submit_bio_read(req);
    }

    let result = result_cell.clone();
    let n_blocks = block_count;
    cl_io.continue_at(Box::new(move || {
        let mut final_data = Vec::with_capacity(n_blocks * block_size);
        for chunk in assembled.iter() {
            if let Some(buf) = chunk.take() {
                final_data.extend_from_slice(&buf);
            }
        }
        result.store(final_data);
    }));
    cl_io.put();
}

/// 多副本写入 — 对齐 bcachefs `bch2_submit_wbio_replicas()`
///
/// 将相同数据写入到 `devs` 中的每个设备，使用 `Closure` 追踪所有副本的完成。
/// 对应 bcachefs 的数据流（write.c:1341-1478）：
///
/// ```c
/// // bcachefs 中对每个 ptr（设备指针）：
/// bkey_for_each_ptr(ptrs, ptr) {
///     if (ptr != last) {
///         n = bio_alloc_clone(NULL, &wbio->bio, ...);
///         n->parent = wbio;
///         bio_inc_remaining(&wbio->bio);  // closure_get 等效
///     } else {
///         n = wbio;  // 最后一个副本复用原始 bio
///     }
///     n->bio.bi_iter.bi_sector = ptr->offset;
///     submit_bio(&n->bio);
/// }
/// ```
///
/// subvol 中：
/// - 最后一个副本不需要 `completion.get()`（复用初始引用）
/// - 非最后一个副本各 `get()` 一次，与 bcachefs `bio_inc_remaining` 等效
/// - 所有副本的 `end_io` 中调用 `cl.put()`，对应 `bio_endio` → `closure_put`
///
/// `completion` 的初始引用由调用方持有，应在此函数返回前设置 `continue_at`。
pub(crate) fn submit_bio_write_replicas(
    devs: &[Arc<BchDev>],
    addr: BlockAddr,
    data: Vec<u8>,
    completion: &Arc<Closure>,
    first_err: &Arc<AtomicFirstError>,
) {
    let n = devs.len();
    debug_assert!(n > 0, "submit_bio_write_replicas: empty devs");

    let shared = Arc::new(data);

    for (i, dev) in devs.iter().enumerate() {
        // 非最后一个副本：增加引用，防止 completion 提前触发
        // 对应 bcachefs: bio_inc_remaining(&wbio->bio)
        if i < n - 1 {
            completion.get();
        }

        let dev = dev.clone();
        let cl = completion.clone();
        let err_cell = first_err.clone();
        let data = shared.as_ref().clone();

        submit_bio_write(
            BioRequest::write(dev, addr, data).set_end_io(move |result| {
                // 对应 bcachefs bio_endio: 记录错误并 closure_put
                if let Err(e) = result {
                    err_cell.set_first(e);
                }
                cl.put();
            }),
        );
    }
}

// ═══════════════════════════════════════════════════════════════
// submit_bio_read_replicas — 多设备尝试读取
// ═══════════════════════════════════════════════════════════════

/// 多设备尝试读取 — 对齐 bcachefs `__bch2_read_extent` 的备选设备重试
///
/// bcachefs 读路径（read.c:1426-1599）：
/// ```c
/// // 1. 从 extent key 的设备指针中选出最佳设备
/// bch2_bkey_pick_read_device(c, k, failed, &pick, dev, flags);
/// // 2. 提交 read bio 到所选设备
/// submit_bio(&rbio->bio);
/// // 3. 读失败时重试：rbio_mark_io_failure → bch2_rbio_retry
/// //    → 重新 lookup extent → pick_read_device（跳过失败设备）
/// ```
///
/// subvol 简化实现：依次尝试 `devs` 中的各设备，第一个成功即完成。
/// 所有设备都失败时记录首错。`completion` 的初始引用代表本次 IO，
/// 无需外部额外 `get()`。
///
/// 结果通过 `result_cell` 返回（`submit_bio_all_blocks_read` 相同模式）。
fn try_read_replica(
    mut devs: Vec<Arc<BchDev>>,
    addr: BlockAddr,
    buf_size: usize,
    completion: Arc<Closure>,
    result_cell: Arc<AtomicCell<Vec<u8>>>,
    first_err: Arc<AtomicFirstError>,
) {
    let dev = match devs.first() {
        Some(d) => d.clone(),
        None => {
            // 所有设备都已尝试且失败
            completion.put();
            return;
        }
    };
    devs.remove(0);

    let sink = result_cell.clone();
    let sink_private = result_cell.clone();
    let remaining = devs;

    submit_bio_read(
        BioRequest::read(dev, addr, vec![0u8; buf_size])
            .set_end_io(move |result| {
                match result {
                    Ok(()) => {
                        // 数据已通过 private → result_cell 写入
                        completion.put();
                    }
                    Err(e) => {
                        first_err.set_first(e);
                        if !remaining.is_empty() {
                            try_read_replica(
                                remaining, addr, buf_size, completion, sink, first_err,
                            );
                        } else {
                            completion.put();
                        }
                    }
                }
            })
            .into_read_private(sink_private),
    );
}

/// 多设备尝试读取 — 提交读请求到首选设备，失败时自动降级到下一个设备。
/// 对应 bcachefs `bch2_read_extent` 的备选设备重试机制。
#[allow(dead_code)]
pub(crate) fn submit_bio_read_replicas(
    devs: Vec<Arc<BchDev>>,
    addr: BlockAddr,
    buf_size: usize,
    completion: &Arc<Closure>,
    result_cell: Arc<AtomicCell<Vec<u8>>>,
    first_err: &Arc<AtomicFirstError>,
) {
    try_read_replica(
        devs,
        addr,
        buf_size,
        completion.clone(),
        result_cell,
        first_err.clone(),
    );
}

pub(crate) fn submit_bio_all_blocks(
    dev: Arc<BchDev>,
    start_addr: BlockAddr,
    data: Vec<u8>,
    completion: &Arc<Closure>,
    first_err: &Arc<AtomicFirstError>,
) {
    let block_size = 4096usize;
    let n_blocks = data.len().div_ceil(block_size);
    for (i, chunk) in data.chunks(block_size).enumerate() {
        completion.get();
        let cl = completion.clone();
        let err_cell = first_err.clone();
        let mut buf = chunk.to_vec();
        buf.resize(block_size, 0);
        submit_bio_write(
            BioRequest::write(dev.clone(), BlockAddr::new(start_addr.raw + i as u64), buf)
                .set_end_io(move |result| {
                    if let Err(e) = result {
                        err_cell.set_first(e);
                    }
                    cl.put();
                }),
        );
    }
    if n_blocks == 0 {
        completion.get();
        completion.put();
    }
}

// ═══════════════════════════════════════════════════════════════
// bcachefs 对齐类型 — 对应 bch2_read / bch2_write API
// ═══════════════════════════════════════════════════════════════

/// 对应 `struct subvol_inum` (fs/snapshots/types.h:250-256)
#[derive(Clone, Copy)]
pub struct SubvolInum {
    pub subvol: u64,
    pub inum: u64,
}

// 对应 `enum bch_read_flags` (fs/data/extents_types.h:55-65)
bitflags::bitflags! {
    pub struct BchReadFlags: u16 {
        const RETRY_IF_STALE           = 1 << 0;
        const MAY_PROMOTE              = 1 << 1;
        const USER_MAPPED              = 1 << 2;
        const SOFT_REQUIRE_READ_DEVICE = 1 << 3;
        const HARD_REQUIRE_READ_DEVICE = 1 << 4;
        const LAST_FRAGMENT            = 1 << 5;
        const MUST_BOUNCE              = 1 << 6;
        const MUST_CLONE               = 1 << 7;
        const IN_RETRY                 = 1 << 8;
        const NO_POISON_CHECK          = 1 << 9;
    }
}

/// 对应 `struct bvec_iter` (Linux bio)
#[derive(Clone, Copy)]
pub struct BvecIter {
    /// 扇区偏移 (512 字节单位)
    pub bi_sector: u64,
    /// 剩余字节数
    pub bi_size: u32,
}

/// 对应 `struct bch_read_bio` (fs/data/read.h:27-101)
/// Rust 简化版：bcachefs 的 rbio 包含完整 bio + completion + 追踪字段，
/// subvol 使用异步 IO，仅保留数据缓冲区
pub struct BchReadBio {
    /// 数据读取目的地
    pub data: Vec<u8>,
    /// 从哪个设备的字节偏移开始读取
    pub offset_into_extent: u32,
    /// 内部状态标记 (对应 rbio 的 bitfield flags)
    pub flags: u16,
}

/// 对应 `struct bch_dev_io_failure` (内嵌于 extents_types.h:33-38)
pub struct BchDevIoFailure {
    pub dev: u8,
    pub csum_nr: u8,
    pub ec_errcode: i16,
    pub errcode: i16,
}

/// 对应 `struct bch_io_failures` (fs/data/extents_types.h:31-41)
pub struct BchIoFailures {
    pub nr: u8,
    pub data: Vec<BchDevIoFailure>,
}

/// 对应 `struct bkey_buf` (fs/btree/bkey_buf.h:10-14)
/// Rust 版：用 Option 保存完整 key/value，替代 C 的栈+堆两段式。
pub struct BkeyBuf {
    pub k: Option<crate::btree::BtreeKey>,
    pub v: Option<KeyValue>,
}

impl BkeyBuf {
    /// 对应 bcachefs `bkey_and_val_eq()`：比较完整 key 字段及 value。
    pub fn bkey_and_val_eq(&self, key: &crate::btree::BtreeKey, value: &KeyValue) -> bool {
        let Some(previous) = self.k.as_ref() else {
            return false;
        };
        let Some(previous_value) = self.v.as_ref() else {
            return false;
        };

        let previous_inode = unsafe { std::ptr::addr_of!(previous.inode).read_unaligned() };
        let key_inode = unsafe { std::ptr::addr_of!(key.inode).read_unaligned() };
        let previous_vaddr = unsafe { std::ptr::addr_of!(previous.vaddr).read_unaligned() };
        let key_vaddr = unsafe { std::ptr::addr_of!(key.vaddr).read_unaligned() };
        let previous_size = unsafe { std::ptr::addr_of!(previous.size).read_unaligned() };
        let key_size = unsafe { std::ptr::addr_of!(key.size).read_unaligned() };
        let previous_snapshot = unsafe {
            std::ptr::addr_of!(previous.snapshot_id).read_unaligned()
        };
        let key_snapshot = unsafe { std::ptr::addr_of!(key.snapshot_id).read_unaligned() };
        let previous_version = unsafe { std::ptr::addr_of!(previous.version).read_unaligned() };
        let key_version = unsafe { std::ptr::addr_of!(key.version).read_unaligned() };

        previous_inode == key_inode
            && previous_vaddr == key_vaddr
            && previous_size == key_size
            && previous_snapshot == key_snapshot
            && previous.key_type == key.key_type
            && previous_version == key_version
            && previous_value == value
    }
}

// 对应 `BCH_WRITE_FLAGS` (fs/data/write_types.h:16-43)
bitflags::bitflags! {
    pub struct BchWriteFlags: u16 {
        const ALLOC_NOWAIT        = 1 << 0;
        const CACHED              = 1 << 1;
        const DATA_ENCODED        = 1 << 2;
        const PAGES_STABLE        = 1 << 3;
        const PAGES_OWNED         = 1 << 4;
        const ONLY_SPECIFIED_DEVS = 1 << 5;
        const MUST_EC             = 1 << 6;
        const WROTE_DATA_INLINE   = 1 << 7;
        const CHECK_ENOSPC        = 1 << 8;
        const SYNC                = 1 << 9;
        const FLUSH               = 1 << 10;
        const MOVE                = 1 << 11;
        const IN_WORKER           = 1 << 12;
        const SUBMITTED           = 1 << 13;
        const CONVERT_UNWRITTEN   = 1 << 14;
    }
}

/// 对应 `struct bch_write_op` (fs/data/write_types.h:76-140)
/// Rust 简化版：bcachefs 的 write_op 包含 closure + bio + encode info + insert_keys 等，
/// subvol 使用异步 IO，仅保留写操作参数
pub struct BchWriteOp {
    pub flags: BchWriteFlags,
    pub subvol: u32,
    pub pos: Bpos,
    pub data: Vec<u8>,
    pub csum_type: u8,
    pub compression_opt: u8,
    pub nr_replicas: u8,
    pub watermark: u8,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_device::{BchDevIoRefKind, MockBlockDevice};
    use crate::storage::superblock::BchMemberState;

    fn test_dev_arc() -> Arc<BchDev> {
        Arc::new(BchDev::new(Arc::new(MockBlockDevice::new()), 0))
    }

    #[test]
    fn test_closure_put_fires() {
        let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let fired_clone = fired.clone();
        let cl = Closure::new();
        cl.continue_at(Box::new(move || {
            fired_clone.store(true, std::sync::atomic::Ordering::Release);
        }));
        cl.put();
        assert!(fired.load(std::sync::atomic::Ordering::Acquire));
    }

    #[test]
    fn test_closure_multi_ref() {
        let count = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let count_clone = count.clone();
        let cl = Closure::new();
        cl.continue_at(Box::new(move || {
            count_clone.fetch_add(1, std::sync::atomic::Ordering::Release);
        }));

        cl.get();
        cl.get();
        cl.put();
        assert_eq!(count.load(std::sync::atomic::Ordering::Acquire), 0);
        cl.put();
        assert_eq!(count.load(std::sync::atomic::Ordering::Acquire), 0);
        cl.put();
        assert_eq!(count.load(std::sync::atomic::Ordering::Acquire), 1);
    }

    #[test]
    fn test_closure_parent() {
        let parent_fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let pf_clone = parent_fired.clone();
        let parent = Closure::new();
        parent.continue_at(Box::new(move || {
            pf_clone.store(true, std::sync::atomic::Ordering::Release);
        }));

        // parent.remaining = 1
        let child = Closure::new_child(&parent); // parent.remaining = 2
        child.put(); // parent.remaining = 1
        assert!(!parent_fired.load(std::sync::atomic::Ordering::Acquire));
        parent.put(); // parent.remaining = 0 → fire
        assert!(parent_fired.load(std::sync::atomic::Ordering::Acquire));
    }

    #[tokio::test]
    async fn test_submit_bio_write_then_read() {
        let dev = test_dev_arc();
        let addr = BlockAddr::new(42);
        let data = vec![1u8, 2, 3, 4];

        let (tx, rx) = tokio::sync::oneshot::channel();
        submit_bio_write(
            BioRequest::write(dev.clone(), addr, data.clone()).set_end_io(move |r| {
                let _ = tx.send(r);
            }),
        );
        rx.await.unwrap().unwrap();

        let (tx, rx) = tokio::sync::oneshot::channel();
        submit_bio_read(
            BioRequest::read(dev.clone(), addr, vec![0u8; 4]).set_end_io(move |r| {
                let _ = tx.send(r);
            }),
        );
        rx.await.unwrap().unwrap();

        let mut buf = vec![0u8; 4];
        dev.bdev().read_block(addr, &mut buf).await.unwrap();
        assert_eq!(buf, data);
    }

    #[tokio::test]
    async fn test_submit_bio_write_rejects_offline_device() {
        let dev = test_dev_arc();
        dev.set_offline();

        let (tx, rx) = tokio::sync::oneshot::channel();
        submit_bio_write(
            BioRequest::write(dev.clone(), BlockAddr::new(7), vec![1, 2, 3, 4]).set_end_io(
                move |r| {
                    let _ = tx.send(r);
                },
            ),
        );

        let result = rx.await.unwrap();
        assert!(matches!(result, Err(StorageError::NotFound(_))));
        assert_eq!(dev.io_ref_count(BchDevIoRefKind::Write), 0);
    }

    #[tokio::test]
    async fn test_submit_bio_write_rejects_read_only_device() {
        let dev = test_dev_arc();
        dev.set_member_state(BchMemberState::Ro);

        let (tx, rx) = tokio::sync::oneshot::channel();
        submit_bio_write(
            BioRequest::write(dev.clone(), BlockAddr::new(8), vec![9, 8, 7, 6]).set_end_io(
                move |r| {
                    let _ = tx.send(r);
                },
            ),
        );

        let result = rx.await.unwrap();
        assert!(matches!(result, Err(StorageError::NotFound(_))));
        assert_eq!(dev.io_ref_count(BchDevIoRefKind::Write), 0);
        assert_eq!(dev.member_state(), BchMemberState::Ro);
    }

    #[tokio::test]
    async fn test_closure_tracks_multi_io() {
        let dev = test_dev_arc();
        let n_blocks = 5u64;
        let cl = Closure::new();

        for i in 0..n_blocks {
            cl.get();
            let cl_io = cl.clone();
            submit_bio_write(
                BioRequest::write(dev.clone(), BlockAddr::new(i), vec![i as u8; 4096]).set_end_io(
                    move |_r| {
                        cl_io.put();
                    },
                ),
            );
        }

        let all_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let done_signal = all_done.clone();
        let cl_done = cl.clone();
        cl.continue_at(Box::new(move || {
            done_signal.store(true, std::sync::atomic::Ordering::Release);
        }));
        cl_done.put();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !all_done.load(std::sync::atomic::Ordering::Acquire) {
            assert!(std::time::Instant::now() < deadline, "timeout");
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    #[tokio::test]
    async fn test_submit_bio_all_blocks() {
        let dev = test_dev_arc();
        let addr = BlockAddr::new(10);
        let data = vec![0xABu8; 8192]; // 2 blocks
        let cl = Closure::new();

        let first_err: Arc<AtomicFirstError> = Arc::new(AtomicFirstError::new());
        submit_bio_all_blocks(dev.clone(), addr, data.clone(), &cl, &first_err);

        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let done_signal = done.clone();
        let cl_done = cl.clone();
        cl.continue_at(Box::new(move || {
            done_signal.store(true, std::sync::atomic::Ordering::Release);
        }));
        cl_done.put();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !done.load(std::sync::atomic::Ordering::Acquire) {
            assert!(std::time::Instant::now() < deadline, "timeout");
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    // ═══════════════════════════════════════════════════════════════
    // submit_bio_write_replicas tests
    // ═══════════════════════════════════════════════════════════════

    #[tokio::test]
    async fn test_replicas_two_devices() {
        let dev0 = test_dev_arc();
        let dev1 = test_dev_arc();
        let addr = BlockAddr::new(100);
        let data = vec![0x42u8; 4096];
        let cl = Closure::new();
        let first_err: Arc<AtomicFirstError> = Arc::new(AtomicFirstError::new());

        submit_bio_write_replicas(&[dev0, dev1], addr, data, &cl, &first_err);

        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let done_signal = done.clone();
        cl.continue_at(Box::new(move || {
            done_signal.store(true, std::sync::atomic::Ordering::Release);
        }));

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !done.load(std::sync::atomic::Ordering::Acquire) {
            assert!(std::time::Instant::now() < deadline, "timeout");
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    #[tokio::test]
    async fn test_replicas_three_devices_one_fails() {
        let dev0 = test_dev_arc();
        let dev1 = test_dev_arc();
        let dev2 = test_dev_arc();
        dev2.set_offline();
        let addr = BlockAddr::new(200);
        let data = vec![0xFFu8; 4096];
        let cl = Closure::new();
        let first_err: Arc<AtomicFirstError> = Arc::new(AtomicFirstError::new());

        submit_bio_write_replicas(&[dev0, dev1, dev2], addr, data, &cl, &first_err);

        let all_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let done_signal = all_done.clone();
        cl.continue_at(Box::new(move || {
            done_signal.store(true, std::sync::atomic::Ordering::Release);
        }));

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !all_done.load(std::sync::atomic::Ordering::Acquire) {
            assert!(std::time::Instant::now() < deadline, "timeout");
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(first_err.take().is_some());
    }

    #[tokio::test]
    async fn test_replicas_single_device() {
        let dev = test_dev_arc();
        let addr = BlockAddr::new(300);
        let data = vec![0x11u8; 4096];
        let cl = Closure::new();
        let first_err: Arc<AtomicFirstError> = Arc::new(AtomicFirstError::new());

        submit_bio_write_replicas(&[dev], addr, data, &cl, &first_err);

        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let done_signal = done.clone();
        cl.continue_at(Box::new(move || {
            done_signal.store(true, std::sync::atomic::Ordering::Release);
        }));

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !done.load(std::sync::atomic::Ordering::Acquire) {
            assert!(std::time::Instant::now() < deadline, "timeout");
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(first_err.take().is_none());
    }

    #[tokio::test]
    async fn test_replicas_all_offline() {
        let dev0 = test_dev_arc();
        let dev1 = test_dev_arc();
        dev0.set_offline();
        dev1.set_offline();
        let addr = BlockAddr::new(400);
        let data = vec![0xEEu8; 4096];
        let cl = Closure::new();
        let first_err: Arc<AtomicFirstError> = Arc::new(AtomicFirstError::new());

        submit_bio_write_replicas(&[dev0, dev1], addr, data, &cl, &first_err);

        let all_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let done_signal = all_done.clone();
        cl.continue_at(Box::new(move || {
            done_signal.store(true, std::sync::atomic::Ordering::Release);
        }));

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !all_done.load(std::sync::atomic::Ordering::Acquire) {
            assert!(std::time::Instant::now() < deadline, "timeout");
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(first_err.take().is_some());
    }

    #[tokio::test]
    async fn test_replicas_data_written_to_all_devices() {
        let dev0 = test_dev_arc();
        let dev1 = test_dev_arc();
        let addr = BlockAddr::new(500);
        let data = vec![0xABu8; 4096];
        let cl = Closure::new();
        let first_err: Arc<AtomicFirstError> = Arc::new(AtomicFirstError::new());

        submit_bio_write_replicas(
            &[dev0.clone(), dev1.clone()],
            addr,
            data.clone(),
            &cl,
            &first_err,
        );

        let all_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let done_signal = all_done.clone();
        cl.continue_at(Box::new(move || {
            done_signal.store(true, std::sync::atomic::Ordering::Release);
        }));

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !all_done.load(std::sync::atomic::Ordering::Acquire) {
            assert!(std::time::Instant::now() < deadline, "timeout");
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        let mut buf0 = vec![0u8; 4096];
        let mut buf1 = vec![0u8; 4096];
        dev0.bdev().read_block(addr, &mut buf0).await.unwrap();
        dev1.bdev().read_block(addr, &mut buf1).await.unwrap();
        assert_eq!(buf0, data);
        assert_eq!(buf1, data);
    }

    // ═══════════════════════════════════════════════════════════════
    // submit_bio_read_replicas tests
    // ═══════════════════════════════════════════════════════════════

    #[tokio::test]
    async fn test_read_replicas_first_device_online() {
        let dev0 = test_dev_arc();
        let dev1 = test_dev_arc();
        let addr = BlockAddr::new(1000);
        let expected = vec![0xAAu8; 4096];

        // Write data to dev0
        dev0.bdev().write_block(addr, &expected).await.unwrap();

        let cl = Closure::new();
        let result_cell: Arc<AtomicCell<Vec<u8>>> = Arc::new(AtomicCell::new());
        let first_err: Arc<AtomicFirstError> = Arc::new(AtomicFirstError::new());

        submit_bio_read_replicas(
            vec![dev0, dev1],
            addr,
            4096,
            &cl,
            result_cell.clone(),
            &first_err,
        );

        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let done_signal = done.clone();
        cl.continue_at(Box::new(move || {
            done_signal.store(true, std::sync::atomic::Ordering::Release);
        }));

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !done.load(std::sync::atomic::Ordering::Acquire) {
            assert!(std::time::Instant::now() < deadline, "timeout");
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        let result = result_cell.take().unwrap();
        assert_eq!(result, expected);
        assert!(first_err.take().is_none());
    }

    #[tokio::test]
    async fn test_read_replicas_fallback_to_second() {
        let dev0 = test_dev_arc();
        let dev1 = test_dev_arc();
        let addr = BlockAddr::new(1001);
        let expected = vec![0xBBu8; 4096];

        dev0.set_offline();
        dev1.bdev().write_block(addr, &expected).await.unwrap();

        let cl = Closure::new();
        let result_cell: Arc<AtomicCell<Vec<u8>>> = Arc::new(AtomicCell::new());
        let first_err: Arc<AtomicFirstError> = Arc::new(AtomicFirstError::new());

        submit_bio_read_replicas(
            vec![dev0, dev1],
            addr,
            4096,
            &cl,
            result_cell.clone(),
            &first_err,
        );

        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let done_signal = done.clone();
        cl.continue_at(Box::new(move || {
            done_signal.store(true, std::sync::atomic::Ordering::Release);
        }));

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !done.load(std::sync::atomic::Ordering::Acquire) {
            assert!(std::time::Instant::now() < deadline, "timeout");
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        let result = result_cell.take().unwrap();
        assert_eq!(result, expected);
        // dev0 failed → first_err should be set
        assert!(first_err.take().is_some());
    }

    #[tokio::test]
    async fn test_read_replicas_all_offline() {
        let dev0 = test_dev_arc();
        let dev1 = test_dev_arc();
        dev0.set_offline();
        dev1.set_offline();

        let cl = Closure::new();
        let result_cell: Arc<AtomicCell<Vec<u8>>> = Arc::new(AtomicCell::new());
        let first_err: Arc<AtomicFirstError> = Arc::new(AtomicFirstError::new());

        submit_bio_read_replicas(
            vec![dev0, dev1],
            BlockAddr::new(1002),
            4096,
            &cl,
            result_cell.clone(),
            &first_err,
        );

        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let done_signal = done.clone();
        cl.continue_at(Box::new(move || {
            done_signal.store(true, std::sync::atomic::Ordering::Release);
        }));

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !done.load(std::sync::atomic::Ordering::Acquire) {
            assert!(std::time::Instant::now() < deadline, "timeout");
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        // No data read, should be None
        assert!(result_cell.take().is_none());
        assert!(first_err.take().is_some());
    }

    #[tokio::test]
    async fn test_read_replicas_empty_devs_fires() {
        let cl = Closure::new();
        let result_cell: Arc<AtomicCell<Vec<u8>>> = Arc::new(AtomicCell::new());
        let first_err: Arc<AtomicFirstError> = Arc::new(AtomicFirstError::new());

        submit_bio_read_replicas(
            vec![],
            BlockAddr::new(1003),
            4096,
            &cl,
            result_cell.clone(),
            &first_err,
        );

        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let done_signal = done.clone();
        cl.continue_at(Box::new(move || {
            done_signal.store(true, std::sync::atomic::Ordering::Release);
        }));

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !done.load(std::sync::atomic::Ordering::Acquire) {
            assert!(std::time::Instant::now() < deadline, "timeout");
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(result_cell.take().is_none());
    }

    #[tokio::test]
    async fn test_read_replicas_single_device_online() {
        let dev = test_dev_arc();
        let addr = BlockAddr::new(1004);
        let expected = vec![0xCCu8; 4096];
        dev.bdev().write_block(addr, &expected).await.unwrap();

        let cl = Closure::new();
        let result_cell: Arc<AtomicCell<Vec<u8>>> = Arc::new(AtomicCell::new());
        let first_err: Arc<AtomicFirstError> = Arc::new(AtomicFirstError::new());

        submit_bio_read_replicas(vec![dev], addr, 4096, &cl, result_cell.clone(), &first_err);

        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let done_signal = done.clone();
        cl.continue_at(Box::new(move || {
            done_signal.store(true, std::sync::atomic::Ordering::Release);
        }));

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !done.load(std::sync::atomic::Ordering::Acquire) {
            assert!(std::time::Instant::now() < deadline, "timeout");
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        let result = result_cell.take().unwrap();
        assert_eq!(result, expected);
    }
}
