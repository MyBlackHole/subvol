//! 空后端 — 用于需要 BlockDevice 引用但不执行 I/O 的测试场景

use async_trait::async_trait;

use crate::block_device::BlockDevice;
use crate::types::{BlockAddr, HealthStatus, StorageError};

/// 空后端 — 所有读写操作返回零填充数据，不存储任何内容。
///
/// 用于 `BchVol::test_trees()` 等仅需要后端引用的测试场景。
#[derive(Debug, Clone)]
pub struct NullDevice;

#[async_trait]
impl BlockDevice for NullDevice {
    async fn read_block(&self, _addr: BlockAddr, buf: &mut [u8]) -> Result<(), StorageError> {
        buf.fill(0);
        Ok(())
    }

    async fn write_block(&self, _addr: BlockAddr, _data: &[u8]) -> Result<(), StorageError> {
        Ok(())
    }

    async fn delete_block(&self, _addr: BlockAddr) -> Result<(), StorageError> {
        Ok(())
    }

    async fn trim_block(&self, _addr: BlockAddr) -> Result<(), StorageError> {
        Ok(())
    }

    async fn flush(&self) -> Result<(), StorageError> {
        Ok(())
    }

    async fn health_check(&self) -> Result<HealthStatus, StorageError> {
        Ok(HealthStatus::Healthy)
    }

    async fn used_space(&self) -> Result<u64, StorageError> {
        Ok(0)
    }
}
