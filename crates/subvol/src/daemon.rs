use std::path::PathBuf;
use std::sync::Arc;

use tokio::signal;
use tokio::sync::{oneshot, Mutex, RwLock};

pub use crate::daemon_server::AppState;
pub use crate::daemon_volume::DaemonError;

use crate::config::SubvolmountdConfig;
use crate::daemon_server;
use crate::daemon_volume;
use subvol_core::bch_vol::BchVol;

pub async fn run(config: SubvolmountdConfig, _config_path: std::path::PathBuf) {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("info"))
        .init();

    let (http_shutdown_tx, http_shutdown_rx) = oneshot::channel::<()>();
    let http_shutdown_tx = Arc::new(Mutex::new(Some(http_shutdown_tx)));

    if let Err(e) = daemon_volume::init_dirs(&config).await {
        tracing::error!("failed to init dirs: {e}");
        std::process::exit(1);
    }
    tracing::info!("directories initialized");

    let nbd_socket = config.resolved_nbd_socket();
    if let Some(parent) = nbd_socket.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            tracing::error!("failed to create NBD socket directory {:?}: {e}", parent);
            std::process::exit(1);
        }
    }

    let nbd_server = Arc::new(subvol_nbd::NbdServer::new(
        nbd_socket.to_string_lossy().to_string(),
    ));

    let pool: Arc<RwLock<Option<Arc<BchVol>>>> = Arc::new(RwLock::new(None));
    match BchVol::open_pool(&config.pool_dir(), "pool").await {
        Ok(pool_vol) => {
            let list = pool_vol.list_subvols().await;
            for (id, sv) in &list {
                let export_name = id.to_string();
                let ro = sv.flags.contains(subvol_core::subvol::BchSubvolumeFlags::READ_ONLY);
                let nbd_export = subvol_nbd::NbdExport::new_with_subvol(
                    export_name.clone(),
                    pool_vol.clone(),
                    *id,
                );
                nbd_server.register_export(nbd_export).await;
                tracing::info!("auto-exported subvol {id} ({}) via NBD", if ro { "ro" } else { "rw" });
            }
            *pool.write().await = Some(pool_vol.clone());
            tracing::info!("pool opened at {} — {} subvol(s) exported", config.pool_dir().display(), list.len());
        }
        Err(e) => {
            tracing::warn!("pool not available at {}: {e} — NBD exports will be empty", config.pool_dir().display());
        }
    }

    let app_state = AppState {
        config: Arc::new(RwLock::new(config.clone())),
        config_path: PathBuf::new(),
        config_persist_lock: Arc::new(Mutex::new(())),
        pool: pool.clone(),
        nbd_server: nbd_server.clone(),
    };
    let http_port = config.http_port;
    let http_handle = tokio::spawn(async move {
        if let Err(e) = daemon_server::run_server(app_state, http_port, http_shutdown_rx).await {
            tracing::error!("HTTP server error: {e}");
        }
    });

    let nbd_server_clone = nbd_server.clone();
    let server_handle = tokio::spawn(async move {
        if let Err(e) = nbd_server_clone.run().await {
            tracing::error!("NBD server error: {e}");
        }
    });

    tracing::info!(
        "subvolmountd ready — http=127.0.0.1:{}, nbd={}, home={}",
        http_port,
        config.resolved_nbd_socket().display(),
        config.resolved_home_dir().display(),
    );

    wait_for_shutdown().await;
    tracing::info!("shutdown signal received, cleaning up...");

    nbd_server.shutdown();
    {
        let mut tx = http_shutdown_tx.lock().await;
        if let Some(tx) = tx.take() {
            let _ = tx.send(());
        }
    }

    {
        let pool_guard = pool.read().await;
        if let Some(pool) = pool_guard.as_ref() {
            if let Err(e) = pool.bch2_fs_read_only().await {
                tracing::warn!("error setting pool read-only: {e}");
            } else {
                tracing::info!("pool set read-only (clean_shutdown=true)");
            }
        }
    }

    let _ = tokio::fs::remove_file(&nbd_socket).await;
    let _ = http_handle.await;
    let _ = server_handle.await;

    tracing::info!("subvolmountd shut down cleanly");
}

async fn wait_for_shutdown() {
    let mut sigint = signal::unix::signal(signal::unix::SignalKind::interrupt())
        .expect("failed to register SIGINT handler");
    let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate())
        .expect("failed to register SIGTERM handler");

    tokio::select! {
        _ = sigint.recv() => {
            tracing::info!("received SIGINT");
        }
        _ = sigterm.recv() => {
            tracing::info!("received SIGTERM");
        }
    }
}
