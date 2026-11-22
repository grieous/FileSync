#!/usr/bin/env bash
# WinSync 中继部署脚本。
#
# 用法：
#   sudo ./install.sh ./sync-server
#
# 做的事：
#   1. 建系统用户 winsync（无登录权限）
#   2. 二进制装到 /opt/winsync/sync-server
#   3. 首次运行生成随机密钥，写入 /etc/winsync/env
#   4. 注册并启动 systemd 服务
#
# 幂等：重复执行只更新二进制，不会覆盖已有密钥。

set -euo pipefail

BIN_SRC="${1:-./sync-server}"
PREFIX=/opt/winsync
ETC=/etc/winsync
SVC_USER=winsync
UNIT_SRC="$(cd "$(dirname "$0")" && pwd)/sync-server.service"

if [[ $EUID -ne 0 ]]; then
    echo "请用 root 运行：sudo $0 $*" >&2
    exit 1
fi

if [[ ! -f "$BIN_SRC" ]]; then
    echo "找不到二进制文件：$BIN_SRC" >&2
    exit 1
fi

if [[ ! -f "$UNIT_SRC" ]]; then
    echo "找不到 systemd 单元文件：$UNIT_SRC" >&2
    exit 1
fi

# 1. 服务用户
if ! id -u "$SVC_USER" >/dev/null 2>&1; then
    useradd --system --no-create-home --shell /usr/sbin/nologin "$SVC_USER"
    echo "已创建系统用户 $SVC_USER"
fi

# 2. 二进制
install -d -m 755 "$PREFIX"
install -m 755 "$BIN_SRC" "$PREFIX/sync-server"
echo "已安装到 $PREFIX/sync-server"

# 3. 环境变量（只在缺失时创建，避免覆盖密钥）
install -d -m 750 "$ETC"
# 房间密钥表：默认给一个 default 房间 + 随机密钥；不想要鉴权就删掉这个文件和 env 里的 SYNC_ROOMS
if [[ ! -f "$ETC/rooms.json" ]]; then
    TOKEN="$(head -c 24 /dev/urandom | base64 | tr -d '/+=' | head -c 32)"
    cat > "$ETC/rooms.json" <<EOF
{
  "default": "$TOKEN"
}
EOF
    chmod 600 "$ETC/rooms.json"
    chown "root:$SVC_USER" "$ETC/rooms.json"
    echo "已生成房间密钥表 $ETC/rooms.json（房间 default + 随机密钥）"
else
    echo "沿用已有房间密钥表 $ETC/rooms.json（未改动）"
fi

if [[ ! -f "$ETC/env" ]]; then
    cat > "$ETC/env" <<EOF
SYNC_ROOMS=$ETC/rooms.json
SYNC_ADDR=0.0.0.0:8443
RUST_LOG=info
EOF
    chmod 600 "$ETC/env"
    chown "root:$SVC_USER" "$ETC/env"
else
    echo "沿用已有配置 $ETC/env"
fi

# 4. systemd
install -m 644 "$UNIT_SRC" /etc/systemd/system/sync-server.service
systemctl daemon-reload
systemctl enable --now sync-server >/dev/null 2>&1 || systemctl restart sync-server

sleep 1
echo
systemctl --no-pager --lines=0 status sync-server || true
echo
echo "================ 部署完成 ================"
echo "监听地址 : $(grep '^SYNC_ADDR=' "$ETC/env" | cut -d= -f2-)"
echo "房间密钥 : $(grep -o '"default": *"[^"]*"' "$ETC/rooms.json" | head -1 | sed 's/.*"\([^"]*\)"$/\1/')"
echo
echo "客户端填房间 default 与上面这把密钥，服务器地址填："
HOST_IP="$(hostname -I 2>/dev/null | awk '{print $1}')"
echo "  ws://${HOST_IP:-服务器IP}:8443/ws"
echo
echo "检查状态 : systemctl status sync-server"
echo "看日志   : journalctl -u sync-server -f"
echo "=========================================="
