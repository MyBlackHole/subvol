//! NFS 存储后端 — 基于块索引文件
//!
//! 文件结构：`{base_path}/{vol_name}/blocks/{block_idx}`
//! 每个 block 一个独立文件，不存在 = 全零（稀疏空洞）。
//! TRIM：删除块文件。
//!
//! 相比单文件布局，块索引文件：
//! - 天然稀疏（不存在的文件 = 未写入的区域）
//! - 无偏移计算，文件名为 block index
//! - delete = 直接 unlink 文件
//! - 无文件锁竞争（每个 block 独立文件）

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt;
use tracing::debug;

use super::{BlockDevice, Result};
use crate::types::{BlockAddr, HealthStatus, StorageError};

/// NFS 后端配置
#[derive(Debug, Clone)]
pub struct NfsConfig {
    pub base_path: PathBuf,
    pub vol_name: String,
    pub block_size: u64,
}
/// NFS 存储后端 — 按 block index 拆分文件
///
/// 每个物理块（paddr）对应一个独立文件：
/// `{base_path}/{vol_name}/blocks/{paddr}`
///
/// - 文件不存在 = 空洞（全零读取）
/// - 写入时自动创建文件
/// - TRIM/DELETE = 删除文件
#[derive(Debug)]
pub struct NfsBlockDevice {
    /// blocks/ 目录路径
    blocks_dir: PathBuf,
    block_size: u64,
    /// 健康状态缓存
    healthy: AtomicBool,
}

impl NfsBlockDevice {
    /// 创建新的 NFS 后端
    pub async fn new(config: NfsConfig) -> Result<Self> {
        let blocks_dir = config.base_path.join(&config.vol_name).join("blocks");
        fs::create_dir_all(&blocks_dir)
            .await
            .map_err(StorageError::Io)?;

        Ok(Self {
            blocks_dir,
            block_size: config.block_size,
            healthy: AtomicBool::new(true),
        })
    }

    /// 打开已有的 NFS 后端
    pub async fn open(config: NfsConfig) -> Result<Self> {
        let blocks_dir = config.base_path.join(&config.vol_name).join("blocks");
        if !blocks_dir.exists() {
            return Err(StorageError::NotFound(format!(
                "blocks dir not found: {:?}",
                blocks_dir
            )));
        }

        Ok(Self {
            blocks_dir,
            block_size: config.block_size,
            healthy: AtomicBool::new(true),
        })
    }

    /// 获取 block 文件的路径
    fn block_path(&self, addr: BlockAddr) -> PathBuf {
        self.blocks_dir.join(addr.raw.to_string())
    }
}

#[async_trait]
impl BlockDevice for NfsBlockDevice {
    async fn read_block(&self, addr: BlockAddr, buf: &mut [u8]) -> Result<()> {
        let path = self.block_path(addr);
        debug!("nfs read: path={:?}", path);

        let mut file = match fs::File::open(&path).await {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // 文件不存在 = 空洞 → 返回全零
                buf.fill(0);
                return Ok(());
            }
            Err(e) => return Err(StorageError::Io(e)),
        };

        use tokio::io::AsyncReadExt;
        let n = file.read(buf).await.map_err(StorageError::Io)?;

        if (n as u64) < self.block_size {
            // 文件小于 block_size（异常）→ 剩余部分填零
            buf[n..].fill(0);
        }

        Ok(())
    }

    async fn write_block(&self, addr: BlockAddr, data: &[u8]) -> Result<()> {
        let path = self.block_path(addr);
        debug!("nfs write: path={:?}, size={}", path, data.len());

        // 创建目录（安全网：避免并发删除后目录丢失）
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await.map_err(StorageError::Io)?;
        }

        // write + create + truncate（覆盖写入，文件大小精确等于 block_size）
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
        debug!("nfs delete: path={:?}", path);

        match fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // 文件不存在 = 已经是空洞，视为成功
                Ok(())
            }
            Err(e) => Err(StorageError::Io(e)),
        }
    }

    async fn trim_block(&self, addr: BlockAddr) -> Result<()> {
        // trim = delete（移除块文件）
        self.delete_block(addr).await
    }

    async fn flush(&self) -> Result<()> {
        // 每个 write_block 已 sync_all，不需要额外 flush
        Ok(())
    }

    async fn health_check(&self) -> Result<HealthStatus> {
        if self.healthy.load(Ordering::Relaxed) {
            if self.blocks_dir.exists() {
                Ok(HealthStatus::Healthy)
            } else {
                self.healthy.store(false, Ordering::Relaxed);
                Ok(HealthStatus::Unreachable {
                    reason: format!("blocks dir missing: {:?}", self.blocks_dir),
                })
            }
        } else {
            Ok(HealthStatus::Unreachable {
                reason: "previously marked unhealthy".into(),
            })
        }
    }

    async fn used_space(&self) -> Result<u64> {
        // 遍历 blocks/ 目录，统计所有块文件大小之和
        let mut total = 0u64;
        let mut read_dir = fs::read_dir(&self.blocks_dir)
            .await
            .map_err(StorageError::Io)?;

        loop {
            match read_dir.next_entry().await {
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

    fn test_addr(raw: u64) -> BlockAddr {
        BlockAddr::new(raw)
    }

    async fn create_test_backend() -> (NfsBlockDevice, TempDir) {
        let dir = TempDir::new().unwrap();
        let config = NfsConfig {
            base_path: dir.path().to_path_buf(),
            vol_name: "test".into(),
            block_size: BLOCK_SIZE,
        };
        let backend = NfsBlockDevice::new(config).await.unwrap();
        (backend, dir)
    }

    #[tokio::test]
    async fn test_nfs_backend_create_open() {
        let (backend, dir) = create_test_backend().await;
        let blocks_dir = dir.path().join("test").join("blocks");
        assert!(blocks_dir.exists(), "blocks dir should exist");
        drop(backend);
        assert!(blocks_dir.exists(), "blocks dir should persist after drop");
    }

    #[tokio::test]
    async fn test_nfs_backend_write_read() {
        let (backend, _dir) = create_test_backend().await;
        let addr = test_addr(42);
        let data = b"hello subvol nfs backend";

        backend.write_block(addr, data).await.unwrap();

        // 验证文件存在
        let path = backend.block_path(addr);
        assert!(path.exists(), "block file should exist after write");

        let mut buf = vec![0u8; data.len()];
        backend.read_block(addr, &mut buf).await.unwrap();
        assert_eq!(&buf, data);
    }

    #[tokio::test]
    async fn test_nfs_backend_sparse_read() {
        let (backend, _dir) = create_test_backend().await;
        let addr = test_addr(999);

        // 未写入的 block 文件不存在 → 读零
        let mut buf = vec![0xFFu8; BLOCK_SIZE as usize];
        backend.read_block(addr, &mut buf).await.unwrap();
        assert_eq!(buf, vec![0u8; BLOCK_SIZE as usize]);
    }

    #[tokio::test]
    async fn test_nfs_backend_trim() {
        let (backend, _dir) = create_test_backend().await;
        let addr = test_addr(2);
        let data = b"trim me";

        backend.write_block(addr, data).await.unwrap();
        let mut buf = vec![0u8; data.len()];
        backend.read_block(addr, &mut buf).await.unwrap();
        assert_eq!(&buf, data);

        backend.trim_block(addr).await.unwrap();

        // 文件应被删除
        let path = backend.block_path(addr);
        assert!(!path.exists(), "block file should be deleted after trim");

        // 读取应返回零
        buf.fill(0xFF);
        backend.read_block(addr, &mut buf).await.unwrap();
        assert_eq!(buf, vec![0u8; data.len()]);
    }

    #[tokio::test]
    async fn test_nfs_backend_delete_block() {
        let (backend, _dir) = create_test_backend().await;
        let addr = test_addr(5);
        let data = b"delete me";

        backend.write_block(addr, data).await.unwrap();
        assert!(backend.block_path(addr).exists());

        backend.delete_block(addr).await.unwrap();
        assert!(!backend.block_path(addr).exists());

        // 重复 delete 应幂等
        backend.delete_block(addr).await.unwrap();
    }

    #[tokio::test]
    async fn test_nfs_backend_used_space() {
        let (backend, _dir) = create_test_backend().await;

        // 空 volume → 0 已用空间
        let used = backend.used_space().await.unwrap();
        assert_eq!(used, 0, "empty volume should use 0 space");

        // 写一个完整的 block → 至少 block_size 已用空间
        let addr = test_addr(10);
        let full_block = vec![0u8; BLOCK_SIZE as usize];
        backend.write_block(addr, &full_block).await.unwrap();
        let used2 = backend.used_space().await.unwrap();
        assert!(
            used2 >= BLOCK_SIZE,
            "one block should use at least {BLOCK_SIZE} bytes, got {used2}"
        );
    }

    #[tokio::test]
    async fn test_nfs_backend_multi_block_independence() {
        let (backend, _dir) = create_test_backend().await;

        let addr1 = test_addr(1);
        let addr2 = test_addr(2);
        let data1 = b"block one data";
        let data2 = b"block two data";

        backend.write_block(addr1, data1).await.unwrap();
        backend.write_block(addr2, data2).await.unwrap();

        let mut buf1 = vec![0u8; data1.len()];
        let mut buf2 = vec![0u8; data2.len()];
        backend.read_block(addr1, &mut buf1).await.unwrap();
        backend.read_block(addr2, &mut buf2).await.unwrap();
        assert_eq!(&buf1, data1);
        assert_eq!(&buf2, data2);

        // 删除 addr1 不影响 addr2
        backend.delete_block(addr1).await.unwrap();
        let mut buf2_after = vec![0u8; data2.len()];
        backend.read_block(addr2, &mut buf2_after).await.unwrap();
        assert_eq!(&buf2_after, data2);
    }
}
