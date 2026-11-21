#!/usr/bin/env bash
# 在 WSL 里一键编译 Linux 静态二进制。
#
# 用法（在 Windows 的 PowerShell 或 CMD 里执行）：
#   wsl -u root bash /mnt/c/Users/gjn86/WorkBuddy/spaces/2026_11_19_20-38-45/sync/deploy/build-linux.sh
#
# 做的事：
#   1. 装 musl-tools（静态链接需要）
#   2. 装 Rust（已装则跳过），走 rsproxy 镜像
#   3. 把源码复制到 WSL 自己的文件系统再编译（在 /mnt/c 下编译会慢很多）
#   4. 产物拷回 Windows 侧的 deploy/ 目录
#
# 只编译 sync-server，不碰 GUI 依赖。

set -euo pipefail

SRC=/mnt/c/Users/gjn86/WorkBuddy/spaces/2026_11_19_20-38-45/sync
WORK="$HOME/winsync-build"
TARGET=x86_64-unknown-linux-musl

blue() { printf '\033[1;34m==> %s\033[0m\n' "$*"; }
red()  { printf '\033[1;31m!!  %s\033[0m\n' "$*"; }

if [[ $EUID -ne 0 ]]; then
    red "请以 root 运行： wsl -u root bash $0"
    exit 1
fi

if [[ ! -d "$SRC/crates" ]]; then
    red "找不到源码目录：$SRC"
    exit 1
fi

export RUSTUP_DIST_SERVER=https://rsproxy.cn
export RUSTUP_UPDATE_ROOT=https://rsproxy.cn/rustup
export DEBIAN_FRONTEND=noninteractive

blue "1/4 安装系统依赖"
apt-get update -qq
apt-get install -y -qq --no-install-recommends musl-tools curl ca-certificates file >/dev/null

blue "2/4 准备 Rust 工具链"
if ! command -v cargo >/dev/null 2>&1; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --profile minimal --no-modify-path
fi
# root 的 rustup 装在 /root/.cargo
export PATH="$HOME/.cargo/bin:$PATH"
rustup target add "$TARGET"

mkdir -p "$HOME/.cargo"
cat > "$HOME/.cargo/config.toml" <<'EOF'
[source.crates-io]
replace-with = 'rsproxy-sparse'

[source.rsproxy-sparse]
registry = "sparse+https://rsproxy.cn/index/"

[net]
git-fetch-with-cli = true
EOF

blue "3/4 复制源码到 Linux 文件系统并编译"
rm -rf "$WORK"
mkdir -p "$WORK"
cp "$SRC/Cargo.toml" "$WORK/"
cp -r "$SRC/crates" "$WORK/"
[[ -f "$SRC/Cargo.lock" ]] && cp "$SRC/Cargo.lock" "$WORK/"

cd "$WORK"
# 只编服务端：纯 Rust 无 C 依赖，可以完全静态链接
cargo build --release -p sync-server --target "$TARGET"

blue "4/4 校验并拷回 Windows 侧"
OUT="$WORK/target/$TARGET/release/sync-server"
if [[ ! -f "$OUT" ]]; then
    red "编译产物不存在，构建可能失败了"
    exit 1
fi

ls -lh "$OUT"
file "$OUT" | sed 's/^/    /'

mkdir -p "$SRC/deploy"
cp "$OUT" "$SRC/deploy/sync-server"
chmod +x "$SRC/deploy/sync-server" 2>/dev/null || true

echo
blue "完成"
echo "产物：$SRC/deploy/sync-server"
echo "Windows 路径：C:\\Users\\gjn86\\WorkBuddy\\spaces\\2026_11_19_20-38-45\\sync\\deploy\\sync-server"
