use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::RwLock;

use super::{BlockDevice, Result};
use crate::types::{BlockAddr, HealthStatus, StorageError};

/// 内存 Mock 后端 — 用于单元测试
#[derive(Debug, Clone)]
pub struct MockBlockDevice {
    blocks: Arc<RwLock<HashMap<BlockAddr, Vec<u8>>>>,
    write_error: Arc<AtomicBool>,
    write_error_addr: Arc<AtomicU64>,
    read_error: Arc<AtomicBool>,
}

impl MockBlockDevice {
    pub fn new() -> Self {
        Self {
            blocks: Arc::new(RwLock::new(HashMap::new())),
            write_error: Arc::new(AtomicBool::new(false)),
            write_error_addr: Arc::new(AtomicU64::new(u64::MAX)),
            read_error: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn set_write_error(&self, enabled: bool) {
        self.write_error.store(enabled, Ordering::Release);
    }

    /// Inject a write failure for one physical block (test-only fault model).
    pub fn set_write_error_addr(&self, addr: Option<BlockAddr>) {
        self.write_error_addr
            .store(addr.map_or(u64::MAX, |addr| addr.raw), Ordering::Release);
    }

    pub fn set_read_error(&self, enabled: bool) {
        self.read_error.store(enabled, Ordering::Release);
    }
}

impl Default for MockBlockDevice {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl BlockDevice for MockBlockDevice {
    async fn read_block(&self, addr: BlockAddr, buf: &mut [u8]) -> Result<()> {
        if self.read_error.load(Ordering::Acquire) {
            return Err(StorageError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "mock read failure",
            )));
        }
        let map = self.blocks.read();
        if let Some(data) = map.get(&addr) {
            let len = data.len().min(buf.len());
            buf[..len].copy_from_slice(&data[..len]);
        } else {
            // 未写入的块返回零填充，与 FileBlockDevice 行为一致
            buf.fill(0);
        }
        Ok(())
    }

    async fn write_block(&self, addr: BlockAddr, data: &[u8]) -> Result<()> {
        if self.write_error.load(Ordering::Acquire)
            || self.write_error_addr.load(Ordering::Acquire) == addr.raw
        {
            return Err(StorageError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "mock write failure",
            )));
        }
        let mut map = self.blocks.write();
        map.insert(addr, data.to_vec());
        Ok(())
    }

    async fn delete_block(&self, addr: BlockAddr) -> Result<()> {
        let mut map = self.blocks.write();
        map.remove(&addr);
        Ok(())
    }

    async fn trim_block(&self, addr: BlockAddr) -> Result<()> {
        self.delete_block(addr).await
    }

    async fn flush(&self) -> Result<()> {
        Ok(())
    }

    async fn health_check(&self) -> Result<HealthStatus> {
        Ok(HealthStatus::Healthy)
    }

    async fn used_space(&self) -> Result<u64> {
        let map = self.blocks.read();
        Ok(map.values().map(|v| v.len() as u64).sum())
    }
}
