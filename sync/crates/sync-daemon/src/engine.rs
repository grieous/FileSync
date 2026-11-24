//! 调度循环：把 exe 的 `App::update` 每帧逻辑改成每 250ms 一跳。
//!
//! 做四件事：
//!   1. 收 worker 消息（日志 / 进度 / 完成），更新运行时并广播 SSE
//!   2. 收目录变更，做 3 秒去抖
//!   3. 维护每个任务的 recursive watcher
//!   4. 到点就起一轮 `session::run`（轮询 28–34s 抖动，或变更去抖后）
//!
//! 同一任务永远不会并发两轮：`running` 是唯一闸门。

use std::sync::Arc;
use std::time::{Duration, Instant};

use notify::Watcher;

use crate::overlap;
use crate::state::{
    log_line, AppState, DirtyRx, Event, ProgressView, ReportView, WorkerMsg, WorkerRx,
};
use crate::util;

/// 自动同步的间隔下限。抖动 0–6 秒，避开多任务同刻打服务器。
const POLL_BASE_SECS: u64 = 28;
/// 目录变更后等这么久再同步，避免写一半就传。
const DEBOUNCE: Duration = Duration::from_secs(3);
/// 进度事件节流：同一个任务最多 5 次/秒。
const PROGRESS_EVERY: Duration = Duration::from_millis(200);
/// watcher 建失败后的重试间隔（目录可能是还没挂载的网络盘）。
const WATCH_RETRY: Duration = Duration::from_secs(5);

/// 启动同步被拒绝时的结构化原因，`api` 直接翻成 HTTP 409。
pub struct StartError {
    pub code: &'static str,
    pub message: String,
}

impl StartError {
    fn not_ready(message: impl Into<String>) -> Self {
        Self {
            code: "not-ready",
            message: message.into(),
        }
    }

    fn root_conflict(message: impl Into<String>) -> Self {
        Self {
            code: "root-conflict",
            message: message.into(),
        }
    }
}

pub async fn run(state: Arc<AppState>, mut worker_rx: WorkerRx, mut dirty_rx: DirtyRx) {
    let mut ticker = tokio::time::interval(Duration::from_millis(250));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        drain_worker(&state, &mut worker_rx);
        drain_dirty(&state, &mut dirty_rx);
        ensure_watchers(&state);
        for id in due_tasks(&state) {
            if let Err(e) = start_sync(&state, &id) {
                // 自动同步被拒不该刷屏：每个任务每轮最多一行
                state.log_task(&id, format!("自动同步跳过：{}", e.message));
            }
        }
    }
}

fn drain_worker(state: &Arc<AppState>, rx: &mut WorkerRx) {
    while let Ok(msg) = rx.try_recv() {
        match msg {
            WorkerMsg::Event { task, ev } => match ev {
                sync_client::Event::Log(line) => {
                    log_line(&format!("[{}] {}", state.task_name(&task), line));
                    if let Ok(mut inner) = state.inner.lock() {
                        if let Some(i) = inner.index_of(&task) {
                            inner.runtimes[i].push_log(line.clone());
                        }
                    }
                    state.emit(Event::Log { task, line });
                }
                sync_client::Event::Progress {
                    done,
                    total,
                    what,
                    up_bps,
                    down_bps,
                } => {
                    let mut emit = false;
                    if let Ok(mut inner) = state.inner.lock() {
                        if let Some(i) = inner.index_of(&task) {
                            let rt = &mut inner.runtimes[i];
                            rt.progress = Some(ProgressView {
                                done,
                                total,
                                what: what.clone(),
                                up_bps,
                                down_bps,
                            });
                            if rt.last_progress_emit.elapsed() >= PROGRESS_EVERY {
                                rt.last_progress_emit = Instant::now();
                                emit = true;
                            }
                        }
                    }
                    if emit {
                        state.emit(Event::Progress {
                            task,
                            done,
                            total,
                            what,
                            up_bps,
                            down_bps,
                        });
                    }
                }
            },
            WorkerMsg::Done {
                task,
                report,
                error,
            } => {
                let mut summary = String::new();
                if let Ok(mut inner) = state.inner.lock() {
                    if let Some(i) = inner.index_of(&task) {
                        let rt = &mut inner.runtimes[i];
                        rt.running = false;
                        rt.progress = None;
                        rt.dirty = false;
                        rt.last_time = Some(util::now_secs());
                        match (&report, &error) {
                            (Some(r), _) => {
                                rt.last = Some(r.clone());
                                rt.last_error = None;
                                summary = r.summary();
                            }
                            (None, Some(e)) => {
                                rt.last = None;
                                rt.last_error = Some(e.clone());
                                summary = format!("失败：{e}");
                            }
                            _ => {}
                        }
                    }
                }
                if !summary.is_empty() {
                    state.log_task(&task, summary);
                }
                state.emit(Event::Done {
                    task,
                    report,
                    error,
                });
                state.emit(Event::State);
            }
        }
    }
}

fn drain_dirty(state: &Arc<AppState>, rx: &mut DirtyRx) {
    while let Ok(id) = rx.try_recv() {
        if let Ok(mut inner) = state.inner.lock() {
            if let Some(i) = inner.index_of(&id) {
                let rt = &mut inner.runtimes[i];
                rt.dirty = true;
                rt.last_change = Instant::now();
            }
        }
    }
}

/// 目录换了、或上次没建成功，就（重新）挂 watcher。
fn ensure_watchers(state: &Arc<AppState>) {
    let wanted: Vec<(String, String)> = match state.inner.lock() {
        Ok(inner) => inner
            .settings
            .tasks
            .iter()
            .zip(inner.runtimes.iter())
            .filter(|(cfg, rt)| rt.watch_root != cfg.root && Instant::now() >= rt.watch_retry_at)
            .map(|(cfg, _)| (cfg.id.clone(), cfg.root.clone()))
            .collect(),
        Err(_) => return,
    };

    for (id, root) in wanted {
        let path = std::path::PathBuf::from(root.trim());
        let mut watcher = None;
        let mut ok = false;
        if path.is_dir() {
            let tx = state.dirty_tx.clone();
            let tid = id.clone();
            if let Ok(mut w) =
                notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                    if let Ok(ev) = res {
                        if !matches!(ev.kind, notify::EventKind::Access(_)) {
                            let _ = tx.send(tid.clone());
                        }
                    }
                })
            {
                if w.watch(&path, notify::RecursiveMode::Recursive).is_ok() {
                    watcher = Some(w);
                    ok = true;
                }
            }
        }
        if let Ok(mut inner) = state.inner.lock() {
            if let Some(i) = inner.index_of(&id) {
                let rt = &mut inner.runtimes[i];
                rt.watcher = watcher;
                if ok {
                    rt.watch_root = root;
                } else {
                    // 记下尝试时间而不是标记成功：目录晚点挂上来还能自动接上
                    rt.watch_retry_at = Instant::now() + WATCH_RETRY;
                }
            }
        }
    }
}

/// 这一跳该起哪些任务。返回 id 列表，锁在函数内释放。
fn due_tasks(state: &Arc<AppState>) -> Vec<String> {
    let auto = state.is_auto();
    let mut out = Vec::new();
    let Ok(mut inner) = state.inner.lock() else {
        return out;
    };
    for i in 0..inner.settings.tasks.len() {
        let cfg = &inner.settings.tasks[i];
        if !auto || cfg.paused {
            continue;
        }
        let id = cfg.id.clone();
        let has_issues = !cfg.issues().is_empty();
        let rt = &mut inner.runtimes[i];
        if rt.running {
            continue;
        }
        let due_poll = Instant::now() >= rt.next_auto;
        let due_change = rt.dirty && rt.last_change.elapsed() > DEBOUNCE;
        if !(due_poll || due_change) {
            continue;
        }
        rt.next_auto = Instant::now() + Duration::from_secs(POLL_BASE_SECS + util::jitter());
        rt.dirty = false;
        if has_issues {
            continue;
        }
        out.push(id);
    }
    out
}

/// 起一轮同步。`api` 的「立即同步」和调度循环都走这里，规则只写一遍。
pub fn start_sync(state: &Arc<AppState>, id: &str) -> Result<(), StartError> {
    let (cfg, limits) = {
        let mut inner = state
            .inner
            .lock()
            .map_err(|_| StartError::not_ready("内部状态不可用"))?;
        let Some(i) = inner.index_of(id) else {
            return Err(StartError::not_ready("任务不存在"));
        };
        if inner.runtimes[i].running {
            return Err(StartError::not_ready("这个任务正在同步"));
        }
        let cfg = inner.settings.tasks[i].clone();
        let issues = cfg.issues();
        if !issues.is_empty() {
            return Err(StartError::not_ready(issues.join("；")));
        }
        let limits = sync_client::Limits {
            up_kbps: inner.settings.limit_up_kbps,
            down_kbps: inner.settings.limit_down_kbps,
        };
        // 先置 running，再放锁：避免两个入口（按钮 + 调度）同时挤进来
        inner.runtimes[i].running = true;
        inner.runtimes[i].progress = None;
        let _ = inner.settings.save();
        (cfg, limits)
    };

    // 目录重叠检测放在锁外：它要读 exe 的配置，可能慢一下
    if let Err(message) = overlap::conflict_for(&cfg.root) {
        if let Ok(mut inner) = state.inner.lock() {
            if let Some(i) = inner.index_of(&cfg.id) {
                inner.runtimes[i].running = false;
            }
        }
        state.log_task(&cfg.id, message.clone());
        state.emit(Event::State);
        return Err(StartError::root_conflict(message));
    }

    state.log_task(&cfg.id, format!("开始同步 {}", cfg.root));
    state.emit(Event::State);

    let worker_tx = state.worker_tx.clone();
    let task_id = cfg.id.clone();
    // 同步会话是重阻塞 IO（哈希、读写盘），放独立线程 + current_thread runtime，
    // 别把 tokio 的 worker 线程占住——否则同一时刻其它 HTTP 请求都会卡。
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                let _ = worker_tx.send(WorkerMsg::Done {
                    task: task_id,
                    report: None,
                    error: Some(format!("运行时创建失败：{e}")),
                });
                return;
            }
        };

        let (evt_tx, evt_rx) = std::sync::mpsc::channel();
        let fwd = {
            let tx = worker_tx.clone();
            let id = cfg.id.clone();
            std::thread::spawn(move || {
                while let Ok(ev) = evt_rx.recv() {
                    if tx
                        .send(WorkerMsg::Event {
                            task: id.clone(),
                            ev,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
        };

        let res = rt.block_on(sync_client::run(&cfg, Some(evt_tx), true, true, 25, limits));
        let _ = fwd.join();

        let msg = match res {
            Ok(report) => WorkerMsg::Done {
                task: cfg.id.clone(),
                report: Some(ReportView::from_report(&report)),
                error: None,
            },
            Err(e) => WorkerMsg::Done {
                task: cfg.id.clone(),
                report: None,
                error: Some(crate::util::describe_error(&e)),
            },
        };
        let _ = worker_tx.send(msg);
    });

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_error_carries_the_code_the_ui_switches_on() {
        assert_eq!(StartError::not_ready("x").code, "not-ready");
        assert_eq!(StartError::root_conflict("y").code, "root-conflict");
    }
}
