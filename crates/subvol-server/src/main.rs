use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use subvol_core::block_device::superblock::{
    data_area_offset, read_superblock, write_superblock, Superblock, DEFAULT_JOURNAL_BUCKET_SIZE,
    DEFAULT_NR_JOURNAL_BUCKETS, SUPERBLOCK_SIZE,
};
use subvol_core::block_device::BchDev;
use subvol_core::data::extents_format::BLOCK_SIZE;
use subvol_core::btree::types::NODE_SIZE;
use subvol_core::Allocator;
use subvol_core::BchVol;

// ═══════════════════════════════════════════════════════════════
// App State
// ═══════════════════════════════════════════════════════════════

struct AppState {
    engine: Mutex<Allocator>,
    dev: Arc<BchDev>,
    vol: Arc<BchVol>,
    superblock: Mutex<Superblock>,
    root_area_start: u64,
}

// ═══════════════════════════════════════════════════════════════
// Request/Response 类型
// ═══════════════════════════════════════════════════════════════

#[derive(Deserialize)]
struct WriteReq {
    inode: u64,
    offset: u64,
    data: String,
}

#[derive(Deserialize)]
struct ReadParams {
    inode: u64,
    offset: u64,
}

#[derive(Deserialize)]
struct DeleteReq {
    inode: u64,
}

#[derive(Serialize)]
struct StatsResp {
    dev_size: u64,
    block_size: u64,
    dev_file: bool,
    initialized: bool,
    journal_buckets: u32,
    key_counts: KeyCounts,
}

#[derive(Serialize)]
struct KeyCounts {
    freespace: u32,
    alloc: u32,
    data_index: u32,
}

// ═══════════════════════════════════════════════════════════════
// Handlers
// ═══════════════════════════════════════════════════════════════

async fn handle_write(
    State(state): State<Arc<AppState>>,
    Json(req): Json<WriteReq>,
) -> (StatusCode, Json<serde_json::Value>) {
    let data = match base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &req.data) {
        Ok(d) => d,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": format!("base64 decode error: {}", e)})),
            );
        }
    };

    let mut engine = state.engine.lock().await;
    match engine.write_extent(req.inode, req.offset, &data).await {
        Ok(_) => {
            if let Err(e) = persist_engine_roots(&state, &mut engine).await {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": e.to_string()})),
                );
            }
            (
                StatusCode::OK,
                Json(serde_json::json!({"status": "ok", "len": data.len()})),
            )
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        ),
    }
}

async fn handle_read(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ReadParams>,
) -> (StatusCode, Json<serde_json::Value>) {
    let engine = state.engine.lock().await;
    match engine.read_extent(params.inode, params.offset).await {
        Ok(data) => {
            let encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &data);
            (
                StatusCode::OK,
                Json(serde_json::json!({"data": encoded, "len": data.len()})),
            )
        }
        Err(e) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": e.to_string()})),
        ),
    }
}

async fn handle_create(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let mut engine = state.engine.lock().await;
    match engine.create_inode().await {
        Ok(inode) => (StatusCode::OK, Json(serde_json::json!({"inode": inode}))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        ),
    }
}

async fn handle_delete(
    State(state): State<Arc<AppState>>,
    Json(req): Json<DeleteReq>,
) -> (StatusCode, Json<serde_json::Value>) {
    let mut engine = state.engine.lock().await;
    match engine.delete_inode(req.inode).await {
        Ok(_) => {
            if let Err(e) = persist_engine_roots(&state, &mut engine).await {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": e.to_string()})),
                );
            }
            (StatusCode::OK, Json(serde_json::json!({"status": "ok"})))
        }
        Err(e) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": e.to_string()})),
        ),
    }
}

async fn persist_engine_roots(
    state: &AppState,
    engine: &mut Allocator,
) -> Result<(), subvol_core::types::StorageError> {
    state
        .vol
        .journal_ref()
        .bch2_journal_flush()
        .await
        .map_err(|e| subvol_core::types::StorageError::Internal(e.to_string()))?;
    let persisted = engine
        .persist_roots(&state.dev, state.root_area_start)
        .await?;
    let mut sb = state.superblock.lock().await;
    for root in &persisted {
        sb.set_root(root.btree_id, root.level, root.root_offset);
    }
    write_superblock(&state.dev, &sb).await
}

async fn handle_stats(State(state): State<Arc<AppState>>) -> Json<StatsResp> {
    let dev = &state.dev;
    let engine = state.engine.lock().await;
    let (fc, ac, dc) = engine.key_counts();
    let vol = &state.vol;
    let initialized = vol.is_initialized();

    Json(StatsResp {
        dev_size: dev.size(),
        block_size: BLOCK_SIZE,
        dev_file: dev.has_file(),
        initialized,
        journal_buckets: DEFAULT_NR_JOURNAL_BUCKETS,
        key_counts: KeyCounts {
            freespace: fc,
            alloc: ac,
            data_index: dc,
        },
    })
}

// ═══════════════════════════════════════════════════════════════
// CLI 参数解析
// ═══════════════════════════════════════════════════════════════

fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim().to_lowercase();
    let (num_str, unit) = if s.ends_with('g') {
        (&s[..s.len() - 1], 1_073_741_824u64)
    } else if s.ends_with('m') {
        (&s[..s.len() - 1], 1_048_576u64)
    } else if s.ends_with('k') {
        (&s[..s.len() - 1], 1024u64)
    } else {
        (&s[..], 1u64)
    };
    let n: u64 = num_str
        .parse()
        .map_err(|_| format!("invalid size: {}", s))?;
    Ok(n * unit)
}

fn print_usage() {
    eprintln!("Usage: subvol-server [OPTIONS]");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  --dev <path>      Backing file for block device storage");
    eprintln!("  --size <bytes>    Device size (default: 1G). Suffix: K/M/G");
    eprintln!("  --port <num>      HTTP port (default: 8080)");
    eprintln!("  --help            Show this help");
}

// ═══════════════════════════════════════════════════════════════
// 初始化流程
// ═══════════════════════════════════════════════════════════════

/// 格式化设备：写入 superblock + 初始化 btrees + 预留 journal bucket
async fn format_device(
    dev: &Arc<BchDev>,
    dev_size: u64,
) -> Result<(Arc<BchVol>, Allocator), Box<dyn std::error::Error>> {
    let nr_buckets = DEFAULT_NR_JOURNAL_BUCKETS;
    let bucket_size = DEFAULT_JOURNAL_BUCKET_SIZE;

    // 1. 创建 superblock
    let sb = Superblock::new(dev_size, nr_buckets, bucket_size);
    write_superblock(dev, &sb).await?;
    println!("  superblock written");

    // 2. 计算预留块（superblock + journal buckets）
    let mut reserved_blocks: Vec<u64> = Vec::new();

    // superblock 占用的块
    let sb_blocks = (SUPERBLOCK_SIZE + BLOCK_SIZE - 1) / BLOCK_SIZE;
    for b in 0..sb_blocks {
        reserved_blocks.push(b);
    }

    // journal buckets 占用的块
    for &addr in &sb.journal_buckets {
        let start_block = addr / BLOCK_SIZE;
        let bucket_blocks = bucket_size as u64 / BLOCK_SIZE;
        for b in start_block..start_block + bucket_blocks {
            reserved_blocks.push(b);
        }
    }

    // 根节点区域位于 journal 之后，必须在 allocator 初始化前预留，
    // 否则首次数据分配会覆盖刚写入的 btree roots。
    let root_area_start = data_area_offset(sb.journal_bucket_count, sb.journal_bucket_size);
    let root_area_blocks = (3 * NODE_SIZE).div_ceil(BLOCK_SIZE);
    for b in (root_area_start / BLOCK_SIZE)..(root_area_start / BLOCK_SIZE + root_area_blocks) {
        reserved_blocks.push(b);
    }

    // 3. 初始化存储引擎
    let vol = BchVol::with_dev(dev.clone(), sb.journal_buckets.clone());
    let mut engine = Allocator::new(&vol, dev);
    engine.init(dev_size, &reserved_blocks).await?;
    println!(
        "  btrees initialized ({} blocks reserved)",
        reserved_blocks.len()
    );

    // 4. 持久化根节点到磁盘并获取真实偏移
    // root 数据写在预留区之后、自由区之前，不经过 journal
    let root_entries = engine.persist_roots(dev, root_area_start).await?;
    println!("  btree roots persisted");

    // 5. 更新 superblock 中的根记录
    let mut sb = sb;
    for re in &root_entries {
        sb.set_root(re.btree_id, re.level, re.root_offset);
    }
    write_superblock(dev, &sb).await?;
    println!("  superblock updated with btree root entries");

    Ok((vol, engine))
}

/// 从已初始化的设备加载 btree 根节点并回放 journal
async fn load_device(
    dev: &Arc<BchDev>,
    sb: &Superblock,
) -> Result<(Arc<BchVol>, Allocator), Box<dyn std::error::Error>> {
    use subvol_core::journal::{JournalReplayer, JournalStartInfo};

    println!(
        "  device already initialized: version={} journal_buckets={}",
        sb.version, sb.journal_bucket_count
    );

    let vol = BchVol::with_dev(dev.clone(), sb.journal_buckets.clone());

    // 从 superblock 的根记录加载 btree 节点
    let mut engine = Allocator::from_roots(&vol, dev, &sb.root_entries).await?;
    println!("  btree roots loaded from disk");

    // 读取 journal bucket 并回放
    let journal = vol.journal_ref();
    let mut info = JournalStartInfo::default();
    let jsets = journal.bch2_journal_read(&mut info).await?;
    if !jsets.is_empty() {
        println!("  journal: {} Jset(s) found, replaying...", jsets.len());

        let mut replayer = JournalReplayer::from_jsets(journal, jsets);
        let applied = replayer.replay_all_to_vol(&vol).await?;

        // 先恢复 journal 中记录的新根，再将未落盘的 key 更新提交到 btree。
        // 这样 journal replay 才真正完成状态恢复，而不是只建立查询 overlay。
        let roots = replayer.root_records.clone();
        engine.apply_root_records(&roots).await?;

        // 根节点替换后再挂载 overlay，确保重放期间查询使用当前根和 journal 最新值。
        let overlay = Arc::new(replayer.overlay.clone());
        engine.set_journal_overlay(overlay);
        let replayed_keys = engine.replay_from_overlay(&mut replayer.overlay).await?;

        println!("  journal: {} entries replayed", applied);
        println!("  journal: {} key updates applied", replayed_keys);

        // journal root 已被应用到内存后，必须先固化新 roots 与 superblock，
        // 再清理 journal；否则下一次挂载会回到旧 root 丢失恢复结果。
        let root_area_start =
            data_area_offset(sb.journal_bucket_count, sb.journal_bucket_size);
        let persisted_roots = engine.persist_roots(dev, root_area_start).await?;
        let mut updated_sb = sb.clone();
        for root in &persisted_roots {
            updated_sb.set_root(root.btree_id, root.level, root.root_offset);
        }
        write_superblock(dev, &updated_sb).await?;

        // 当前单版本格式不持久化 replay 起点；成功应用后清除旧 jset，
        // 防止下次挂载重复执行同一批事务。
        journal.bch2_journal_discard_replayed().await?;

        // 标记回放完成
        journal.bch2_journal_set_replay_done();

        // 清除 overlay（恢复常规查询路径）
        engine.clear_journal_overlay();
    } else {
        println!("  journal: clean (no entries to replay)");
        journal.bch2_journal_set_replay_done();
    }

    Ok((vol, engine))
}

// ═══════════════════════════════════════════════════════════════
// Main
// ═══════════════════════════════════════════════════════════════

#[tokio::main]
async fn main() {
    subvol_core::log::init_from_env();

    let args: Vec<String> = std::env::args().collect();

    let mut dev_path: Option<PathBuf> = None;
    let mut dev_size: u64 = 1 << 30;
    let mut port: u16 = 8080;
    let mut i = 1;

    while i < args.len() {
        match args[i].as_str() {
            "--dev" => {
                i += 1;
                if i >= args.len() {
                    print_usage();
                    std::process::exit(1);
                }
                dev_path = Some(PathBuf::from(&args[i]));
            }
            "--size" => {
                i += 1;
                if i >= args.len() {
                    print_usage();
                    std::process::exit(1);
                }
                dev_size = parse_size(&args[i]).unwrap_or_else(|e| {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                });
            }
            "--port" => {
                i += 1;
                if i >= args.len() {
                    print_usage();
                    std::process::exit(1);
                }
                port = args[i].parse().unwrap_or_else(|_| {
                    eprintln!("Error: invalid port");
                    std::process::exit(1);
                });
            }
            "--help" | "-h" => {
                print_usage();
                return;
            }
            _ => {
                eprintln!("Unknown argument: {}", args[i]);
                print_usage();
                std::process::exit(1);
            }
        }
        i += 1;
    }

    // ── 创建设备 ──

    let dev: Arc<BchDev> = if let Some(path) = &dev_path {
        println!("Device file: {} ({} bytes)", path.display(), dev_size);
        Arc::new(BchDev::with_file(Arc::new(BchVol::new()), path, dev_size))
    } else {
        println!("In-memory device ({} bytes)", dev_size);
        Arc::new(BchDev::with_size(Arc::new(BchVol::new()), dev_size))
    };

    // ── 检测 / 初始化 ──

    println!("Checking device...");
    let (vol, engine) = match read_superblock(&dev).await {
        Ok(Some(sb)) => {
            println!("Found superblock, loading...");
            load_device(&dev, &sb).await.unwrap_or_else(|e| {
                eprintln!("Load failed: {}", e);
                std::process::exit(1);
            })
        }
        Ok(None) => {
            println!("Uninitialized device, formatting...");
            format_device(&dev, dev_size).await.unwrap_or_else(|e| {
                eprintln!("Format failed: {}", e);
                std::process::exit(1);
            })
        }
        Err(e) => {
            eprintln!("Failed to read superblock: {}", e);
            std::process::exit(1);
        }
    };

    let superblock = read_superblock(&dev)
        .await
        .expect("read formatted superblock")
        .expect("formatted device must have a superblock");
    let root_area_start = data_area_offset(
        superblock.journal_bucket_count,
        superblock.journal_bucket_size,
    );

    println!("Ready.");

    let state = Arc::new(AppState {
        engine: Mutex::new(engine),
        dev,
        vol,
        superblock: Mutex::new(superblock),
        root_area_start,
    });

    let app = Router::new()
        .route("/write", post(handle_write))
        .route("/read", get(handle_read))
        .route("/create", post(handle_create))
        .route("/delete", post(handle_delete))
        .route("/stats", get(handle_stats))
        .with_state(state);

    let addr = format!("0.0.0.0:{}", port);
    println!("subvol-server listening on http://{}", addr);

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("bind failed");

    axum::serve(listener, app).await.expect("server error");
}
