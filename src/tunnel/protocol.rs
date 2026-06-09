//! Wire framing + control-plane message types for tunnel v2.
//! Mirrors `tunnelClient/v2/protocol.ts`.
//!
//! Data-plane frames are length-prefixed JSON: a big-endian `u32` byte length
//! followed by the UTF-8 JSON body.

use anyhow::{Result, bail};
use bytes::{Buf, BufMut, BytesMut};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt};

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
pub async fn read_frame<R, T>(stream: &mut R) -> Result<T>
where
    R: AsyncRead + Unpin,
    T: for<'de> Deserialize<'de>,
{
    let len_buf = read_exactly(stream, 4).await?;
    let len = (&len_buf[..]).get_u32() as usize;
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

#[derive(Debug, Serialize)]
pub struct AuthPayload {
    pub secret: String,
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
#[derive(Debug, Serialize)]
pub struct TunnelHello {
    pub client_id: String,
    pub secret: String,
    pub tunnel_connection_id: String,
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
