//! Local TestSprite backend: a drop-in stand-in for `api.testsprite.com` that
//! needs no account, no LLM, and no cloud.
//!
//! It runs two listeners:
//!   * an HTTP API (axum) implementing the REST contract, and
//!   * a control WebSocket at `/ws` that accepts the client's tunnel handshake.
//!
//! Because the executor runs the generated tests directly against the local
//! app, the tunnel control socket only needs to ack the client — it never has
//! to push a `RequestTunnel`/open a data plane.

pub mod api;
pub mod engine;
pub mod llm;
pub mod store;

use anyhow::Result;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::routing::get;
use futures::{SinkExt, StreamExt};

/// Run the local backend on `port` (HTTP + control WS on the same listener).
///
/// When an OpenAI key is available (env `OPENAI_API_KEY` or
/// `~/.config/jfc/credentials.toml`), the backend behaves like the real cloud:
/// the LLM generates the PRD, test plan, and Python test code. Otherwise it
/// falls back to a deterministic engine driven by the code summary.
pub async fn serve(port: u16, model: &str) -> Result<()> {
    let llm = llm::LlmClient::from_env(model);
    let mode = match &llm {
        Some(c) => format!("LLM ({})", c.model),
        None => "deterministic (no OpenAI key found)".to_string(),
    };
    let state = api::AppState::new(llm);
    let app = api::router(state).route("/ws", get(control_ws));

    let addr = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("local TestSprite backend on http://{addr} (mode: {mode})");
    eprintln!("[testsprite-rs] local backend listening on http://{addr}  [mode: {mode}]");
    eprintln!("    point the client at it with:");
    eprintln!("      API_URL=http://{addr}");
    eprintln!("      TSEMCP_TUNNEL_CONTROL_URL=ws://{addr}/ws");
    eprintln!("      TSEMCP_TUNNEL_VERSION=2");
    eprintln!("      API_KEY=local            # any value; the local backend ignores it");
    eprintln!("    (the proxy probe is best-effort; execution hits the app directly)");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Accept the client's control WebSocket: read its `Auth`, reply `Ack`, and keep
/// the socket alive (answering heartbeats). No `RequestTunnel` is sent.
async fn control_ws(ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(handle_control)
}

async fn handle_control(socket: WebSocket) {
    let (mut tx, mut rx) = socket.split();
    tracing::info!("[control-ws] client connected");
    while let Some(Ok(msg)) = rx.next().await {
        match msg {
            Message::Text(t) => {
                tracing::debug!("[control-ws] recv: {t}");
                // Ack any client message (Auth or Heartbeat).
                if tx
                    .send(Message::Text("{\"type\":\"Ack\"}".to_string().into()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Message::Close(_) => break,
            Message::Ping(p) => {
                if tx.send(Message::Pong(p)).await.is_err() {
                    break;
                }
            }
            // Binary/Pong frames carry no control semantics here.
            Message::Binary(_) | Message::Pong(_) => {
                tracing::trace!("[control-ws] ignoring non-text frame");
            }
        }
    }
    tracing::info!("[control-ws] client disconnected");
}
