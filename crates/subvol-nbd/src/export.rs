use std::sync::Arc;

use subvol_core::subvol::{
    bch2_subvol_is_ro, bch2_subvolume_get, bch2_subvolume_get_snapshot, BCACHEFS_ROOT_SUBVOL,
};
use subvol_core::BchVol;

/// NBD 导出定义
///
/// 一个导出对应一个卷，直接携带 `BchVol`（对应 bcachefs fuse 携带 `bch_fs` 的模式）。
/// `subvol_id = None` 时读写 root subvolume，`Some(id)` 时读写指定子卷。
pub struct NbdExport {
    /// 导出名称（卷名）
    pub name: String,
    /// NBD 传输标记
    pub flags: u16,
    /// 卷实例
    pub vol: Arc<BchVol>,
    /// 子卷 ID（None = root subvolume）
    pub subvol_id: Option<u32>,
    /// 导出设备边界；与 FUSE 的 regular-file size 保持一致。
    pub(crate) capacity: u64,
}

impl NbdExport {
    pub(crate) fn validate_range(
        &self,
        offset: u64,
        len: u64,
    ) -> Result<(), subvol_core::StorageError> {
        if len == 0 {
            return Ok(());
        }

        let size = self.size();
        let end = offset.checked_add(len).ok_or_else(|| {
            subvol_core::StorageError::InvalidArgument(format!(
                "request range overflows export size: offset={offset} len={len} size={size}"
            ))
        })?;

        if end > size {
            return Err(subvol_core::StorageError::InvalidArgument(format!(
                "request range exceeds export size: offset={offset} len={len} size={size}"
            )));
        }

        Ok(())
    }

    /// 创建普通卷导出（读写 root snapshot）
    pub fn new(name: impl Into<String>, vol: Arc<BchVol>) -> Self {
        let flags = crate::protocol::NBD_FLAG_HAS_FLAGS
            | crate::protocol::NBD_FLAG_SEND_FLUSH
            | crate::protocol::NBD_FLAG_SEND_FUA
            | crate::protocol::NBD_FLAG_SEND_TRIM;
        let capacity = vol.capacity();
        Self {
            name: name.into(),
            flags,
            vol,
            subvol_id: None,
            capacity,
        }
    }

    /// 创建子卷导出（只读状态由子卷 BCH_SUBVOLUME_RO/UNLINKED 标志决定）
    pub fn new_with_subvol(name: impl Into<String>, vol: Arc<BchVol>, subvol_id: u32) -> Self {
        let flags = crate::protocol::NBD_FLAG_HAS_FLAGS
            | crate::protocol::NBD_FLAG_SEND_FLUSH
            | crate::protocol::NBD_FLAG_SEND_FUA
            | crate::protocol::NBD_FLAG_SEND_TRIM;
        // The local bcachefs `struct bch_subvolume` has no size field; this
        // repository's size extension is the block-export boundary used by
        // FUSE as well.  Zero/legacy records retain the whole-volume boundary.
        let capacity = {
            let trans = subvol_core::btree::BtreeTrans::new_ro(&vol);
            bch2_subvolume_get(&trans, subvol_id, true)
                .ok()
                .and_then(|subvol| (subvol.size != 0).then_some(subvol.size))
                .unwrap_or_else(|| vol.capacity())
        };
        Self {
            name: name.into(),
            flags,
            vol,
            subvol_id: Some(subvol_id),
            capacity,
        }
    }

    /// 卷大小（字节）
    pub fn size(&self) -> u64 {
        self.capacity
    }

    /// Whether this export is immutable. Snapshot subvolumes are marked
    /// `BCH_SUBVOLUME_RO` by bcachefs `bch2_subvolume_snapshot()`
    /// (`fs/snapshots/subvolume.c:644`) and `bch2_subvol_is_ro()` rejects
    /// writes for that flag (`fs/snapshots/subvolume.c:323-329`); the export
    /// must therefore remain read-only at the block protocol boundary.
    pub fn is_read_only(&self) -> bool {
        if self.flags & crate::protocol::NBD_FLAG_READ_ONLY != 0 || self.vol.is_read_only() {
            return true;
        }
        self.subvol_id.is_some_and(|subvol_id| {
            let trans = subvol_core::btree::BtreeTrans::new_ro(&self.vol);
            bch2_subvol_is_ro(&trans, subvol_id).is_err()
        })
    }

    /// 读取 extent（自动路由到 root 或子卷）
    pub async fn read(
        &self,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<(), subvol_core::StorageError> {
        self.validate_range(offset, buf.len() as u64)?;
        let subvol_id = self.subvol_id.unwrap_or(BCACHEFS_ROOT_SUBVOL as u32);
        let mut rbio = subvol_core::io::BchReadBio {
            data: buf.to_vec(),
            offset_into_extent: 0,
            flags: 0,
        };
        let iter = subvol_core::io::BvecIter {
            bi_sector: offset >> 9,
            bi_size: buf.len() as u32,
        };
        let inum = subvol_core::io::SubvolInum {
            subvol: subvol_id as u64,
            inum: 0,
        };
        let mut failed = subvol_core::io::BchIoFailures {
            nr: 0,
            data: vec![],
        };
        let mut prev_read = subvol_core::io::BkeyBuf { k: None, v: None };
        let mut trans = subvol_core::btree::BtreeTrans::new_ro(&self.vol);
        self.vol
            .bch2_read(
                &mut trans,
                &mut rbio,
                iter,
                inum,
                &mut failed,
                &mut prev_read,
                subvol_core::io::BchReadFlags::empty(),
            )
            .await?;
        buf.copy_from_slice(&rbio.data[..buf.len()]);
        Ok(())
    }

    /// 写入 extent（自动路由到 root 或子卷）
    pub async fn write(&self, offset: u64, buf: &[u8]) -> Result<(), subvol_core::StorageError> {
        if self.is_read_only() {
            return Err(subvol_core::StorageError::InvalidData(
                "export is read-only".into(),
            ));
        }
        self.validate_range(offset, buf.len() as u64)?;
        let sid = self.subvol_id.unwrap_or(BCACHEFS_ROOT_SUBVOL as u32);
        let mut op = subvol_core::io::BchWriteOp {
            flags: subvol_core::io::BchWriteFlags::SYNC,
            subvol: sid,
            pos: subvol_core::btree::Bpos::new(0, offset, sid),
            data: buf.to_vec(),
            csum_type: 5,
            compression_opt: 0,
            // Match bcachefs write.c:2736-2747: carry the configured data
            // replica count into the write operation.  The core allocator
            // also validates this against the currently writable devices.
            nr_replicas: self.vol.opts.data_replicas.max(1),
            watermark: 0,
        };
        self.vol.bch2_write(&mut op).await
    }

    /// 裁剪 extent（自动路由到 root 或子卷）
    pub async fn trim(&self, offset: u64, len: u64) -> Result<(), subvol_core::StorageError> {
        if self.is_read_only() {
            return Err(subvol_core::StorageError::InvalidData(
                "export is read-only".into(),
            ));
        }
        self.validate_range(offset, len)?;
        if len == 0 {
            return Ok(());
        }
        let bs = self.vol.block_size() as u64;
        let start_block = offset / bs;
        let nblocks = len
            .checked_add(bs - 1)
            .and_then(|len| len.checked_div(bs))
            .ok_or_else(|| {
                subvol_core::StorageError::InvalidArgument(
                    "trim range overflows block rounding".into(),
                )
            })?;
        let end_block = start_block.checked_add(nblocks).ok_or_else(|| {
            subvol_core::StorageError::InvalidArgument(
                "trim range overflows btree position".into(),
            )
        })?;
        let sid = self.subvol_id.unwrap_or(BCACHEFS_ROOT_SUBVOL as u32);
        // bcachefs range keys are snapshot-scoped.  The NBD API carries a
        // subvolume ID, so resolve it exactly as bch2_read/bch2_write do
        // before invoking bch2_btree_delete_range; trim-hole tracking must
        // use the same snapshot namespace as subsequent reads.
        let snapshot_id = {
            let trans = subvol_core::btree::BtreeTrans::new_ro(&self.vol);
            bch2_subvolume_get_snapshot(&trans, sid)?
        };
        self.vol
            .bch2_btree_delete_range(
                subvol_core::btree::BtreeId::Extents,
                subvol_core::btree::Bpos::new(0, start_block, snapshot_id),
                subvol_core::btree::Bpos::new(0, end_block, snapshot_id),
                0,
            )
            .await
    }

    /// 刷新后端缓存
    pub async fn flush(&self) -> Result<(), subvol_core::StorageError> {
        self.vol.flush().await
    }
}
