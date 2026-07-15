//! In-memory test store + executor — the part the cloud runs in a sandbox.
//!
//! Because the local backend runs on the same machine as the target app, the
//! executor hits the app **directly** (no tunnel data plane needed). Each test
//! transitions RUNNING -> PASSED/FAILED and is stored so the client's
//! `GET /mcp/project/test/{id}` polling observes completion.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{Value, json};
use tokio::sync::RwLock;
use uuid::Uuid;

use super::engine::{self, EndpointSpec};
use super::executors::{ExecCtx, Executor};

/// A stored test entity, shaped exactly like the client's `TestEntity`.
#[derive(Clone)]
pub struct StoredTest {
    pub entity: Value,
}

#[derive(Clone, Default)]
pub struct Store {
    inner: Arc<RwLock<HashMap<String, StoredTest>>>,
}

impl Store {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn insert(&self, test_id: String, entity: Value) {
        self.inner
            .write()
            .await
            .insert(test_id, StoredTest { entity });
    }

    pub async fn get(&self, test_id: &str) -> Option<Value> {
        self.inner
            .read()
            .await
            .get(test_id)
            .map(|t| t.entity.clone())
    }

    /// All stored test entities (for the Coverage Guard: only entities that
    /// actually ran are present here, vs. the full planned-case set).
    pub async fn all(&self) -> Vec<Value> {
        self.inner
            .read()
            .await
            .values()
            .map(|t| t.entity.clone())
            .collect()
    }

    async fn set_status(&self, test_id: &str, status: &str, error: &str, code: &str) {
        if let Some(t) = self.inner.write().await.get_mut(test_id) {
            t.entity["testStatus"] = json!(status);
            t.entity["testError"] = json!(error);
            t.entity["code"] = json!(code);
            t.entity["modified"] = json!(now_iso());
        }
    }
}

/// Build the initial RUNNING entity for a planned case.
pub fn new_running_entity(
    project_id: &str,
    test_id: &str,
    user_id: &str,
    title: &str,
    description: &str,
) -> Value {
    json!({
        "projectId": project_id,
        "testId": test_id,
        "userId": user_id,
        "title": title,
        "description": description,
        "code": "",
        "testStatus": "RUNNING",
        "testError": "",
        "testType": "BACKEND",
        "createFrom": "mcp-local",
        "created": now_iso(),
        "modified": now_iso(),
    })
}

/// Execute one endpoint spec against `base_url`; returns (passed, error, code).
pub async fn execute_spec(spec: &EndpointSpec, base_url: &str, vars: &HashMap<String, String>) -> (bool, String, String) {
    let code = engine::python_for(spec, base_url, vars);
    let url = format!(
        "{}{}",
        base_url.trim_end_matches('/'),
        engine::concrete_path(&spec.path, vars)
    );
    let client = reqwest::Client::new();

    let req = match spec.method.as_str() {
        "GET" => client.get(&url),
        "DELETE" => client.delete(&url),
        m => {
            let builder = client.request(m.parse().unwrap_or(reqwest::Method::POST), &url);
            match &spec.body {
                Some(b) => builder.json(b),
                None => builder,
            }
        }
    };

    match req.timeout(std::time::Duration::from_secs(30)).send().await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let ok = match spec.expect_status {
                Some(expected) => status == expected,
                None => status < 500,
            };
            let err = if ok {
                String::new()
            } else {
                format!("expected {:?}, got {status}", spec.expect_status)
            };
            (ok, err, code)
        }
        Err(e) => (false, format!("request failed: {e}"), code),
    }
}

/// Execute LLM-generated Python via `python3`; returns (passed, error, code).
pub async fn execute_python(code: &str) -> (bool, String, String) {
    let dir = std::env::temp_dir();
    let file = dir.join(format!("ts_rs_{}.py", Uuid::new_v4()));
    if let Err(e) = tokio::fs::write(&file, code).await {
        return (
            false,
            format!("could not write test file: {e}"),
            code.to_string(),
        );
    }
    let output = tokio::process::Command::new("python3")
        .arg(&file)
        .output()
        .await;
    let _ = tokio::fs::remove_file(&file).await;
    match output {
        Ok(out) if out.status.success() => (true, String::new(), code.to_string()),
        Ok(out) => {
            let err = crate::server::executors::clip(&String::from_utf8_lossy(&out.stderr), 2000);
            (false, err, code.to_string())
        }
        Err(e) => (
            false,
            format!("python3 failed to launch: {e}"),
            code.to_string(),
        ),
    }
}

/// Spawn async execution of all cases through one [`Executor`]. The store, API,
/// and planner never branch on modality — the executor is the only seam. Each
/// entity transitions RUNNING -> PASSED/FAILED as its case completes.
pub fn spawn_execution(
    store: Store,
    executor: Arc<dyn Executor>,
    ctx: ExecCtx,
    cases: Vec<(String, Value)>,
) {
    tokio::spawn(async move {
        let label = executor.label();
        for (test_id, case) in cases {
            let outcome = executor.run(&case, &ctx).await;
            let status = if outcome.passed { "PASSED" } else { "FAILED" };
            store
                .set_status(&test_id, status, &outcome.error, &outcome.code)
                .await;
            tracing::info!("local-exec[{label}]: {test_id} {status}");
        }
    });
}

fn now_iso() -> String {
    // Seconds since epoch as a stable, dependency-free timestamp string.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("1970-01-01T00:00:00Z+{secs}")
}
