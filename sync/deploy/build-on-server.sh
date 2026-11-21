#!/usr/bin/env bash
# 在 Linux 服务器上原生编译 sync-server，产出 musl 静态二进制。
#
# 用法：
#   bash build-on-server.sh <源码目录> [sudo密码]
#
# 只编译服务端，不碰 GUI 依赖。

set -euo pipefail

SRC="${1:-$(pwd)}"
SUDO_PASS="${2:-}"
TARGET=x86_64-unknown-linux-musl

blue() { printf '\n\033[1;34m==> %s\033[0m\n' "$*"; }

sudo_run() {
    if [[ -n "$SUDO_PASS" ]]; then
        printf '%s\n' "$SUDO_PASS" | sudo -S -p '' "$@"
    else
        sudo "$@"
    fi
}

export RUSTUP_DIST_SERVER=https://rsproxy.cn
export RUSTUP_UPDATE_ROOT=https://rsproxy.cn/rustup
export DEBIAN_FRONTEND=noninteractive

if [[ ! -d "$SRC/crates" ]]; then
    echo "找不到源码目录: $SRC" >&2
    exit 1
fi

blue "[1/4] 安装编译依赖"
# build-essential 提供 libc6-dev / gcc / Scrt1.o 等，最小化系统默认没有。
# musl-tools 提供 musl 的静态启动文件。
sudo_run apt-get update -qq
sudo_run apt-get install -y -qq --no-install-recommends \
    build-essential musl-tools curl ca-certificates file

blue "[2/4] 准备 Rust 工具链"
if ! command -v cargo >/dev/null 2>&1; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --profile minimal --no-modify-path
fi
export PATH="$HOME/.cargo/bin:$PATH"
rustup target add "$TARGET" >/dev/null 2>&1 || true

mkdir -p "$HOME/.cargo"
if [[ ! -f "$HOME/.cargo/config.toml" ]]; then
    cat > "$HOME/.cargo/config.toml" <<'CFG'
[source.crates-io]
replace-with = 'rsproxy-sparse'

[source.rsproxy-sparse]
registry = "sparse+https://rsproxy.cn/index/"

[net]
git-fetch-with-cli = true
CFG
fi

blue "[3/4] 编译（只编 sync-server）"
cd "$SRC"
cargo build --release -p sync-server --target "$TARGET"

blue "[4/4] 校验产物"
OUT="$SRC/target/$TARGET/release/sync-server"
if [[ ! -f "$OUT" ]]; then
    echo "编译产物不存在" >&2
    exit 1
fi
ls -lh "$OUT"
file "$OUT" | sed 's/^/  /'
echo
echo "OK: $OUT"
