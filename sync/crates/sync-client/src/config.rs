//! 配置：服务器地址、token、房间名、同步目录。
//!
//! 存放在 `%APPDATA%\WinSync\config.json`，GUI 和 CLI 共用同一份。

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Config {
    /// WebSocket 地址，例如 ws://1.2.3.4:8443/ws
    pub server: String,
    /// 与服务端一致的共享密钥
    pub token: String,
    /// 房间名，两台设备必须相同
    pub workspace: String,
    /// 同步目录（绝对路径）
    pub root: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: "ws://127.0.0.1:8443/ws".to_string(),
            token: String::new(),
            workspace: "default".to_string(),
            root: String::new(),
        }
    }
}

impl Config {
    pub fn dir() -> PathBuf {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join("WinSync")
    }

    pub fn file() -> PathBuf {
        if let Some(p) = std::env::var_os("WINSYNC_CONFIG") {
            return PathBuf::from(p);
        }
        Self::dir().join("config.json")
    }

    /// 状态库路径。`WINSYNC_DB` 可覆盖，用于本机同时跑两个实例做测试，
    /// 或者做成绿色版（把状态放在程序目录下）。
    pub fn state_db() -> PathBuf {
        if let Some(p) = std::env::var_os("WINSYNC_DB") {
            return PathBuf::from(p);
        }
        Self::dir().join("state.db")
    }

    pub fn is_ready(&self) -> bool {
        !self.server.is_empty()
            && !self.token.is_empty()
            && !self.root.is_empty()
            && std::path::Path::new(&self.root).is_dir()
    }

    pub fn load() -> Result<Option<Self>> {
        let path = Self::file();
        if !path.exists() {
            return Ok(None);
        }
        let bytes =
            std::fs::read(&path).with_context(|| format!("读取配置 {}", path.display()))?;

        // Windows PowerShell 的 `Set-Content -Encoding utf8` 会写入 UTF-8 BOM，
        // 而 serde_json 不接受 BOM，会把整份配置判为非法，导致程序静默退回默认值。
        // 这里主动剥掉 BOM，避免用户被这个看不见的字符坑到。
        let text = String::from_utf8_lossy(&bytes);
        let text = text.trim_start_matches('\u{feff}');

        Ok(Some(
            serde_json::from_str(text).with_context(|| format!("解析配置 {}", path.display()))?,
        ))
    }

    pub fn save(&self) -> Result<()> {
        std::fs::create_dir_all(Self::dir())?;
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(Self::file(), text)?;
        Ok(())
    }

    /// 拼接带 token 与房间名的完整 URL
    pub fn url(&self) -> String {
        let sep = if self.server.contains('?') { '&' } else { '?' };
        format!(
            "{}{sep}token={}&workspace={}",
            self.server, self.token, self.workspace
        )
    }
}
