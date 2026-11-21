//! SSE：一条长连接把「刚发生了什么」推给界面。
//!
//! 只做广播转发，不做状态聚合——权威快照永远在 `/api/state`，
//! 事件丢了（Lag）就发一条 `lagged`，前端收到后重新拉一次 state 即可。

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use futures_util::stream::{self, Stream, StreamExt};
use tokio::sync::broadcast::error::RecvError;

use crate::state::{AppState, Event};

fn encode(event: Event) -> SseEvent {
    match event {
        Event::Log { task, line } => SseEvent::default()
            .event("log")
            .data(serde_json::json!({ "task": task, "line": line }).to_string()),
        Event::Progress {
            task,
            done,
            total,
            what,
        } => SseEvent::default().event("progress").data(
            serde_json::json!({ "task": task, "done": done, "total": total, "what": what })
                .to_string(),
        ),
        Event::Done {
            task,
            report,
            error,
        } => SseEvent::default().event("done").data(
            serde_json::json!({ "task": task, "report": report, "error": error }).to_string(),
        ),
        Event::State => SseEvent::default().event("state").data("{}"),
    }
}

/// 建一条 SSE 流。客户端断开时 stream 被 drop，`broadcast` 订阅自动回收。
pub fn stream(state: Arc<AppState>) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let rx = state.events.subscribe();
    let hello = SseEvent::default().event("hello").data(
        serde_json::json!({
            "version": state.boot.version,
            "pid": state.boot.pid,
        })
        .to_string(),
    );

    let live = stream::unfold(rx, |mut rx| async move {
        match rx.recv().await {
            Ok(ev) => Some((Ok(encode(ev)), rx)),
            // 慢客户端：告诉它「你漏了 N 条，重新对账」，而不是静默丢
            Err(RecvError::Lagged(n)) => Some((
                Ok(SseEvent::default()
                    .event("lagged")
                    .data(serde_json::json!({ "dropped": n }).to_string())),
                rx,
            )),
            Err(RecvError::Closed) => None,
        }
    });

    Sse::new(stream::once(async move { Ok(hello) }).chain(live)).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keepalive"),
    )
}
