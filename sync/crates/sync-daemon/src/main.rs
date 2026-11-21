//! WinSync 守护进程：给 dsh 插件用的 HTTP + SSE 后端。
//!
//! 启动顺序（每一步失败都要能说清楚，别让插件对着空端口干等）：
//!   1. 定位自己的状态目录（`WINSYNC_STATE_DIR`，默认 `%LOCALAPPDATA%\dsh-winsync`）
//!   2. 抢自己的单实例锁（与 exe 的锁无关，两者可同时运行）
//!   3. 绑 127.0.0.1:0，生成一次性 Bearer
//!   4. 先写 `.tmp` 再改名，落 `daemon.json`
//!   5. 起 axum；退出时删 `daemon.json`
//!
//! 它不写 exe 的任何文件：配置、状态库、日志全在自己的树里。

mod api;
mod auth;
mod engine;
mod events;
mod fsapi;
mod overlap;
mod state;
mod util;
mod win;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sync_client::Settings;

use crate::state::{AppState, BootInfo};

/// 握手文件名。插件读它拿端口和 Bearer，浏览器永远看不到。
const DAEMON_FILE: &str = "daemon.json";

#[derive(Debug, Serialize, Deserialize)]
struct DaemonFile {
    pid: u32,
    port: u16,
    token: String,
    version: String,
    started_at: i64,
}

/// 退出时清掉握手文件。硬杀时这段不会执行，插件侧有陈旧握手回收兜底。
struct HandshakeGuard(PathBuf);

impl Drop for HandshakeGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// 解析自己的状态目录。daemon 是唯一会设 `WINSYNC_STATE_DIR` 的进程，
/// 这样 `Config::dir()` 之后的每一次调用（配置、状态库、日志）都自然落到这里。
fn prepare_state_dir() -> Result<PathBuf> {
    let dir = match std::env::var_os("WINSYNC_STATE_DIR") {
        Some(p) => PathBuf::from(p),
        None => {
            let base = std::env::var_os("LOCALAPPDATA")
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os("USERPROFILE")
                        .map(PathBuf::from)
                        .map(|p| p.join("AppData").join("Local"))
                })
                .unwrap_or_else(|| PathBuf::from("."));
            let dir = base.join("dsh-winsync");
            std::env::set_var("WINSYNC_STATE_DIR", &dir);
            dir
        }
    };
    std::fs::create_dir_all(&dir).with_context(|| format!("创建状态目录 {}", dir.display()))?;
    Ok(dir)
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_env("WINSYNC_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .try_init();
}

fn read_daemon_file(dir: &Path) -> Option<DaemonFile> {
    let text = std::fs::read_to_string(dir.join(DAEMON_FILE)).ok()?;
    serde_json::from_str(&text).ok()
}

/// 先写临时文件再改名：插件可能正好在读，不能让它看到半截 JSON。
fn write_daemon_file(dir: &Path, file: &DaemonFile) -> Result<()> {
    let path = dir.join(DAEMON_FILE);
    let tmp = dir.join(format!("{DAEMON_FILE}.tmp"));
    std::fs::write(&tmp, serde_json::to_vec_pretty(file)?)
        .with_context(|| format!("写 {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("改名到 {}", path.display()))?;
    Ok(())
}

/// 探测一个端口上是不是已经有本 daemon 在跑。只用裸 TCP，不引 HTTP 客户端依赖。
async fn health_ok(port: u16, token: &str) -> bool {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let Ok(mut stream) = tokio::time::timeout(
        Duration::from_millis(400),
        tokio::net::TcpStream::connect(("127.0.0.1", port)),
    )
    .await
    .unwrap_or(Err(std::io::Error::other("timeout"))) else {
        return false;
    };
    let req = format!(
        "GET /health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
    );
    if stream.write_all(req.as_bytes()).await.is_err() {
        return false;
    }
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(Duration::from_millis(600), stream.read_to_end(&mut buf)).await;
    String::from_utf8_lossy(&buf).contains("\"ok\":true")
}

/// 32 字节随机 token（两个 UUIDv4 拼起来，36 位十六进制不含分隔符 → 64 字符）。
fn new_token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

#[tokio::main]
async fn main() -> Result<()> {
    let data_dir = prepare_state_dir()?;
    init_tracing();

    if !win::acquire_single_instance() {
        // 锁在别人手里。是「已经在跑」还是「上次硬杀留下的僵局」，看握手文件说话。
        if let Some(existing) = read_daemon_file(&data_dir) {
            if health_ok(existing.port, &existing.token).await {
                println!(
                    "dsh-winsync 已在运行：pid={} port={}",
                    existing.pid, existing.port
                );
                return Ok(());
            }
            let alive = win::pid_alive(existing.pid);
            tracing::warn!(
                "daemon.json 里的实例（pid={} port={}）无响应，进程{}；不接管已被持有的单实例锁，退出",
                existing.pid,
                existing.port,
                if alive { "仍在" } else { "已退出" }
            );
        } else {
            tracing::warn!("另一个实例持有单实例锁，但没有可读的 daemon.json");
        }
        std::process::exit(2);
    }

    let settings = match Settings::load() {
        Ok(Some(s)) => s,
        Ok(None) => Settings::default(),
        Err(e) => {
            // 配置坏了不能直接躺下：备份一份再以空配置启动，用户还有机会在界面里修
            let path = sync_client::Config::file();
            let backup = path.with_extension("json.bad");
            let _ = std::fs::copy(&path, &backup);
            tracing::error!(
                "配置读取失败（{e}），已备份到 {}，以空配置启动",
                backup.display()
            );
            Settings::default()
        }
    };

    // 端口先绑成功，再写 daemon.json：反过来会出现「文件在、端口没起来」的假活。
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .context("绑定 127.0.0.1")?;
    let port = listener.local_addr()?.port();

    let boot = BootInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        pid: std::process::id(),
        port,
        token: new_token(),
        data_dir: data_dir.clone(),
        config_path: sync_client::Config::file(),
        started_at: util::now_secs(),
    };

    write_daemon_file(
        &data_dir,
        &DaemonFile {
            pid: boot.pid,
            port: boot.port,
            token: boot.token.clone(),
            version: boot.version.clone(),
            started_at: boot.started_at,
        },
    )?;
    let _guard = HandshakeGuard(data_dir.join(DAEMON_FILE));

    let (state, worker_rx, dirty_rx) = AppState::new(boot, settings);
    state.emit(state::Event::State);

    let engine_state = state.clone();
    tokio::spawn(async move { engine::run(engine_state, worker_rx, dirty_rx).await });

    let app = api::router(state.clone());

    tracing::info!(
        "dsh-winsync {} 已启动 pid={} port={} dir={}",
        state.boot.version,
        state.boot.pid,
        state.boot.port,
        state.boot.data_dir.display()
    );

    let server = axum::serve(listener, app).with_graceful_shutdown(shutdown_signal());
    if let Err(e) = server.await {
        tracing::error!("HTTP 服务异常退出：{e:#}");
        return Err(e.into());
    }
    tracing::info!("dsh-winsync 已退出，握手文件已清理");
    Ok(())
}

/// Ctrl-C / 关闭控制台时走正常退出，好让 Drop 清掉 daemon.json。
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("收到退出信号，准备关闭");
}

/// 让 `Arc<AppState>` 在 handler 里能直接 clone 的小助手。
pub type SharedState = Arc<AppState>;
