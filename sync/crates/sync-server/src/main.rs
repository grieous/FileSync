//! 无状态中继（stateless relay）。
//!
//! 职责只有三件：
//!   1. 校验 token
//!   2. 按 workspace 把两台设备的连接配对
//!   3. 在配对的两个连接之间原样转发帧
//!
//! **不写磁盘、不建数据库、不需要 volume。** 进程重启只是所有连接断开，
//! 客户端会自动重连并重新配对，没有任何状态需要恢复。
//!
//! 启动：
//!   SYNC_TOKEN=<shared> SYNC_ADDR=0.0.0.0:8443 cargo run -p sync-server
//!
//! TLS：本进程只监听明文 HTTP/WS。生产环境请放在 Caddy / nginx 后面终止 TLS，
//! 或后续接入 rustls（见设计文档第 4.1 节）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use sync_proto::{Frame, Msg};
use tokio::sync::{mpsc, Mutex};

/// 连接空闲多久判定为僵尸。
/// 客户端两次同步之间会有几十秒静默，所以不能设太短。
/// 可用 `SYNC_IDLE_SECS` 覆盖（测试僵尸回收逻辑时很有用）。
fn idle_timeout() -> std::time::Duration {
    let secs = std::env::var("SYNC_IDLE_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(180);
    std::time::Duration::from_secs(secs)
}

#[derive(Default)]
struct Room {
    clients: Vec<Client>,
}

struct Client {
    id: u64,
    device_id: String,
    tx: mpsc::UnboundedSender<Frame>,
}

#[derive(Clone)]
struct AppState {
    token: Arc<String>,
    rooms: Arc<Mutex<HashMap<String, Room>>>,
    next_id: Arc<AtomicU64>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let token = std::env::var("SYNC_TOKEN").unwrap_or_else(|_| {
        tracing::warn!("SYNC_TOKEN 未设置，使用开发默认值 dev-token（生产环境必须设置）");
        "dev-token".to_string()
    });
    let addr = std::env::var("SYNC_ADDR").unwrap_or_else(|_| "0.0.0.0:8443".to_string());

    let state = AppState {
        token: Arc::new(token),
        rooms: Arc::new(Mutex::new(HashMap::new())),
        next_id: Arc::new(AtomicU64::new(1)),
    };

    let app = Router::new().route("/ws", get(ws_handler)).with_state(state);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| panic!("绑定 {addr} 失败: {e}"));

    tracing::info!("relay 已启动于 {addr}（无状态，不落盘）");
    axum::serve(listener, app).await.expect("server 异常退出");
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(st): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if q.get("token").map(String::as_str) != Some(st.token.as_str()) {
        tracing::warn!("拒绝连接：token 不匹配");
        return (StatusCode::UNAUTHORIZED, "bad token").into_response();
    }
    let room = q
        .get("workspace")
        .cloned()
        .unwrap_or_else(|| "default".to_string());
    ws.on_upgrade(move |socket| handle(socket, st, room))
        .into_response()
}

async fn handle(socket: WebSocket, st: AppState, default_room: String) {
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Frame>();
    let my_id = st.next_id.fetch_add(1, Ordering::Relaxed);

    let mut joined: Option<String> = None;
    let mut device_id = String::new();

    loop {
        tokio::select! {
            outbound = rx.recv() => {
                let Some(frame) = outbound else { break };
                if sink.send(Message::Binary(frame.encode())).await.is_err() {
                    break;
                }
            }

            incoming = tokio::time::timeout(idle_timeout(), stream.next()) => {
                let msg = match incoming {
                    // 超时：客户端可能已经崩了/被强杀，TCP 没来得及 FIN。
                    // 不清理的话它会一直占着房间，让后续连接"配对成功"到一个死连接。
                    Err(_) => {
                        tracing::info!("conn {my_id} 空闲超时，判定为僵尸连接，关闭");
                        break;
                    }
                    Ok(None) => break,
                    Ok(Some(Err(_))) => break,
                    Ok(Some(Ok(m))) => m,
                };

                let data = msg.into_data();
                if data.is_empty() { continue; }

                let frame = match Frame::decode(&data) {
                    Ok(f) => f,
                    Err(e) => {
                        tracing::warn!("conn {my_id}: 非法帧，忽略: {e}");
                        continue;
                    }
                };

                // 尚未入房：只接受 Hello
                if joined.is_none() {
                    if let Frame::Control(Msg::Hello { device_id: d, workspace_id, proto_ver }) = &frame {
                        if *proto_ver != sync_proto::PROTO_VER {
                            let bye = Frame::Control(Msg::Bye {
                                reason: format!("协议版本不匹配，服务端 {}", sync_proto::PROTO_VER),
                            });
                            let _ = sink.send(Message::Binary(bye.encode())).await;
                            break;
                        }
                        device_id = d.clone();
                        let name = if workspace_id.is_empty() { default_room.clone() } else { workspace_id.clone() };
                        tracing::info!("conn {my_id} 加入 {name}，device={device_id}");
                        if !join(&st, &name, Client { id: my_id, device_id: device_id.clone(), tx: tx.clone() }).await {
                            break;
                        }
                        joined = Some(name);
                    }
                    continue;
                }

                forward(&st, my_id, frame).await;
            }
        }
    }

    if let Some(name) = joined {
        leave(&st, &name, my_id).await;
        tracing::info!("conn {my_id} 离开 {name}，device={device_id}");
    }
}

/// 入房。房间满（>2）返回 false，连接会被关闭。
async fn join(st: &AppState, room_name: &str, client: Client) -> bool {
    let mut rooms = st.rooms.lock().await;
    let room = rooms.entry(room_name.to_string()).or_default();

    // 同一台设备重连时，先踢掉它自己那条旧连接。
    // 否则旧连接（可能是僵尸）会一直占着位置，让新连接"配对成功"到一个死连接上。
    if let Some(pos) = room
        .clients
        .iter()
        .position(|c| c.device_id == client.device_id)
    {
        let old = room.clients.remove(pos);
        let _ = old.tx.send(Frame::Control(Msg::Bye {
            reason: "同一设备重新连接，旧连接被替换".into(),
        }));
        tracing::info!("房间 {room_name} 踢掉同 device 的旧连接 conn {}", old.id);
    }

    if room.clients.len() >= 2 {
        let _ = client.tx.send(Frame::Control(Msg::Bye {
            reason: "房间已满：本版本仅支持 2 台设备".into(),
        }));
        return false;
    }

    room.clients.push(client);

    if room.clients.len() == 2 {
        let info: Vec<(u64, String)> = room
            .clients
            .iter()
            .map(|c| (c.id, c.device_id.clone()))
            .collect();
        for i in 0..2 {
            let peer = &info[1 - i];
            let _ = room.clients[i].tx.send(Frame::Control(Msg::PeerStatus {
                peer_online: true,
                peer_device_id: Some(peer.1.clone()),
            }));
        }
        tracing::info!("房间 {room_name} 配对成功");
    } else {
        let idx = room.clients.len() - 1;
        let _ = room.clients[idx].tx.send(Frame::Control(Msg::PeerStatus {
            peer_online: false,
            peer_device_id: None,
        }));
    }
    true
}

async fn leave(st: &AppState, room_name: &str, id: u64) {
    let mut rooms = st.rooms.lock().await;
    if let Some(room) = rooms.get_mut(room_name) {
        room.clients.retain(|c| c.id != id);
        for c in &room.clients {
            let _ = c.tx.send(Frame::Control(Msg::PeerStatus {
                peer_online: false,
                peer_device_id: None,
            }));
        }
        if room.clients.is_empty() {
            rooms.remove(room_name);
        }
    }
}

async fn forward(st: &AppState, from: u64, frame: Frame) {
    let rooms = st.rooms.lock().await;
    for room in rooms.values() {
        if room.clients.iter().any(|c| c.id == from) {
            for c in &room.clients {
                if c.id != from {
                    let _ = c.tx.send(frame.clone());
                }
            }
            return;
        }
    }
}
