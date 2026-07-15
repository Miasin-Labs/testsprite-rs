//! Wire framing + control-plane message types for tunnel v2.
//! Mirrors `tunnelClient/v2/protocol.ts`.
//!
//! Data-plane frames are length-prefixed JSON: a big-endian `u32` byte length
//! followed by the UTF-8 JSON body.

use anyhow::{Result, bail};
use bytes::{Buf, BufMut, BytesMut};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt};

/// Largest control/open frame we will allocate for.
///
/// The length prefix is chosen by the remote control plane, so without a cap a
/// single 4-byte header can ask us to allocate up to 4 GiB. Every frame this
/// protocol carries is a small JSON object (a `StreamOpenRequest` or a hello),
/// so 1 MiB is orders of magnitude more than any legitimate sender needs.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// Encode a serializable value as a length-prefixed JSON frame.
pub fn encode_frame<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let body = serde_json::to_vec(value)?;
    let mut out = BytesMut::with_capacity(4 + body.len());
    out.put_u32(body.len() as u32);
    out.extend_from_slice(&body);
    Ok(out.to_vec())
}

/// Read exactly `n` bytes from an async reader.
pub async fn read_exactly<R: AsyncRead + Unpin>(stream: &mut R, n: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; n];
    stream.read_exact(&mut buf).await?;
    Ok(buf)
}

/// Read one length-prefixed JSON frame and deserialize it.
///
/// Rejects any frame larger than [`MAX_FRAME_BYTES`] BEFORE allocating, so a
/// hostile or MITM'd control plane cannot exhaust memory with a header alone.
pub async fn read_frame<R, T>(stream: &mut R) -> Result<T>
where
    R: AsyncRead + Unpin,
    T: for<'de> Deserialize<'de>,
{
    let len_buf = read_exactly(stream, 4).await?;
    let len = (&len_buf[..]).get_u32() as usize;
    if len > MAX_FRAME_BYTES {
        bail!("tunnel frame too large: {len} bytes (max {MAX_FRAME_BYTES})");
    }
    let body = read_exactly(stream, len).await?;
    Ok(serde_json::from_slice(&body)?)
}

/// Control-plane message we send to the server over the WebSocket.
#[derive(Debug, Serialize)]
#[serde(tag = "type")]
pub enum ControlClientMsg {
    Auth { payload: AuthPayload },
    Heartbeat,
}

#[derive(Serialize)]
pub struct AuthPayload {
    pub secret: String,
}

// Manual Debug so the secret never lands in a log line via `{:?}` — the derived
// impl (and thus `ControlClientMsg`'s) would print it verbatim.
impl std::fmt::Debug for AuthPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthPayload")
            .field("secret", &"[REDACTED]")
            .finish()
    }
}

/// Control-plane message received from the server.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum ControlServerMsg {
    Ack,
    RequestTunnel { payload: RequestTunnelPayload },
    CloseTunnel { payload: CloseTunnelPayload },
}

#[derive(Debug, Deserialize)]
pub struct RequestTunnelPayload {
    pub tunnel_connection_id: String,
    #[allow(dead_code)]
    pub target_host: String,
    #[allow(dead_code)]
    pub target_port: u16,
}

#[derive(Debug, Deserialize)]
pub struct CloseTunnelPayload {
    #[serde(default)]
    pub reason: String,
}

/// Hello frame written when a data-plane TCP connection is established.
#[derive(Serialize)]
pub struct TunnelHello {
    pub client_id: String,
    pub secret: String,
    pub tunnel_connection_id: String,
}

impl std::fmt::Debug for TunnelHello {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TunnelHello")
            .field("client_id", &self.client_id)
            .field("secret", &"[REDACTED]")
            .field("tunnel_connection_id", &self.tunnel_connection_id)
            .finish()
    }
}

/// Per-stream open request the server multiplexes over yamux. Names the local
/// target the cloud wants reached.
#[derive(Debug, Deserialize)]
pub struct StreamOpenRequest {
    // request_id / tunnel_connection_id / mux_stream_id are part of the wire
    // frame and parsed for completeness, but only used for logging context.
    #[serde(default)]
    #[allow(dead_code)]
    pub request_id: String,
    #[serde(default)]
    pub inbound_request_id: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub tunnel_connection_id: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub mux_stream_id: u64,
    pub target_host: String,
    pub target_port: u16,
}

impl StreamOpenRequest {
    /// Connect candidates: `localhost` resolves to both loopback addresses
    /// (matches `dialCandidates` in the original client).
    pub fn dial_candidates(&self) -> Vec<(String, u16)> {
        let mut out = Vec::new();
        if self.target_host.eq_ignore_ascii_case("localhost") {
            out.push(("127.0.0.1".to_string(), self.target_port));
            out.push(("::1".to_string(), self.target_port));
        }
        out.push((self.target_host.clone(), self.target_port));
        out
    }
}

/// Is `host` one of the names for this machine's loopback interface?
fn is_loopback_alias(host: &str) -> bool {
    matches!(
        host.trim_matches(|c| c == '[' || c == ']')
            .to_ascii_lowercase()
            .as_str(),
        "localhost" | "127.0.0.1" | "::1" | "0.0.0.0"
    )
}

/// The single local target a tunnel is authorized to reach.
///
/// The control plane names `target_host:target_port` on every inbound stream,
/// and the client used to dial whatever it was told. That makes a malicious or
/// MITM'd control server able to pivot through the developer's machine to any
/// host it can name — an internal service, a metadata endpoint, another port on
/// localhost. The tunnel exists to expose exactly one app, so it should be able
/// to reach exactly that app.
#[derive(Debug, Clone)]
pub struct TunnelTarget {
    pub host: String,
    pub port: u16,
}

impl TunnelTarget {
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
        }
    }

    /// True iff the server is asking for the target we actually exposed.
    pub fn allows(&self, host: &str, port: u16) -> bool {
        if port != self.port {
            return false;
        }
        host.eq_ignore_ascii_case(&self.host)
            || (is_loopback_alias(host) && is_loopback_alias(&self.host))
    }
}

/// Parse a `host:port` tunnel data address (IPv6 in brackets supported).
pub fn parse_tunnel_addr(addr: &str) -> Result<(String, u16)> {
    if let Some(rest) = addr.strip_prefix('[') {
        let Some(close) = rest.find(']') else {
            bail!("invalid tunnel address: {addr}")
        };
        let host = &rest[..close];
        let port = rest[close + 1..].trim_start_matches(':');
        return Ok((host.to_string(), port.parse()?));
    }
    let sep = addr
        .rfind(':')
        .ok_or_else(|| anyhow::anyhow!("invalid tunnel address: {addr}"))?;
    Ok((addr[..sep].to_string(), addr[sep + 1..].parse()?))
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_bearing_frames_redact_the_secret_in_debug() {
        let auth = AuthPayload {
            secret: "topsecret".into(),
        };
        let dbg = format!("{auth:?}");
        assert!(!dbg.contains("topsecret"), "{dbg}");
        assert!(dbg.contains("[REDACTED]"), "{dbg}");
        // The enum that wraps it must not leak it either.
        let msg = ControlClientMsg::Auth { payload: auth };
        assert!(!format!("{msg:?}").contains("topsecret"));

        let hello = TunnelHello {
            client_id: "cid".into(),
            secret: "topsecret".into(),
            tunnel_connection_id: "tcid".into(),
        };
        let dbg = format!("{hello:?}");
        assert!(!dbg.contains("topsecret"), "{dbg}");
        assert!(dbg.contains("cid") && dbg.contains("tcid"), "{dbg}");
        // Serialization still carries the real secret over the wire.
        assert!(serde_json::to_string(&hello).unwrap().contains("topsecret"));
    }

    #[test]
    fn a_tunnel_only_dials_the_app_it_exposed() {
        let target = TunnelTarget::new("127.0.0.1", 8080);
        assert!(target.allows("127.0.0.1", 8080));
        // Loopback aliases are the same machine.
        assert!(target.allows("localhost", 8080));
        assert!(target.allows("::1", 8080));

        // Everything the control plane might pivot to is refused: another port
        // on this box, an internal service, a cloud metadata endpoint.
        assert!(!target.allows("127.0.0.1", 22));
        assert!(!target.allows("127.0.0.1", 5432));
        assert!(!target.allows("169.254.169.254", 80));
        assert!(!target.allows("10.0.0.5", 8080));
        assert!(!target.allows("internal.corp", 8080));
    }

    #[test]
    fn a_non_loopback_target_does_not_admit_loopback() {
        let target = TunnelTarget::new("app.internal", 3000);
        assert!(target.allows("app.internal", 3000));
        assert!(target.allows("APP.INTERNAL", 3000));
        assert!(!target.allows("127.0.0.1", 3000));
    }

    #[tokio::test]
    async fn an_oversized_frame_header_is_refused_before_allocating() {
        // The length prefix comes from the remote control plane. Without a cap
        // this header alone allocates 4 GiB.
        let mut framed: &[u8] = &[0xFF, 0xFF, 0xFF, 0xFF];
        let err = read_frame::<_, serde_json::Value>(&mut framed)
            .await
            .expect_err("must refuse");
        assert!(err.to_string().contains("frame too large"), "{err}");
    }

    #[tokio::test]
    async fn a_normal_frame_still_round_trips() {
        let hello = TunnelHello {
            client_id: "c".into(),
            secret: "s".into(),
            tunnel_connection_id: "t".into(),
        };
        let bytes = encode_frame(&hello).unwrap();
        let mut cursor: &[u8] = &bytes;
        let back: serde_json::Value = read_frame(&mut cursor).await.unwrap();
        assert_eq!(back["client_id"], "c");
    }
}
