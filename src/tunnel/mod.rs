//! Tunnel orchestration. Mirrors `tunnelClient/v2/index.ts` (`TunnelClientWrapper`).

pub mod client;
pub mod protocol;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use client::{TunnelClient, TunnelOptions};
pub use protocol::TunnelTarget;

use crate::backend::BackendClient;
use crate::envs;

/// Manages the lifecycle of a v2 tunnel and exposes the proxy URL the cloud
/// runner uses to reach the local app.
pub struct Tunnel {
    client: Arc<TunnelClient>,
    pub proxy_url: String,
}

impl Tunnel {
    /// Create a tunnel via `POST /api/tunnel/v2`, connect the control plane, and
    /// return a handle plus the credentialed proxy URL.
    ///
    /// `target` is the local app being exposed, and is the ONLY host:port the
    /// control plane may direct us to dial — see [`TunnelTarget`].
    ///
    /// Mirrors `TunnelClientWrapper.start`: when the tunnel version isn't pinned
    /// via env (`TSEMCP_TUNNEL_VERSION`), ask the backend which version to use.
    /// Only v2 is implemented here; v1 (the legacy HMAC-challenge TCP tunnel) is
    /// rejected explicitly rather than silently mishandled.
    pub async fn start(backend: &BackendClient, target: TunnelTarget) -> Result<Self> {
        let pinned = envs::tunnel::version();
        let version = if pinned == 0 {
            backend.tunnel_version().await.unwrap_or(2)
        } else {
            pinned
        };
        if version == 1 {
            anyhow::bail!(
                "backend requested tunnel v1, which testsprite-rs does not implement (v2 only)"
            );
        }

        let (client_id, secret) = backend.tunnel_create().await?;

        // proxy URL with embedded id:secret credentials.
        let mut proxy = url_with_creds(&envs::tunnel::proxy_url(), &client_id, &secret);
        // The original trims a trailing slash from the serialized URL.
        if proxy.ends_with('/') {
            proxy.pop();
        }

        let opts = TunnelOptions {
            client_id,
            secret,
            control_url: envs::tunnel::control_url(),
            tunnel_addr: envs::tunnel::data_address(),
            heartbeat: Duration::from_millis(10_000),
            target,
        };
        let client = Arc::new(TunnelClient::new(opts));
        client.start().await?;

        Ok(Self {
            client,
            proxy_url: proxy,
        })
    }

    pub async fn stop(&self) {
        self.client.stop().await;
    }
}

/// Embed `user:pass@` credentials into a URL's authority.
fn url_with_creds(base: &str, user: &str, pass: &str) -> String {
    match base.split_once("://") {
        Some((scheme, rest)) => format!("{scheme}://{user}:{pass}@{rest}"),
        None => base.to_string(),
    }
}
