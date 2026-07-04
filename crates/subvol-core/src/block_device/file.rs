//! 文件后端 — 基于块索引文件（同步 I/O）
//!
//! 文件结构：`{dir}/blocks/{block_idx}`
//! 每个 block 一个独立文件，不存在 = 全零（稀疏空洞）。
//! TRIM/DELETE：删除块文件。
//! 使用 std::fs + FileExt（pread/pwrite）实现同步 I/O。

use std::io::Write;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use tokio::task::spawn_blocking;
use tracing::debug;

use super::{BlockDevice, Result};
use crate::types::{BlockAddr, HealthStatus, StorageError};

/// 文件块设备 — 每个 block 一个文件，同步 I/O
#[derive(Debug)]
pub struct FileBlockDevice {
    blocks_dir: PathBuf,
    block_size: u64,
    capacity_blocks: u64,
}

impl FileBlockDevice {
    /// 创建新块设备
    pub async fn create(
        dir: impl AsRef<Path>,
        capacity_blocks: u64,
        block_size: u64,
    ) -> Result<Self> {
        let blocks_dir = dir.as_ref().join("blocks");
        let dir2 = blocks_dir.clone();
        spawn_blocking(move || std::fs::create_dir_all(&dir2))
            .await
            .map_err(|e| StorageError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?
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
        let mut count = 0u64;
        let mut rd = std::fs::read_dir(&blocks_dir).map_err(StorageError::Io)?;
        while let Some(entry) = rd.next().transpose().map_err(StorageError::Io)? {
            if entry.file_type().map_err(StorageError::Io)?.is_file() {
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
impl BlockDevice for FileBlockDevice {
    async fn read_block(&self, addr: BlockAddr, buf: &mut [u8]) -> Result<()> {
        let path = self.block_path(addr);
        debug!("file read: path={:?}", path);

        let file = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                buf.fill(0);
                return Ok(());
            }
            Err(e) => return Err(StorageError::Io(e)),
        };

        let n = file.read_at(buf, 0).map_err(StorageError::Io)?;
        if (n as u64) < self.block_size {
            buf[n..].fill(0);
        }
        Ok(())
    }

    async fn write_block(&self, addr: BlockAddr, data: &[u8]) -> Result<()> {
        let path = self.block_path(addr);
        debug!("file write: path={:?}, size={}", path, data.len());

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(StorageError::Io)?;
        }

        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .map_err(StorageError::Io)?;

        file.write_all(data).map_err(StorageError::Io)?;
        file.sync_all().map_err(StorageError::Io)?;
        Ok(())
    }

    async fn delete_block(&self, addr: BlockAddr) -> Result<()> {
        let path = self.block_path(addr);
        match std::fs::remove_file(&path) {
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
        // spawn_blocking for directory traversal
        let dir = self.blocks_dir.clone();
        spawn_blocking(move || -> Result<u64> {
            let mut total = 0u64;
            let rd = std::fs::read_dir(&dir).map_err(StorageError::Io)?;
            for entry in rd {
                let entry = entry.map_err(StorageError::Io)?;
                if entry.file_type().map_err(StorageError::Io)?.is_file() {
                    let meta = entry.metadata().map_err(StorageError::Io)?;
                    total += meta.len();
                }
            }
            Ok(total)
        })
        .await
        .map_err(|e| StorageError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?
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

    async fn create_test_backend() -> (FileBlockDevice, TempDir) {
        let dir = TempDir::new().unwrap();
        let backend = FileBlockDevice::create(dir.path(), CAPACITY_BLOCKS, BLOCK_SIZE)
            .await
            .unwrap();
        (backend, dir)
    }

    #[tokio::test]
    async fn test_file_backend_create_open() {
        let (backend, dir) = create_test_backend().await;
        let p = dir.path().to_path_buf();
        drop(backend);
        let reopened = FileBlockDevice::open(&p, BLOCK_SIZE).await.unwrap();
        assert_eq!(reopened.block_size(), BLOCK_SIZE);
    }

    #[tokio::test]
    async fn test_file_backend_write_read() {
        let (backend, _dir) = create_test_backend().await;
        let addr = test_addr(10);
        let data = b"hello file backend";
        backend.write_block(addr, data).await.unwrap();
        let mut buf = vec![0u8; data.len()];
        backend.read_block(addr, &mut buf).await.unwrap();
        assert_eq!(&buf, data);
    }

    #[tokio::test]
    async fn test_file_backend_read_unwritten_returns_zeros() {
        let (backend, _dir) = create_test_backend().await;
        let addr = test_addr(999);
        let mut buf = vec![0xFFu8; 16];
        backend.read_block(addr, &mut buf).await.unwrap();
        assert_eq!(buf, vec![0u8; 16]);
    }

    #[tokio::test]
    async fn test_file_backend_trim() {
        let (backend, _dir) = create_test_backend().await;
        let addr = test_addr(2);
        let data = b"trim me";
        backend.write_block(addr, data).await.unwrap();
        let mut buf = vec![0u8; data.len()];
        backend.read_block(addr, &mut buf).await.unwrap();
        assert_eq!(&buf, data);
        backend.trim_block(addr).await.unwrap();
        buf.fill(0xFF);
        backend.read_block(addr, &mut buf).await.unwrap();
        assert_eq!(buf, vec![0u8; data.len()]);
    }

    #[tokio::test]
    async fn test_file_backend_delete() {
        let (backend, _dir) = create_test_backend().await;
        let addr = test_addr(1);
        backend.write_block(addr, b"delete me").await.unwrap();
        backend.delete_block(addr).await.unwrap();
        let mut buf = vec![0xFFu8; 9];
        backend.read_block(addr, &mut buf).await.unwrap();
        assert_eq!(buf, vec![0u8; 9]);
    }

    #[tokio::test]
    async fn test_file_backend_flush() {
        let (backend, _dir) = create_test_backend().await;
        backend.flush().await.unwrap();
    }

    #[tokio::test]
    async fn test_file_backend_health() {
        let (backend, _dir) = create_test_backend().await;
        let health = backend.health_check().await.unwrap();
        assert_eq!(health, HealthStatus::Healthy);
    }

    #[tokio::test]
    async fn test_file_backend_used_space() {
        let (backend, _dir) = create_test_backend().await;
        let used = backend.used_space().await.unwrap();
        assert_eq!(used, 0, "empty volume");
    }

    #[tokio::test]
    async fn test_file_backend_close_reopen() {
        let dir = TempDir::new().unwrap();
        let data = b"persistent data";
        let addr = test_addr(77);
        {
            let backend = FileBlockDevice::create(dir.path(), CAPACITY_BLOCKS, BLOCK_SIZE)
                .await
                .unwrap();
            backend.write_block(addr, data).await.unwrap();
            backend.flush().await.unwrap();
        }
        {
            let reopened = FileBlockDevice::open(dir.path(), BLOCK_SIZE).await.unwrap();
            let mut buf = vec![0u8; data.len()];
            reopened.read_block(addr, &mut buf).await.unwrap();
            assert_eq!(&buf, data);
        }
    }

    #[tokio::test]
    async fn test_file_backend_multiple_blocks() {
        let (backend, _dir) = create_test_backend().await;
        for i in 0..5 {
            let addr = test_addr(i * 2);
            backend
                .write_block(addr, format!("block-{i}").as_bytes())
                .await
                .unwrap();
        }
        for i in 0..5 {
            let addr = test_addr(i * 2);
            let expected = format!("block-{i}");
            let mut buf = vec![0u8; expected.len()];
            backend.read_block(addr, &mut buf).await.unwrap();
            assert_eq!(&buf, expected.as_bytes());
        }
    }

    #[tokio::test]
    async fn test_file_backend_overwrite() {
        let (backend, _dir) = create_test_backend().await;
        let addr = test_addr(100);
        backend.write_block(addr, b"first write").await.unwrap();
        let mut buf = vec![0u8; 11];
        backend.read_block(addr, &mut buf).await.unwrap();
        assert_eq!(&buf, b"first write");
        backend.write_block(addr, b"second").await.unwrap();
        let mut buf = vec![0u8; 11];
        backend.read_block(addr, &mut buf).await.unwrap();
        assert_eq!(&buf[..6], b"second");
    }

    #[tokio::test]
    async fn test_file_backend_trim_then_write() {
        let (backend, _dir) = create_test_backend().await;
        let addr = test_addr(50);
        backend.write_block(addr, b"original").await.unwrap();
        backend.trim_block(addr).await.unwrap();
        backend.write_block(addr, b"rewritten").await.unwrap();
        let mut buf = vec![0u8; 9];
        backend.read_block(addr, &mut buf).await.unwrap();
        assert_eq!(&buf, b"rewritten");
    }
}
