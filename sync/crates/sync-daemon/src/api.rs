//! HTTP 契约。所有 `/api/*` 走 Bearer 鉴权，`/health` 免鉴权。
//!
//! 错误统一 `{ok:false,error:{code,message}}`：前端按 `code` 分支，不匹配文案。
//! code 取值固定为 `not-ready` / `root-conflict` / `root-changed` / `bad-request` /
//! `unauthorized` / `internal`。

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sync_client::{Config, SyncMode};
use sync_core::{Activity, Filter};

use crate::engine;
use crate::state::{AppState, Event, SettingsView, TaskView};
use crate::util;
use crate::{auth, events, fsapi, overlap, win};

/// 一处定义错误形状，避免每个 handler 自己拼 JSON。
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    fn bad(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad-request", message)
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "bad-request", message)
    }

    fn conflict(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, code, message)
    }

    fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({
                "ok": false,
                "error": { "code": self.code, "message": self.message }
            })),
        )
            .into_response()
    }
}

impl From<engine::StartError> for ApiError {
    fn from(e: engine::StartError) -> Self {
        Self::conflict(e.code, e.message)
    }
}

type ApiResult<T = Json<Value>> = Result<T, ApiError>;

/// 锁被毒化只可能是别的 handler panic 了；这里统一翻成 500，不让进程躺下。
fn lock_err() -> ApiError {
    ApiError::internal("内部状态不可用（锁已中毒）")
}

pub fn router(state: Arc<AppState>) -> Router {
    let api = Router::new()
        .route("/api/state", get(get_state))
        .route("/api/auto", post(set_auto))
        .route("/api/tasks", post(create_task))
        .route(
            "/api/tasks/:id",
            axum::routing::patch(patch_task).delete(delete_task),
        )
        .route("/api/tasks/:id/sync", post(sync_now))
        .route("/api/tasks/:id/pause", post(pause_task))
        .route("/api/tasks/:id/resume", post(resume_task))
        .route(
            "/api/activities",
            get(get_activities).delete(clear_activities),
        )
        .route("/api/export.csv", get(export_csv))
        .route("/api/logs", get(get_logs))
        .route("/api/logfile", get(get_logfile))
        .route("/api/prune", post(prune))
        .route("/api/autostart", get(get_autostart).post(set_autostart))
        .route("/api/open-dir", post(open_dir))
        .route("/api/fs/list", get(fs_list))
        .route("/api/events", get(events_handler))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_bearer,
        ));

    Router::new()
        .route("/health", get(health))
        .merge(api)
        .with_state(state)
}

/* ------------------------------------------------------------------ 基础 */

async fn health(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(json!({
        "ok": true,
        "version": state.boot.version,
        "pid": state.boot.pid,
    }))
}

#[derive(Serialize)]
struct StateResponse {
    version: String,
    pid: u32,
    data_dir: String,
    config_path: String,
    device: String,
    auto: bool,
    settings: SettingsView,
    tasks: Vec<TaskView>,
}

async fn get_state(State(state): State<Arc<AppState>>) -> ApiResult {
    let inner = state.inner.lock().map_err(|_| lock_err())?;
    let body = StateResponse {
        version: state.boot.version.clone(),
        pid: state.boot.pid,
        data_dir: state.boot.data_dir.display().to_string(),
        config_path: state.boot.config_path.display().to_string(),
        device: std::env::var("COMPUTERNAME").unwrap_or_else(|_| "本机".into()),
        auto: state.is_auto(),
        settings: SettingsView::from(&inner.settings),
        tasks: inner.task_views(),
    };
    Ok(Json(serde_json::to_value(body).unwrap_or_default()))
}

#[derive(Deserialize)]
struct AutoInput {
    on: bool,
}

async fn set_auto(State(state): State<Arc<AppState>>, Json(body): Json<AutoInput>) -> ApiResult {
    state.set_auto(body.on);
    state.emit(Event::State);
    Ok(Json(json!({ "ok": true, "auto": body.on })))
}

/* ------------------------------------------------------------------ 任务 */

#[derive(Deserialize)]
struct TaskInput {
    #[serde(default)]
    name: String,
    server: String,
    #[serde(default)]
    token: String,
    #[serde(default = "default_workspace")]
    workspace: String,
    root: String,
    #[serde(default)]
    mode: SyncMode,
    #[serde(default)]
    filter: Filter,
    #[serde(default)]
    paused: bool,
}

fn default_workspace() -> String {
    "default".to_string()
}

impl TaskInput {
    fn into_config(self) -> Result<Config, ApiError> {
        let mut cfg = Config {
            id: sync_client::new_id(),
            name: self.name.trim().to_string(),
            server: self.server.trim().to_string(),
            token: self.token.trim().to_string(),
            workspace: self.workspace.trim().to_string(),
            root: self.root.trim().to_string(),
            mode: self.mode,
            filter: self.filter,
            paused: self.paused,
        };
        let issues = cfg.issues();
        if !issues.is_empty() {
            return Err(ApiError::bad(issues.join("；")));
        }
        if cfg.name.is_empty() {
            cfg.name = cfg.display_name();
        }
        Ok(cfg)
    }
}

async fn create_task(
    State(state): State<Arc<AppState>>,
    Json(input): Json<TaskInput>,
) -> ApiResult {
    let cfg = input.into_config()?;
    if let Err(message) = overlap::conflict_for(&cfg.root) {
        return Err(ApiError::conflict("root-conflict", message));
    }

    let view = {
        let mut inner = state.inner.lock().map_err(|_| lock_err())?;
        inner.settings.tasks.push(cfg.clone());
        inner.sync_runtimes();
        let idx = inner.settings.tasks.len() - 1;
        let view = TaskView::new(&inner.settings.tasks[idx], &inner.runtimes[idx]);
        inner
            .settings
            .save()
            .map_err(|e| ApiError::internal(format!("保存配置失败：{e:#}")))?;
        view
    };

    state.log_task(&cfg.id, "任务已创建");
    state.emit(Event::State);
    Ok(Json(json!({ "ok": true, "task": view })))
}

#[derive(Deserialize)]
struct TaskPatch {
    name: Option<String>,
    server: Option<String>,
    /// `Some("")` 表示「不改」——界面上密钥框留空就是不动原值。
    token: Option<String>,
    workspace: Option<String>,
    root: Option<String>,
    mode: Option<SyncMode>,
    filter: Option<Filter>,
    paused: Option<bool>,
    /// 换目录必须显式带 true，否则 409 root-changed。
    #[serde(default)]
    force: bool,
}

async fn patch_task(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(patch): Json<TaskPatch>,
) -> ApiResult {
    let view = {
        let mut inner = state.inner.lock().map_err(|_| lock_err())?;
        let idx = inner
            .index_of(&id)
            .ok_or_else(|| ApiError::not_found("任务不存在"))?;

        let mut cfg = inner.settings.tasks[idx].clone();
        if let Some(v) = patch.name {
            cfg.name = v.trim().to_string();
        }
        if let Some(v) = patch.server {
            cfg.server = v.trim().to_string();
        }
        if let Some(v) = patch.token {
            if !v.trim().is_empty() {
                cfg.token = v.trim().to_string();
            }
        }
        if let Some(v) = patch.workspace {
            cfg.workspace = v.trim().to_string();
        }
        if let Some(v) = patch.mode {
            cfg.mode = v;
        }
        if let Some(v) = patch.filter {
            cfg.filter = v;
        }
        if let Some(v) = patch.paused {
            cfg.paused = v;
        }
        if let Some(v) = patch.root {
            let next = v.trim().to_string();
            if next != cfg.root {
                if !patch.force {
                    return Err(ApiError::conflict(
                        "root-changed",
                        "更换同步目录会重建本地基线，需要显式确认（force: true）",
                    ));
                }
                if let Err(message) = overlap::conflict_for(&next) {
                    return Err(ApiError::conflict("root-conflict", message));
                }
                cfg.root = next;
            }
        }

        let issues = cfg.issues();
        if !issues.is_empty() {
            return Err(ApiError::bad(issues.join("；")));
        }
        if cfg.name.is_empty() {
            cfg.name = cfg.display_name();
        }

        inner.settings.tasks[idx] = cfg;
        // 目录可能变了：清掉 watcher 标记，让调度循环下一拍重建
        inner.runtimes[idx].watch_root.clear();
        inner.runtimes[idx].watch_retry_at = std::time::Instant::now();
        let view = TaskView::new(&inner.settings.tasks[idx], &inner.runtimes[idx]);
        inner
            .settings
            .save()
            .map_err(|e| ApiError::internal(format!("保存配置失败：{e:#}")))?;
        view
    };

    state.emit(Event::State);
    Ok(Json(json!({ "ok": true, "task": view })))
}

async fn delete_task(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> ApiResult {
    let name = {
        let mut inner = state.inner.lock().map_err(|_| lock_err())?;
        let idx = inner
            .index_of(&id)
            .ok_or_else(|| ApiError::not_found("任务不存在"))?;
        let task = inner.settings.tasks.remove(idx);
        if idx < inner.runtimes.len() {
            inner.runtimes.remove(idx);
        }
        inner.sync_runtimes();
        inner
            .settings
            .save()
            .map_err(|e| ApiError::internal(format!("保存配置失败：{e:#}")))?;
        task.display_name()
    };
    // 状态库故意不删：删错了还能从记录里看出同步过什么
    crate::state::log_line(&format!("已删除任务 {name}（状态库保留）"));
    state.emit(Event::State);
    Ok(Json(json!({ "ok": true })))
}

async fn sync_now(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> ApiResult {
    let cfg_id = {
        let inner = state.inner.lock().map_err(|_| lock_err())?;
        inner
            .task(&id)
            .map(|t| t.id.clone())
            .ok_or_else(|| ApiError::not_found("任务不存在"))?
    };
    engine::start_sync(&state, &cfg_id)?;
    Ok(Json(json!({ "ok": true })))
}

async fn pause_task(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> ApiResult {
    set_paused(&state, &id, true)
}

async fn resume_task(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> ApiResult {
    set_paused(&state, &id, false)
}

fn set_paused(state: &Arc<AppState>, id: &str, paused: bool) -> ApiResult {
    let task_id = {
        let mut inner = state.inner.lock().map_err(|_| lock_err())?;
        let idx = inner
            .index_of(id)
            .ok_or_else(|| ApiError::not_found("任务不存在"))?;
        inner.settings.tasks[idx].paused = paused;
        inner
            .settings
            .save()
            .map_err(|e| ApiError::internal(format!("保存配置失败：{e:#}")))?;
        inner.settings.tasks[idx].id.clone()
    };
    state.log_task(
        &task_id,
        if paused {
            "任务已暂停（不再自动同步，仍可手动「立即同步」）"
        } else {
            "任务已恢复自动同步"
        },
    );
    state.emit(Event::State);
    Ok(Json(json!({ "ok": true, "paused": paused })))
}

/* ------------------------------------------------------------------ 记录 */

#[derive(Deserialize)]
struct ActivityQuery {
    #[serde(default)]
    task: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Serialize)]
struct ActivityView {
    task: String,
    path: String,
    action: String,
    ok: bool,
    message: String,
    created_at: i64,
}

/// `task=all` 或省略 → 所有任务；否则只取那一个。
fn target_ids(state: &Arc<AppState>, task: Option<&str>) -> Result<Vec<String>, ApiError> {
    let inner = state.inner.lock().map_err(|_| lock_err())?;
    match task {
        None | Some("all") | Some("") => {
            Ok(inner.settings.tasks.iter().map(|t| t.id.clone()).collect())
        }
        Some(id) => {
            let idx = inner
                .index_of(id)
                .ok_or_else(|| ApiError::not_found("任务不存在"))?;
            Ok(vec![inner.settings.tasks[idx].id.clone()])
        }
    }
}

fn query_rows(state: &Arc<AppState>, query: &ActivityQuery) -> Result<Vec<ActivityView>, ApiError> {
    let ids = target_ids(state, query.task.as_deref())?;
    let limit = query.limit.unwrap_or(500).clamp(1, 2000);
    let needle = query.q.as_deref().unwrap_or("").trim().to_lowercase();
    let status = query.status.as_deref().unwrap_or("all");

    let mut rows: Vec<ActivityView> = state
        .activities(&ids, limit * 2)
        .into_iter()
        .filter(|(_, a)| match status {
            "ok" => a.ok,
            "fail" => !a.ok,
            _ => true,
        })
        .filter(|(_, a)| {
            needle.is_empty()
                || a.path.to_lowercase().contains(&needle)
                || a.message.to_lowercase().contains(&needle)
        })
        .map(|(task, a)| ActivityView {
            task,
            path: a.path,
            action: a.action,
            ok: a.ok,
            message: a.message,
            created_at: a.created_at,
        })
        .collect();
    rows.truncate(limit);
    Ok(rows)
}

async fn get_activities(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ActivityQuery>,
) -> ApiResult {
    let rows = query_rows(&state, &query)?;
    let total = rows.len();
    Ok(Json(json!({ "ok": true, "rows": rows, "total": total })))
}

async fn clear_activities(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ActivityQuery>,
) -> ApiResult {
    let ids = target_ids(&state, query.task.as_deref())?;
    let mut deleted = 0usize;
    for id in ids {
        let db = {
            let inner = state.inner.lock().map_err(|_| lock_err())?;
            inner.task(&id).map(|t| t.state_db())
        };
        let Some(db) = db else { continue };
        if let Ok(store) = sync_core::Store::open(&db) {
            if let Ok(list) = store.recent_activities(100_000) {
                deleted += list.len();
            }
            let _ = store.clear_activities();
        }
    }
    state.emit(Event::State);
    Ok(Json(json!({ "ok": true, "deleted": deleted })))
}

async fn export_csv(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ActivityQuery>,
) -> Result<Response, ApiError> {
    let rows = query_rows(&state, &query)?;
    // 表头与 exe 导出的完全一致，否则两边的表没法直接对比
    let mut out = String::from("time,task,file,event,status,message\n");
    for r in &rows {
        out.push_str(&format!(
            "{},{},{},{},{},{}\n",
            util::fmt_ts_full(r.created_at),
            util::csv_escape(&r.task),
            util::csv_escape(&r.path),
            r.action,
            if r.ok { "成功" } else { "失败" },
            util::csv_escape(&r.message)
        ));
    }
    Ok((
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"winsync-records.csv\"",
            ),
        ],
        out,
    )
        .into_response())
}

#[derive(Deserialize)]
struct LogQuery {
    #[serde(default)]
    task: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

async fn get_logs(State(state): State<Arc<AppState>>, Query(query): Query<LogQuery>) -> ApiResult {
    let inner = state.inner.lock().map_err(|_| lock_err())?;
    let limit = query.limit.unwrap_or(400).min(crate::state::LOG_RING);
    let lines: Vec<String> = match query.task.as_deref() {
        Some(id) if !id.is_empty() && id != "all" => {
            let idx = inner
                .index_of(id)
                .ok_or_else(|| ApiError::not_found("任务不存在"))?;
            let rt = &inner.runtimes[idx];
            rt.logs.iter().rev().take(limit).rev().cloned().collect()
        }
        _ => inner
            .runtimes
            .iter()
            .flat_map(|rt| rt.logs.iter().cloned())
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .take(limit)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect(),
    };
    Ok(Json(json!({ "ok": true, "lines": lines })))
}

#[derive(Deserialize)]
struct LogFileQuery {
    #[serde(default)]
    lines: Option<usize>,
}

async fn get_logfile(Query(query): Query<LogFileQuery>) -> ApiResult {
    let n = query.lines.unwrap_or(200).clamp(1, 5000);
    let path = Config::dir().join("sync.log");
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(n);
    let lines: Vec<String> = all[start..].iter().map(|s| s.to_string()).collect();
    Ok(Json(
        json!({ "ok": true, "path": path.display().to_string(), "lines": lines }),
    ))
}

/* ------------------------------------------------------------------ 维护 */

async fn prune(State(state): State<Arc<AppState>>) -> ApiResult {
    let (days, keep, ids) = {
        let inner = state.inner.lock().map_err(|_| lock_err())?;
        (
            inner.settings.log_keep_days,
            inner.settings.log_keep_count,
            inner
                .settings
                .tasks
                .iter()
                .map(|t| t.id.clone())
                .collect::<Vec<_>>(),
        )
    };
    if days == 0 && keep == 0 {
        return Ok(Json(
            json!({ "ok": true, "pruned": 0, "skipped": "未设置保留策略" }),
        ));
    }
    let mut pruned = 0usize;
    for id in ids {
        let db = {
            let inner = state.inner.lock().map_err(|_| lock_err())?;
            inner.task(&id).map(|t| t.state_db())
        };
        let Some(db) = db else { continue };
        if let Ok(store) = sync_core::Store::open(&db) {
            if days > 0 {
                let cutoff = util::now_secs() - days as i64 * 86400;
                if let Ok(n) = store.prune_activities_before(cutoff) {
                    pruned += n;
                }
            }
            if keep > 0 {
                let _ = store.prune_activities(keep as usize);
            }
        }
    }
    state.emit(Event::State);
    Ok(Json(json!({ "ok": true, "pruned": pruned })))
}

async fn get_autostart() -> ApiResult {
    Ok(Json(json!({ "ok": true, "on": win::autostart_get() })))
}

#[derive(Deserialize)]
struct AutostartInput {
    on: bool,
}

async fn set_autostart(Json(body): Json<AutostartInput>) -> ApiResult {
    let ok = win::autostart_set(body.on);
    if !ok {
        return Err(ApiError::internal("写入开机自启注册表失败"));
    }
    Ok(Json(json!({ "ok": true, "on": body.on })))
}

#[derive(Deserialize)]
struct OpenDirInput {
    #[serde(default)]
    task: Option<String>,
    #[serde(default)]
    path: Option<String>,
}

async fn open_dir(State(state): State<Arc<AppState>>, Json(body): Json<OpenDirInput>) -> ApiResult {
    // 只允许打开任务根目录本身：不接受任意路径，避免变成「随便读写本机」的入口
    let root = {
        let inner = state.inner.lock().map_err(|_| lock_err())?;
        match body.task.as_deref() {
            Some(id) => inner
                .task(id)
                .map(|t| t.root.clone())
                .ok_or_else(|| ApiError::not_found("任务不存在"))?,
            None => return Err(ApiError::bad("需要指定 task")),
        }
    };
    if let Some(path) = body.path.as_deref() {
        let wanted = std::path::PathBuf::from(path);
        let root_path = std::path::PathBuf::from(&root);
        let same = match (wanted.canonicalize(), root_path.canonicalize()) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        };
        if !same {
            return Err(ApiError::bad("只能打开任务自己的同步目录"));
        }
    }
    if !win::open_dir(std::path::Path::new(&root)) {
        return Err(ApiError::internal("打开资源管理器失败"));
    }
    Ok(Json(json!({ "ok": true, "path": root })))
}

#[derive(Deserialize)]
struct FsQuery {
    task: String,
    #[serde(default)]
    path: Option<String>,
}

async fn fs_list(State(state): State<Arc<AppState>>, Query(query): Query<FsQuery>) -> ApiResult {
    let root = {
        let inner = state.inner.lock().map_err(|_| lock_err())?;
        inner
            .task(&query.task)
            .map(|t| t.root.clone())
            .ok_or_else(|| ApiError::not_found("任务不存在"))?
    };
    let view =
        fsapi::list(&root, query.path.as_deref().unwrap_or("")).map_err(|e| ApiError::bad(e))?;
    Ok(Json(json!({
        "ok": true,
        "path": view.path,
        "parent": view.parent,
        "dirs": view.dirs,
    })))
}

async fn events_handler(
    State(state): State<Arc<AppState>>,
) -> axum::response::sse::Sse<
    impl futures_util::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>,
> {
    events::stream(state)
}

/// 让 `Activity` 在别处被引用时不用再 import sync_core（也提醒读代码的人：这是 DTO 源）。
#[allow(dead_code)]
fn _activity_type_hint(_: &Activity) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_input_rejects_broken_config() {
        let input = TaskInput {
            name: String::new(),
            server: "http://x".into(),
            token: String::new(),
            workspace: "default".into(),
            root: String::new(),
            mode: SyncMode::Bidirectional,
            filter: Filter::default(),
            paused: false,
        };
        let err = input.into_config().unwrap_err();
        assert_eq!(err.code, "bad-request");
        assert!(err.message.contains("ws://"));
    }

    #[test]
    fn patch_force_flag_defaults_to_false() {
        let patch: TaskPatch = serde_json::from_str(r#"{"root":"D:\\X"}"#).unwrap();
        assert!(!patch.force);
    }

    #[test]
    fn timestamps_in_export_use_full_form() {
        assert_eq!(util::fmt_ts_full(0), "1970-01-01 08:00:00");
    }
}
