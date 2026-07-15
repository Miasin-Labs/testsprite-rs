//! Bring the target app up before a run and tear it down after, so backend/spec
//! cases hit a LIVE server instead of env-failing on a dead URL. Opt-in via
//! `test run --serve` with a `project set-start "<cmd>"` command.

use std::time::Duration;

use anyhow::Context;
use tokio::process::Child;

/// Split a target URL into `(host, port)` (default port by scheme).
fn host_port(url: &str) -> (String, u16) {
    let (default_port, rest) = if let Some(r) = url.strip_prefix("http://") {
        (80u16, r)
    } else if let Some(r) = url.strip_prefix("https://") {
        (443u16, r)
    } else {
        (80u16, url)
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    if let Some((host, port)) = authority.rsplit_once(':')
        && let Ok(port) = port.parse::<u16>()
    {
        return (host.to_string(), port);
    }
    (authority.to_string(), default_port)
}

/// If `target` is already reachable, do nothing (`Ok(None)`). Otherwise run
/// `command` (via `sh -c`, `kill_on_drop` so the caller's binding tears it down
/// on drop — even on early return / panic / SIGINT), wait up to `ready_secs` for
/// the port to accept connections, and return the child (`Ok(Some(child)))`.
/// Bails if the app never becomes reachable within the timeout.
pub async fn start_and_wait(
    command: &str,
    target: &str,
    ready_secs: u64,
) -> anyhow::Result<Option<Child>> {
    let (host, port) = host_port(target);

    if crate::net::check_port_listening(&host, port, Duration::from_millis(300)).await {
        eprintln!("serve: {host}:{port} already reachable — not starting a second copy");
        return Ok(None);
    }

    eprintln!("serve: starting target app (`{command}`) for {target}");
    let mut child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("starting app: {command}"))?;

    if crate::net::check_port_listening(&host, port, Duration::from_secs(ready_secs)).await {
        eprintln!("serve: app reachable at {host}:{port}");
        Ok(Some(child))
    } else {
        let _ = child.start_kill();
        let _ = child.wait().await;
        anyhow::bail!(
            "app did not become reachable at {host}:{port} within {ready_secs}s (start command: `{command}`)"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::host_port;

    #[test]
    fn parses_host_and_port() {
        assert_eq!(
            host_port("http://127.0.0.1:8080"),
            ("127.0.0.1".into(), 8080)
        );
        assert_eq!(
            host_port("http://localhost:9200/api"),
            ("localhost".into(), 9200)
        );
        assert_eq!(
            host_port("https://example.com"),
            ("example.com".into(), 443)
        );
        assert_eq!(host_port("http://host"), ("host".into(), 80));
    }
}
