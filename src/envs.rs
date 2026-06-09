//! Environment / endpoint configuration.
//!
//! Mirrors `common/envs.ts` from the original `@testsprite/testsprite-mcp`
//! plugin. Every value is overridable via an environment variable; the
//! defaults point at TestSprite production infrastructure (recovered from the
//! shipped bundle and verified live against `api.testsprite.com`).

use std::env;

/// Main REST API base. Auth via `Authorization: Bearer <API_KEY>`.
pub fn api_url() -> String {
    env::var("API_URL").unwrap_or_else(|_| "https://api.testsprite.com".to_string())
}

/// Public website base; used to build dashboard result URLs.
pub fn testsprite_url() -> String {
    env::var("TESTSPRITE_URL").unwrap_or_else(|_| "https://www.testsprite.com".to_string())
}

/// The user's API key (encrypted AEAD token, `sk-user-...`).
pub fn api_key() -> Option<String> {
    env::var("TSMCP_API_KEY")
        .ok()
        .or_else(|| env::var("API_KEY").ok())
}

/// Tunnel control / data / proxy endpoints (tunnel "v2").
pub mod tunnel {
    use super::env;

    /// Control-plane WebSocket. Carries Auth/Heartbeat/RequestTunnel/CloseTunnel.
    pub fn control_url() -> String {
        env::var("TSEMCP_TUNNEL_CONTROL_URL")
            .unwrap_or_else(|_| "wss://control.tun.testsprite.com/ws".to_string())
    }

    /// Data-plane `host:port` (yamux-multiplexed TCP).
    pub fn data_address() -> String {
        if let Ok(addr) = env::var("TSEMCP_TUNNEL_DATA_ADDRESS") {
            return addr;
        }
        let host = env::var("TSEMCP_TUNNEL_DATA_HOST")
            .unwrap_or_else(|_| "data.tun.testsprite.com".to_string());
        let port = env::var("TSEMCP_TUNNEL_DATA_PORT").unwrap_or_else(|_| "7400".to_string());
        format!("{host}:{port}")
    }

    /// HTTP/SOCKS proxy URL handed to the cloud runner (creds = client id/secret).
    pub fn proxy_url() -> String {
        env::var("TSEMCP_TUNNEL_PROXY_URL")
            .unwrap_or_else(|_| "http://proxy.tun.testsprite.com:9090".to_string())
    }

    /// Forced tunnel version (0 = ask backend, 1 = v1, 2 = v2).
    pub fn version() -> u8 {
        env::var("TSEMCP_TUNNEL_VERSION")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    }
}
