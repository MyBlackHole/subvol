use std::ffi::OsStr;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use fuser::{
    FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo, KernelConfig,
    LockOwner, MountOption, OpenFlags, ReplyAttr, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, ReplyWrite, Request, SessionACL, WriteFlags,
};

use subvol_core::{BchVol, StorageError};

const ROOT_INODE: u64 = 1;
const TTL: Duration = Duration::MAX;

fn subvol_ino(id: u32) -> u64 {
    id as u64 + 1
}

fn ino_subvol(ino: u64) -> Option<u32> {
    if ino == ROOT_INODE {
        None
    } else {
        Some((ino - 1) as u32)
    }
}

struct SubvolEntry {
    id: u32,
    capacity: u64,
    read_only: bool,
}

pub struct VolFuseFs {
    vol: Arc<BchVol>,
    subvols: Vec<SubvolEntry>,
    rt: tokio::runtime::Runtime,
    signal_fd: Option<File>,
}

fn storage_errno(error: &StorageError) -> fuser::Errno {
    match error {
        StorageError::Io(error)
            if error.kind() == std::io::ErrorKind::PermissionDenied
                && error.to_string() == "subvolume is read-only" =>
        {
            fuser::Errno::EROFS
        }
        StorageError::Io(error) => error
            .raw_os_error()
            .map(fuser::Errno::from_i32)
            .unwrap_or(fuser::Errno::EIO),
        StorageError::Unreachable(_) => fuser::Errno::EHOSTUNREACH,
        StorageError::VolumeNotFound(_) => fuser::Errno::ENOENT,
        StorageError::NotFound(message) if message == "no writable extent device" => {
            fuser::Errno::ENOSPC
        }
        StorageError::NotFound(message) if message == "no online extent replica" => {
            fuser::Errno::EIO
        }
        StorageError::NotFound(_) => fuser::Errno::ENOENT,
        StorageError::InvalidArgument(_) | StorageError::InvalidBlockSize(_) => {
            fuser::Errno::EINVAL
        }
        StorageError::AddressSpaceExhausted { .. }
        | StorageError::BtreeNodeFull
        | StorageError::WatermarkTooLow { .. } => fuser::Errno::ENOSPC,
        StorageError::QuotaExceeded { .. } => fuser::Errno::EDQUOT,
        StorageError::AlreadyExists(_) => fuser::Errno::EEXIST,
        StorageError::ChecksumMismatch { .. } => fuser::Errno::EIO,
        _ => fuser::Errno::EIO,
    }
}

fn root_dir_attr() -> FileAttr {
    FileAttr {
        ino: INodeNo(ROOT_INODE),
        size: 0,
        blocks: 0,
        atime: SystemTime::UNIX_EPOCH,
        mtime: SystemTime::UNIX_EPOCH,
        ctime: SystemTime::UNIX_EPOCH,
        crtime: SystemTime::UNIX_EPOCH,
        kind: FileType::Directory,
        perm: 0o755,
        nlink: 2,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

fn vol_file_attr(size: u64, read_only: bool) -> FileAttr {
    FileAttr {
        ino: INodeNo(0),
        size,
        blocks: size / 512,
        atime: SystemTime::UNIX_EPOCH,
        mtime: SystemTime::UNIX_EPOCH,
        ctime: SystemTime::UNIX_EPOCH,
        crtime: SystemTime::UNIX_EPOCH,
        kind: FileType::RegularFile,
        perm: if read_only { 0o444 } else { 0o644 },
        nlink: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

impl VolFuseFs {
    /// 创建 FUSE 实例，内部创建 tokio multi-thread runtime。
    /// 卷的 open/start 必须在此 runtime 上完成以保证后台任务存活。
    pub fn new(vol: Arc<BchVol>) -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("failed to create tokio runtime for FUSE");
        Self::with_runtime(vol, rt, None)
    }

    /// 在现有 runtime 上创建 FUSE 实例。
    ///
    /// `open_pool` 等异步操作用 `rt.block_on()` 执行后，journal 后台
    /// 任务将在 `rt` 上长期运行；生命周期绑定到 mount 会话。
    pub fn with_runtime(
        vol: Arc<BchVol>,
        rt: tokio::runtime::Runtime,
        signal_fd: Option<File>,
    ) -> Self {
        Self {
            vol,
            subvols: Vec::new(),
            rt,
            signal_fd,
        }
    }

    /// 创建 FUSE 实例，使用内部 runtime，附带 signal FD。
    /// 等价于 `with_runtime(vol, rt, signal_fd)` 但构建内部 runtime。
    pub fn with_signal(vol: Arc<BchVol>, signal_fd: Option<File>) -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("failed to create tokio runtime for FUSE");
        Self::with_runtime(vol, rt, signal_fd)
    }

    pub fn mount(self, mountpoint: &Path) -> Result<(), std::io::Error> {
        let mut config = fuser::Config::default();
        config.mount_options = vec![
            MountOption::AutoUnmount,
            MountOption::FSName("subvol-pool".to_string()),
            MountOption::CUSTOM("subtype=subvol".to_string()),
        ];
        config.acl = SessionACL::All;
        fuser::mount2(self, mountpoint, &config)
    }

    fn subvol_by_id(&self, id: u32) -> Option<&SubvolEntry> {
        self.subvols.iter().find(|sv| sv.id == id)
    }

    fn read_vol(
        &self,
        subvol_id: u32,
        offset: u64,
        buf: Vec<u8>,
    ) -> Result<Vec<u8>, subvol_core::StorageError> {
        if buf.is_empty() {
            return Ok(buf);
        }
        let bs = self.vol.block_size() as u64;
        if offset % bs == 0 && buf.len() as u64 % bs == 0 {
            let read_len = buf.len();
            let rbio = self.rt.block_on(async {
                let mut rbio = subvol_core::io::BchReadBio {
                    data: buf,
                    offset_into_extent: 0,
                    flags: 0,
                };
                let iter = subvol_core::io::BvecIter {
                    bi_sector: offset >> 9,
                    bi_size: read_len as u32,
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
                Ok::<_, subvol_core::StorageError>(rbio)
            })?;
            Ok(rbio.data)
        } else {
            let block_start = offset / bs;
            let block_end = (offset + buf.len() as u64 + bs - 1) / bs;
            let nblocks = (block_end - block_start) as usize;
            let aligned_off = block_start * bs;
            let aligned_len = nblocks * bs as usize;
            let aligned_buf = vec![0u8; aligned_len];
            let rbio = self.rt.block_on(async {
                let mut rbio = subvol_core::io::BchReadBio {
                    data: aligned_buf,
                    offset_into_extent: 0,
                    flags: 0,
                };
                let iter = subvol_core::io::BvecIter {
                    bi_sector: aligned_off >> 9,
                    bi_size: aligned_len as u32,
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
                Ok::<_, subvol_core::StorageError>(rbio)
            })?;
            let start = (offset - aligned_off) as usize;
            Ok(rbio.data[start..start + buf.len()].to_vec())
        }
    }
}

impl Filesystem for VolFuseFs {
    fn init(&mut self, _req: &Request, _config: &mut KernelConfig) -> Result<(), std::io::Error> {
        let subvols = self.rt.block_on(async {
            let list = self.vol.list_subvols().await;
            let mut entries: Vec<SubvolEntry> = Vec::new();
            let vol_capacity = self.vol.capacity();
            for (id, subvol) in &list {
                let capacity = if subvol.size != 0 {
                    subvol.size
                } else {
                    vol_capacity
                };
                let ro = subvol.flags.contains(subvol_core::subvol::BchSubvolumeFlags::READ_ONLY);
                entries.push(SubvolEntry { id: *id, capacity, read_only: ro });
            }
            entries
        });
        self.subvols = subvols;

        if let Some(mut fd) = self.signal_fd.take() {
            let _ = fd.write_all(&[0]);
        }
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        if parent != INodeNo(ROOT_INODE) {
            reply.error(fuser::Errno::ENOENT);
            return;
        }
        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(fuser::Errno::ENOENT);
                return;
            }
        };
        let id: u32 = match name_str.parse() {
            Ok(id) => id,
            Err(_) => {
                reply.error(fuser::Errno::ENOENT);
                return;
            }
        };
        match self.subvol_by_id(id) {
            Some(entry) => {
                let mut attr = vol_file_attr(entry.capacity, entry.read_only);
                attr.ino = INodeNo(subvol_ino(id));
                reply.entry(&TTL, &attr, Generation(0));
            }
            None => reply.error(fuser::Errno::ENOENT),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        if ino == INodeNo(ROOT_INODE) {
            reply.attr(&TTL, &root_dir_attr());
            return;
        }
        let id = match ino_subvol(ino.0) {
            Some(id) => id,
            None => {
                reply.error(fuser::Errno::ENOENT);
                return;
            }
        };
        match self.subvol_by_id(id) {
            Some(entry) => {
                let mut attr = vol_file_attr(entry.capacity, entry.read_only);
                attr.ino = ino;
                reply.attr(&TTL, &attr);
            }
            None => reply.error(fuser::Errno::ENOENT),
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        if ino != INodeNo(ROOT_INODE) {
            reply.error(fuser::Errno::ENOENT);
            return;
        }
        let mut entries: Vec<(INodeNo, FileType, String)> = vec![
            (INodeNo(ROOT_INODE), FileType::Directory, ".".to_string()),
            (INodeNo(ROOT_INODE), FileType::Directory, "..".to_string()),
        ];
        for sv in &self.subvols {
            entries.push((
                INodeNo(subvol_ino(sv.id)),
                FileType::RegularFile,
                sv.id.to_string(),
            ));
        }
        for (i, (ino, kind, name)) in entries.iter().enumerate().skip(offset as usize) {
            if !reply.add(*ino, (i + 1) as u64, *kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let id = match ino_subvol(ino.0) {
            Some(id) => id,
            None => {
                reply.error(fuser::Errno::ENOENT);
                return;
            }
        };
        let entry = match self.subvol_by_id(id) {
            Some(e) => e,
            None => {
                reply.error(fuser::Errno::ENOENT);
                return;
            }
        };
        if entry.read_only && !matches!(flags.acc_mode(), fuser::OpenAccMode::O_RDONLY) {
            reply.error(fuser::Errno::EROFS);
            return;
        }
        reply.opened(FileHandle(0), FopenFlags::FOPEN_KEEP_CACHE);
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let id = match ino_subvol(ino.0) {
            Some(id) => id,
            None => {
                reply.error(fuser::Errno::ENOENT);
                return;
            }
        };
        let entry = match self.subvol_by_id(id) {
            Some(e) => e,
            None => {
                reply.error(fuser::Errno::ENOENT);
                return;
            }
        };
        if offset >= entry.capacity {
            reply.data(&[]);
            return;
        }
        let len = (size as u64).min(entry.capacity - offset) as usize;
        let buf = vec![0u8; len];
        match self.read_vol(id, offset, buf) {
            Ok(buf) => reply.data(&buf),
            Err(err) => {
                eprintln!("FUSE read failed at offset {offset}, len {len}: {err}");
                reply.error(storage_errno(&err));
            }
        }
    }

    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let id = match ino_subvol(ino.0) {
            Some(id) => id,
            None => {
                reply.error(fuser::Errno::ENOENT);
                return;
            }
        };
        let entry = match self.subvol_by_id(id) {
            Some(e) => e,
            None => {
                reply.error(fuser::Errno::ENOENT);
                return;
            }
        };
        if entry.read_only {
            reply.error(fuser::Errno::EROFS);
            return;
        }
        if offset >= entry.capacity {
            reply.error(fuser::Errno::ENOSPC);
            return;
        }
        let len = (data.len() as u64).min(entry.capacity - offset) as usize;
        let buf = &data[..len];
        let bs = self.vol.block_size() as u64;
        let aligned = offset % bs == 0 && len as u64 % bs == 0;
        let result = if aligned {
            self.rt.block_on(async {
                let mut op = subvol_core::io::BchWriteOp {
                    flags: subvol_core::io::BchWriteFlags::SYNC,
                    subvol: id,
                    pos: subvol_core::btree::Bpos::new(0, offset, id),
                    data: buf.to_vec(),
                    csum_type: 5,
                    compression_opt: 0,
                    nr_replicas: self.vol.opts.data_replicas.max(1),
                    watermark: 0,
                };
                self.vol.bch2_write(&mut op).await
            })
        } else {
            let block_start = offset / bs;
            let block_end = (offset + len as u64 + bs - 1) / bs;
            let nblocks = (block_end - block_start) as usize;
            let aligned_off = block_start * bs;
            let aligned_len = nblocks * bs as usize;
            let tmp = match self.read_vol(id, aligned_off, vec![0u8; aligned_len]) {
                Ok(b) => b,
                Err(err) => {
                    eprintln!("FUSE write read-modify failed: {err}");
                    reply.error(storage_errno(&err));
                    return;
                }
            };
            let data_off = (offset - aligned_off) as usize;
            let mut write_tmp = tmp;
            write_tmp[data_off..data_off + len].copy_from_slice(buf);
            let result = self.rt.block_on(async {
                for bi in 0..nblocks {
                    let boff = bi * bs as usize;
                    let block_data = &write_tmp[boff..boff + bs as usize];
                    let mut op = subvol_core::io::BchWriteOp {
                        flags: subvol_core::io::BchWriteFlags::SYNC,
                        subvol: id,
                        pos: subvol_core::btree::Bpos::new(0, aligned_off + bi as u64 * bs, id),
                        data: block_data.to_vec(),
                        csum_type: 5,
                        compression_opt: 0,
                        nr_replicas: self.vol.opts.data_replicas.max(1),
                        watermark: 0,
                    };
                    self.vol.bch2_write(&mut op).await?;
                }
                Ok::<_, StorageError>(())
            });
            result
        };
        match result {
            Ok(()) => reply.written(len as u32),
            Err(err) => {
                eprintln!("FUSE write failed: {err}");
                reply.error(storage_errno(&err));
            }
        }
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        let result = self.rt.block_on(async { self.vol.flush().await });
        match result {
            Ok(()) => reply.ok(),
            Err(err) => {
                eprintln!("FUSE flush failed: {err}");
                reply.error(storage_errno(&err));
            }
        }
    }

    fn fallocate(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        length: u64,
        mode: i32,
        reply: ReplyEmpty,
    ) {
        let id = match ino_subvol(ino.0) {
            Some(id) => id,
            None => {
                reply.error(fuser::Errno::ENOENT);
                return;
            }
        };
        let entry = match self.subvol_by_id(id) {
            Some(e) => e,
            None => {
                reply.error(fuser::Errno::ENOENT);
                return;
            }
        };
        if entry.read_only && mode & 0x02 == 0 {
            reply.error(fuser::Errno::EROFS);
            return;
        }
        // Mode 0x02 = FALLOC_FL_KEEP_SIZE, 0x08 = FALLOC_FL_PUNCH_HOLE
        if mode & 0x08 != 0 {
            let _ = self.rt.block_on(async {
                let snapshot_id = {
                    let trans = subvol_core::btree::BtreeTrans::new_ro(&self.vol);
                    subvol_core::subvol::bch2_subvolume_get_snapshot(&trans, id)?
                };
                let bs = self.vol.block_size() as u64;
                let start_block = offset / bs;
                let nblocks = length / bs;
                if nblocks == 0 {
                    return Ok::<(), StorageError>(());
                }
                let end_block = start_block + nblocks;
                self.vol
                    .bch2_btree_delete_range(
                        subvol_core::btree::BtreeId::Extents,
                        subvol_core::btree::Bpos::new(0, start_block, snapshot_id),
                        subvol_core::btree::Bpos::new(0, end_block, snapshot_id),
                        0,
                    )
                    .await
            });
            reply.ok();
        } else {
            reply.error(fuser::Errno::ENOSYS);
        }
    }
}
