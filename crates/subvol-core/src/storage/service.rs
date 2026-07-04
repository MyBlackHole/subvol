//! StorageService — 块设备存储服务
//!
//! 封装块设备布局知识。Volume 不直接操作 Superblock 类型，
//! 全部通过 StorageService API 访问元数据和块 I/O。

use std::sync::Arc;

use crate::block_device::BchDev;
use crate::journal::JournalSuperblockState;
use crate::types::StorageError;
use crate::BchVol;

use super::superblock::BchSb;

/// 块设备存储服务
///
/// 管理块设备级元数据（Superblock）和后端刷新。
pub struct StorageService {
    vol: Arc<BchVol>,
    device: Arc<BchDev>,
    sb: BchSb,
}

impl StorageService {
    /// 按 superblock 内的 metadata id 解析设备后创建设备服务。
    pub async fn create_on_sb(vol: Arc<BchVol>, sb: BchSb) -> Result<Self, StorageError> {
        let mut sb = sb;
        sb.normalize_members();
        let primary_idx = sb.primary_dev_idx;
        let dev = vol
            .primary_device_rcu_noerror()
            .ok_or_else(|| StorageError::NotFound(format!("device {} not found", primary_idx)))?;
        if !dev.is_online() {
            return Err(StorageError::NotFound(format!(
                "device {} not found",
                primary_idx
            )));
        }
        sb.write_to_device(&dev).await?;
        Ok(Self {
            vol,
            device: dev,
            sb,
        })
    }

    /// 按 superblock 内的 metadata id 解析设备后打开设备服务。
    pub async fn open_on_sb(vol: Arc<BchVol>) -> Result<Self, StorageError> {
        let mut sb = vol.superblock().clone();
        sb.normalize_members();
        let primary_idx = sb.primary_dev_idx;
        let dev = vol
            .primary_device_rcu_noerror()
            .ok_or_else(|| StorageError::NotFound(format!("device {} not found", primary_idx)))?;
        if !dev.is_online() {
            return Err(StorageError::NotFound(format!(
                "device {} not found",
                primary_idx
            )));
        }
        let sb = BchSb::read_from_device(&dev).await?;
        Ok(Self {
            vol,
            device: dev,
            sb,
        })
    }

    /// 关闭设备（写 Superblock + flush）
    pub async fn close(&mut self) -> Result<(), StorageError> {
        let write_result = self.sb.write_to_device(&self.device).await;
        let flush_result = self.device.bdev().flush().await;
        // 两个持久化阶段都必须执行；保留 superblock 写入错误优先级，
        // 但不能因首个错误跳过后端 flush。
        match write_result {
            Err(error) => Err(error),
            Ok(()) => flush_result,
        }
    }

    // ──── Superblock 字段访问器（Volume 不直接操作 Superblock 类型） ────

    pub fn superblock(&self) -> &BchSb {
        &self.sb
    }

    pub fn journal_seq(&self) -> u64 {
        self.sb.journal_seq
    }

    pub fn clean_shutdown(&self) -> bool {
        self.sb.clean_shutdown
    }

    pub fn set_journal_seq(&mut self, seq: u64) {
        self.sb.journal_seq = seq;
    }

    pub fn set_clean_shutdown(&mut self, val: bool) {
        self.sb.clean_shutdown = val;
    }

    // ──── Journal 字段访问器（Wave 1 新增） ────

    pub fn journal_buckets(&self) -> &[u64] {
        &self.sb.journal_buckets
    }

    pub fn journal_last_seq(&self) -> u64 {
        self.sb.journal_last_seq
    }

    pub fn journal_last_bucket(&self) -> u32 {
        self.sb.journal_last_bucket
    }

    pub fn root_addrs(&self) -> &[u64] {
        &self.sb.root_addrs
    }

    /// 将 superblock 中的 journal 字段转换为 JournalSuperblockState
    pub fn journal_superblock_state(&self) -> JournalSuperblockState {
        let n = self.sb.journal_buckets.len();
        JournalSuperblockState {
            bucket_addrs: self.sb.journal_buckets.clone(),
            last_seq: self.sb.journal_last_seq,
            last_seq_ondisk: self.sb.journal_seq,
            last_bucket: self.sb.journal_last_bucket,
            discard_idx: self.sb.journal_discard_idx,
            dirty_idx: self.sb.journal_dirty_idx,
            dirty_idx_ondisk: self.sb.journal_dirty_idx_ondisk,
            bucket_seq: if self.sb.journal_bucket_seq.len() == n {
                self.sb.journal_bucket_seq.clone()
            } else {
                vec![0; n]
            },
            replayed_seqs: self.sb.replayed_seqs.clone(),
        }
    }

    pub fn set_journal_buckets(&mut self, buckets: Vec<u64>) {
        self.sb.journal_buckets = buckets;
    }

    pub fn set_journal_last_seq(&mut self, seq: u64) {
        self.sb.journal_last_seq = seq;
    }

    pub fn set_journal_last_bucket(&mut self, idx: u32) {
        self.sb.journal_last_bucket = idx;
    }

    pub fn set_root_addr(&mut self, ty_index: usize, addr: u64) {
        // 确保 Vec 长度足够
        if ty_index >= self.sb.root_addrs.len() {
            self.sb.root_addrs.resize(ty_index + 1, 0);
        }
        self.sb.root_addrs[ty_index] = addr;
    }

    // ──── 后端刷新 ────

    pub async fn flush(&self) -> Result<(), StorageError> {
        self.device.bdev().flush().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_device::{BchDev, MockBlockDevice};
    use crate::storage::superblock::BchSbMember;
    use crate::BchVol;

    fn test_sb() -> BchSb {
        BchSb::with_volume_info(
            "test-vol".into(),
            1,
            "pool".into(),
            4096,
            64 * 1024 * 1024,
            crate::types::BackendType::Nfs,
        )
    }

    #[tokio::test]
    async fn test_create_open_roundtrip() {
        let sb = test_sb();
        let vol = BchVol::test_trees();
        let ca = vol.primary_device_rcu_noerror().unwrap();
        let nbuckets = 65536 / crate::alloc::BLOCKS_PER_BUCKET;
        vol.superblock_mut()
            .member_mut(ca.dev_idx)
            .unwrap()
            .nbuckets = nbuckets;
        crate::alloc::bch2_dev_buckets_resize(&vol, &ca, nbuckets).unwrap();
        let vol = Arc::new(vol);

        let mut svc = StorageService::create_on_sb(vol.clone(), sb.clone())
            .await
            .unwrap();
        {
            let actual = svc.superblock();
            assert_eq!(actual.vol_name, sb.vol_name);
            assert_eq!(actual.vol_id, sb.vol_id);
            assert_eq!(actual.block_size, sb.block_size);
            assert_eq!(actual.capacity, sb.capacity);
        }
        assert!(!svc.clean_shutdown());

        svc.set_clean_shutdown(true);
        svc.close().await.unwrap();

        // 重新打开
        let svc2 = StorageService::open_on_sb(vol.clone()).await.unwrap();
        {
            let actual = svc2.superblock();
            assert_eq!(actual.vol_name, sb.vol_name);
            assert_eq!(actual.capacity, sb.capacity);
        }
        assert!(svc2.clean_shutdown());
    }


    #[tokio::test]
    async fn create_open_falls_back_when_primary_device_is_offline() {
        let mut vol = BchVol::test_trees();
        let dev1 = Arc::new(BchDev::new(Arc::new(MockBlockDevice::new()), 1));
        vol.device_registry.insert_bch_dev(dev1);
        vol.superblock_mut()
            .members
            .push(BchSbMember::new(1, "dev-1"));
        vol.primary_device_rcu_noerror().unwrap().set_offline();
        let vol = Arc::new(vol);

        let sb = test_sb();
        let svc = StorageService::create_on_sb(vol.clone(), sb.clone())
            .await
            .unwrap();
        assert_eq!(svc.superblock().vol_name, sb.vol_name);
        let reopened = StorageService::open_on_sb(vol).await.unwrap();
        assert_eq!(reopened.superblock().vol_name, sb.vol_name);
    }
}
