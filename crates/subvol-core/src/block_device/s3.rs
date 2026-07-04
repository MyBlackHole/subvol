use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use aws_sdk_s3::config::Region;
use aws_sdk_s3::primitives::ByteStream;
use tokio::sync::RwLock;
use tracing::{debug, error};

use super::{BlockDevice, Result};
use crate::types::{BlockAddr, HealthStatus, StorageError};

/// S3 后端配置
#[derive(Debug, Clone)]
pub struct S3Config {
    pub region: String,
    pub bucket: String,
    pub key_prefix: String,
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    pub endpoint_url: Option<String>,
}

impl Default for S3Config {
    fn default() -> Self {
        Self {
            region: "us-east-1".into(),
            bucket: "subvol".into(),
            key_prefix: String::new(),
            connect_timeout: Duration::from_secs(30),
            request_timeout: Duration::from_secs(60),
            endpoint_url: None,
        }
    }
}

/// S3 客户端操作抽象
///
/// 将 S3 SDK 调用抽象为 trait，方便测试时注入 Mock。
/// 所有方法使用 `String` 错误类型，由 `S3BlockDevice` 负责映射到 [`StorageError`]。
#[async_trait]
pub trait S3ClientOps: Send + Sync + std::fmt::Debug {
    /// 读取对象，返回完整字节数据。
    /// 当对象不存在时应返回包含 "NoSuchKey" 的错误字符串。
    async fn get_object(&self, bucket: &str, key: &str) -> std::result::Result<Vec<u8>, String>;

    /// 写入对象
    async fn put_object(
        &self,
        bucket: &str,
        key: &str,
        data: Vec<u8>,
    ) -> std::result::Result<(), String>;

    /// 删除对象
    async fn delete_object(&self, bucket: &str, key: &str) -> std::result::Result<(), String>;

    /// 检查 bucket 是否存在且可访问
    async fn head_bucket(&self, bucket: &str) -> std::result::Result<(), String>;

    /// 列出前缀下的所有对象，返回 (key, size) 列表
    async fn list_objects(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> std::result::Result<Vec<(String, u64)>, String>;
}

/// 真实的 S3 客户端，封装 `aws_sdk_s3::Client`
#[derive(Debug)]
pub struct RealS3Client {
    client: aws_sdk_s3::Client,
}

impl RealS3Client {
    pub fn new(client: aws_sdk_s3::Client) -> Self {
        Self { client }
    }
}

#[async_trait]
impl S3ClientOps for RealS3Client {
    async fn get_object(&self, bucket: &str, key: &str) -> std::result::Result<Vec<u8>, String> {
        let output = self
            .client
            .get_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| format!("{e}"))?;

        let data = output
            .body
            .collect()
            .await
            .map_err(|e| format!("S3 read stream failed: {e}"))?;

        Ok(data.into_bytes().to_vec())
    }

    async fn put_object(
        &self,
        bucket: &str,
        key: &str,
        data: Vec<u8>,
    ) -> std::result::Result<(), String> {
        self.client
            .put_object()
            .bucket(bucket)
            .key(key)
            .body(ByteStream::from(data))
            .send()
            .await
            .map_err(|e| format!("{e}"))?;
        Ok(())
    }

    async fn delete_object(&self, bucket: &str, key: &str) -> std::result::Result<(), String> {
        self.client
            .delete_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| format!("{e}"))?;
        Ok(())
    }

    async fn head_bucket(&self, bucket: &str) -> std::result::Result<(), String> {
        self.client
            .head_bucket()
            .bucket(bucket)
            .send()
            .await
            .map_err(|e| format!("{e}"))?;
        Ok(())
    }

    async fn list_objects(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> std::result::Result<Vec<(String, u64)>, String> {
        let mut objects = Vec::new();
        let mut continuation_token: Option<String> = None;

        loop {
            let mut req = self.client.list_objects_v2().bucket(bucket).prefix(prefix);

            if let Some(ref token) = continuation_token {
                req = req.continuation_token(token.clone());
            }

            let resp = req.send().await.map_err(|e| format!("{e}"))?;

            for obj in resp.contents() {
                let key = obj.key().unwrap_or("").to_string();
                let size = obj.size().unwrap_or(0).max(0) as u64;
                objects.push((key, size));
            }

            if resp.is_truncated() == Some(true) {
                continuation_token = resp.next_continuation_token().map(|s| s.to_string());
            } else {
                break;
            }
        }

        Ok(objects)
    }
}

/// S3 存储后端
#[derive(Debug)]
pub struct S3BlockDevice {
    client: Box<dyn S3ClientOps>,
    config: S3Config,
    /// MinIO 兼容模式下缓存健康状态
    healthy: Arc<RwLock<bool>>,
}

impl S3BlockDevice {
    /// 使用真实 S3 客户端创建后端
    pub async fn new(config: S3Config) -> Result<Self> {
        let mut config_loader = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(Region::new(config.region.clone()))
            .timeout_config(
                aws_config::timeout::TimeoutConfig::builder()
                    .connect_timeout(config.connect_timeout)
                    .operation_timeout(config.request_timeout)
                    .build(),
            );

        if let Some(endpoint) = &config.endpoint_url {
            config_loader = config_loader.endpoint_url(endpoint.clone());
        }

        let shared_config = config_loader.load().await;

        // 构建 S3 专用配置（添加 path-style 等 S3 特有选项）
        let mut s3_config_builder = aws_sdk_s3::config::Builder::from(&shared_config);
        if config.endpoint_url.is_some() {
            s3_config_builder = s3_config_builder.force_path_style(true);
        }
        let s3_config = s3_config_builder.build();
        let client = aws_sdk_s3::Client::from_conf(s3_config);

        Ok(Self {
            client: Box::new(RealS3Client::new(client)),
            config,
            healthy: Arc::new(RwLock::new(true)),
        })
    }

    /// 使用自定义客户端创建后端（主要用于测试）
    pub fn new_with_client(config: S3Config, client: Box<dyn S3ClientOps>) -> Self {
        Self {
            client,
            config,
            healthy: Arc::new(RwLock::new(true)),
        }
    }

    fn object_key(&self, addr: BlockAddr) -> String {
        let prefix = if self.config.key_prefix.is_empty() {
            String::new()
        } else {
            format!("{}/", self.config.key_prefix.trim_end_matches('/'))
        };
        if addr.ver > 0 {
            format!("{}blocks/{}.{}", prefix, addr.raw, addr.ver)
        } else {
            format!("{}blocks/{}", prefix, addr.raw)
        }
    }

    async fn set_healthy(&self, ok: bool) {
        let mut h = self.healthy.write().await;
        *h = ok;
    }
}

#[async_trait]
impl BlockDevice for S3BlockDevice {
    async fn read_block(&self, addr: BlockAddr, buf: &mut [u8]) -> Result<()> {
        let key = self.object_key(addr);
        debug!("s3 read: bucket={}, key={}", self.config.bucket, key);

        match self.client.get_object(&self.config.bucket, &key).await {
            Ok(data) => {
                let len = data.len().min(buf.len());
                buf[..len].copy_from_slice(&data[..len]);
                Ok(())
            }
            Err(e) => {
                if e.contains("NoSuchKey") {
                    Err(StorageError::BlockNotFound(addr))
                } else {
                    Err(StorageError::Unreachable(format!(
                        "S3 GetObject failed: {e}"
                    )))
                }
            }
        }
    }

    async fn write_block(&self, addr: BlockAddr, data: &[u8]) -> Result<()> {
        let key = self.object_key(addr);
        debug!(
            "s3 write: bucket={}, key={}, size={}",
            self.config.bucket,
            key,
            data.len()
        );

        self.client
            .put_object(&self.config.bucket, &key, data.to_vec())
            .await
            .map_err(|e| StorageError::Unreachable(format!("S3 PutObject failed: {e}")))?;

        self.set_healthy(true).await;
        Ok(())
    }

    async fn delete_block(&self, addr: BlockAddr) -> Result<()> {
        let key = self.object_key(addr);
        debug!("s3 delete: bucket={}, key={}", self.config.bucket, key);

        self.client
            .delete_object(&self.config.bucket, &key)
            .await
            .map_err(|e| StorageError::Unreachable(format!("S3 DeleteObject failed: {e}")))?;

        Ok(())
    }

    async fn trim_block(&self, addr: BlockAddr) -> Result<()> {
        // S3 trim = delete
        self.delete_block(addr).await
    }

    async fn flush(&self) -> Result<()> {
        // S3 写操作已经是强一致的（单对象 put）
        Ok(())
    }

    async fn health_check(&self) -> Result<HealthStatus> {
        match self.client.head_bucket(&self.config.bucket).await {
            Ok(_) => {
                self.set_healthy(true).await;
                Ok(HealthStatus::Healthy)
            }
            Err(e) => {
                let msg = format!("S3 health check failed: {e}");
                error!("{msg}");
                self.set_healthy(false).await;
                Ok(HealthStatus::Unreachable {
                    reason: format!("bucket '{}' check failed: {e}", self.config.bucket),
                })
            }
        }
    }

    async fn used_space(&self) -> Result<u64> {
        let objects = self
            .client
            .list_objects(&self.config.bucket, &self.config.key_prefix)
            .await
            .map_err(|e| StorageError::Unreachable(format!("S3 ListObjectsV2 failed: {e}")))?;

        Ok(objects.iter().map(|(_, size)| *size).sum())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use async_trait::async_trait;
    use tokio::sync::RwLock;

    use super::*;

    /// 基于 HashMap 的模拟 S3 客户端
    #[derive(Debug)]
    pub struct MockS3Client {
        data: RwLock<HashMap<String, Vec<u8>>>,
    }

    impl MockS3Client {
        pub fn new() -> Self {
            Self {
                data: RwLock::new(HashMap::new()),
            }
        }
    }

    #[async_trait]
    impl S3ClientOps for MockS3Client {
        async fn get_object(
            &self,
            _bucket: &str,
            key: &str,
        ) -> std::result::Result<Vec<u8>, String> {
            let data = self.data.read().await;
            data.get(key)
                .cloned()
                .ok_or_else(|| format!("NoSuchKey: key {key} not found"))
        }

        async fn put_object(
            &self,
            _bucket: &str,
            key: &str,
            data: Vec<u8>,
        ) -> std::result::Result<(), String> {
            self.data.write().await.insert(key.to_string(), data);
            Ok(())
        }

        async fn delete_object(&self, _bucket: &str, key: &str) -> std::result::Result<(), String> {
            self.data.write().await.remove(key);
            Ok(())
        }

        async fn head_bucket(&self, _bucket: &str) -> std::result::Result<(), String> {
            Ok(())
        }

        async fn list_objects(
            &self,
            _bucket: &str,
            prefix: &str,
        ) -> std::result::Result<Vec<(String, u64)>, String> {
            let data = self.data.read().await;
            let results: Vec<(String, u64)> = data
                .iter()
                .filter(|(k, _)| k.starts_with(prefix))
                .map(|(k, v)| (k.clone(), v.len() as u64))
                .collect();
            Ok(results)
        }
    }

    fn test_addr(raw: u64) -> BlockAddr {
        BlockAddr::new(raw)
    }

    #[tokio::test]
    async fn test_s3_blockdevice_write_read() {
        let config = S3Config::default();
        let client = Box::new(MockS3Client::new());
        let device = S3BlockDevice::new_with_client(config, client);

        let addr = test_addr(42);
        let data = b"hello s3 block";
        device.write_block(addr, data).await.unwrap();

        let mut buf = vec![0u8; data.len()];
        device.read_block(addr, &mut buf).await.unwrap();
        assert_eq!(&buf, data);
    }

    #[tokio::test]
    async fn test_s3_blockdevice_delete() {
        let config = S3Config::default();
        let client = Box::new(MockS3Client::new());
        let device = S3BlockDevice::new_with_client(config, client);

        let addr = test_addr(1);
        device.write_block(addr, b"data").await.unwrap();

        let mut buf = vec![0u8; 4];
        device.read_block(addr, &mut buf).await.unwrap();
        assert_eq!(&buf, b"data");

        device.delete_block(addr).await.unwrap();
        let result = device.read_block(addr, &mut buf).await;
        assert!(matches!(result, Err(StorageError::BlockNotFound(_))));
    }

    #[tokio::test]
    async fn test_s3_blockdevice_trim() {
        let config = S3Config::default();
        let client = Box::new(MockS3Client::new());
        let device = S3BlockDevice::new_with_client(config, client);

        let addr = test_addr(2);
        device.write_block(addr, b"trim data").await.unwrap();
        device.trim_block(addr).await.unwrap();

        let result = device.read_block(addr, &mut [0u8; 9]).await;
        assert!(matches!(result, Err(StorageError::BlockNotFound(_))));
    }

    #[tokio::test]
    async fn test_s3_blockdevice_used_space() {
        let config = S3Config::default();
        let client = Box::new(MockS3Client::new());
        let device = S3BlockDevice::new_with_client(config, client);

        assert_eq!(device.used_space().await.unwrap(), 0);

        let addr1 = test_addr(10);
        let addr2 = test_addr(20);
        device.write_block(addr1, b"1234567890").await.unwrap();
        device.write_block(addr2, b"abcdefghij").await.unwrap();
        assert_eq!(device.used_space().await.unwrap(), 20);
    }

    #[tokio::test]
    async fn test_s3_blockdevice_health() {
        let config = S3Config::default();
        let client = Box::new(MockS3Client::new());
        let device = S3BlockDevice::new_with_client(config, client);

        let health = device.health_check().await.unwrap();
        assert_eq!(health, HealthStatus::Healthy);
    }
}
