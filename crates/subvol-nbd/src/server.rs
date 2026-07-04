use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use bytes::BytesMut;
use tokio::net::UnixListener;
use tokio::sync::{Mutex, Notify, OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock, Semaphore};
use tokio::task::JoinSet;

use crate::error::{NbdError, NbdResult};
use crate::export::NbdExport;
use crate::handshake;
use crate::protocol::*;

pub struct NbdServer {
    socket_path: String,
    exports: Arc<RwLock<HashMap<String, NbdExport>>>,
    shutdown: Arc<Notify>,
    shutdown_requested: Arc<AtomicBool>,
}

const MAX_IN_FLIGHT_REQUESTS: usize = 64;

fn storage_error_status(error: &subvol_core::StorageError) -> u32 {
    match error {
        subvol_core::StorageError::InvalidArgument(_) => NBD_EINVAL,
        subvol_core::StorageError::AddressSpaceExhausted { .. }
        | subvol_core::StorageError::QuotaExceeded { .. } => NBD_ENOSPC,
        subvol_core::StorageError::NotFound(_) | subvol_core::StorageError::BlockNotFound(_) => {
            NBD_ENOENT
        }
        subvol_core::StorageError::Io(error)
            if error.kind() == std::io::ErrorKind::PermissionDenied =>
        {
            NBD_EPERM
        }
        _ => NBD_EIO,
    }
}

enum OperationGuard {
    Read(OwnedRwLockReadGuard<()>),
    Write(OwnedRwLockWriteGuard<()>),
}

impl OperationGuard {
    fn keep(&self) {
        match self {
            Self::Read(guard) => {
                let _ = guard;
            }
            Self::Write(guard) => {
                let _ = guard;
            }
        }
    }
}

impl NbdServer {
    pub fn new(socket_path: impl Into<String>) -> Self {
        Self {
            socket_path: socket_path.into(),
            exports: Arc::new(RwLock::new(HashMap::new())),
            shutdown: Arc::new(Notify::new()),
            shutdown_requested: Arc::new(AtomicBool::new(false)),
        }
    }

    pub async fn register_export(&self, export: NbdExport) {
        let mut exports = self.exports.write().await;
        exports.insert(export.name.clone(), export);
    }

    pub async fn unregister_export(&self, name: &str) {
        let mut exports = self.exports.write().await;
        exports.remove(name);
    }

    pub async fn is_exported(&self, name: &str) -> bool {
        self.exports.read().await.contains_key(name)
    }

    pub async fn list_exports(&self) -> Vec<(String, u64)> {
        self.exports
            .read()
            .await
            .values()
            .map(|e| (e.name.clone(), e.size()))
            .collect()
    }

    pub async fn run(&self) -> NbdResult<()> {
        if self.shutdown_requested.load(Ordering::Acquire) {
            return Ok(());
        }
        let socket_path = &self.socket_path;
        let _ = tokio::fs::remove_file(socket_path).await;

        let listener = UnixListener::bind(socket_path).map_err(|e| {
            NbdError::Io(std::io::Error::new(
                e.kind(),
                format!("bind NBD socket {socket_path}: {e}"),
            ))
        })?;

        tracing::info!("NBD server listening on {}", socket_path);

        let shutdown = self.shutdown.clone();
        let shutdown_requested = self.shutdown_requested.clone();
        let mut connections = JoinSet::new();
        loop {
            match tokio::select! {
                result = listener.accept() => result,
                _ = shutdown.notified() => break,
            } {
                Ok((stream, _addr)) => {
                    let exports = self.exports.clone();
                    let shutdown = self.shutdown.clone();
                    let shutdown_requested = self.shutdown_requested.clone();
                    connections.spawn(async move {
                        if let Err(e) =
                            handle_connection(stream, exports, shutdown, shutdown_requested).await
                        {
                            tracing::warn!("NBD connection error: {e}");
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("NBD accept error: {e}");
                    break;
                }
            }
            if shutdown_requested.load(Ordering::Acquire) {
                break;
            }
        }

        let _ = tokio::fs::remove_file(socket_path).await;
        // `shutdown()` only stops accepting new clients; every accepted
        // connection must converge before `run()` returns.  This mirrors the
        // filesystem shutdown contract: the caller may checkpoint volumes
        // only after request loops have observed the shutdown boundary and
        // drained their in-flight operations.
        while let Some(result) = connections.join_next().await {
            if let Err(e) = result {
                tracing::warn!("NBD connection task failed during shutdown: {e}");
            }
        }
        tracing::info!("NBD server stopped");

        Ok(())
    }

    /// Stop accepting new clients and remove the Unix socket once `run` exits.
    pub fn shutdown(&self) {
        self.shutdown_requested.store(true, Ordering::Release);
        self.shutdown.notify_waiters();
    }

    pub fn socket_path(&self) -> &str {
        &self.socket_path
    }
}

async fn handle_connection(
    mut stream: tokio::net::UnixStream,
    exports: Arc<RwLock<HashMap<String, NbdExport>>>,
    shutdown: Arc<Notify>,
    shutdown_requested: Arc<AtomicBool>,
) -> NbdResult<()> {
    if shutdown_requested.load(Ordering::Acquire) {
        return Ok(());
    }
    let exports_guard = exports.read().await;
    let export = tokio::select! {
        result = handshake::negotiate(&mut stream, &exports_guard) => result?,
        _ = shutdown.notified() => return Ok(()),
    };
    drop(exports_guard);

    tracing::info!(
        "NBD client connected to export '{}', size={}, flags={:#x}",
        export.name,
        export.size(),
        export.flags,
    );

    let (mut reader, writer) = stream.into_split();
    let writer = Arc::new(Mutex::new(writer));
    let permits = Arc::new(Semaphore::new(MAX_IN_FLIGHT_REQUESTS));
    // Reads can run concurrently, while writes/TRIM/FLUSH retain the wire
    // ordering boundary required by the block interface.
    let operation_lock = Arc::new(RwLock::new(()));
    let mut tasks = JoinSet::new();

    loop {
        if shutdown_requested.load(Ordering::Acquire) {
            break;
        }
        let req = tokio::select! {
            result = read_request(&mut reader) => result?,
            _ = shutdown.notified() => break,
        };
        let Some(req) = req else {
            break;
        };

        if req.len > NBD_MAX_BLOCK_SIZE
            && matches!(req.r#type, NBD_CMD_READ | NBD_CMD_WRITE | NBD_CMD_TRIM)
        {
            // Preserve the request framing before rejecting an oversized write;
            // the connection remains usable for subsequent requests.
            if req.r#type == NBD_CMD_WRITE {
                discard_write_data(&mut reader, req.len).await?;
            }
            let mut output = writer.lock().await;
            send_response(&mut *output, req.handle, NBD_EINVAL).await?;
            continue;
        }

        let permit = permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| NbdError::Protocol("request concurrency limiter closed"))?;
        let disconnect = req.r#type == NBD_CMD_DISC;

        // Read the complete write frame before taking the volume write lock.
        // A client that stalls after a WRITE header must not block unrelated
        // requests for this export while the connection remains frame-bound.
        let write_data = if req.r#type == NBD_CMD_WRITE {
            if shutdown_requested.load(Ordering::Acquire) {
                break;
            }
            Some(tokio::select! {
                result = read_write_data(&mut reader, req.len) => result?,
                _ = shutdown.notified() => break,
            })
        } else {
            None
        };

        // Once framing is complete, serialize mutating operations exactly as
        // before; this preserves WRITE/TRIM/FLUSH ordering at the volume.
        let operation_guard = match req.r#type {
            NBD_CMD_READ => Some(OperationGuard::Read(
                operation_lock.clone().read_owned().await,
            )),
            NBD_CMD_WRITE | NBD_CMD_TRIM | NBD_CMD_FLUSH => Some(OperationGuard::Write(
                operation_lock.clone().write_owned().await,
            )),
            _ => None,
        };
        let export = export.clone_inner();
        let writer = writer.clone();
        tasks.spawn(async move {
            let _operation_guard = operation_guard;
            if let Some(ref guard) = _operation_guard {
                guard.keep();
            }
            let result = match req.r#type {
                NBD_CMD_READ => handle_read(&writer, &export, &req).await,
                NBD_CMD_WRITE => handle_write(&writer, &export, &req, write_data).await,
                NBD_CMD_TRIM => handle_trim(&writer, &export, &req).await,
                NBD_CMD_FLUSH => handle_flush(&writer, &export, &req).await,
                NBD_CMD_DISC => Ok(()),
                t => {
                    tracing::warn!("unknown NBD command type: {t}");
                    let mut output = writer.lock().await;
                    send_response(&mut *output, req.handle, NBD_EINVAL).await
                }
            };
            drop(permit);
            if let Err(e) = result {
                tracing::warn!("NBD request {} failed: {e}", req.handle);
            }
        });

        if disconnect {
            break;
        }
    }

    while let Some(result) = tasks.join_next().await {
        if let Err(e) = result {
            tracing::warn!("NBD request task failed: {e}");
        }
    }

    Ok(())
}

async fn handle_read(
    writer: &Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
    export: &NbdExport,
    req: &NbdRequest,
) -> NbdResult<()> {
    let offset = req.offset;
    let len = req.len as u64;

    if let Err(e) = export.validate_range(offset, len) {
        tracing::error!("read_extent rejected: {:?}", e);
        let mut stream = writer.lock().await;
        send_response(&mut *stream, req.handle, NBD_EINVAL).await?;
        return Ok(());
    }

    let mut buf = BytesMut::zeroed(len as usize);

    match export.read(offset, &mut buf).await {
        Ok(()) => {
            let mut stream = writer.lock().await;
            send_response_with_data(&mut *stream, req.handle, 0, &buf).await?;
        }
        Err(e) => {
            tracing::error!("read_extent failed: {:?}", e);
            let mut stream = writer.lock().await;
            send_response(&mut *stream, req.handle, storage_error_status(&e)).await?;
        }
    }

    Ok(())
}

async fn handle_write(
    writer: &Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
    export: &NbdExport,
    req: &NbdRequest,
    write_data: Option<bytes::Bytes>,
) -> NbdResult<()> {
    let write_data = write_data.expect("write request payload was read before dispatch");

    if export.is_read_only() {
        let mut stream = writer.lock().await;
        send_response(&mut *stream, req.handle, NBD_EPERM).await?;
        return Ok(());
    }

    match export.write(req.offset, &write_data).await {
        Ok(()) => {
            if req.flags & NBD_CMD_FLAG_FUA != 0 {
                if let Err(e) = export.flush().await {
                    tracing::error!("FUA flush failed: {:?}", e);
                    let mut stream = writer.lock().await;
                    send_response(&mut *stream, req.handle, storage_error_status(&e)).await?;
                    return Ok(());
                }
            }
            let mut stream = writer.lock().await;
            send_response(&mut *stream, req.handle, 0).await?;
        }
        Err(e) => {
            tracing::error!("write_extent failed: {:?}", e);
            let mut stream = writer.lock().await;
            send_response(&mut *stream, req.handle, storage_error_status(&e)).await?;
        }
    }

    Ok(())
}

async fn handle_trim(
    writer: &Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
    export: &NbdExport,
    req: &NbdRequest,
) -> NbdResult<()> {
    if export.is_read_only() {
        let mut stream = writer.lock().await;
        send_response(&mut *stream, req.handle, NBD_EPERM).await?;
        return Ok(());
    }

    match export.trim(req.offset, req.len as u64).await {
        Ok(()) => {
            let mut stream = writer.lock().await;
            send_response(&mut *stream, req.handle, 0).await?
        }
        Err(e) => {
            tracing::error!("trim_extent failed: {:?}", e);
            let mut stream = writer.lock().await;
            send_response(&mut *stream, req.handle, storage_error_status(&e)).await?;
        }
    }
    Ok(())
}

async fn handle_flush(
    writer: &Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
    export: &NbdExport,
    req: &NbdRequest,
) -> NbdResult<()> {
    match export.flush().await {
        Ok(()) => {
            let mut stream = writer.lock().await;
            send_response(&mut *stream, req.handle, 0).await?
        }
        Err(e) => {
            tracing::error!("flush failed: {:?}", e);
            let mut stream = writer.lock().await;
            send_response(&mut *stream, req.handle, storage_error_status(&e)).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::storage_error_status;
    use crate::protocol::{NBD_EINVAL, NBD_EIO, NBD_ENOENT, NBD_ENOSPC, NBD_EPERM};
    use subvol_core::types::{BlockAddr, StorageError};

    #[test]
    fn storage_errors_preserve_block_errno_semantics() {
        assert_eq!(
            storage_error_status(&StorageError::AddressSpaceExhausted { max_raw_addr: 0 }),
            NBD_ENOSPC
        );
        assert_eq!(
            storage_error_status(&StorageError::QuotaExceeded {
                message: "quota".into()
            }),
            NBD_ENOSPC
        );
        assert_eq!(
            storage_error_status(&StorageError::BlockNotFound(BlockAddr::new(1))),
            NBD_ENOENT
        );
        assert_eq!(
            storage_error_status(&StorageError::InvalidArgument("range".into())),
            NBD_EINVAL
        );
        assert_eq!(
            storage_error_status(&StorageError::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "read-only",
            ))),
            NBD_EPERM
        );
        assert_eq!(
            storage_error_status(&StorageError::Io(std::io::Error::other("disk"))),
            NBD_EIO
        );
    }
}
