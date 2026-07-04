#!/usr/bin/env bash
set -euo pipefail

# subvolmountd systemd 安装脚本
# 用法: sudo ./scripts/install-systemd.sh [用户名]
# 默认用当前 $USER 作为运行用户

INSTALL_USER="${1:-$USER}"
INSTALL_DIR="$(dirname "$0")/.."
BIN_SRC="$INSTALL_DIR/target/release/subvolmountd"
SERVICE_SRC="$INSTALL_DIR/subvolmountd.service"
SERVICE_DST="/etc/systemd/system/subvolmountd.service"
BIN_DST="/usr/bin/subvolmountd"
CONFIG_DIR="/home/$INSTALL_USER/.subvolmount"

if [ "$EUID" -ne 0 ]; then
    echo "请用 sudo 运行: sudo $0 [$USER]"
    exit 1
fi

echo "=== subvolmountd systemd 安装 ==="
echo "运行用户: $INSTALL_USER"

# 1. 构建 release 版本
if [ ! -f "$BIN_SRC" ]; then
    echo "构建 subvolmountd release 版本..."
    (cd "$INSTALL_DIR" && cargo build --release -p subvolmountd)
fi

# 2. 安装二进制
echo "安装二进制: $BIN_DST"
cp "$BIN_SRC" "$BIN_DST"
chmod 755 "$BIN_DST"

# 3. 安装 systemd 单元
echo "安装 systemd 单元: $SERVICE_DST"
cp "$SERVICE_SRC" "$SERVICE_DST"
chmod 644 "$SERVICE_DST"

# 4. 创建配置目录
if [ ! -d "$CONFIG_DIR" ]; then
    echo "创建配置目录: $CONFIG_DIR"
    mkdir -p "$CONFIG_DIR"
    chown "$INSTALL_USER:$INSTALL_USER" "$CONFIG_DIR"
    cat > "$CONFIG_DIR/config.json" << EOF
{
    "home_dir": "$CONFIG_DIR/data",
    "nbd_socket_path": "$CONFIG_DIR/subvolmountd.sock",
    "auto_exports": [],
    "http_port": 9876
}
EOF
    mkdir -p "$CONFIG_DIR/data"
    chown -R "$INSTALL_USER:$INSTALL_USER" "$CONFIG_DIR"
    echo "默认配置已生成: $CONFIG_DIR/config.json"
fi

# 5. 重载 systemd 并启用服务
echo "重载 systemd 配置..."
systemctl daemon-reload

echo "启用 subvolmountd 开机自启..."
systemctl enable subvolmountd

echo "启动 subvolmountd..."
systemctl restart subvolmountd

echo ""
echo "=== 安装完成 ==="
echo "查看状态: systemctl status subvolmountd"
echo "查看日志: journalctl -u subvolmountd -f"
echo "配置文件: $CONFIG_DIR/config.json"