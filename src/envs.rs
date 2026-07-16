//! Environment / endpoint configuration.
//!
//! Mirrors `common/envs.ts` from the original `@testsprite/testsprite-mcp`
//! plugin. Every value is overridable via an environment variable; the
//! defaults point at TestSprite production infrastructure (recovered from the
//! shipped bundle and verified live against `api.testsprite.com`).

use std::env;

/// Default model for local LLM-backed planning/analysis.
///
/// User/account-specific model names are allowed; override with
/// `TESTSPRITE_MODEL` (or `TESTSPRITE_DEFAULT_MODEL`). The fallback follows the
/// repo owner's preferred Codex model; if unavailable in another account,
/// callers can pass `--model` explicitly.
pub fn default_model() -> String {
    env::var("TESTSPRITE_MODEL")
        .or_else(|_| env::var("TESTSPRITE_DEFAULT_MODEL"))
        .unwrap_or_else(|_| "gpt-5.3-codex".to_string())
}

/// Main REST API base. Auth via `Authorization: Bearer <API_KEY>`.
pub fn api_url() -> String {
    env::var("API_URL").unwrap_or_else(|_| "https://api.testsprite.com".to_string())
}

/// Public website base; used to build dashboard result URLs.
pub fn testsprite_url() -> String {
    env::var("TESTSPRITE_URL").unwrap_or_else(|_| "https://www.testsprite.com".to_string())
}

/// Playwright Docker image for browser tests the host can't run natively
/// (notably webkit — it needs system libs the host may lack). This is a small
/// derived image (official base + the `playwright` npm package the base omits),
/// auto-built on first use. Override to point at a self-managed image.
pub fn playwright_image() -> String {
    env::var("TESTSPRITE_PLAYWRIGHT_IMAGE")
        .unwrap_or_else(|_| "testsprite-rs-playwright:1.60.0".to_string())
}

/// Keep at most this many run-history rows per test (`0` = unlimited). The
/// append-only `runs` table is auto-pruned to this bound on every write, so
/// frequent/scheduled runs don't bloat `testsprite.db`.
pub fn run_history_keep() -> usize {
    env::var("TESTSPRITE_RUN_HISTORY_KEEP")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200)
}

/// Seconds to wait for the target app to become reachable when `test run
/// --serve` starts it before the run (default 30).
pub fn serve_ready_secs() -> u64 {
    env::var("TESTSPRITE_SERVE_READY_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30)
}

/// Hard wall-clock, in seconds, for a single test's execution. A test that
/// exceeds it is FAILED (verdict `timeout`) instead of hanging the whole run —
/// the backstop for deadlocks/infinite loops. `0` disables it (default 300).
pub fn test_timeout_secs() -> u64 {
    env::var("TESTSPRITE_TEST_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300)
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
