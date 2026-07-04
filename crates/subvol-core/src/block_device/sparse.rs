//! 稀疏文件后端 — 基于块索引文件
//!
//! 文件结构：`{dir}/blocks/{block_idx}`
//! 每个 block 一个独立文件，不存在 = 全零（稀疏空洞）。
//! TRIM：删除块文件。

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use tokio::fs::{self, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::debug;

use super::{BlockDevice, Result};
use crate::types::{BlockAddr, HealthStatus, StorageError};

/// 稀疏文件块设备 — 每个 block 一个文件
#[derive(Debug)]
pub struct SparseFileBlockDevice {
    blocks_dir: PathBuf,
    block_size: u64,
    capacity_blocks: u64,
}

impl SparseFileBlockDevice {
    /// 创建新块设备
    pub async fn create(
        dir: impl AsRef<Path>,
        capacity_blocks: u64,
        block_size: u64,
    ) -> Result<Self> {
        let blocks_dir = dir.as_ref().join("blocks");
        fs::create_dir_all(&blocks_dir)
            .await
            .map_err(StorageError::Io)?;
        Ok(Self {
            blocks_dir,
            block_size,
            capacity_blocks,
        })
    }

    /// 打开已有的块设备
    pub async fn open(dir: impl AsRef<Path>, block_size: u64) -> Result<Self> {
        let blocks_dir = dir.as_ref().join("blocks");
        if !blocks_dir.exists() {
            return Err(StorageError::NotFound(format!(
                "blocks dir not found: {:?}",
                blocks_dir
            )));
        }
        // 统计已有 block 文件数作为容量
        let mut count = 0u64;
        let mut rd = fs::read_dir(&blocks_dir).await.map_err(StorageError::Io)?;
        while let Some(entry) = rd.next_entry().await.map_err(StorageError::Io)? {
            if entry.file_type().await.map_err(StorageError::Io)?.is_file() {
                count += 1;
            }
        }
        Ok(Self {
            blocks_dir,
            block_size,
            capacity_blocks: count,
        })
    }

    fn block_path(&self, addr: BlockAddr) -> PathBuf {
        self.blocks_dir.join(addr.raw.to_string())
    }

    pub fn capacity_blocks(&self) -> u64 {
        self.capacity_blocks
    }

    pub fn block_size(&self) -> u64 {
        self.block_size
    }

    pub fn path(&self) -> &Path {
        &self.blocks_dir
    }
}

#[async_trait]
impl BlockDevice for SparseFileBlockDevice {
    async fn read_block(&self, addr: BlockAddr, buf: &mut [u8]) -> Result<()> {
        let path = self.block_path(addr);
        let mut file = match fs::File::open(&path).await {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                buf.fill(0);
                return Ok(());
            }
            Err(e) => return Err(StorageError::Io(e)),
        };
        let n = file.read(buf).await.map_err(StorageError::Io)?;
        if (n as u64) < self.block_size {
            buf[n..].fill(0);
        }
        Ok(())
    }

    async fn write_block(&self, addr: BlockAddr, data: &[u8]) -> Result<()> {
        let path = self.block_path(addr);
        debug!("sparse write: path={:?}, size={}", path, data.len());
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await.map_err(StorageError::Io)?;
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .await
            .map_err(StorageError::Io)?;
        file.write_all(data).await.map_err(StorageError::Io)?;
        file.sync_all().await.map_err(StorageError::Io)?;
        Ok(())
    }

    async fn delete_block(&self, addr: BlockAddr) -> Result<()> {
        let path = self.block_path(addr);
        match fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(StorageError::Io(e)),
        }
    }

    async fn trim_block(&self, addr: BlockAddr) -> Result<()> {
        self.delete_block(addr).await
    }

    async fn flush(&self) -> Result<()> {
        Ok(())
    }

    async fn health_check(&self) -> Result<HealthStatus> {
        if self.blocks_dir.exists() {
            Ok(HealthStatus::Healthy)
        } else {
            Ok(HealthStatus::Unreachable {
                reason: format!("blocks dir missing: {:?}", self.blocks_dir),
            })
        }
    }

    async fn used_space(&self) -> Result<u64> {
        let mut total = 0u64;
        let mut rd = fs::read_dir(&self.blocks_dir)
            .await
            .map_err(StorageError::Io)?;
        loop {
            match rd.next_entry().await {
                Ok(Some(entry)) => {
                    let meta = entry.metadata().await.map_err(StorageError::Io)?;
                    if meta.is_file() {
                        total += meta.len();
                    }
                }
                Ok(None) => break,
                Err(e) => return Err(StorageError::Io(e)),
            }
        }
        Ok(total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const BLOCK_SIZE: u64 = 4096;
    const CAPACITY_BLOCKS: u64 = 1024;

    fn test_addr(raw: u64) -> BlockAddr {
        BlockAddr::new(raw)
    }

    async fn create_test_backend() -> (SparseFileBlockDevice, TempDir) {
        let dir = TempDir::new().unwrap();
        let backend = SparseFileBlockDevice::create(dir.path(), CAPACITY_BLOCKS, BLOCK_SIZE)
            .await
            .unwrap();
        (backend, dir)
    }

    #[tokio::test]
    async fn test_create_open() {
        let (backend, dir) = create_test_backend().await;
        assert_eq!(backend.capacity_blocks(), CAPACITY_BLOCKS);
        assert_eq!(backend.block_size(), BLOCK_SIZE);
        let p = dir.path().to_path_buf();
        drop(backend);
        let _reopened = SparseFileBlockDevice::open(&p, BLOCK_SIZE).await.unwrap();
        // capacity_blocks is u64, always >= 0
    }

    #[tokio::test]
    async fn test_write_read() {
        let (backend, _dir) = create_test_backend().await;
        let addr = test_addr(10);
        let data = b"hello sparse file";
        backend.write_block(addr, data).await.unwrap();
        let mut buf = vec![0u8; data.len()];
        backend.read_block(addr, &mut buf).await.unwrap();
        assert_eq!(&buf, data);
    }

    #[tokio::test]
    async fn test_read_unwritten_zeros() {
        let (backend, _dir) = create_test_backend().await;
        let addr = test_addr(500);
        let mut buf = vec![0xFFu8; 32];
        backend.read_block(addr, &mut buf).await.unwrap();
        assert_eq!(buf, vec![0u8; 32]);
    }

    #[tokio::test]
    async fn test_trim() {
        let (backend, _dir) = create_test_backend().await;
        let addr = test_addr(7);
        backend.write_block(addr, b"trim target").await.unwrap();
        let mut buf = vec![0u8; 11];
        backend.read_block(addr, &mut buf).await.unwrap();
        assert_eq!(&buf, b"trim target");
        backend.trim_block(addr).await.unwrap();
        buf.fill(0xFF);
        backend.read_block(addr, &mut buf).await.unwrap();
        assert_eq!(buf, vec![0u8; 11]);
    }

    #[tokio::test]
    async fn test_trim_then_write() {
        let (backend, _dir) = create_test_backend().await;
        let addr = test_addr(20);
        backend.write_block(addr, b"original").await.unwrap();
        backend.trim_block(addr).await.unwrap();
        backend.write_block(addr, b"rewritten").await.unwrap();
        let mut buf = vec![0u8; 9];
        backend.read_block(addr, &mut buf).await.unwrap();
        assert_eq!(&buf, b"rewritten");
    }

    #[tokio::test]
    async fn test_used_space() {
        let (backend, _dir) = create_test_backend().await;
        let used = backend.used_space().await.unwrap();
        assert_eq!(used, 0, "empty volume");
        let full = vec![0xFFu8; BLOCK_SIZE as usize];
        backend.write_block(test_addr(1), &full).await.unwrap();
        let used2 = backend.used_space().await.unwrap();
        assert!(used2 >= BLOCK_SIZE);
    }
}
