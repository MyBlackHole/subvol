//! BlockDevice trait + 后端实现

pub mod bch_dev;
pub mod devs_mask;
pub mod file;
mod nfs;
mod s3;
pub mod sparse;

#[cfg(test)]
pub mod mock;

#[cfg(test)]
pub use mock::MockBlockDevice;

pub use bch_dev::{BchDev, BchDevIoRefGuard, BchDevIoRefKind};
pub use devs_mask::BchDevsMask;
pub use file::FileBlockDevice;
pub use nfs::{NfsBlockDevice, NfsConfig};
pub use s3::{S3BlockDevice, S3Config};
pub use sparse::SparseFileBlockDevice;

use async_trait::async_trait;
use futures::future::join_all;

use crate::journal::Crc32CHasher;
use crate::types::{BlockAddr, HealthStatus, StorageError};

/// 统一结果类型
pub type Result<T> = std::result::Result<T, StorageError>;

/// 计算数据块的 CRC32C 校验和
///
/// 对齐 bcachefs 的 `bch2_csum_set` 设计，用于数据完整性保护。
pub fn block_crc32c(data: &[u8]) -> u32 {
    Crc32CHasher::hash(data)
}

/// 存储后端抽象
#[async_trait]
pub trait BlockDevice: Send + Sync + std::fmt::Debug {
    async fn read_block(&self, addr: BlockAddr, buf: &mut [u8]) -> Result<()>;
    async fn write_block(&self, addr: BlockAddr, buf: &[u8]) -> Result<()>;
    async fn delete_block(&self, addr: BlockAddr) -> Result<()>;
    async fn trim_block(&self, addr: BlockAddr) -> Result<()>;
    async fn flush(&self) -> Result<()>;
    async fn health_check(&self) -> Result<HealthStatus>;
    async fn used_space(&self) -> Result<u64>;

    /// 批读取多个连续 block。
    ///
    /// bcachefs 将 extent 读取拆成多个 bio 后并行提交，再由完成引用
    /// 聚合结果；这里保留相同的提交/聚合顺序，同时按 block 独立缓冲区
    /// 允许后端 I/O 并行执行。
    async fn read_blocks(&self, addr: BlockAddr, count: u32, buf: &mut [u8]) -> Result<()> {
        let block_size = buf.len() / count as usize;
        let reads = (0..count as usize).map(|i| async move {
            let mut block = vec![0u8; block_size];
            self.read_block(BlockAddr::new(addr.raw + i as u64), &mut block)
                .await
                .map(|_| block)
        });
        let results = join_all(reads).await;
        for (i, result) in results.into_iter().enumerate() {
            let off = i * block_size;
            buf[off..off + block_size].copy_from_slice(&result?);
        }
        Ok(())
    }

    /// 读取块并计算 CRC32C 校验和
    ///
    /// 默认实现委托给 `read_block`，然后在读取的数据上计算校验和。
    /// 支持校验和的后端可以重写此方法以使用硬件加速或原生校验和验证。
    /// 返回读取数据的 CRC32C 校验和。
    ///
    /// 调用方应将返回值与预期的校验和进行比较，以检测数据损坏。
    async fn read_block_with_csum(&self, addr: BlockAddr, buf: &mut [u8]) -> Result<u32> {
        self.read_block(addr, buf).await?;
        Ok(block_crc32c(buf))
    }

    /// 写入块并计算 CRC32C 校验和
    ///
    /// 默认实现计算校验和，然后委托给 `write_block`。
    /// 支持校验和的后端可以重写此方法以原子方式存储数据+校验和。
    /// 返回写入数据的 CRC32C 校验和。
    async fn write_block_with_csum(&self, addr: BlockAddr, data: &[u8]) -> Result<u32> {
        let csum = block_crc32c(data);
        self.write_block(addr, data).await?;
        Ok(csum)
    }
}

#[cfg(test)]
mod tests {
    use super::mock::MockBlockDevice;
    use super::*;
    use crate::types::BlockAddr;

    fn test_addr(raw: u64) -> BlockAddr {
        BlockAddr::new(raw)
    }

    #[tokio::test]
    async fn test_mock_backend_write_read() {
        let backend = MockBlockDevice::new();
        let addr = test_addr(42);
        let data = b"hello subvol";

        backend.write_block(addr, data).await.unwrap();
        let mut buf = vec![0u8; data.len()];
        backend.read_block(addr, &mut buf).await.unwrap();
        assert_eq!(&buf, data);
    }

    #[tokio::test]
    async fn test_mock_backend_read_unwritten() {
        let backend = MockBlockDevice::new();
        let addr = test_addr(999);
        let mut buf = vec![0xFFu8; 16]; // 非零初始值
        backend.read_block(addr, &mut buf).await.unwrap();
        assert_eq!(buf, vec![0u8; 16], "未写入块应返回零填充");
    }

    #[tokio::test]
    async fn test_mock_backend_delete() {
        let backend = MockBlockDevice::new();
        let addr = test_addr(1);
        backend.write_block(addr, b"data").await.unwrap();

        let mut buf = vec![0u8; 4];
        backend.read_block(addr, &mut buf).await.unwrap();
        assert_eq!(&buf, b"data");

        backend.delete_block(addr).await.unwrap();
        let mut buf = vec![0xFFu8; 4];
        backend.read_block(addr, &mut buf).await.unwrap();
        assert_eq!(buf, vec![0u8; 4], "delete 后应返回零填充");
    }

    #[tokio::test]
    async fn test_mock_backend_trim() {
        let backend = MockBlockDevice::new();
        let addr = test_addr(2);
        backend.write_block(addr, b"trim me").await.unwrap();
        backend.trim_block(addr).await.unwrap();
        let mut buf = vec![0xFFu8; 7];
        backend.read_block(addr, &mut buf).await.unwrap();
        assert_eq!(buf, vec![0u8; 7], "trim 后应返回零填充");
    }

    #[tokio::test]
    async fn test_mock_backend_flush() {
        let backend = MockBlockDevice::new();
        backend.flush().await.unwrap();
    }

    #[tokio::test]
    async fn test_mock_backend_health() {
        let backend = MockBlockDevice::new();
        let health = backend.health_check().await.unwrap();
        assert_eq!(health, HealthStatus::Healthy);
    }

    #[tokio::test]
    async fn test_mock_backend_used_space() {
        let backend = MockBlockDevice::new();
        assert_eq!(backend.used_space().await.unwrap(), 0);

        let addr = test_addr(10);
        backend.write_block(addr, b"1234567890").await.unwrap();
        assert_eq!(backend.used_space().await.unwrap(), 10);
    }

    #[tokio::test]
    async fn test_mock_backend_multiple_blocks() {
        let backend = MockBlockDevice::new();

        let addrs: Vec<_> = (0..5).map(|i| test_addr(i)).collect();
        for (i, addr) in addrs.iter().enumerate() {
            backend
                .write_block(*addr, format!("block-{i}").as_bytes())
                .await
                .unwrap();
        }

        for (i, addr) in addrs.iter().enumerate() {
            let mut buf = vec![0u8; 7];
            backend.read_block(*addr, &mut buf).await.unwrap();
            assert_eq!(&buf, format!("block-{i}").as_bytes());
        }
    }

    #[tokio::test]
    async fn test_mock_backend_read_blocks_preserves_order() {
        let backend = MockBlockDevice::new();
        for i in 0..4u64 {
            backend
                .write_block(test_addr(100 + i), &[i as u8; 8])
                .await
                .unwrap();
        }

        let mut buf = vec![0u8; 32];
        backend
            .read_blocks(test_addr(100), 4, &mut buf)
            .await
            .unwrap();
        for i in 0..4 {
            assert_eq!(&buf[i * 8..(i + 1) * 8], &[i as u8; 8]);
        }
    }

    #[tokio::test]
    async fn test_block_crc32c_consistent() {
        let data = b"hello subvol";
        let csum1 = block_crc32c(data);
        let csum2 = block_crc32c(data);
        assert_eq!(csum1, csum2, "相同数据的 CRC32C 应一致");
    }

    #[tokio::test]
    async fn test_block_crc32c_different_data() {
        let csum_a = block_crc32c(b"data A");
        let csum_b = block_crc32c(b"data B");
        assert_ne!(csum_a, csum_b, "不同数据的 CRC32C 应不同");
    }

    #[tokio::test]
    async fn test_block_crc32c_empty() {
        let csum = block_crc32c(b"");
        assert_eq!(csum, 0, "空数据的 CRC32 应为 0");
    }

    #[tokio::test]
    async fn test_mock_write_block_with_csum() {
        let backend = MockBlockDevice::new();
        let addr = test_addr(100);
        let data = b"checksum test data";

        let csum = backend.write_block_with_csum(addr, data).await.unwrap();
        assert_eq!(csum, block_crc32c(data), "返回的校验和应与直接计算一致");
    }

    #[tokio::test]
    async fn test_mock_read_block_with_csum() {
        let backend = MockBlockDevice::new();
        let addr = test_addr(101);
        let data = b"verify me";

        backend.write_block(addr, data).await.unwrap();
        let mut buf = vec![0u8; data.len()];
        let csum = backend.read_block_with_csum(addr, &mut buf).await.unwrap();

        assert_eq!(&buf, data, "读取的数据应与写入一致");
        assert_eq!(csum, block_crc32c(data), "读取时计算的校验和应匹配");
    }

    #[tokio::test]
    async fn test_mock_read_block_zero_fill() {
        let backend = MockBlockDevice::new();
        let addr = test_addr(999);
        let mut buf = vec![0xFFu8; 32];

        backend.read_block(addr, &mut buf).await.unwrap();
        assert_eq!(buf, vec![0u8; 32], "未写入块应返回零填充");

        let csum = backend.read_block_with_csum(addr, &mut buf).await.unwrap();
        assert_eq!(
            csum,
            block_crc32c(&[0u8; 32]),
            "零填充块的校验和应匹配零数据 CRC32C"
        );
    }
}
