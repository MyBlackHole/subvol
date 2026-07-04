# Research: subvol-server HTTP API 路由结构

- **Query**: 探索 subvol-server 的 HTTP API 路由结构
- **Scope**: 内部（代码分析）
- **Date**: 2026-07-21

## Findings

### 源文件

| 文件路径 | 说明 |
|---|---|
| `crates/subvol-server/src/main.rs` | 唯一源文件，包含所有路由定义、handler、CLI 参数解析和 main 入口 |
| `crates/subvol-server/Cargo.toml` | 依赖声明 |

### 路由定义（共 3 个端点）

所有路由在 `main.rs:365-369` 注册：

```rust
let app = Router::new()
    .route("/write", post(handle_write))
    .route("/read", get(handle_read))
    .route("/stats", get(handle_stats))
    .with_state(state);
```

| 方法 | 路径 | Handler 函数 | 行号 |
|------|------|-------------|------|
| `POST` | `/write` | `handle_write` | 82-109 |
| `GET` | `/read` | `handle_read` | 111-135 |
| `GET` | `/stats` | `handle_stats` | 137-157 |

### POST /write — 写入数据

**请求体 (JSON):**
```json
{
  "inode": 0,       // u64, 目标 inode 编号
  "offset": 0,      // u64, 写入偏移量
  "data": "..."     // String, base64 编码的二进制数据
}
```

**成功响应 (200):**
```json
{ "status": "ok", "len": 1234 }
```

**错误响应:**
- `400 BAD_REQUEST` — base64 解码失败: `{ "error": "base64 decode error: ..." }`
- `500 INTERNAL_SERVER_ERROR` — 写入失败: `{ "error": "..." }`

### GET /read — 读取数据

**查询参数:**
```
?inode=<u64>&offset=<u64>
```

**成功响应 (200):**
```json
{ "data": "<base64 编码的二进制数据>", "len": 1234 }
```

**错误响应:**
- `404 NOT_FOUND` — 未找到数据: `{ "error": "..." }`

### GET /stats — 统计信息

**响应 (200):**
```json
{
  "dev_size": 1073741824,
  "block_size": 4096,
  "dev_file": false,
  "initialized": true,
  "journal_buckets": 4,
  "key_counts": {
    "freespace": 10,
    "alloc": 5,
    "data_index": 3
  }
}
```

## 启动入口与默认端口

- **入口函数**: `main.rs:267` — `#[tokio::main] async fn main()`
- **默认端口**: `8080`（`main.rs:273`）
- **绑定地址**: `0.0.0.0`（监听所有网络接口）
- **CLI 参数**:
  - `--dev <path>` — 指定块设备后端文件路径
  - `--size <bytes>` — 设备大小，支持 K/M/G 后缀（默认 1G）
  - `--port <num>` — HTTP 端口（默认 8080）
  - `--help` / `-h` — 显示帮助

### 启动流程

1. 解析 CLI 参数
2. 创建 `BchDev`（文件后端或内存后端）
3. 读取 superblock：
   - 已初始化 → `load_device()` 从磁盘加载 btree 根节点
   - 未初始化 → `format_device()` 写入 superblock + 初始化 btrees + 持久化根节点
4. 构建 `AppState`（包含 `Allocator`、`BchDev`、`BchVol`）
5. 构建 axum `Router` 并绑定到 `0.0.0.0:{port}`
6. 启动 HTTP 服务

## 认证 / Token

**无任何认证机制。** 具体来说：

- 代码中没有任何 `Authorization` header 检查
- 没有 token 验证逻辑
- 没有 API key 机制
- `tower-http` 的 `cors` feature 虽在 `Cargo.toml` 中声明（第 12 行），但**未在代码中导入或使用**（无 `CorsLayer`、无 `use tower_http::cors::CorsLayer`）
- 所有 3 个端点完全开放，无需任何认证即可访问

## Caveats / 未找到

- 未发现任何认证、授权或 token 验证机制
- CORS 中间件虽在依赖中声明但未启用
- 没有健康检查端点（`/health` 或 `/ping`）
- 没有 API 版本前缀（如 `/v1/`）
- 没有请求日志或追踪中间件
