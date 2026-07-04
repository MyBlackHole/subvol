//! Multi-block Btree node extent I/O.
//!
//! bcachefs allocates a stable extent for a node, writes `struct btree_node`
//! at offset zero, then appends block-aligned `struct btree_node_entry` records
//! (`fs/btree/write.c:440-527, 584-622`).  This module preserves that boundary:
//! normal writeback changes `sectors_written`, not the physical start address.

use crate::block_device::{BchDev, BchDevIoRefKind};
use crate::btree::node::{
    BtreeNode, BtreeNodeHeader, BLOCK_SIZE, BTREE_NODE_MAGIC, BTREE_NODE_VERSION,
    SECTORS_PER_BLOCK, SECTOR_SIZE,
};
use crate::btree::types::BtreePtrV2;
use crate::io::{
    submit_bio_all_blocks, submit_bio_all_blocks_read, submit_bio_read_replicas, Closure,
};
use crate::types::{AtomicCell, AtomicFirstError, BlockAddr, StorageError};
use std::sync::Arc;

/// Read a contiguous range of physical blocks.
pub async fn __bch2_read_blocks(
    dev: Arc<BchDev>,
    start_block: u64,
    block_count: usize,
) -> Result<Vec<u8>, StorageError> {
    let _dev_ref = dev
        .try_get_io_ref_guard(BchDevIoRefKind::Read)
        .ok_or_else(|| StorageError::NotFound("device offline".into()))?;
    let dev = dev.clone();
    let completion = Closure::new();
    let result_cell = Arc::new(AtomicCell::new());
    let first_err = Arc::new(AtomicFirstError::new());
    submit_bio_all_blocks_read(
        dev,
        BlockAddr::new(start_block),
        block_count,
        &completion,
        result_cell.clone(),
        &first_err,
    );
    completion.wait_async().await;
    if let Some(err) = first_err.take() {
        return Err(err);
    }
    Ok(result_cell.take().unwrap_or_default())
}

/// Write one block-aligned node record at a sector offset in a stable extent.
pub async fn __bch2_write_node_record(
    dev: Arc<BchDev>,
    extent_start: u64,
    sector_offset: u16,
    record: &[u8],
) -> Result<(), StorageError> {
    let _dev_ref = dev
        .try_get_io_ref_guard(BchDevIoRefKind::Write)
        .ok_or_else(|| StorageError::NotFound("device offline".into()))?;
    let dev = dev.clone();
    if sector_offset % SECTORS_PER_BLOCK != 0 || record.len() % BLOCK_SIZE != 0 {
        return Err(StorageError::InvalidData(
            "btree node record I/O must be block aligned".into(),
        ));
    }
    let start_block = extent_start + u64::from(sector_offset / SECTORS_PER_BLOCK);
    let completion = Closure::new();
    let first_err = Arc::new(AtomicFirstError::new());
    submit_bio_all_blocks(
        dev,
        BlockAddr::new(start_block),
        record.to_vec(),
        &completion,
        &first_err,
    );
    completion.wait_async().await;
    if let Some(err) = first_err.take() {
        return Err(err);
    }
    Ok(())
}

/// Read exactly the range committed by `ptr.sectors_written` and decode it.
///
/// Checksum validation happens as part of `deserialize_from_extent()` so the
/// load boundary rejects corrupted raw records before the node reaches
/// `read_done`.
pub async fn __bch2_load_btree_node_from_ptr(
    dev: Arc<BchDev>,
    ptr: BtreePtrV2,
) -> Result<BtreeNode, StorageError> {
    if ptr.sectors_written == 0 || ptr.sectors_written % SECTORS_PER_BLOCK != 0 {
        return Err(StorageError::InvalidData(
            "invalid btree pointer sectors_written".into(),
        ));
    }
    let block_count = usize::from(ptr.sectors_written / SECTORS_PER_BLOCK);
    let data = __bch2_read_blocks(dev.clone(), ptr.block_addr, block_count).await?;
    BtreeNode::deserialize_from_extent(&data, ptr)
}

/// Compatibility loader for call sites that only have an address.
///
/// It reads the initial header to construct a one-record pointer.  Recursive
/// recovery must use `__bch2_load_btree_node_from_ptr()` so append records are bounded
/// by the parent/root pointer rather than guessed from disk contents.
pub async fn __bch2_load_btree_node(
    dev: Arc<BchDev>,
    bucket_addr: u64,
) -> Result<BtreeNode, StorageError> {
    let first = __bch2_read_blocks(dev.clone(), bucket_addr, 1).await?;
    if first.len() < std::mem::size_of::<BtreeNodeHeader>() {
        return Err(StorageError::InvalidData(
            "btree initial block is too short".into(),
        ));
    }
    let header: BtreeNodeHeader =
        unsafe { std::ptr::read_unaligned(first.as_ptr().cast::<BtreeNodeHeader>()) };
    let magic = { header.magic };
    let version = { header.version };
    let record_bytes = { header.record_bytes as usize };
    if magic != BTREE_NODE_MAGIC || version != BTREE_NODE_VERSION {
        return Err(StorageError::InvalidData(
            "invalid btree initial record header".into(),
        ));
    }
    if record_bytes == 0 || record_bytes % BLOCK_SIZE != 0 {
        return Err(StorageError::InvalidData(
            "invalid btree initial record length".into(),
        ));
    }

    let data = if record_bytes == BLOCK_SIZE {
        first
    } else {
        __bch2_read_blocks(dev.clone(), bucket_addr, record_bytes / BLOCK_SIZE).await?
    };
    let ptr = BtreePtrV2 {
        block_addr: bucket_addr,
        sectors_written: (record_bytes / SECTOR_SIZE) as u16,
        level: header.level,
        dev_idx: dev.dev_idx,
        generation: header.generation,
    };
    BtreeNode::deserialize_from_extent(&data, ptr)
}

/// 使用多设备副本读取 btree 节点（适应多设备场景）。
///
/// 与 `__bch2_load_btree_node` 类似，但通过 `submit_bio_read_replicas`
/// 在多个在线设备间进行读取，优先尝试列表中的第一个设备，
/// 失败时自动降级到下一个设备。
pub async fn __bch2_load_btree_node_replicas(
    devs: Vec<Arc<BchDev>>,
    bucket_addr: u64,
) -> Result<BtreeNode, StorageError> {
    if devs.is_empty() {
        return Err(StorageError::NotFound(
            "bch2_load_btree_node_replicas: no devices available".into(),
        ));
    }

    // ── 读取 header 块 ──
    let first = read_single_block_replicas(&devs, bucket_addr).await?;
    if first.len() < std::mem::size_of::<BtreeNodeHeader>() {
        return Err(StorageError::InvalidData(
            "btree initial block is too short".into(),
        ));
    }
    let header: BtreeNodeHeader =
        unsafe { std::ptr::read_unaligned(first.as_ptr().cast::<BtreeNodeHeader>()) };
    let magic = { header.magic };
    let version = { header.version };
    let record_bytes = { header.record_bytes as usize };
    if magic != BTREE_NODE_MAGIC || version != BTREE_NODE_VERSION {
        return Err(StorageError::InvalidData(
            "invalid btree initial record header".into(),
        ));
    }
    if record_bytes == 0 || record_bytes % BLOCK_SIZE != 0 {
        return Err(StorageError::InvalidData(
            "invalid btree initial record length".into(),
        ));
    }

    // ── 读取完整记录（可能跨多块） ──
    let data = if record_bytes == BLOCK_SIZE {
        first
    } else {
        let total_blocks = record_bytes / BLOCK_SIZE;
        let mut data = first;
        for i in 1..total_blocks {
            let block_data = read_single_block_replicas(&devs, bucket_addr + i as u64).await?;
            data.extend_from_slice(&block_data);
        }
        data
    };

    let ptr = BtreePtrV2 {
        block_addr: bucket_addr,
        sectors_written: (record_bytes / SECTOR_SIZE) as u16,
        level: header.level,
        dev_idx: devs[0].dev_idx,
        generation: header.generation,
    };
    BtreeNode::deserialize_from_extent(&data, ptr)
}

/// 使用多设备副本读取单个块。依次尝试设备列表中的每个设备，
/// 第一个成功即返回，全部失败则返回错误。
async fn read_single_block_replicas(
    devs: &[Arc<BchDev>],
    block_addr: u64,
) -> Result<Vec<u8>, StorageError> {
    let completion = Closure::new();
    let result_cell = Arc::new(AtomicCell::new());
    let first_err = Arc::new(AtomicFirstError::new());

    submit_bio_read_replicas(
        devs.to_vec(),
        BlockAddr::new(block_addr),
        BLOCK_SIZE,
        &completion,
        result_cell.clone(),
        &first_err,
    );

    // 使用 oneshot 通道等待 IO 完成
    // 注意：submit_bio_read_replicas 直接使用 completion 的初始引用，
    // IO 完成时 end_io 回调会调用 completion.put() 触发回调。
    let (tx, rx) = tokio::sync::oneshot::channel();
    completion.continue_at(Box::new(move || {
        let _ = tx.send(());
    }));

    rx.await
        .map_err(|_| StorageError::NotFound("read IO wait cancelled".into()))?;

    result_cell
        .take()
        .ok_or_else(|| StorageError::NotFound("failed to read block from any device".into()))
}

/// Initial-write a node and return the committed physical pointer.
pub async fn __bch2_write_initial_node(
    node: &BtreeNode,
    bucket_addr: u64,
    generation: u32,
    dev: Arc<BchDev>,
) -> Result<BtreePtrV2, StorageError> {
    let _dev_ref = dev
        .try_get_io_ref_guard(BchDevIoRefKind::Write)
        .ok_or_else(|| StorageError::NotFound("device offline".into()))?;
    let record = node.serialize_initial_record(bucket_addr, generation)?;
    __bch2_write_node_record(dev.clone(), bucket_addr, 0, &record).await?;
    Ok(BtreePtrV2 {
        block_addr: bucket_addr,
        sectors_written: (record.len() / SECTOR_SIZE) as u16,
        level: node.level,
        dev_idx: dev.dev_idx,
        generation,
    })
}

/// Existing low-level API retained while callers migrate to full pointers.
pub async fn __bch2_write_node_to_bucket(
    node: &BtreeNode,
    bucket_addr: u64,
    dev: Arc<BchDev>,
) -> Result<(), StorageError> {
    __bch2_write_initial_node(node, bucket_addr, 1, dev)
        .await
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_device::{BchDev, MockBlockDevice};
    use crate::btree::key::{BchVal, BtreeKey, KeyType};
    use crate::btree::types::BtreePtrV2;
    use crate::types::StorageError;
    use std::sync::Arc;

    fn build_filled_node(count: u32) -> BtreeNode {
        let mut node = BtreeNode::new_leaf();
        for i in 0..count {
            node.insert(
                BtreeKey::new(i as u64, 1, KeyType::Normal),
                BchVal::new(i as u64, i as u16),
            );
        }
        node
    }

    #[tokio::test]
    async fn test_bucket_io_roundtrip() {
        let backend = MockBlockDevice::new();
        let dev = Arc::new(BchDev::new(Arc::new(backend.clone()), 0));
        let mut node = build_filled_node(20);
        node.compact();

        let ptr = __bch2_write_initial_node(&node, 100, 7, dev.clone())
            .await
            .unwrap();
        let loaded = __bch2_load_btree_node_from_ptr(dev.clone(), ptr)
            .await
            .unwrap();

        assert_eq!(loaded.packed_keys + loaded.unpacked_keys, node.packed_keys + node.unpacked_keys);
        assert_eq!(loaded.level, node.level);
        for i in 0..20 {
            let key = BtreeKey::new(i, 1, KeyType::Normal);
            assert!(loaded.search(&key).is_some(), "key {} lost in bucket io", i);
        }
    }

    #[tokio::test]
    async fn test_btree_node_read_falls_back_to_online_replica() {
        let backend0 = MockBlockDevice::new();
        let backend1 = MockBlockDevice::new();
        let dev0 = Arc::new(BchDev::new(Arc::new(backend0), 0));
        let dev1 = Arc::new(BchDev::new(Arc::new(backend1), 1));
        let mut node = build_filled_node(20);
        node.compact();
        let record = node.serialize_initial_record(100, 7).unwrap();
        __bch2_write_node_record(dev0.clone(), 100, 0, &record)
            .await
            .unwrap();
        __bch2_write_node_record(dev1.clone(), 100, 0, &record)
            .await
            .unwrap();

        dev0.set_offline();
        let loaded = __bch2_load_btree_node_replicas(vec![dev0, dev1], 100)
            .await
            .expect("online metadata replica should satisfy read");
        assert_eq!(loaded.packed_keys + loaded.unpacked_keys, node.packed_keys + node.unpacked_keys);
        assert!(loaded
            .search(&BtreeKey::new(19, 1, KeyType::Normal))
            .is_some());
    }

    #[tokio::test]
    async fn test_load_btree_node_rejects_corrupt_initial_record() {
        let backend = MockBlockDevice::new();
        let dev = Arc::new(BchDev::new(Arc::new(backend.clone()), 0));
        let mut node = build_filled_node(20);
        node.compact();

        let mut record = node.serialize_initial_record(100, 7).unwrap();
        let record_mid = record.len() / 2;
        record[record_mid] ^= 0x80;
        __bch2_write_node_record(dev.clone(), 100, 0, &record)
            .await
            .unwrap();

        let ptr = BtreePtrV2 {
            block_addr: 100,
            sectors_written: (record.len() / SECTOR_SIZE) as u16,
            level: node.level,
            dev_idx: 0,
            generation: 7,
        };
        let result = __bch2_load_btree_node_from_ptr(dev.clone(), ptr).await;
        assert!(matches!(result, Err(StorageError::ChecksumMismatch { .. })));
    }

    #[tokio::test]
    async fn test_multi_block_initial_record_roundtrip() {
        let backend = MockBlockDevice::new();
        let dev = Arc::new(BchDev::new(Arc::new(backend.clone()), 0));
        let mut node = build_filled_node(180);
        node.compact();

        let ptr = __bch2_write_initial_node(&node, 500, 3, dev.clone())
            .await
            .unwrap();
        assert!(ptr.sectors_written > SECTORS_PER_BLOCK);
        let loaded = __bch2_load_btree_node_from_ptr(dev.clone(), ptr)
            .await
            .unwrap();
        assert_eq!(loaded.packed_keys + loaded.unpacked_keys, node.packed_keys + node.unpacked_keys);
        assert!(loaded
            .search(&BtreeKey::new(179, 1, KeyType::Normal))
            .is_some());
    }

    #[tokio::test]
    async fn test_load_btree_node_rejects_corrupt_append_record() {
        let backend = MockBlockDevice::new();
        let dev = Arc::new(BchDev::new(Arc::new(backend.clone()), 0));
        let mut node = build_filled_node(20);
        node.compact();
        // bcachefs: append 需要节点有足够剩余空间容纳新的 write block
        node.node_size = 32768;
        let initial = __bch2_write_initial_node(&node, 900, 4, dev.clone())
            .await
            .unwrap();
        // bcachefs: write 完成后 written 推进且标记 just_written，
        // prep_for_write 执行 cleanup + init_next 为后续 insert 准备新 bset
        node.written = initial.sectors_written;
        node.set_just_written();
        crate::btree::io::bch2_btree_node_prep_for_write(&mut node);

        node.insert(
            BtreeKey::new(1000, 1, KeyType::Normal),
            BchVal::new(0xCAFE, 1),
        );
        let mut append = node
            .serialize_append_record(4, initial.sectors_written)
            .unwrap();
        let append_mid = append.len() / 2;
        append[append_mid] ^= 0x20;
        __bch2_write_node_record(
            dev.clone(),
            initial.block_addr,
            initial.sectors_written,
            &append,
        )
        .await
        .unwrap();

        let ptr = BtreePtrV2 {
            sectors_written: initial.sectors_written + (append.len() / SECTOR_SIZE) as u16,
            ..initial
        };
        let result = __bch2_load_btree_node_from_ptr(dev.clone(), ptr).await;
        assert!(matches!(result, Err(StorageError::ChecksumMismatch { .. })));
    }

    #[tokio::test]
    async fn test_append_record_respects_pointer_boundary() {
        let backend = MockBlockDevice::new();
        let dev = Arc::new(BchDev::new(Arc::new(backend.clone()), 0));
        let mut node = build_filled_node(20);
        node.compact();
        // bcachefs: append 需要节点有足够剩余空间容纳新的 write block
        node.node_size = 32768;
        let initial_ptr = __bch2_write_initial_node(&node, 900, 4, dev.clone())
            .await
            .unwrap();
        // bcachefs: write 完成后 written 推进且标记 just_written，
        // prep_for_write 执行 cleanup + init_next 为后续 insert 准备新 bset
        node.written = initial_ptr.sectors_written;
        node.set_just_written();
        crate::btree::io::bch2_btree_node_prep_for_write(&mut node);

        node.insert(
            BtreeKey::new(1000, 1, KeyType::Normal),
            BchVal::new(0xCAFE, 1),
        );
        let append = node
            .serialize_append_record(4, initial_ptr.sectors_written)
            .unwrap();
        __bch2_write_node_record(
            dev.clone(),
            initial_ptr.block_addr,
            initial_ptr.sectors_written,
            &append,
        )
        .await
        .unwrap();

        let before_commit = __bch2_load_btree_node_from_ptr(dev.clone(), initial_ptr)
            .await
            .unwrap();
        assert!(before_commit
            .search(&BtreeKey::new(1000, 1, KeyType::Normal))
            .is_none());

        let committed_ptr = BtreePtrV2 {
            sectors_written: initial_ptr.sectors_written + (append.len() / SECTOR_SIZE) as u16,
            ..initial_ptr
        };
        let after_commit = __bch2_load_btree_node_from_ptr(dev.clone(), committed_ptr)
            .await
            .unwrap();
        assert!(after_commit
            .search(&BtreeKey::new(1000, 1, KeyType::Normal))
            .is_some());
    }
}
