//! `WS /api/stream` (`TASKS.md` T5.1, issue #18). For this task, the socket
//! only has to prove it's alive and honest about it: send a `hello` on
//! connect, then a `heartbeat` every [`HEARTBEAT_INTERVAL`] until the
//! client disconnects. The frontend (`webui/src/lib/connection.ts`) never
//! calls a bare WS `open` event "connected" — only an actual message does
//! — and treats a stall in these heartbeats as a disconnect too, which is
//! what this endpoint's steady cadence exists to make possible.
//!
//! Actual device data (`subscribe`-style pushes) is T5.2's job; wiring
//! that in is exactly why this handler already threads `Arc<Shared>`
//! through rather than being stateless.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use serde_json::json;

use crate::protocol::Shared;

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);

pub fn routes() -> Router<Arc<Shared>> {
    Router::new().route("/api/stream", get(upgrade))
}

async fn upgrade(ws: WebSocketUpgrade, State(shared): State<Arc<Shared>>) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, shared))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

async fn handle_socket(mut socket: WebSocket, shared: Arc<Shared>) {
    let hello = json!({
        "type": "hello",
        "server_version": shared.server_version,
        "device_count": shared.backend.list_devices().len(),
        "ts": now_ms(),
    });
    if socket
        .send(Message::Text(hello.to_string().into()))
        .await
        .is_err()
    {
        return;
    }

    let mut ticker = tokio::time::interval(HEARTBEAT_INTERVAL);
    ticker.tick().await; // first tick fires immediately — skip it, `hello` just played that role.

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let heartbeat = json!({
                    "type": "heartbeat",
                    "server_version": shared.server_version,
                    "device_count": shared.backend.list_devices().len(),
                    "ts": now_ms(),
                });
                if socket.send(Message::Text(heartbeat.to_string().into())).await.is_err() {
                    return;
                }
            }
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Close(_))) | None => return,
                    Some(Err(_)) => return,
                    // T5.1 is server-push-only. Later tasks (T5.2 device
                    // selection, T5.4 approval actions) give the client
                    // something to say here.
                    Some(Ok(_)) => {}
                }
            }
        }
    }
}
