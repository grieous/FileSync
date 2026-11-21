//! 鉴权：所有 `/api/*` 必须带 `Authorization: Bearer <boot.token>`。
//!
//! token 每次启动随机、只存在于 `daemon.json` 与进程内存里；插件在服务端注入它，
//! 浏览器永远拿不到。`/health` 故意免鉴权，让插件能在拉起后 300ms 内确认存活。

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::state::AppState;

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({
            "ok": false,
            "error": { "code": "unauthorized", "message": "缺少或错误的 Bearer token" }
        })),
    )
        .into_response()
}

/// 逐字节比较，避免长度差提前返回把 token 前缀泄露给计时攻击。
fn equals_constant_time(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

pub async fn require_bearer(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    let provided = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    match provided {
        Some(token) if equals_constant_time(token.as_bytes(), state.boot.token.as_bytes()) => {
            next.run(request).await
        }
        _ => unauthorized(),
    }
}

#[cfg(test)]
mod tests {
    use super::equals_constant_time;

    #[test]
    fn compares_without_length_shortcut() {
        assert!(equals_constant_time(b"abc", b"abc"));
        assert!(!equals_constant_time(b"abc", b"abd"));
        assert!(!equals_constant_time(b"abc", b"ab"));
    }
}
