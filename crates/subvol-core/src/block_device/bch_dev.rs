use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::types::StorageError;
use crate::BchVol;

pub struct BchDev {
    vol: Arc<BchVol>,
    /// 设备大小（字节）
    size: u64,
    /// 文件句柄（如果是文件设备）
    file: Option<File>,
    /// 内存后备缓冲区（内存设备时使用）
    buf: Option<Arc<Mutex<Vec<u8>>>>,
}

impl BchDev {
    pub fn new(vol: Arc<BchVol>) -> Self {
        BchDev {
            vol,
            size: 0,
            file: None,
            buf: None,
        }
    }

    pub fn with_size(vol: Arc<BchVol>, size: u64) -> Self {
        BchDev {
            vol,
            size,
            file: None,
            buf: Some(Arc::new(Mutex::new(vec![0u8; size as usize]))),
        }
    }

    /// 创建文件设备
    pub fn with_file(vol: Arc<BchVol>, path: impl Into<PathBuf>, size: u64) -> Self {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path.into())
            .ok();
        if let Some(ref file) = file {
            if file.metadata().map(|meta| meta.len() < size).unwrap_or(false) {
                let _ = file.set_len(size);
            }
        }
        BchDev {
            vol,
            size,
            file,
            buf: None,
        }
    }

    /// 是否为文件设备
    pub fn has_file(&self) -> bool {
        self.file.is_some()
    }

    pub fn set_size(&mut self, size: u64) {
        self.size = size;
        if let Some(ref buf) = self.buf {
            let mut guard = buf.lock().unwrap();
            guard.resize(size as usize, 0);
        }
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn vol(&self) -> &Arc<BchVol> {
        &self.vol
    }

    /// 写入数据到设备的指定偏移
    pub async fn write_at(&self, offset: u64, data: &[u8]) -> Result<(), StorageError> {
        if offset + data.len() as u64 > self.size && self.size > 0 {
            crate::log_error!(
                "write_at 越界: offset={} len={} size={}",
                offset,
                data.len(),
                self.size
            );
            return Err(StorageError::Invalid(format!(
                "write beyond device: offset={} len={} size={}",
                offset,
                data.len(),
                self.size
            )));
        }
        if let Some(ref f) = self.file {
            f.write_all_at(data, offset).map_err(|e| {
                crate::log_error!(
                    "write_at IO 错误: offset={} len={} err={}",
                    offset,
                    data.len(),
                    e
                );
                StorageError::Io(e.to_string())
            })?;
        } else if let Some(ref buf) = self.buf {
            let mut guard = buf.lock().unwrap();
            let end = offset as usize + data.len();
            if end > guard.len() {
                guard.resize(end, 0);
            }
            guard[offset as usize..end].copy_from_slice(data);
        }
        crate::log_verbose!("write_at: offset={} len={}", offset, data.len());
        Ok(())
    }

    /// Flush completed writes to the backing device.
    ///
    /// File-backed devices use `sync_data` as the userspace equivalent of
    /// the block-layer flush/FUA barrier; memory-backed devices are already
    /// immediately visible to readers.
    pub async fn flush(&self) -> Result<(), StorageError> {
        if let Some(ref file) = self.file {
            file.sync_data().map_err(|e| {
                crate::log_error!("flush device IO error: {}", e);
                StorageError::Io(e.to_string())
            })?;
        }
        Ok(())
    }

    /// 从设备的指定偏移读取数据
    pub async fn read_at(&self, offset: u64, len: usize) -> Result<Vec<u8>, StorageError> {
        if offset + len as u64 > self.size && self.size > 0 {
            crate::log_error!(
                "read_at 越界: offset={} len={} size={}",
                offset,
                len,
                self.size
            );
            return Err(StorageError::Invalid(format!(
                "read beyond device: offset={} len={} size={}",
                offset, len, self.size
            )));
        }
        let data = if let Some(ref f) = self.file {
            let mut buf = vec![0u8; len];
            f.read_exact_at(&mut buf, offset).map_err(|e| {
                crate::log_error!("read_at IO 错误: offset={} len={} err={}", offset, len, e);
                StorageError::Io(e.to_string())
            })?;
            buf
        } else if let Some(ref buf) = self.buf {
            let guard = buf.lock().unwrap();
            let start = offset as usize;
            let end = start + len;
            if end > guard.len() {
                return Err(StorageError::Invalid(format!(
                    "read beyond buffer: offset={} len={} buf_size={}",
                    offset,
                    len,
                    guard.len()
                )));
            }
            guard[start..end].to_vec()
        } else {
            vec![0u8; len]
        };
        crate::log_verbose!("read_at: offset={} len={}", offset, len);
        Ok(data)
    }
}
