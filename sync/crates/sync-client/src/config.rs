//! 配置：一个「同步任务」= 一条本机目录 ⇄ 服务器房间的绑定。
//!
//! 参考飞牛同步的组织方式：任务可以有多个、每个有名字和状态、可以单独暂停，
//! 日志按任务归档。这里把每个任务存成一条记录，状态库也按任务分文件。
//!
//! 配置文件：`%APPDATA%\WinSync\config.json`
//! 状态库：`%APPDATA%\WinSync\tasks\<task-id>.db`（base / hash_cache / activity 都在里面）

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sync_core::Filter;

/// 同步方向。语义与同类产品一致：只约束**本机允许的动作**。
///
/// - `Bidirectional`：既推也拉（常规用法，两端都设这个）
/// - `UploadOnly`：只把本机变更推给对方，不接受对方变更
/// - `DownloadOnly`：只接受对方变更，不推自己的
///
/// 单向场景要两端配合：发送端设 `UploadOnly`，接收端设 `DownloadOnly`。
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum SyncMode {
    #[default]
    Bidirectional,
    UploadOnly,
    DownloadOnly,
}

impl SyncMode {
    pub fn label(self) -> &'static str {
        match self {
            SyncMode::Bidirectional => "双向同步",
            SyncMode::UploadOnly => "仅上传（本机 → 对端）",
            SyncMode::DownloadOnly => "仅下载（对端 → 本机）",
        }
    }

    /// 允许把本机变更推出去
    pub fn allows_push(self) -> bool {
        !matches!(self, SyncMode::DownloadOnly)
    }

    /// 允许把对端变更拉到本机（含删除传播）
    pub fn allows_pull(self) -> bool {
        !matches!(self, SyncMode::UploadOnly)
    }
}

/// 一个同步任务。
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Config {
    /// 任务 id，同时是状态库文件名。旧配置迁移时自动生成。
    #[serde(default)]
    pub id: String,
    /// 任务名，界面上显示的标题
    #[serde(default)]
    pub name: String,
    /// WebSocket 地址，例如 ws://1.2.3.4:8443/ws
    pub server: String,
    /// 与服务端一致的共享密钥
    pub token: String,
    /// 房间名，两台设备必须相同
    pub workspace: String,
    /// 同步目录（绝对路径）
    pub root: String,
    /// 同步方向
    #[serde(default)]
    pub mode: SyncMode,
    /// 过滤规则。两端必须一致，否则会互相误判删除。
    #[serde(default)]
    pub filter: Filter,
    /// 暂停：不再参与自动同步（手动「立即同步」仍然可用）
    #[serde(default)]
    pub paused: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            id: new_id(),
            name: String::new(),
            server: "ws://127.0.0.1:8443/ws".to_string(),
            token: String::new(),
            workspace: "default".to_string(),
            root: String::new(),
            mode: SyncMode::default(),
            filter: Filter::default(),
            paused: false,
        }
    }
}

/// 短 id：8 位十六进制，够用且好认（也用作文件名）。
pub fn new_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..8].to_string()
}

impl Config {
    /// 配置、状态库、日志的根目录。
    ///
    /// 默认是 exe 用的 `%APPDATA%\WinSync`；设了 `WINSYNC_STATE_DIR` 就整个换掉。
    /// dsh 插件的 daemon 靠这个变量把自己那棵树指到 `%LOCALAPPDATA%\dsh-winsync`，
    /// 与 exe 完全隔离。exe 不设该变量，行为一字不变。
    pub fn dir() -> PathBuf {
        if let Some(p) = std::env::var_os("WINSYNC_STATE_DIR") {
            return PathBuf::from(p);
        }
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

    /// 每个任务一个状态库，彼此隔离：base、哈希缓存、设备身份、记录都互不干扰。
    pub fn state_db(&self) -> PathBuf {
        if let Some(p) = std::env::var_os("WINSYNC_DB") {
            return PathBuf::from(p);
        }
        let dir = Self::dir().join("tasks");
        let _ = std::fs::create_dir_all(&dir);
        let id = if self.id.is_empty() {
            "default".to_string()
        } else {
            self.id.clone()
        };
        dir.join(format!("{id}.db"))
    }

    /// 旧版本单任务用的状态库位置
    pub fn legacy_state_db() -> PathBuf {
        Self::dir().join("state.db")
    }

    pub fn display_name(&self) -> String {
        if self.name.trim().is_empty() {
            let tail = std::path::Path::new(&self.root)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| self.root.clone());
            if tail.is_empty() {
                "未命名任务".to_string()
            } else {
                tail
            }
        } else {
            self.name.clone()
        }
    }

    pub fn is_ready(&self) -> bool {
        !self.server.is_empty()
            && !self.token.is_empty()
            && !self.root.is_empty()
            && std::path::Path::new(&self.root).is_dir()
    }

    /// 逐项检查，返回人能看懂的问题列表。
    pub fn issues(&self) -> Vec<String> {
        let mut v: Vec<String> = Vec::new();

        let server = self.server.trim();
        if server.is_empty() {
            v.push("未填写服务器地址".into());
        } else if !server.starts_with("ws://") && !server.starts_with("wss://") {
            v.push("服务器地址必须以 ws:// 或 wss:// 开头".into());
        } else if !server.contains("/ws") {
            v.push("服务器地址通常以 /ws 结尾，例如 ws://192.168.3.222:18443/ws".into());
        }

        if self.token.trim().is_empty() {
            v.push("未填写密钥（中继为每个房间配的那把，见 rooms.json）".into());
        }
        if self.workspace.trim().is_empty() {
            v.push("未填写房间名（两台设备必须相同）".into());
        }

        let root = self.root.trim();
        if root.is_empty() {
            v.push("未选择同步目录".into());
        } else if !std::path::Path::new(root).is_dir() {
            v.push(format!("同步目录不存在：{root}"));
        }

        if let (Some(a), Some(b)) = (self.filter.size_min, self.filter.size_max) {
            if a >= b {
                v.push("过滤规则里「最小大小」不应大于等于「最大大小」".into());
            }
        }

        v
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

/// 配置根对象：任务列表 + 全局设置。
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Settings {
    #[serde(default = "settings_version")]
    pub version: u32,
    #[serde(default)]
    pub tasks: Vec<Config>,
    /// 有新版本时自动更新（当前版本还不支持自动更新，保留字段与开关）
    #[serde(default = "default_true")]
    pub auto_update: bool,
    /// 记录保留条数，0 = 不限
    #[serde(default)]
    pub log_keep_count: u32,
    /// 记录保留天数，0 = 永久
    #[serde(default = "default_log_days")]
    pub log_keep_days: u32,
    /// 全局限速，KB/s，0 = 不限
    #[serde(default)]
    pub limit_up_kbps: u32,
    #[serde(default)]
    pub limit_down_kbps: u32,
}

fn default_true() -> bool {
    true
}

fn default_log_days() -> u32 {
    30
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: settings_version(),
            tasks: Vec::new(),
            auto_update: true,
            log_keep_count: 0,
            log_keep_days: default_log_days(),
            limit_up_kbps: 0,
            limit_down_kbps: 0,
        }
    }
}

fn settings_version() -> u32 {
    2
}

impl Settings {
    /// 读取配置，并处理旧版单任务格式：
    /// 旧文件是扁平的 `{server, token, workspace, root}`，这里自动包成一个任务，
    /// 同时把它原来的 state.db 复制到新任务名下，避免丢失共识基线与设备身份。
    pub fn load() -> Result<Option<Self>> {
        let path = Config::file();
        if !path.exists() {
            return Ok(None);
        }
        let bytes = std::fs::read(&path).with_context(|| format!("读取配置 {}", path.display()))?;

        // PowerShell 的 `Set-Content -Encoding utf8` 会写 BOM，serde_json 不接受
        let text = String::from_utf8_lossy(&bytes);
        let text = text.trim_start_matches('\u{feff}');

        let value: serde_json::Value = serde_json::from_str(text)
            .with_context(|| format!("解析配置 {}", path.display()))?;

        if value.get("tasks").is_some() {
            let mut s: Settings = serde_json::from_value(value)?;
            for t in &mut s.tasks {
                if t.id.is_empty() {
                    t.id = new_id();
                }
            }
            return Ok(Some(s));
        }

        // 旧格式：单任务
        let legacy: Config = serde_json::from_value(value)?;
        let mut task = legacy;
        if task.id.is_empty() {
            task.id = new_id();
        }
        if task.name.is_empty() {
            task.name = task.display_name();
        }

        // 迁移状态库：复制而非移动，万一新版本有问题还能回退
        let old_db = Config::legacy_state_db();
        let new_db = task.state_db();
        if old_db.exists() && !new_db.exists() {
            if let Some(parent) = new_db.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::copy(&old_db, &new_db);
            tracing::info!(
                "已把旧状态库迁移到任务 {}（{}）",
                task.display_name(),
                new_db.display()
            );
        }

        let settings = Settings {
            tasks: vec![task],
            ..Default::default()
        };
        // 立刻写回新格式，避免下次启动再走一遍迁移
        let _ = settings.save();
        Ok(Some(settings))
    }

    pub fn save(&self) -> Result<()> {
        std::fs::create_dir_all(Config::dir())?;
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(Config::file(), text)?;
        Ok(())
    }

    pub fn find(&self, id: &str) -> Option<usize> {
        self.tasks.iter().position(|t| t.id == id || t.name == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_flat_config_wraps_into_one_task() {
        let json = r#"{
            "server": "ws://1.2.3.4:8443/ws",
            "token": "secret",
            "workspace": "default",
            "root": "D:\\Sync"
        }"#;
        let value: serde_json::Value = serde_json::from_str(json).unwrap();
        assert!(value.get("tasks").is_none());

        let legacy: Config = serde_json::from_value(value).unwrap();
        assert_eq!(legacy.server, "ws://1.2.3.4:8443/ws");
        assert_eq!(legacy.mode, SyncMode::Bidirectional);
        assert!(legacy.filter.ignore_extensions.is_empty());
        assert!(!legacy.paused);
        assert!(legacy.id.is_empty(), "旧配置里没有 id，迁移时补");
    }

    #[test]
    fn settings_roundtrip_keeps_pause_and_filter() {
        let mut t = Config::default();
        t.id = "abcd1234".into();
        t.name = "文档".into();
        t.paused = true;
        t.mode = SyncMode::UploadOnly;
        t.filter.ignore_dotfiles = true;
        t.filter.ignore_extensions = vec!["lnk".into()];

        let s = Settings {
            tasks: vec![t],
            ..Default::default()
        };
        let text = serde_json::to_string(&s).unwrap();
        let back: Settings = serde_json::from_str(&text).unwrap();
        assert_eq!(back.tasks.len(), 1);
        assert_eq!(back.tasks[0].name, "文档");
        assert!(back.tasks[0].paused);
        assert_eq!(back.tasks[0].mode, SyncMode::UploadOnly);
        assert_eq!(back.tasks[0].filter.ignore_extensions, vec!["lnk".to_string()]);
    }

    #[test]
    fn display_name_falls_back_to_folder() {
        let mut t = Config::default();
        t.root = r"D:\Data\文档".into();
        assert_eq!(t.display_name(), "文档");
        t.name = "自定义".into();
        assert_eq!(t.display_name(), "自定义");
    }

    #[test]
    fn issues_report_missing_and_bad_fields() {
        let mut t = Config::default();
        t.server = "http://x".into();
        t.token = "".into();
        t.root = "".into();
        let issues = t.issues();
        assert!(issues.iter().any(|s| s.contains("ws://")));
        assert!(issues.iter().any(|s| s.contains("密钥")));
        assert!(issues.iter().any(|s| s.contains("同步目录")));
    }
}
