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
    TunnelTarget,
};

type ControlWs = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub struct TunnelOptions {
    pub client_id: String,
    pub secret: String,
    pub control_url: String,
    pub tunnel_addr: String,
    pub heartbeat: Duration,
    /// The only local target the control plane may ask us to dial.
    pub target: TunnelTarget,
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

        let allowed = Arc::new(self.opts.target.clone());
        while let Some(stream) = futures::future::poll_fn(|cx| conn.poll_next_inbound(cx)).await {
            let stream = stream.map_err(|e| anyhow!("yamux error: {e}"))?;
            let allowed = Arc::clone(&allowed);
            tokio::spawn(async move {
                if let Err(e) = handle_stream(stream, allowed).await {
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
async fn handle_stream(stream: yamux::Stream, allowed: Arc<TunnelTarget>) -> Result<()> {
    let mut stream = stream.compat();
    let req: StreamOpenRequest = protocol::read_frame(&mut stream).await?;
    tracing::info!(
        "[TunnelClient] Open request {} : {}:{}",
        req.inbound_request_id,
        req.target_host,
        req.target_port
    );

    // The server chose this host:port. Dial it only if it is the app we exposed.
    if !allowed.allows(&req.target_host, req.target_port) {
        tracing::warn!(
            "[TunnelClient] refusing to dial {}:{} — tunnel is authorized only for {}:{}",
            req.target_host,
            req.target_port,
            allowed.host,
            allowed.port
        );
        return Err(anyhow!(
            "control server asked for a non-exposed target {}:{}",
            req.target_host,
            req.target_port
        ));
    }

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

#[cfg(test)]
mod tests {
    //! End-to-end tests wire a real local control WebSocket, a real yamux data
    //! plane, and a real echo target together and drive the whole splice through
    //! [`TunnelClient`]. Everything binds to `127.0.0.1:0`; server-side
    //! observations flow back to the test body over tokio channels, and every
    //! await that could stall is wrapped in a 5s timeout.

    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::time::Duration;

    use bytes::Bytes;
    use futures::{SinkExt, StreamExt};
    use serde_json::{Value, json};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::{mpsc, oneshot};
    use tokio::time::timeout;
    use tokio_tungstenite::accept_hdr_async;
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
    use tokio_util::compat::{FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};

    use super::{TunnelClient, TunnelOptions};
    use crate::tunnel::protocol::{self, TunnelTarget};

    /// Cap for any await that could otherwise hang the test forever.
    const T: Duration = Duration::from_secs(5);

    /// The `TunnelHello` the data plane observed a client write on dial.
    #[derive(Debug)]
    struct HelloObs {
        client_id: String,
        secret: String,
        tunnel_connection_id: String,
    }

    /// A test-driven command for the control server to push over the WebSocket.
    enum ServerCmd {
        Text(String),
        Ping(Vec<u8>),
    }

    /// Handle to a running fake data plane.
    struct DataPlane {
        addr: SocketAddr,
        /// One item per accepted connection: the hello the client wrote.
        hellos: mpsc::UnboundedReceiver<HelloObs>,
        /// One item per accepted connection: `Some(bytes)` if the stream echoed
        /// `payload` back, `None` if it closed with no echo (refused/failed dial).
        echoes: mpsc::UnboundedReceiver<Option<Vec<u8>>>,
    }

    /// Handle to a running fake control server.
    struct Control {
        addr: SocketAddr,
        cmd: mpsc::Sender<ServerCmd>,
        /// The `client_id` query param seen during the WS handshake.
        client_id: oneshot::Receiver<String>,
        /// The secret carried in the first `Auth` frame.
        secret: oneshot::Receiver<String>,
        heartbeats: mpsc::UnboundedReceiver<()>,
        pongs: mpsc::UnboundedReceiver<Vec<u8>>,
    }

    /// Bind an echo target on an ephemeral port. Every accepted connection is
    /// announced on the returned receiver (so a test can assert it is NOT
    /// contacted); the connection's bytes are echoed back until EOF.
    async fn spawn_echo_server() -> (SocketAddr, mpsc::UnboundedReceiver<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (conn_tx, conn_rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let _ = conn_tx.send(());
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    loop {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => {
                                if sock.write_all(&buf[..n]).await.is_err() {
                                    return;
                                }
                            }
                        }
                    }
                });
            }
        });
        (addr, conn_rx)
    }

    /// Bind a data-plane listener. For each connection it reads the length-
    /// prefixed `TunnelHello` (exactly how the client frames it), runs a yamux
    /// **server** session, opens ONE outbound stream, sends a `StreamOpenRequest`
    /// naming `target_host:target_port`, writes `payload`, then tries to read the
    /// echo back. The connection is then held open so `run_tunnel` keeps the
    /// tunnel id active (the dedup/redial assertions depend on this).
    async fn spawn_data_plane(target_host: &str, target_port: u16, payload: Vec<u8>) -> DataPlane {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (hello_tx, hellos) = mpsc::unbounded_channel();
        let (echo_tx, echoes) = mpsc::unbounded_channel();
        let target_host = target_host.to_string();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                sock.set_nodelay(true).ok();
                let hello_tx = hello_tx.clone();
                let echo_tx = echo_tx.clone();
                let target_host = target_host.clone();
                let payload = payload.clone();
                tokio::spawn(async move {
                    // 1. Read the hello the client wrote before it started yamux.
                    let hello: Value = match protocol::read_frame(&mut sock).await {
                        Ok(v) => v,
                        Err(_) => return,
                    };
                    let _ = hello_tx.send(HelloObs {
                        client_id: hello["client_id"].as_str().unwrap_or_default().to_string(),
                        secret: hello["secret"].as_str().unwrap_or_default().to_string(),
                        tunnel_connection_id: hello["tunnel_connection_id"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                    });

                    // 2. The client dialed us, so it is the yamux client; we are
                    //    the server and open the stream toward it.
                    let mut conn = yamux::Connection::new(
                        sock.compat(),
                        yamux::Config::default(),
                        yamux::Mode::Server,
                    );
                    let stream =
                        match futures::future::poll_fn(|cx| conn.poll_new_outbound(cx)).await {
                            Ok(s) => s,
                            Err(_) => return,
                        };
                    // Keep polling the connection so both directions make progress.
                    tokio::spawn(async move {
                        while let Some(res) =
                            futures::future::poll_fn(|cx| conn.poll_next_inbound(cx)).await
                        {
                            if res.is_err() {
                                break;
                            }
                        }
                    });

                    let mut s = stream.compat();
                    let open = json!({
                        "inbound_request_id": "req-1",
                        "target_host": target_host,
                        "target_port": target_port,
                    });
                    if s.write_all(&protocol::encode_frame(&open).unwrap())
                        .await
                        .is_err()
                        || s.write_all(&payload).await.is_err()
                        || s.flush().await.is_err()
                    {
                        let _ = echo_tx.send(None);
                        std::future::pending::<()>().await;
                    }

                    // A refused/failed target resets the stream, so read_exact
                    // errors quickly; a working echo returns exactly `payload`.
                    let mut buf = vec![0u8; payload.len()];
                    match timeout(Duration::from_secs(2), s.read_exact(&mut buf)).await {
                        Ok(Ok(_)) => {
                            let _ = echo_tx.send(Some(buf));
                        }
                        _ => {
                            let _ = echo_tx.send(None);
                        }
                    }
                    // Hold the connection open until the runtime is torn down.
                    std::future::pending::<()>().await;
                });
            }
        });
        DataPlane {
            addr,
            hellos,
            echoes,
        }
    }

    /// Bind a control WebSocket server. It captures the `client_id` query param
    /// in the handshake, asserts+reports the `Auth` secret, acks it, then reports
    /// every heartbeat/pong and forwards test-driven [`ServerCmd`]s to the client.
    async fn spawn_control_server() -> Control {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (cmd, mut cmd_rx) = mpsc::channel::<ServerCmd>(16);
        let (cid_tx, client_id) = oneshot::channel::<String>();
        let (sec_tx, secret) = oneshot::channel::<String>();
        let (hb_tx, heartbeats) = mpsc::unbounded_channel();
        let (pong_tx, pongs) = mpsc::unbounded_channel();

        tokio::spawn(async move {
            let Ok((sock, _)) = listener.accept().await else {
                return;
            };
            let ws = accept_hdr_async(
                sock,
                move |req: &Request,
                      resp: Response|
                      -> std::result::Result<Response, ErrorResponse> {
                    let cid = req
                        .uri()
                        .query()
                        .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("client_id=")))
                        .unwrap_or("")
                        .to_string();
                    let _ = cid_tx.send(cid);
                    Ok(resp)
                },
            )
            .await;
            let Ok(ws) = ws else { return };
            let (mut write, mut read) = ws.split();

            // The first client frame is the Auth text frame.
            if let Some(Ok(Message::Text(t))) = read.next().await {
                let v: Value = serde_json::from_str(&t).unwrap_or(Value::Null);
                if v["type"] == "Auth" {
                    let _ = sec_tx.send(
                        v["payload"]["secret"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                    );
                }
            }
            let _ = write
                .send(Message::Text(json!({ "type": "Ack" }).to_string().into()))
                .await;

            let reader = async move {
                while let Some(Ok(msg)) = read.next().await {
                    match msg {
                        Message::Text(t) => {
                            let v: Value = serde_json::from_str(&t).unwrap_or(Value::Null);
                            if v["type"] == "Heartbeat" {
                                let _ = hb_tx.send(());
                            }
                        }
                        Message::Pong(p) => {
                            let _ = pong_tx.send(p.to_vec());
                        }
                        Message::Close(_) => break,
                        _ => {}
                    }
                }
            };
            let writer = async move {
                while let Some(cmd) = cmd_rx.recv().await {
                    let m = match cmd {
                        ServerCmd::Text(s) => Message::Text(s.into()),
                        ServerCmd::Ping(p) => Message::Ping(Bytes::from(p)),
                    };
                    if write.send(m).await.is_err() {
                        break;
                    }
                }
            };
            tokio::join!(reader, writer);
        });

        Control {
            addr,
            cmd,
            client_id,
            secret,
            heartbeats,
            pongs,
        }
    }

    /// Build a `RequestTunnel` control frame with the exact serde shape the
    /// client deserializes (`RequestTunnelPayload` has no field defaults).
    fn request_tunnel(id: &str, target: &SocketAddr) -> String {
        json!({
            "type": "RequestTunnel",
            "payload": {
                "tunnel_connection_id": id,
                "target_host": target.ip().to_string(),
                "target_port": target.port(),
            }
        })
        .to_string()
    }

    fn close_tunnel() -> String {
        json!({ "type": "CloseTunnel", "payload": { "reason": "test" } }).to_string()
    }

    fn client(opts: TunnelOptions) -> Arc<TunnelClient> {
        Arc::new(TunnelClient::new(opts))
    }

    /// Full happy path: control auth → RequestTunnel → data-plane dial + hello →
    /// yamux stream → StreamOpenRequest → connect_target → bidirectional splice.
    #[tokio::test]
    async fn full_round_trip_splices_client_through_the_local_target() {
        let (echo_addr, mut echo_conns) = spawn_echo_server().await;
        let payload = b"hello through the tunnel".to_vec();
        let mut dp = spawn_data_plane("127.0.0.1", echo_addr.port(), payload.clone()).await;
        let ctl = spawn_control_server().await;

        let client = client(TunnelOptions {
            client_id: "cid-happy".into(),
            secret: "s3cr3t".into(),
            control_url: format!("ws://{}/", ctl.addr),
            tunnel_addr: dp.addr.to_string(),
            heartbeat: Duration::from_secs(30),
            target: TunnelTarget::new("127.0.0.1", echo_addr.port()),
        });
        timeout(T, client.start()).await.unwrap().unwrap();

        // Handshake carried the client id; the Auth frame carried the secret.
        assert_eq!(
            timeout(T, ctl.client_id).await.unwrap().unwrap(),
            "cid-happy"
        );
        assert_eq!(timeout(T, ctl.secret).await.unwrap().unwrap(), "s3cr3t");

        ctl.cmd
            .send(ServerCmd::Text(request_tunnel("tc-1", &echo_addr)))
            .await
            .unwrap();

        // The data plane saw exactly the hello the client wrote on dial.
        let hello = timeout(T, dp.hellos.recv()).await.unwrap().unwrap();
        assert_eq!(hello.client_id, "cid-happy");
        assert_eq!(hello.secret, "s3cr3t");
        assert_eq!(hello.tunnel_connection_id, "tc-1");

        // Bytes went client → local echo target → back through the tunnel.
        let echoed = timeout(T, dp.echoes.recv()).await.unwrap().unwrap();
        assert_eq!(echoed.as_deref(), Some(payload.as_slice()));
        // The local app really was contacted.
        assert!(timeout(T, echo_conns.recv()).await.unwrap().is_some());

        client.stop().await;
    }

    /// The 50ms heartbeat ticker keeps firing at the control server.
    #[tokio::test]
    async fn heartbeats_keep_reaching_the_control_server() {
        let mut ctl = spawn_control_server().await;
        let client = client(TunnelOptions {
            client_id: "cid-hb".into(),
            secret: "hb".into(),
            control_url: format!("ws://{}/", ctl.addr),
            tunnel_addr: "127.0.0.1:1".into(),
            heartbeat: Duration::from_millis(50),
            target: TunnelTarget::new("127.0.0.1", 1),
        });
        timeout(T, client.start()).await.unwrap().unwrap();
        assert_eq!(timeout(T, ctl.secret).await.unwrap().unwrap(), "hb");

        // Two beats prove the ticker is periodic, not just the immediate tick.
        timeout(T, ctl.heartbeats.recv()).await.unwrap().unwrap();
        timeout(T, ctl.heartbeats.recv()).await.unwrap().unwrap();

        client.stop().await;
    }

    /// A WS Ping from the control server is answered with a matching Pong.
    #[tokio::test]
    async fn a_control_ping_is_answered_with_a_pong() {
        let mut ctl = spawn_control_server().await;
        let client = client(TunnelOptions {
            client_id: "cid-ping".into(),
            secret: "pp".into(),
            control_url: format!("ws://{}/", ctl.addr),
            tunnel_addr: "127.0.0.1:1".into(),
            heartbeat: Duration::from_secs(30),
            target: TunnelTarget::new("127.0.0.1", 1),
        });
        timeout(T, client.start()).await.unwrap().unwrap();
        assert_eq!(timeout(T, ctl.secret).await.unwrap().unwrap(), "pp");

        ctl.cmd
            .send(ServerCmd::Ping(b"ping-payload".to_vec()))
            .await
            .unwrap();
        let pong = timeout(T, ctl.pongs.recv()).await.unwrap().unwrap();
        assert_eq!(pong, b"ping-payload");

        client.stop().await;
    }

    /// Unparseable and unknown control text must not kill the control loop: a
    /// real RequestTunnel that follows is still honored.
    #[tokio::test]
    async fn garbage_control_text_is_tolerated_and_serving_continues() {
        let (echo_addr, _echo_conns) = spawn_echo_server().await;
        let mut dp = spawn_data_plane("127.0.0.1", echo_addr.port(), b"x".to_vec()).await;
        let ctl = spawn_control_server().await;
        let client = client(TunnelOptions {
            client_id: "cid-garbage".into(),
            secret: "g".into(),
            control_url: format!("ws://{}/", ctl.addr),
            tunnel_addr: dp.addr.to_string(),
            heartbeat: Duration::from_secs(30),
            target: TunnelTarget::new("127.0.0.1", echo_addr.port()),
        });
        timeout(T, client.start()).await.unwrap().unwrap();
        assert_eq!(timeout(T, ctl.secret).await.unwrap().unwrap(), "g");

        // Not JSON, then valid JSON with an unknown tag — both are swallowed.
        ctl.cmd
            .send(ServerCmd::Text("this is not json {{{".into()))
            .await
            .unwrap();
        ctl.cmd
            .send(ServerCmd::Text(
                json!({ "type": "NopeUnknown" }).to_string(),
            ))
            .await
            .unwrap();
        // The loop is still alive and honors a real request.
        ctl.cmd
            .send(ServerCmd::Text(request_tunnel("tc-after", &echo_addr)))
            .await
            .unwrap();
        let hello = timeout(T, dp.hellos.recv()).await.unwrap().unwrap();
        assert_eq!(hello.tunnel_connection_id, "tc-after");

        client.stop().await;
    }

    /// CloseTunnel clears the active set, so the SAME id is allowed to redial: a
    /// second dial (second hello) reaches the data plane.
    #[tokio::test]
    async fn close_tunnel_lets_the_same_id_redial() {
        let (echo_addr, _echo_conns) = spawn_echo_server().await;
        let mut dp = spawn_data_plane("127.0.0.1", echo_addr.port(), b"x".to_vec()).await;
        let ctl = spawn_control_server().await;
        let client = client(TunnelOptions {
            client_id: "cid-redial".into(),
            secret: "r".into(),
            control_url: format!("ws://{}/", ctl.addr),
            tunnel_addr: dp.addr.to_string(),
            heartbeat: Duration::from_secs(30),
            target: TunnelTarget::new("127.0.0.1", echo_addr.port()),
        });
        timeout(T, client.start()).await.unwrap().unwrap();
        assert_eq!(timeout(T, ctl.secret).await.unwrap().unwrap(), "r");

        ctl.cmd
            .send(ServerCmd::Text(request_tunnel("dup-id", &echo_addr)))
            .await
            .unwrap();
        let h1 = timeout(T, dp.hellos.recv()).await.unwrap().unwrap();
        assert_eq!(h1.tunnel_connection_id, "dup-id");

        ctl.cmd.send(ServerCmd::Text(close_tunnel())).await.unwrap();
        ctl.cmd
            .send(ServerCmd::Text(request_tunnel("dup-id", &echo_addr)))
            .await
            .unwrap();
        let h2 = timeout(T, dp.hellos.recv()).await.unwrap().unwrap();
        assert_eq!(h2.tunnel_connection_id, "dup-id");

        client.stop().await;
    }

    /// Without a CloseTunnel, a duplicate id is idempotent: no second dial ever
    /// reaches the data plane while the first tunnel is still active.
    #[tokio::test]
    async fn a_duplicate_request_without_close_does_not_redial() {
        let (echo_addr, _echo_conns) = spawn_echo_server().await;
        let mut dp = spawn_data_plane("127.0.0.1", echo_addr.port(), b"y".to_vec()).await;
        let ctl = spawn_control_server().await;
        let client = client(TunnelOptions {
            client_id: "cid-dup".into(),
            secret: "d".into(),
            control_url: format!("ws://{}/", ctl.addr),
            tunnel_addr: dp.addr.to_string(),
            heartbeat: Duration::from_secs(30),
            target: TunnelTarget::new("127.0.0.1", echo_addr.port()),
        });
        timeout(T, client.start()).await.unwrap().unwrap();
        assert_eq!(timeout(T, ctl.secret).await.unwrap().unwrap(), "d");

        ctl.cmd
            .send(ServerCmd::Text(request_tunnel("same-id", &echo_addr)))
            .await
            .unwrap();
        let h1 = timeout(T, dp.hellos.recv()).await.unwrap().unwrap();
        assert_eq!(h1.tunnel_connection_id, "same-id");

        // Duplicate id, no close → spawn_tunnel is a no-op → no second hello.
        ctl.cmd
            .send(ServerCmd::Text(request_tunnel("same-id", &echo_addr)))
            .await
            .unwrap();
        assert!(
            timeout(Duration::from_millis(500), dp.hellos.recv())
                .await
                .is_err(),
            "a duplicate request must not trigger a second dial"
        );

        client.stop().await;
    }

    /// Security: the client refuses a stream whose target is not the one it
    /// exposed. The forbidden port is never dialed and the real app is untouched.
    #[tokio::test]
    async fn a_non_allowed_target_is_refused_and_the_local_app_is_never_contacted() {
        let (echo_addr, mut echo_conns) = spawn_echo_server().await;
        // Name a DIFFERENT port than the one we authorized → refusal before dial.
        let forbidden = echo_addr.port().wrapping_add(1).max(1);
        let mut dp =
            spawn_data_plane("127.0.0.1", forbidden, b"should-never-arrive".to_vec()).await;
        let ctl = spawn_control_server().await;
        let client = client(TunnelOptions {
            client_id: "cid-sec".into(),
            secret: "sec".into(),
            control_url: format!("ws://{}/", ctl.addr),
            tunnel_addr: dp.addr.to_string(),
            // Authorized ONLY for the echo server's port.
            target: TunnelTarget::new("127.0.0.1", echo_addr.port()),
            heartbeat: Duration::from_secs(30),
        });
        timeout(T, client.start()).await.unwrap().unwrap();
        assert_eq!(timeout(T, ctl.secret).await.unwrap().unwrap(), "sec");

        ctl.cmd
            .send(ServerCmd::Text(request_tunnel("tc-sec", &echo_addr)))
            .await
            .unwrap();
        // The client still dials the data plane...
        let hello = timeout(T, dp.hellos.recv()).await.unwrap().unwrap();
        assert_eq!(hello.tunnel_connection_id, "tc-sec");
        // ...but refuses the forbidden target: the stream closes with no echo.
        assert_eq!(timeout(T, dp.echoes.recv()).await.unwrap().unwrap(), None);
        // The refusal happens before any dial, so the real app is never touched.
        assert!(echo_conns.try_recv().is_err());

        client.stop().await;
    }

    /// connect_target failure: an authorized target that refuses the TCP
    /// connection (port 0 refuses immediately) yields no echo. Port 0 keeps this
    /// deterministic with no dead-port reclaim race and no slow timeout.
    #[tokio::test]
    async fn an_authorized_but_unreachable_target_yields_no_echo() {
        let mut dp = spawn_data_plane("127.0.0.1", 0, b"unreachable".to_vec()).await;
        let ctl = spawn_control_server().await;
        let client = client(TunnelOptions {
            client_id: "cid-cf".into(),
            secret: "cf".into(),
            control_url: format!("ws://{}/", ctl.addr),
            tunnel_addr: dp.addr.to_string(),
            heartbeat: Duration::from_secs(30),
            target: TunnelTarget::new("127.0.0.1", 0),
        });
        timeout(T, client.start()).await.unwrap().unwrap();
        assert_eq!(timeout(T, ctl.secret).await.unwrap().unwrap(), "cf");

        let target = SocketAddr::from(([127, 0, 0, 1], 0));
        ctl.cmd
            .send(ServerCmd::Text(request_tunnel("tc-cf", &target)))
            .await
            .unwrap();
        let hello = timeout(T, dp.hellos.recv()).await.unwrap().unwrap();
        assert_eq!(hello.tunnel_connection_id, "tc-cf");
        // connect_target exhausted its candidates and errored → no echo.
        assert_eq!(timeout(T, dp.echoes.recv()).await.unwrap().unwrap(), None);

        client.stop().await;
    }
}
