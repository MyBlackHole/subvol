#!/usr/bin/env bash
#
# subvolmount fio 端到端性能基准测试
#
# 前置条件:
#   - cargo (Rust 工具链)
#   - fio (flexible I/O tester)
#   - nbd-client + nbd 内核模块
#   - sudo 权限 (nbd-client 需要 root)
#
# 用法:
#   ./scripts/bench-fio.sh [--size <GiB>] [--output <dir>]
#
# 输出:
#   在 --output 目录生成 JSON 结果文件，终端打印摘要

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
OUTPUT_DIR="${OUTPUT_DIR:-bench-results}"
VOLUME_SIZE="${VOLUME_SIZE:-1}"
BLOCK_SIZE=4096
VOLUME_NAME="bench-$(date +%s)"
SOCKET_DIR="/tmp/subvolmount-bench"
DAEMON_LOG="$OUTPUT_DIR/daemon.log"
NBD_DEVICE="/dev/nbd0"

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; CYAN='\033[0;36m'; NC='\033[0m'
info()  { echo -e "${CYAN}[INFO]${NC}  $*"; }
pass()  { echo -e "${GREEN}[PASS]${NC}  $*"; }
warn()  { echo -e "${YELLOW}[WARN]${NC}  $*"; }
fail()  { echo -e "${RED}[FAIL]${NC}  $*"; exit 1; }

check_prereqs() {
    command -v fio >/dev/null 2>&1 || fail "fio 未安装 (apt install fio / brew install fio)"
    command -v cargo >/dev/null 2>&1 || fail "cargo 未安装"
    if ! lsmod 2>/dev/null | grep -q nbd && ! modprobe nbd 2>/dev/null; then
        fail "nbd 内核模块无法加载"
    fi
    command -v nbd-client >/dev/null 2>&1 || fail "nbd-client 未安装 (apt install nbd-client)"
    echo "1" | sudo tee /proc/sys/net/ipv4/tcp_tw_reuse >/dev/null 2>&1 || true
}

cleanup() {
    info "清理..."
    if [ -e "$NBD_DEVICE" ]; then
        sudo nbd-client -d "$NBD_DEVICE" 2>/dev/null || true
    fi
    if [ -n "${DAEMON_PID:-}" ]; then
        kill "$DAEMON_PID" 2>/dev/null || true
        wait "$DAEMON_PID" 2>/dev/null || true
    fi
    rm -rf "$SOCKET_DIR" 2>/dev/null || true
    info "清理完成"
}
trap cleanup EXIT

# ─── 主流程 ───

main() {
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --size) VOLUME_SIZE="$2"; shift 2 ;;
            --output) OUTPUT_DIR="$2"; shift 2 ;;
            *) fail "未知参数: $1" ;;
        esac
    done

    check_prereqs

    mkdir -p "$OUTPUT_DIR" "$SOCKET_DIR"
    OUTPUT_DIR="$(cd "$OUTPUT_DIR" && pwd)"  # 转绝对路径
    readonly DAEMON_LOG="$OUTPUT_DIR/daemon.log"
    readonly CONFIG_DIR="$OUTPUT_DIR/subvolmountd-config"

    info "卷大小: ${VOLUME_SIZE} GiB"
    info "输出目录: $OUTPUT_DIR"
    info "Daemon 日志: $DAEMON_LOG"

    # ── 构建项目 ──
    info "构建 subvolmountd + subvolmount..."
    cargo build --release -q --manifest-path "$PROJECT_DIR/Cargo.toml" 2>&1
    pass "构建完成"

    # ── 启动 daemon ──
    info "启动 subvolmountd..."
    rm -f "$DAEMON_LOG"
    mkdir -p "$CONFIG_DIR"

    # 生成简洁配置
    cat > "$CONFIG_DIR/config.toml" <<-EOF
[daemon]
listen = "127.0.0.1:9876"
nbd_socket = "${SOCKET_DIR}/nbd.sock"

[storage]
backend = "nfs"
base_path = "${OUTPUT_DIR}/volumes"
EOF

    "$PROJECT_DIR/target/release/subvolmountd" \
        --config "$CONFIG_DIR/config.toml" \
        >> "$DAEMON_LOG" 2>&1 &
    DAEMON_PID=$!
    sleep 2

    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
        fail "daemon 启动失败, 日志: $(tail -5 "$DAEMON_LOG")"
    fi
    pass "subvolmountd 已启动 (PID=$DAEMON_PID)"

    # ── 创建 volume ──
    VOLUME_SIZE_BYTES=$((VOLUME_SIZE * 1024 * 1024 * 1024))
    info "创建 volume: $VOLUME_NAME (${VOLUME_SIZE} GiB)..."
    "$PROJECT_DIR/target/release/subvolmount" volume create \
        --name "$VOLUME_NAME" \
        --size "$VOLUME_SIZE_BYTES" \
        --block-size "$BLOCK_SIZE" \
        --backend nfs
    pass "volume 创建完成"

    # ── 挂载 NBD ──
    info "挂载 NBD 到 $NBD_DEVICE..."
    "$PROJECT_DIR/target/release/subvolmount" mount \
        --name "$VOLUME_NAME" \
        --device "$NBD_DEVICE"
    pass "NBD 挂载完成 ($NBD_DEVICE)"

    # ── 等待设备就绪 ──
    sleep 2
    if [ ! -e "$NBD_DEVICE" ]; then
        fail "NBD 设备 $NBD_DEVICE 不存在"
    fi

    # ── fio 基准测试 ──
    info "开始 fio 基准测试..."

    FIO_JOBS=(
        "seq-read:--rw=read --bs=4k --iodepth=16"
        "seq-write:--rw=write --bs=4k --iodepth=16"
        "rand-read:--rw=randread --bs=4k --iodepth=16"
        "rand-write:--rw=randwrite --bs=4k --iodepth=16"
        "rand-read-64k:--rw=randread --bs=64k --iodepth=8"
        "rand-write-64k:--rw=randwrite --bs=64k --iodepth=8"
        "latency-read:--rw=randread --bs=4k --iodepth=1 --lat_percentiles=1"
        "latency-write:--rw=randwrite --bs=4k --iodepth=1 --lat_percentiles=1"
    )

    for entry in "${FIO_JOBS[@]}"; do
        name="${entry%%:*}"
        opts="${entry#*:}"

        info "  运行: $name"
        fio $opts \
            --name="$name" \
            --filename="$NBD_DEVICE" \
            --direct=1 \
            --size="${VOLUME_SIZE}Gi" \
            --runtime=30 \
            --time_based \
            --ramp_time=5 \
            --output-format=json \
            > "$OUTPUT_DIR/fio-${name}.json" 2>/dev/null

        # 提取关键指标
        read_iops=$(jq '.jobs[0].read.iops // 0' "$OUTPUT_DIR/fio-${name}.json" 2>/dev/null)
        write_iops=$(jq '.jobs[0].write.iops // 0' "$OUTPUT_DIR/fio-${name}.json" 2>/dev/null)
        read_bw=$(jq '.jobs[0].read.bw_bytes // 0' "$OUTPUT_DIR/fio-${name}.json" 2>/dev/null)
        write_bw=$(jq '.jobs[0].write.bw_bytes // 0' "$OUTPUT_DIR/fio-${name}.json" 2>/dev/null)

        if [ "$read_iops" != "0" ]; then
            printf "  ${GREEN}%-30s${NC} 读: %6.0f IOPS  (%s MiB/s)\n" \
                "$name" "$read_iops" "$(echo "scale=1; $read_bw/1048576" | bc 2>/dev/null || echo "?")"
        fi
        if [ "$write_iops" != "0" ]; then
            printf "  ${GREEN}%-30s${NC} 写: %6.0f IOPS  (%s MiB/s)\n" \
                "$name" "$write_iops" "$(echo "scale=1; $write_bw/1048576" | bc 2>/dev/null || echo "?")"
        fi
    done

    # ── 汇总报告 ──
    info "生成汇总报告..."
    cat > "$OUTPUT_DIR/summary.md" <<-EOF
# subvolmount fio 基准测试报告

- 日期: $(date -u '+%Y-%m-%dT%H:%M:%SZ')
- 卷大小: ${VOLUME_SIZE} GiB
- 块大小: ${BLOCK_SIZE} bytes
- 后端: NFS (稀疏文件)
- NBD 设备: $NBD_DEVICE

## 结果

| 测试 | IOPS (读) | IOPS (写) | MiB/s (读) | MiB/s (写) |
|------|-----------|-----------|------------|------------|
EOF

    for entry in "${FIO_JOBS[@]}"; do
        name="${entry%%:*}"
        json="$OUTPUT_DIR/fio-${name}.json"
        if [ -f "$json" ]; then
            read_iops=$(jq '.jobs[0].read.iops // 0' "$json" 2>/dev/null)
            write_iops=$(jq '.jobs[0].write.iops // 0' "$json" 2>/dev/null)
            read_bw=$(echo "scale=1; $(jq '.jobs[0].read.bw_bytes // 0' "$json" 2>/dev/null)/1048576" | bc)
            write_bw=$(echo "scale=1; $(jq '.jobs[0].write.bw_bytes // 0' "$json" 2>/dev/null)/1048576" | bc)
            echo "| $name | $read_iops | $write_iops | ${read_bw} MiB/s | ${write_bw} MiB/s |" >> "$OUTPUT_DIR/summary.md"
        fi
    done

    # ── 清理（由 trap 自动执行） ──
    info "所有基准测试完成！"
    info "结果目录: $OUTPUT_DIR"
    echo "汇总: $OUTPUT_DIR/summary.md"
}

main "$@"
