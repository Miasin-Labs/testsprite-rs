//! Tunnel v2 client: control WebSocket + yamux data plane.
//! Mirrors `tunnelClient/v2/client.ts`.
//!
//! Flow:
//!   1. Open the control WebSocket and send `Auth { secret }`.
//!   2. On each `RequestTunnel`, dial the data-plane TCP address, write a
//!      length-prefixed `TunnelHello`, then run a yamux **client** session over
//!      it. (The control server is the yamux *server*; it opens streams to us.)
//!   3. For each inbound yamux stream: read a `StreamOpenRequest` frame naming
//!      `target_host:target_port`, connect to that local target, and splice the
//!      two halves bidirectionally.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use futures::{SinkExt, StreamExt};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tokio_util::compat::{FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};

use super::protocol::{
    self,
    AuthPayload,
    ControlClientMsg,
    ControlServerMsg,
    StreamOpenRequest,
    TunnelHello,
};

type ControlWs = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub struct TunnelOptions {
    pub client_id: String,
    pub secret: String,
    pub control_url: String,
    pub tunnel_addr: String,
    pub heartbeat: Duration,
}

/// A running tunnel. `stop` aborts the background tasks.
pub struct TunnelClient {
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    active_tunnels: Arc<Mutex<HashSet<String>>>,
    opts: Arc<TunnelOptions>,
}

impl TunnelClient {
    pub fn new(opts: TunnelOptions) -> Self {
        Self {
            tasks: Mutex::new(Vec::new()),
            active_tunnels: Arc::new(Mutex::new(HashSet::new())),
            opts: Arc::new(opts),
        }
    }

    /// Connect the control WebSocket and run the control loop in the background.
    /// Returns once the control channel is connected.
    pub async fn start(self: &Arc<Self>) -> Result<()> {
        // The server expects the client id as a query param (matches the
        // original `controlUrl.searchParams.set("client_id", clientId)`).
        let sep = if self.opts.control_url.contains('?') {
            '&'
        } else {
            '?'
        };
        let url = format!(
            "{}{sep}client_id={}",
            self.opts.control_url, self.opts.client_id
        );
        let (ws, _) = tokio_tungstenite::connect_async(&url)
            .await
            .map_err(|e| anyhow!("control websocket connect failed: {e}"))?;
        tracing::info!("[TunnelClient] Control websocket connected");

        let me = Arc::clone(self);
        let handle = tokio::spawn(async move {
            if let Err(e) = me.run_control(ws).await {
                tracing::warn!("[TunnelClient] control loop ended: {e}");
            }
        });
        self.tasks.lock().await.push(handle);
        Ok(())
    }

    async fn run_control(self: Arc<Self>, ws: ControlWs) -> Result<()> {
        let (mut write, mut read) = ws.split();

        // Authenticate.
        let auth = ControlClientMsg::Auth {
            payload: AuthPayload {
                secret: self.opts.secret.clone(),
            },
        };
        write
            .send(Message::Text(serde_json::to_string(&auth)?.into()))
            .await?;

        // Heartbeat ticker drives sends from the same loop.
        let mut beat = tokio::time::interval(self.opts.heartbeat);
        beat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = beat.tick() => {
                    let hb = serde_json::to_string(&ControlClientMsg::Heartbeat)?;
                    if write.send(Message::Text(hb.into())).await.is_err() {
                        break;
                    }
                }
                msg = read.next() => {
                    let Some(msg) = msg else { break };
                    if !self.dispatch_ws(msg?, &mut write).await {
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    /// Handle one WebSocket frame. Returns `false` to terminate the loop.
    async fn dispatch_ws(
        self: &Arc<Self>,
        msg: Message,
        write: &mut (impl SinkExt<Message> + Unpin),
    ) -> bool {
        match msg {
            Message::Text(t) => {
                self.on_control_text(&t).await;
                true
            }
            Message::Ping(p) => {
                if write.send(Message::Pong(p)).await.is_err() {
                    tracing::debug!("[TunnelClient] failed to send pong");
                }
                true
            }
            Message::Close(_) => false,
            other => {
                tracing::trace!("[TunnelClient] ignoring control frame: {other:?}");
                true
            }
        }
    }

    async fn on_control_text(self: &Arc<Self>, text: &str) {
        tracing::debug!("[TunnelClient] Control message: {text}");
        match serde_json::from_str::<ControlServerMsg>(text) {
            Ok(ControlServerMsg::Ack) => {}
            Ok(ControlServerMsg::RequestTunnel { payload }) => {
                self.spawn_tunnel(payload.tunnel_connection_id).await;
            }
            Ok(ControlServerMsg::CloseTunnel { payload }) => {
                tracing::info!("[TunnelClient] Received close tunnel: {}", payload.reason);
                self.active_tunnels.lock().await.clear();
            }
            Err(e) => tracing::debug!("[TunnelClient] unparseable control message: {e}"),
        }
    }

    /// Start a data-plane runtime for a tunnel id (idempotent).
    async fn spawn_tunnel(self: &Arc<Self>, id: String) {
        if !self.active_tunnels.lock().await.insert(id.clone()) {
            return; // already running
        }
        tracing::info!("[TunnelClient] Starting tunnel runtime on demand: {id}");
        let me = Arc::clone(self);
        let handle = tokio::spawn(async move {
            if let Err(e) = me.run_tunnel(&id).await {
                tracing::warn!("[TunnelClient] tunnel {id} ended: {e}");
            }
            me.active_tunnels.lock().await.remove(&id);
        });
        self.tasks.lock().await.push(handle);
    }

    /// Dial the data-plane address, send the hello frame, and run a yamux client
    /// session, handling each inbound stream.
    async fn run_tunnel(self: &Arc<Self>, tunnel_connection_id: &str) -> Result<()> {
        let (host, port) = protocol::parse_tunnel_addr(&self.opts.tunnel_addr)?;
        let mut socket = TcpStream::connect((host.as_str(), port)).await?;
        socket.set_nodelay(true).ok();
        tracing::info!("[TunnelClient] Tunnel tcp connected: {tunnel_connection_id}");

        let hello = TunnelHello {
            client_id: self.opts.client_id.clone(),
            secret: self.opts.secret.clone(),
            tunnel_connection_id: tunnel_connection_id.to_string(),
        };
        socket.write_all(&protocol::encode_frame(&hello)?).await?;

        // The control server opens streams to us → we are the yamux client.
        let mut conn = yamux::Connection::new(
            socket.compat(),
            yamux::Config::default(),
            yamux::Mode::Client,
        );

        while let Some(stream) = futures::future::poll_fn(|cx| conn.poll_next_inbound(cx)).await {
            let stream = stream.map_err(|e| anyhow!("yamux error: {e}"))?;
            tokio::spawn(async move {
                if let Err(e) = handle_stream(stream).await {
                    tracing::debug!("[TunnelClient] stream ended: {e}");
                }
            });
        }
        Ok(())
    }

    pub async fn stop(&self) {
        for h in self.tasks.lock().await.drain(..) {
            h.abort();
        }
        self.active_tunnels.lock().await.clear();
        tracing::info!("[TunnelClient] Tunnel client stopped");
    }
}

/// Handle one inbound yamux stream: read the open request, connect the local
/// target, splice bidirectionally.
async fn handle_stream(stream: yamux::Stream) -> Result<()> {
    let mut stream = stream.compat();
    let req: StreamOpenRequest = protocol::read_frame(&mut stream).await?;
    tracing::info!(
        "[TunnelClient] Open request {} : {}:{}",
        req.inbound_request_id,
        req.target_host,
        req.target_port
    );

    let target = connect_target(&req).await?;
    tracing::debug!(
        "[TunnelClient] Target connected: {}",
        req.inbound_request_id
    );

    let (mut sr, mut sw) = tokio::io::split(stream);
    let (mut tr, mut tw) = tokio::io::split(target);
    let c2t = tokio::io::copy(&mut sr, &mut tw);
    let t2c = tokio::io::copy(&mut tr, &mut sw);
    let (a, b) = tokio::join!(c2t, t2c);
    if let Err(e) = a.and(b) {
        tracing::debug!("[TunnelClient] proxy copy ended: {e}");
    }
    Ok(())
}

/// Try each dial candidate in order (localhost → 127.0.0.1/::1 → host).
async fn connect_target(req: &StreamOpenRequest) -> Result<TcpStream> {
    let mut last_err = None;
    for (host, port) in req.dial_candidates() {
        match tokio::time::timeout(
            Duration::from_secs(10),
            TcpStream::connect((host.as_str(), port)),
        )
        .await
        {
            Ok(Ok(sock)) => {
                sock.set_nodelay(true).ok();
                return Ok(sock);
            }
            Ok(Err(e)) => last_err = Some(e.to_string()),
            Err(_) => last_err = Some("connect timeout".to_string()),
        }
    }
    Err(anyhow!(
        "target connect failed for {}:{}: {}",
        req.target_host,
        req.target_port,
        last_err.unwrap_or_default()
    ))
}
