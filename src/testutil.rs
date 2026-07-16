//! Test-only helpers shared across module tests.
//!
//! Environment variables are process-global and `cargo test` runs tests
//! concurrently, so every test that mutates env config must serialize through
//! the single [`env_guard`] here — per-module locks cannot stop two *modules*
//! from racing on the same variable. The guard also restores the previous
//! values on drop, so a panicking test cannot leak state into later tests.

use std::sync::{Mutex, MutexGuard};

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Holds the process-wide env lock for its lifetime and restores the previous
/// values of every touched variable on drop.
pub struct EnvGuard {
    saved: Vec<(String, Option<String>)>,
    _lock: MutexGuard<'static, ()>,
}

/// Apply `(key, value)` pairs (`None` removes the variable) under the global
/// env lock. Keep the returned guard alive for the whole test body.
pub fn env_guard(pairs: &[(&str, Option<&str>)]) -> EnvGuard {
    let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut saved = Vec::with_capacity(pairs.len());
    for (k, v) in pairs {
        saved.push(((*k).to_string(), std::env::var(k).ok()));
        // SAFETY: `set_var`/`remove_var` are unsound only against concurrent
        // env access from other threads; every env-mutating test funnels
        // through ENV_LOCK (held until the guard drops), serializing them.
        unsafe {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }
    EnvGuard { saved, _lock: lock }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (k, v) in self.saved.drain(..) {
            // SAFETY: the lock field is still held while `saved` is restored
            // (fields drop in declaration order: `saved` before `_lock`).
            unsafe {
                match v {
                    Some(v) => std::env::set_var(&k, v),
                    None => std::env::remove_var(&k),
                }
            }
        }
    }
}

/// Serve an axum router on an ephemeral localhost port and return its base
/// URL (`http://127.0.0.1:<port>`). The server runs until the handle is
/// aborted or the test runtime shuts down — real clients (reqwest,
/// `BackendClient`, `LlmClient`) can be pointed at it for deterministic
/// network tests with no real endpoint involved.
pub async fn serve_router(router: axum::Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral test port");
    let addr = listener.local_addr().expect("test listener addr");
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (format!("http://{addr}"), handle)
}
