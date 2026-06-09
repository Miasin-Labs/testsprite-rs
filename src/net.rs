//! Local-network helpers. Mirrors `common/network.ts`.

use std::time::Duration;

use tokio::net::TcpStream;

/// Is something listening on `host:port`? Retries briefly.
pub async fn check_port_listening(host: &str, port: u16, wait: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        if tokio::time::timeout(Duration::from_millis(500), TcpStream::connect((host, port)))
            .await
            .map(|r| r.is_ok())
            .unwrap_or(false)
        {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Probe the local endpoint directly; returns the HTTP status if reachable.
pub async fn probe_local_endpoint(endpoint: &str) -> anyhow::Result<u16> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;
    let status = client.get(endpoint).send().await?.status().as_u16();
    Ok(status)
}

/// Probe the local endpoint *through* the cloud proxy, verifying the tunnel is
/// actually carrying traffic back to localhost. Returns the HTTP status.
pub async fn probe_through_tunnel(target_url: &str, proxy_url: &str) -> anyhow::Result<u16> {
    let proxy = reqwest::Proxy::all(proxy_url)?;
    let client = reqwest::Client::builder()
        .proxy(proxy)
        .timeout(Duration::from_secs(15))
        .build()?;
    let status = client.get(target_url).send().await?.status().as_u16();
    Ok(status)
}
