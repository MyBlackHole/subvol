use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post, delete},
    Json, Router,
};
use tokio::sync::RwLock;

use subvol_core::bch_vol::BchVol;

use crate::config::SubvolmountdConfig;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<RwLock<SubvolmountdConfig>>,
    #[allow(dead_code)]
    pub config_path: std::path::PathBuf,
    #[allow(dead_code)]
    pub config_persist_lock: Arc<tokio::sync::Mutex<()>>,
    pub pool: Arc<RwLock<Option<Arc<BchVol>>>>,
    pub nbd_server: Arc<subvol_nbd::NbdServer>,
}

#[derive(serde::Serialize)]
struct StatusResponse {
    version: &'static str,
    nbd_socket: String,
    pool_initialized: bool,
}

#[derive(serde::Deserialize)]
struct CreateSubvolRequest {
    size: u64,
}

#[derive(serde::Serialize)]
struct SubvolEntry {
    id: u32,
    size: u64,
    readonly: bool,
}

#[derive(serde::Serialize)]
struct SubvolListResponse {
    subvols: Vec<SubvolEntry>,
}

#[derive(serde::Serialize)]
struct CreateSubvolResponse {
    id: u32,
}

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(handle_status))
        .route("/daemon/status", get(handle_status))
        .route("/subvols", get(handle_list_subvols))
        .route("/subvols", post(handle_create_subvol))
        .route("/subvols/{id}", delete(handle_delete_subvol))
        .route("/nbd", get(handle_nbd_list_exports))
        .route("/help", get(handle_help))
        .with_state(state)
}

async fn handle_status(State(state): State<AppState>) -> impl IntoResponse {
    let config = state.config.read().await;
    let pool_initialized = state.pool.read().await.is_some();
    Json(serde_json::json!(StatusResponse {
        version: "0.1.0",
        nbd_socket: config.resolved_nbd_socket().to_string_lossy().to_string(),
        pool_initialized,
    }))
}

async fn handle_nbd_list_exports(State(state): State<AppState>) -> impl IntoResponse {
    let exports = state.nbd_server.list_exports().await;
    (
        StatusCode::OK,
        Json(serde_json::json!({ "exports": exports })),
    )
}

async fn handle_list_subvols(State(state): State<AppState>) -> impl IntoResponse {
    let pool = state.pool.read().await;
    let Some(vol) = pool.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({
            "error": "pool not initialized"
        })));
    };
    let list = vol.list_subvols().await;
    let entries: Vec<SubvolEntry> = list
        .into_iter()
        .map(|(id, sv)| SubvolEntry {
            id,
            size: sv.size,
            readonly: sv.flags.contains(subvol_core::subvol::BchSubvolumeFlags::READ_ONLY),
        })
        .collect();
    (StatusCode::OK, Json(serde_json::json!(SubvolListResponse { subvols: entries })))
}

async fn handle_create_subvol(
    State(state): State<AppState>,
    Json(req): Json<CreateSubvolRequest>,
) -> impl IntoResponse {
    let pool = state.pool.read().await;
    let Some(vol) = pool.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({
            "error": "pool not initialized"
        })));
    };
    match vol.create_subvol("api", req.size).await {
        Ok(id) => {
            let export = subvol_nbd::NbdExport::new_with_subvol(
                id.to_string(),
                vol.clone(),
                id,
            );
            state.nbd_server.register_export(export).await;
            (StatusCode::CREATED, Json(serde_json::json!(CreateSubvolResponse { id })))
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
            "error": e.to_string()
        }))),
    }
}

async fn handle_delete_subvol(
    State(state): State<AppState>,
    Path(id): Path<u32>,
) -> impl IntoResponse {
    let pool = state.pool.read().await;
    let Some(vol) = pool.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({
            "error": "pool not initialized"
        })));
    };
    match vol.delete_subvol(id).await {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({ "ok": true }))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
            "error": e.to_string()
        }))),
    }
}

async fn handle_help() -> impl IntoResponse {
    let help_text = r#"
subvolmountd HTTP API

GET  /                        — Server status
GET  /daemon/status           — Server status

NBD:
GET  /nbd                     — List NBD exports

GET  /help                    — This help
"#;
    (StatusCode::OK, help_text)
}

pub async fn run_server(
    state: AppState,
    port: u16,
    shutdown_rx: tokio::sync::oneshot::Receiver<()>,
) -> Result<(), std::io::Error> {
    let app = axum::Router::new().nest("/api/v1", build_router(state));
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            shutdown_rx.await.ok();
        })
        .await
}
