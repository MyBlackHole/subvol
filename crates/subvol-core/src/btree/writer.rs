//! BtreeNodeWriter trait — bcachefs 对齐的 btree 节点异步写盘能力
//!
//! bcachefs 对齐的 IO 路径：
//! 1. 调用方设置 `will_make_reachable` → 提交写盘（fire-and-forget）→ 获得磁盘地址
//! 2. IO 完成后回调中清理 `will_make_reachable` + `write_in_flight`
//! 3. 调用方不等待 IO 完成即可使用地址创建 routing entry
//!
//! 对应 bcachefs `bch2_btree_node_write` + `__btree_node_write_done` 的分离模式。

use std::sync::Arc;

use super::node::BtreeNode;
use crate::alloc::{AllocRequest, BchDataType, DEFAULT_BLOCK_SIZE};
use crate::btree::io::__bch2_btree_node_write;
use crate::btree::io::bch2_btree_add_journal_pin;
use crate::types::{StorageError, Watermark};
use async_trait::async_trait;

/// bcachefs 对齐：提交写盘 btree 节点 + 追加 BtreeRoot journal entry。
///
/// 对应 bcachefs 中 `bch2_btree_node_write`（提交）+ 异步 `__btree_node_write_done`（完成）。
///
/// # 异步 IO 语义
///
/// `write_btree_node` 仅提交写盘并返回磁盘地址，不等待 IO 完成。
/// IO 完成后回调负责清理：
/// - `clear_will_make_reachable()`
/// - `clear_write_in_flight()`
/// - `bch2_btree_node_write_done_clean()`
#[async_trait]
pub trait BtreeNodeWriter: Send + Sync {
    /// 序列化 + 提交写盘节点，返回磁盘地址。
    /// bcachefs 对齐：不等待 IO 完成（fire-and-forget），地址在提交后即返回。
    async fn write_btree_node(
        &self,
        node: Arc<BtreeNode>,
        watermark: Watermark,
    ) -> Result<u64, StorageError>;

}

/// 测试用 Noop writer — 返回 fake 地址，不实际写盘。
/// bcachefs 对齐：同步清理 will_make_reachable（模拟 __btree_node_write_done）。
pub(crate) struct NoopWriter;

#[async_trait]
impl BtreeNodeWriter for NoopWriter {
    async fn write_btree_node(
        &self,
        node: Arc<BtreeNode>,
        _watermark: Watermark,
    ) -> Result<u64, StorageError> {
        node.clear_will_make_reachable();
        use std::sync::atomic::{AtomicU64, Ordering};
        // bcachefs obtains write addresses from the allocator, independently
        // of the in-memory btree cache key space.  Keep the test writer's
        // synthetic addresses in a disjoint range for the same invariant.
        static NEXT: AtomicU64 = AtomicU64::new(1 << 32);
        Ok(NEXT.fetch_add(1, Ordering::Relaxed))
    }

}

/// bcachefs 对齐: 真实异步 IO 的 btree 节点写盘器
///
/// 对比 NoopWriter（测试用同步 fake），BtreeWriter：
/// - 通过 `__bch2_btree_node_write` 提交真实 IO
/// - IO 回调中执行 `bch2_btree_node_write_done`（三步: will_make_reachable → journal pin drop → write_in_flight）
/// - 地址由 BchAllocator 分配
pub struct BtreeWriter;

impl BtreeWriter {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl BtreeNodeWriter for BtreeWriter {
    async fn write_btree_node(
        &self,
        node: Arc<BtreeNode>,
        _watermark: Watermark,
    ) -> Result<u64, StorageError> {
        let block_addr = if node.block_addr() == 0 {
            let vol = node.vol_arc().ok_or_else(|| {
                StorageError::NotFound("btree node allocation: volume is not attached".into())
            })?;
            let blocks = u64::from(node.node_size).div_ceil(DEFAULT_BLOCK_SIZE);
            let req = AllocRequest::new(Watermark::Btree, BchDataType::Btree);
            let block_addr = vol.alloc_btree_sectors(&req, blocks)?;
            if !node.try_set_block_addr(block_addr) {
                return Err(StorageError::InvalidData(
                    "btree node block address was allocated concurrently".into(),
                ));
            }
            block_addr
        } else {
            node.block_addr()
        };

        // bcachefs 对齐: journal 属于 filesystem context；节点写入从节点关联的
        // volume 获取 journal，而不是由 writer 调用方额外注入。
        if let Some(vol) = node.vol_arc() {
            let j = vol.journal_arc();
            bch2_btree_add_journal_pin(&node, &j, node.journal_seq);
            // 设置 node.journal 弱引用，write_done 通过它释放 pin
            node.set_journal_ref(&j);
        }

        // bcachefs 对齐: 火抛 IO — 锁 + 序列化 + 提交，不等待 IO 完成
        // IO 完成后自动调用 bch2_btree_node_write_done（通过 node.journal 访问 journal）
        __bch2_btree_node_write(node)?;

        Ok(block_addr)
    }

}
