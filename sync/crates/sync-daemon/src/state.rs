//! 运行时状态：一次进程里所有可变事实都收在这里。
//!
//! 两条约定：
//! - `Inner` 用同步 `Mutex`，任何持有期间都不 `.await`（await 会把 guard 带过 yield 点，
//!   既编译不过也容易死锁）。需要 await 的地方先取快照、放锁、再干活。
//! - SSE 只发**事件**，`/api/state` 才是权威快照；前端拿事件做增量、拿 state 做对账。

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::Serialize;
use sync_client::{Config, Settings};
use sync_core::{Activity, Filter};
use tokio::sync::{broadcast, mpsc};

use crate::util::{filter_summary, short_server};

/// 每个任务在内存里的环形日志上限，与 exe 的 UI 一致。
pub const LOG_RING: usize = 400;

/// 进程启动时定型、之后不变的事实。
#[derive(Debug, Clone)]
pub struct BootInfo {
    pub version: String,
    pub pid: u32,
    pub port: u16,
    /// 只写进 `daemon.json` 给插件读，绝不出现在任何 HTTP 响应或模型上下文里。
    pub token: String,
    pub data_dir: PathBuf,
    pub config_path: PathBuf,
    pub started_at: i64,
}

/// 一次同步的进度（SSE 与 state 共用）。
#[derive(Debug, Clone, Serialize)]
pub struct ProgressView {
    /// 已传字节。早期这里是「第几个文件」，单个大文件永远停在 0/1，界面上等于没有进度。
    pub done: u64,
    pub total: u64,
    pub what: String,
    /// 实时速率（字节/秒）。1.2 秒没数据在会话层就归零，快照里再兜一次底。
    #[serde(default)]
    pub up_bps: u64,
    #[serde(default)]
    pub down_bps: u64,
}

/// `session::Report` 的可序列化映射。
/// 不改 session 的公开类型，只在这里搬一次字段——session 仍是唯一的算法真源。
#[derive(Debug, Clone, Serialize)]
pub struct ReportView {
    pub pushed: usize,
    pub pulled: usize,
    pub deleted: usize,
    pub conflicts: usize,
    pub bytes_sent: u64,
    pub bytes_recv: u64,
    pub up_to_date: bool,
}

impl ReportView {
    pub fn from_report(r: &sync_client::Report) -> Self {
        Self {
            pushed: r.pushed,
            pulled: r.pulled,
            deleted: r.deleted,
            conflicts: r.conflicts,
            bytes_sent: r.bytes_sent,
            bytes_recv: r.bytes_recv,
            up_to_date: r.up_to_date,
        }
    }

    /// 日志里那一行摘要，措辞与 exe 保持一致。
    pub fn summary(&self) -> String {
        if self.up_to_date {
            "两端已一致，无需传输".to_string()
        } else {
            format!(
                "完成：推送 {}，拉取 {}，删除 {}，冲突 {}",
                self.pushed, self.pulled, self.deleted, self.conflicts
            )
        }
    }
}

/// SSE 事件。`State` 是「结构变了，去拉 /api/state」的提示，不带载荷。
#[derive(Debug, Clone)]
pub enum Event {
    Log {
        task: String,
        line: String,
    },
    Progress {
        task: String,
        done: u64,
        total: u64,
        what: String,
        up_bps: u64,
        down_bps: u64,
    },
    Done {
        task: String,
        report: Option<ReportView>,
        error: Option<String>,
    },
    State,
}

/// 单个任务的运行时。
pub struct TaskRuntime {
    pub running: bool,
    pub progress: Option<ProgressView>,
    pub last: Option<ReportView>,
    pub last_error: Option<String>,
    pub last_time: Option<i64>,
    pub next_auto: Instant,
    pub logs: VecDeque<String>,
    pub watcher: Option<notify::RecommendedWatcher>,
    pub watch_root: String,
    pub dirty: bool,
    pub last_change: Instant,
    /// 进度事件节流：上一次广播的时刻。
    pub last_progress_emit: Instant,
    /// watcher 建失败后的下一次重试时刻（目录可能还没挂载）。
    pub watch_retry_at: Instant,
}

impl Default for TaskRuntime {
    fn default() -> Self {
        Self {
            running: false,
            progress: None,
            last: None,
            last_error: None,
            last_time: None,
            // 首次自动同步留 5s，别和 dsh 启动抢 IO
            next_auto: Instant::now() + std::time::Duration::from_secs(5),
            logs: VecDeque::new(),
            watcher: None,
            watch_root: String::new(),
            dirty: false,
            last_change: Instant::now(),
            last_progress_emit: Instant::now() - std::time::Duration::from_secs(1),
            watch_retry_at: Instant::now(),
        }
    }
}

impl TaskRuntime {
    /// 推一行日志进环形缓冲，超出上限就丢最老的。
    pub fn push_log(&mut self, line: impl Into<String>) {
        self.logs.push_back(line.into());
        while self.logs.len() > LOG_RING {
            self.logs.pop_front();
        }
    }
}

/// 设置的可序列化视图：任务列表单独由 `tasks` 提供，这里不重复。
#[derive(Debug, Clone, Serialize)]
pub struct SettingsView {
    pub auto_update: bool,
    pub log_keep_count: u32,
    pub log_keep_days: u32,
    pub limit_up_kbps: u32,
    pub limit_down_kbps: u32,
}

impl From<&Settings> for SettingsView {
    fn from(s: &Settings) -> Self {
        Self {
            auto_update: s.auto_update,
            log_keep_count: s.log_keep_count,
            log_keep_days: s.log_keep_days,
            limit_up_kbps: s.limit_up_kbps,
            limit_down_kbps: s.limit_down_kbps,
        }
    }
}

/// `/api/state` 里的任务行。
#[derive(Debug, Clone, Serialize)]
pub struct TaskView {
    pub id: String,
    pub name: String,
    pub display_name: String,
    pub server: String,
    pub server_short: String,
    pub workspace: String,
    pub root: String,
    pub has_token: bool,
    pub mode: sync_client::SyncMode,
    pub filter: Filter,
    pub filter_summary: String,
    pub filter_sig: String,
    pub paused: bool,
    pub issues: Vec<String>,
    pub runtime: RuntimeView,
}

#[derive(Debug, Clone, Serialize)]
pub struct RuntimeView {
    pub running: bool,
    pub progress: Option<ProgressView>,
    pub last: Option<ReportView>,
    pub last_error: Option<String>,
    pub last_time: Option<i64>,
    pub next_auto_in: u64,
    pub logs: Vec<String>,
}

impl TaskView {
    pub fn new(cfg: &Config, rt: &TaskRuntime) -> Self {
        let next_auto_in = rt
            .next_auto
            .saturating_duration_since(Instant::now())
            .as_secs();
        Self {
            id: cfg.id.clone(),
            name: cfg.name.clone(),
            display_name: cfg.display_name(),
            server: cfg.server.clone(),
            server_short: short_server(&cfg.server),
            workspace: cfg.workspace.clone(),
            root: cfg.root.clone(),
            has_token: !cfg.token.trim().is_empty(),
            mode: cfg.mode,
            filter: cfg.filter.clone(),
            filter_summary: filter_summary(&cfg.filter),
            filter_sig: cfg.filter.signature(),
            paused: cfg.paused,
            issues: cfg.issues(),
            runtime: RuntimeView {
                running: rt.running,
                // 传完之后 last_progress_emit 就不再更新：超过 3 秒就把速率清零，
                // 免得界面上一直挂着「↑ 2.4 MB/s」这种僵尸数字
                progress: rt.progress.clone().map(|mut p| {
                    if rt.last_progress_emit.elapsed() >= std::time::Duration::from_secs(3) {
                        p.up_bps = 0;
                        p.down_bps = 0;
                    }
                    p
                }),
                last: rt.last.clone(),
                last_error: rt.last_error.clone(),
                last_time: rt.last_time,
                next_auto_in,
                logs: rt.logs.iter().cloned().collect(),
            },
        }
    }
}

/// 任务列表 + 运行时列表，永远等长同序。
pub struct Inner {
    pub settings: Settings,
    pub runtimes: Vec<TaskRuntime>,
}

impl Inner {
    pub fn index_of(&self, id: &str) -> Option<usize> {
        self.settings
            .tasks
            .iter()
            .position(|t| t.id == id || t.name == id)
    }

    pub fn task(&self, id: &str) -> Option<&Config> {
        self.index_of(id).map(|i| &self.settings.tasks[i])
    }

    /// 任务列表快照，顺序与 `settings.tasks` 一致。
    pub fn task_views(&self) -> Vec<TaskView> {
        self.settings
            .tasks
            .iter()
            .zip(self.runtimes.iter())
            .map(|(cfg, rt)| TaskView::new(cfg, rt))
            .collect()
    }

    /// 给新任务补一条运行时，保证两个列表永远对齐。
    pub fn sync_runtimes(&mut self) {
        while self.runtimes.len() < self.settings.tasks.len() {
            self.runtimes.push(TaskRuntime::default());
        }
        self.runtimes.truncate(self.settings.tasks.len());
    }
}

/// 进程内共享状态。`axum` 的 handler 拿 `State<Arc<AppState>>`。
pub struct AppState {
    pub boot: BootInfo,
    pub inner: Mutex<Inner>,
    /// 自动同步总开关。不落盘：与 exe 的行为一致，重启回到「开」。
    pub auto: AtomicBool,
    pub events: broadcast::Sender<Event>,
    /// 同步线程 → 调度循环
    pub worker_tx: mpsc::UnboundedSender<WorkerMsg>,
    /// 目录监听 → 调度循环
    pub dirty_tx: mpsc::UnboundedSender<String>,
    /// 插件请求退出：发 true 让 axum 走 graceful shutdown，Drop 才能清掉 daemon.json
    pub shutdown: tokio::sync::watch::Sender<bool>,
}

/// 调度循环的接收端；放在这里是为了只在一处写通道类型。
pub type WorkerRx = mpsc::UnboundedReceiver<WorkerMsg>;
pub type DirtyRx = mpsc::UnboundedReceiver<String>;

/// worker 线程回传给调度循环的消息。
pub enum WorkerMsg {
    Event {
        task: String,
        ev: sync_client::Event,
    },
    Done {
        task: String,
        report: Option<ReportView>,
        error: Option<String>,
    },
}

impl AppState {
    /// 建状态，并把调度循环要用的两个接收端交出去。
    /// 通道只在这里建一次，之后 `AppState` 只持有发送端。
    pub fn new(
        boot: BootInfo,
        settings: Settings,
        shutdown: tokio::sync::watch::Sender<bool>,
    ) -> (Arc<Self>, WorkerRx, DirtyRx) {
        let (events, _) = broadcast::channel(1024);
        let (worker_tx, worker_rx) = mpsc::unbounded_channel();
        let (dirty_tx, dirty_rx) = mpsc::unbounded_channel();
        let mut inner = Inner {
            settings,
            runtimes: Vec::new(),
        };
        inner.sync_runtimes();
        let state = Arc::new(Self {
            boot,
            inner: Mutex::new(inner),
            auto: AtomicBool::new(true),
            events,
            worker_tx,
            dirty_tx,
            shutdown,
        });
        (state, worker_rx, dirty_rx)
    }

    pub fn is_auto(&self) -> bool {
        self.auto.load(Ordering::Relaxed)
    }

    pub fn set_auto(&self, on: bool) {
        self.auto.store(on, Ordering::Relaxed);
    }

    /// 广播一个事件；没有订阅者不算错误。
    pub fn emit(&self, event: Event) {
        let _ = self.events.send(event);
    }

    /// 往某个任务的环形日志里写一行，同时落到 sync.log 并广播。
    pub fn log_task(&self, task_id: &str, line: impl Into<String>) {
        let line = line.into();
        log_line(&format!("[{}] {}", self.task_name(task_id), line));
        if let Ok(mut inner) = self.inner.lock() {
            if let Some(i) = inner.index_of(task_id) {
                inner.runtimes[i].push_log(line.clone());
            }
        }
        self.emit(Event::Log {
            task: task_id.to_string(),
            line,
        });
    }

    pub fn task_name(&self, task_id: &str) -> String {
        self.inner
            .lock()
            .ok()
            .and_then(|inner| inner.task(task_id).map(|t| t.display_name()))
            .unwrap_or_else(|| "系统".to_string())
    }

    /// 任务的活动记录（读状态库，不碰内存态）。
    pub fn activities(&self, ids: &[String], limit: usize) -> Vec<(String, Activity)> {
        let mut out = Vec::new();
        let tasks: Vec<Config> = match self.inner.lock() {
            Ok(inner) => ids
                .iter()
                .filter_map(|id| inner.task(id).cloned())
                .collect(),
            Err(_) => return out,
        };
        for task in tasks {
            if let Ok(store) = sync_core::Store::open(&task.state_db()) {
                if let Ok(list) = store.recent_activities(limit) {
                    for a in list {
                        out.push((task.display_name(), a));
                    }
                }
            }
        }
        out.sort_by(|a, b| b.1.created_at.cmp(&a.1.created_at));
        out.truncate(limit);
        out
    }
}

/// 追加一行到自己的 `sync.log`（路径随 `WINSYNC_STATE_DIR` 走）。
pub fn log_line(s: &str) {
    let dir = sync_client::Config::dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("sync.log"))
    {
        use std::io::Write;
        let _ = writeln!(f, "[{}] {}", crate::util::stamp_full(), s);
    }
}
