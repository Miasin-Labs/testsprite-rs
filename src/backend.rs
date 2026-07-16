//! Backend REST client for `api.testsprite.com`. Mirrors `common/backendClient.ts`.
//!
//! Auth: `Authorization: Bearer <API_KEY>` on every request, plus `apiKey` is
//! injected into the JSON body of POSTs (matching the original `postJSON`).

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use crate::envs;
use crate::types::{AccountInfo, TargetScope, TestCase, TestEntity, TestType};

#[derive(Clone)]
pub struct BackendClient {
    http: reqwest::Client,
    base: String,
    api_key: String,
}

impl BackendClient {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::builder()
                .user_agent("testsprite-rs/0.1")
                .build()
                .expect("reqwest client"),
            base: envs::api_url(),
            api_key: api_key.into(),
        }
    }

    /// Construct from the ambient API key env, erroring if absent.
    pub fn from_env() -> Result<Self> {
        let key = envs::api_key().ok_or_else(|| {
            anyhow!(
                "No API key. Set API_KEY (create one at {}/dashboard/settings/apikey).",
                envs::testsprite_url()
            )
        })?;
        Ok(Self::new(key))
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base.trim_end_matches('/'), path)
    }

    /// Map non-2xx into the same hard-stop semantics the plugin uses for 401.
    async fn check(&self, resp: reqwest::Response) -> Result<reqwest::Response> {
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let body = resp.text().await.unwrap_or_default();
        if status.as_u16() == 401 {
            bail!(
                "AUTH_FAILED (401): Unauthorized. Create a new API key at {}/dashboard/settings/apikey. \
                 Stop execution and run `check`.",
                envs::testsprite_url()
            );
        }
        bail!("Backend error: {} - {}", status.as_u16(), body);
    }

    fn get(&self, path: &str) -> reqwest::RequestBuilder {
        self.http.get(self.url(path)).bearer_auth(&self.api_key)
    }

    /// POST JSON with `apiKey` injected into the body (matches `postJSON`).
    async fn post_json(&self, path: &str, mut body: Value) -> Result<reqwest::Response> {
        if let Value::Object(ref mut m) = body {
            m.insert("apiKey".into(), Value::String(self.api_key.clone()));
        }
        let resp = self
            .http
            .post(self.url(path))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .context("request failed")?;
        self.check(resp).await
    }

    // --- account ---

    pub async fn get_account_info(&self) -> Result<AccountInfo> {
        let resp = self.check(self.get("/api/me").send().await?).await?;
        Ok(resp.json().await?)
    }

    // --- PRD ---

    /// `POST /mcp/common/generate-prd` (multipart). `code_summary_json` is the
    /// JSON-stringified code summary; PRD files are uploaded as `files`.
    pub async fn generate_standard_prd(
        &self,
        prd_files: &[std::path::PathBuf],
        code_summary_json: &str,
        test_type: TestType,
    ) -> Result<Value> {
        let mut form = reqwest::multipart::Form::new()
            .text("codeSummary", code_summary_json.to_string())
            .text("testType", test_type.to_string());
        for f in prd_files {
            let bytes = tokio::fs::read(f)
                .await
                .with_context(|| format!("read {f:?}"))?;
            let name = f
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("prd")
                .to_string();
            let part = reqwest::multipart::Part::bytes(bytes).file_name(name);
            form = form.part("files", part);
        }
        let resp = self
            .http
            .post(self.url("/mcp/common/generate-prd"))
            .bearer_auth(&self.api_key)
            .multipart(form)
            .send()
            .await?;
        Ok(self.check(resp).await?.json().await?)
    }

    // --- test plans ---

    pub async fn generate_frontend_test_plan(
        &self,
        standard_prd: &Value,
        target_scope: Option<TargetScope>,
    ) -> Result<Value> {
        let resp = self
            .post_json(
                "/mcp/frontend-test/generate-plan",
                json!({ "standard_prd": standard_prd, "target_scope": target_scope }),
            )
            .await?;
        Ok(resp.json().await?)
    }

    pub async fn generate_backend_test_plan(
        &self,
        prd_content: &str,
        target_scope: TargetScope,
    ) -> Result<Vec<TestCase>> {
        let resp = self
            .post_json(
                "/mcp/backend-test/plan",
                json!({ "prdContent": prd_content, "targetScope": target_scope }),
            )
            .await?;
        let v: Value = resp.json().await?;
        let plan = v.get("plan").cloned().unwrap_or(v);
        serde_json::from_value(plan).context("parse backend test plan")
    }

    // --- run ---

    pub async fn run_frontend_test(&self, body: Value) -> Result<Vec<String>> {
        self.run_test("/mcp/frontend-test/run", body).await
    }

    pub async fn run_backend_test(&self, body: Value) -> Result<Vec<String>> {
        self.run_test("/mcp/backend-test/run", body).await
    }

    async fn run_test(&self, path: &str, body: Value) -> Result<Vec<String>> {
        let resp = self.post_json(path, body).await?;
        let v: Value = resp.json().await?;
        // Accept either `{testIds:[...]}` (body top level) or `{data:{testIds:[...]}}`.
        let ids = v
            .get("testIds")
            .or_else(|| v.get("data").and_then(|d| d.get("testIds")))
            .and_then(|t| t.as_array())
            .ok_or_else(|| anyhow!("Invalid response: expected array of test IDs, got {v}"))?;
        Ok(ids
            .iter()
            .filter_map(|x| x.as_str().map(String::from))
            .collect())
    }

    // --- poll ---

    pub async fn get_test_entity(&self, test_id: &str) -> Result<TestEntity> {
        let resp = self
            .check(
                self.get(&format!("/mcp/project/test/{test_id}"))
                    .send()
                    .await?,
            )
            .await?;
        Ok(resp.json().await?)
    }

    /// Fetch one test; record it and return whether it has finished.
    async fn poll_one(&self, id: &str, slot: &mut Option<TestEntity>) -> bool {
        match self.get_test_entity(id).await {
            Ok(entity) => {
                let finished = !entity.is_running();
                *slot = Some(entity);
                finished
            }
            Err(e) => {
                tracing::warn!("transient error fetching {id}: {e}");
                false
            }
        }
    }

    /// Poll all test ids every 3s until none are RUNNING. Calls `on_update`
    /// with the count of completed tests after each cycle.
    pub async fn poll_test_status<F: FnMut(usize, &[Option<TestEntity>])>(
        &self,
        test_ids: &[String],
        mut on_update: F,
    ) -> Result<Vec<TestEntity>> {
        let mut results: Vec<Option<TestEntity>> = vec![None; test_ids.len()];
        let mut done = vec![false; test_ids.len()];
        loop {
            for (i, id) in test_ids.iter().enumerate() {
                if !done[i] {
                    done[i] = self.poll_one(id, &mut results[i]).await;
                }
            }
            let completed = done.iter().filter(|d| **d).count();
            on_update(completed, &results);
            if done.iter().all(|d| *d) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(3000)).await;
        }
        Ok(results.into_iter().flatten().collect())
    }

    // --- tunnel control ---

    /// `POST /api/tunnel/v2` -> `{ id, secret }`.
    ///
    /// An empty JSON object body is required (the original axios `.post(url)`
    /// sends `{}` by default; a bodyless POST 500s server-side).
    pub async fn tunnel_create(&self) -> Result<(String, String)> {
        let resp = self
            .check(
                self.http
                    .post(self.url("/api/tunnel/v2"))
                    .bearer_auth(&self.api_key)
                    .json(&serde_json::json!({}))
                    .send()
                    .await?,
            )
            .await?;
        let v: Value = resp.json().await?;
        let id = v
            .get("id")
            .and_then(|x| x.as_str())
            .ok_or_else(|| anyhow!("no tunnel id"))?;
        let secret = v
            .get("secret")
            .and_then(|x| x.as_str())
            .ok_or_else(|| anyhow!("no tunnel secret"))?;
        Ok((id.to_string(), secret.to_string()))
    }

    /// `GET /api/tunnel/v2/version` -> tunnel protocol version.
    pub async fn tunnel_version(&self) -> Result<u8> {
        let resp = self
            .check(self.get("/api/tunnel/v2/version").send().await?)
            .await?;
        let v: Value = resp.json().await?;
        Ok(v.get("version").and_then(|x| x.as_u64()).unwrap_or(2) as u8)
    }
}

/// Build the dashboard result URL for a finished test.
pub fn dashboard_url(project_id: &str, test_id: &str) -> String {
    format!(
        "{}/dashboard/mcp/tests/{}/{}",
        envs::testsprite_url(),
        project_id,
        test_id
    )
}

/// Read a code-summary YAML file and re-encode it as compact JSON (the plugin
/// does `YAML.parse` then `JSON.stringify`).
pub async fn code_summary_to_json(yaml_path: &Path) -> Result<String> {
    let text = tokio::fs::read_to_string(yaml_path)
        .await
        .with_context(|| format!("read {yaml_path:?}"))?;
    let value: serde_yaml::Value = serde_yaml::from_str(&text)?;
    let json: Value = serde_json::to_value(value)?;
    Ok(serde_json::to_string(&json)?)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::extract::{Path as AxumPath, State};
    use axum::http::StatusCode;
    use axum::routing::{get, post};
    use axum::{Json, Router};

    use super::*;

    #[tokio::test]
    async fn account_info_round_trips_through_the_local_backend_router() {
        let state =
            crate::server::api::AppState::new(None, crate::server::executors::TestKind::Backend);
        let (base, server) = crate::testutil::serve_router(crate::server::api::router(state)).await;
        let client = {
            let _guard = crate::testutil::env_guard(&[("API_URL", Some(base.as_str()))]);
            BackendClient::new("test-key")
        };
        let info = client.get_account_info().await.unwrap();
        assert_eq!(info.user.as_deref(), Some("local@localhost"));
        assert_eq!(info.sub_plan.as_deref(), Some("Local"));
        assert_eq!(info.credits, Some(999999));
        server.abort();
    }

    #[test]
    fn dashboard_url_uses_testsprite_base() {
        let _guard =
            crate::testutil::env_guard(&[("TESTSPRITE_URL", Some("https://dash.example"))]);
        assert_eq!(
            dashboard_url("p1", "t1"),
            "https://dash.example/dashboard/mcp/tests/p1/t1"
        );
    }

    #[tokio::test]
    async fn code_summary_yaml_round_trips_to_compact_json() {
        let root = crate::local::tmp_root();
        let p = root.join("code_summary.yaml");
        std::fs::write(
            &p,
            "project_name: demo\nfeatures:\n  - name: Login\napi_endpoints:\n  - method: GET\n    path: /health\n",
        )
        .unwrap();
        let out = code_summary_to_json(&p).await.unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["project_name"], "demo");
        assert_eq!(v["features"][0]["name"], "Login");
        assert_eq!(v["api_endpoints"][0]["path"], "/health");
        assert!(!out.contains('\n'));
        std::fs::remove_dir_all(root).ok();
    }

    /// Point a freshly built client at a served router. The client reads
    /// `envs::api_url()` only in `new`, so the guard just has to span the
    /// construction, matching the existing round-trip test above.
    fn client_for(base: &str) -> BackendClient {
        let _guard = crate::testutil::env_guard(&[("API_URL", Some(base))]);
        BackendClient::new("test-key")
    }

    /// A local api-router backend (deterministic engine, no LLM, no cloud).
    fn local_backend() -> Router {
        crate::server::api::router(crate::server::api::AppState::new(
            None,
            crate::server::executors::TestKind::Backend,
        ))
    }

    #[tokio::test]
    async fn post_json_injects_api_key_into_json_body() {
        // `postJSON` parity: the api key is added to the JSON body of every POST
        // (in addition to the bearer header). Capture the received body to prove
        // the injection actually reaches the wire without clobbering caller keys.
        async fn capture(
            State(sink): State<Arc<Mutex<Value>>>,
            Json(body): Json<Value>,
        ) -> Json<Value> {
            *sink.lock().unwrap() = body;
            Json(json!({ "ok": true }))
        }

        let captured: Arc<Mutex<Value>> = Arc::new(Mutex::new(Value::Null));
        let router = Router::new()
            .route("/capture", post(capture))
            .with_state(captured.clone());
        let (base, server) = crate::testutil::serve_router(router).await;
        let client = client_for(&base);

        let resp = tokio::time::timeout(
            Duration::from_secs(5),
            client.post_json("/capture", json!({ "foo": "bar" })),
        )
        .await
        .expect("post_json timed out")
        .expect("post_json ok");
        let out: Value = resp.json().await.unwrap();
        assert_eq!(out["ok"], true);

        let body = captured.lock().unwrap().clone();
        assert_eq!(body["apiKey"], "test-key");
        // The caller's own field survives the injection.
        assert_eq!(body["foo"], "bar");
        server.abort();
    }

    #[tokio::test]
    async fn check_maps_401_to_auth_failed_and_500_to_backend_error() {
        async fn unauthorized() -> (StatusCode, Json<Value>) {
            (StatusCode::UNAUTHORIZED, Json(json!({ "message": "nope" })))
        }
        async fn server_error() -> (StatusCode, String) {
            (StatusCode::INTERNAL_SERVER_ERROR, "boom".to_string())
        }

        let router = Router::new()
            .route("/unauthorized", post(unauthorized))
            .route("/server-error", post(server_error));
        let (base, server) = crate::testutil::serve_router(router).await;
        let client = client_for(&base);

        let err401 = tokio::time::timeout(
            Duration::from_secs(5),
            client.post_json("/unauthorized", json!({})),
        )
        .await
        .expect("401 request timed out")
        .expect_err("401 must map to an error");
        assert!(err401.to_string().contains("AUTH_FAILED (401)"), "{err401}");

        let err500 = tokio::time::timeout(
            Duration::from_secs(5),
            client.post_json("/server-error", json!({})),
        )
        .await
        .expect("500 request timed out")
        .expect_err("500 must map to an error");
        let msg = err500.to_string();
        // Non-401 errors surface the status and the raw body for triage.
        assert!(msg.contains("Backend error: 500"), "{msg}");
        assert!(msg.contains("boom"), "{msg}");
        server.abort();
    }

    #[tokio::test]
    async fn generate_standard_prd_uploads_files_and_returns_prd_object() {
        let (base, server) = crate::testutil::serve_router(local_backend()).await;
        let client = client_for(&base);

        // A real file so the multipart `files` loop actually reads + attaches it.
        let root = crate::local::tmp_root();
        let prd_file = root.join("prd.md");
        std::fs::write(&prd_file, "# Product\nLogin flow.\n").unwrap();

        // A code summary with an endpoint yields a nonempty deterministic PRD.
        let code_summary = json!({
            "project_name": "demo",
            "base_url": "http://127.0.0.1:1",
            "api_endpoints": [ { "method": "GET", "path": "/health" } ],
        })
        .to_string();

        let prd = tokio::time::timeout(
            Duration::from_secs(5),
            client.generate_standard_prd(
                std::slice::from_ref(&prd_file),
                &code_summary,
                TestType::Backend,
            ),
        )
        .await
        .expect("generate_standard_prd timed out")
        .expect("generate_standard_prd ok");

        assert!(prd.is_object(), "expected a PRD object, got {prd}");
        assert_eq!(prd["meta"]["project"], "demo");
        assert!(
            prd["features"].as_array().is_some_and(|f| !f.is_empty()),
            "features should be nonempty: {prd}"
        );
        std::fs::remove_dir_all(root).ok();
        server.abort();
    }

    #[tokio::test]
    async fn generate_backend_test_plan_parses_cases_with_ids() {
        let (base, server) = crate::testutil::serve_router(local_backend()).await;
        let client = client_for(&base);

        // prdContent is a stringified PRD; the engine plans one case per endpoint.
        let prd_content = json!({
            "project_name": "demo",
            "api_endpoints": [
                { "method": "GET", "path": "/health" },
                { "method": "POST", "path": "/users" },
            ],
        })
        .to_string();

        let cases = tokio::time::timeout(
            Duration::from_secs(5),
            client.generate_backend_test_plan(&prd_content, TargetScope::Codebase),
        )
        .await
        .expect("generate_backend_test_plan timed out")
        .expect("generate_backend_test_plan ok");

        assert_eq!(cases.len(), 2);
        assert!(
            cases.iter().all(|c| !c.id.is_empty()),
            "every case has an id"
        );
        assert_eq!(cases[0].id, "TC001");
        server.abort();
    }

    #[tokio::test]
    async fn generate_frontend_test_plan_returns_an_array() {
        let (base, server) = crate::testutil::serve_router(local_backend()).await;
        let client = client_for(&base);

        let standard_prd = json!({
            "api_endpoints": [
                { "method": "GET", "path": "/a" },
                { "method": "GET", "path": "/b" },
            ],
        });

        let plan = tokio::time::timeout(
            Duration::from_secs(5),
            client.generate_frontend_test_plan(&standard_prd, Some(TargetScope::Codebase)),
        )
        .await
        .expect("generate_frontend_test_plan timed out")
        .expect("generate_frontend_test_plan ok");

        let arr = plan.as_array().expect("frontend plan is an array");
        assert_eq!(arr.len(), 2);
        server.abort();
    }

    #[tokio::test]
    async fn run_backend_and_frontend_return_ids_matching_plan_length() {
        let (base, server) = crate::testutil::serve_router(local_backend()).await;
        let client = client_for(&base);

        // An explicit testPlan is echoed as one running test id per case. The
        // cases carry no spec, so the spawned executor fails fast (no LLM, no
        // network) — we only assert on the id vector the call returns.
        let body = json!({
            "testPlan": [
                { "id": "TC001", "title": "a", "description": "" },
                { "id": "TC002", "title": "b", "description": "" },
                { "id": "TC003", "title": "c", "description": "" },
            ],
        });

        let backend_ids = tokio::time::timeout(
            Duration::from_secs(5),
            client.run_backend_test(body.clone()),
        )
        .await
        .expect("run_backend_test timed out")
        .expect("run_backend_test ok");
        assert_eq!(backend_ids.len(), 3);

        let frontend_ids =
            tokio::time::timeout(Duration::from_secs(5), client.run_frontend_test(body))
                .await
                .expect("run_frontend_test timed out")
                .expect("run_frontend_test ok");
        assert_eq!(frontend_ids.len(), 3);
        server.abort();
    }

    #[tokio::test]
    async fn run_test_errors_when_response_has_no_test_id_array() {
        // The run endpoint replies with an object that has no `testIds` field.
        async fn no_ids() -> Json<Value> {
            Json(json!({ "unexpected": true }))
        }
        let router = Router::new().route("/mcp/backend-test/run", post(no_ids));
        let (base, server) = crate::testutil::serve_router(router).await;
        let client = client_for(&base);

        let err = tokio::time::timeout(Duration::from_secs(5), client.run_backend_test(json!({})))
            .await
            .expect("run timed out")
            .expect_err("missing testIds must error");
        assert!(
            err.to_string().contains("expected array of test IDs"),
            "{err}"
        );
        server.abort();
    }

    #[tokio::test]
    async fn poll_completes_in_one_cycle_when_entities_are_already_finished() {
        // Entities are served already-finished (PASSED), so `is_running()` is
        // false on the first fetch and `poll_test_status` returns without ever
        // hitting its 3s sleep.
        async fn finished(AxumPath(id): AxumPath<String>) -> Json<Value> {
            Json(json!({
                "projectId": "p1",
                "testId": id,
                "userId": "u1",
                "title": "T",
                "description": "D",
                "code": "print(1)",
                "testStatus": "PASSED",
                "testError": "",
            }))
        }
        let router = Router::new().route("/mcp/project/test/{id}", get(finished));
        let (base, server) = crate::testutil::serve_router(router).await;
        let client = client_for(&base);

        // get_test_entity round-trips a single finished entity.
        let one = tokio::time::timeout(Duration::from_secs(5), client.get_test_entity("solo"))
            .await
            .expect("get_test_entity timed out")
            .expect("get_test_entity ok");
        assert_eq!(one.test_id.as_deref(), Some("solo"));
        assert!(one.passed());
        assert!(!one.is_running());

        let ids = ["a".to_string(), "b".to_string()];
        let mut updates: Vec<usize> = Vec::new();
        let started = std::time::Instant::now();
        let results = tokio::time::timeout(
            Duration::from_secs(5),
            client.poll_test_status(&ids, |completed: usize, slots: &[Option<TestEntity>]| {
                assert_eq!(slots.len(), 2);
                updates.push(completed);
            }),
        )
        .await
        .expect("poll_test_status timed out")
        .expect("poll_test_status ok");

        // One cycle, no sleep: on_update fired exactly once with all completed.
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "poll should not have slept"
        );
        assert_eq!(updates, vec![2]);
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(TestEntity::passed));
        server.abort();
    }

    #[tokio::test]
    async fn poll_one_returns_false_and_leaves_slot_empty_on_transient_error() {
        async fn boom(AxumPath(_id): AxumPath<String>) -> StatusCode {
            StatusCode::INTERNAL_SERVER_ERROR
        }
        let router = Router::new().route("/mcp/project/test/{id}", get(boom));
        let (base, server) = crate::testutil::serve_router(router).await;
        let client = client_for(&base);

        let mut slot: Option<TestEntity> = None;
        let finished =
            tokio::time::timeout(Duration::from_secs(5), client.poll_one("t1", &mut slot))
                .await
                .expect("poll_one timed out");
        // A fetch error is transient: not finished, and the slot stays empty so a
        // later cycle can retry.
        assert!(!finished);
        assert!(slot.is_none());
        server.abort();
    }

    #[tokio::test]
    async fn tunnel_create_and_version_round_trip_through_the_local_router() {
        let (base, server) = crate::testutil::serve_router(local_backend()).await;
        let client = client_for(&base);

        let (id, secret) = tokio::time::timeout(Duration::from_secs(5), client.tunnel_create())
            .await
            .expect("tunnel_create timed out")
            .expect("tunnel_create ok");
        assert!(!id.is_empty(), "tunnel id should be minted");
        assert!(!secret.is_empty(), "tunnel secret should be minted");

        let version = tokio::time::timeout(Duration::from_secs(5), client.tunnel_version())
            .await
            .expect("tunnel_version timed out")
            .expect("tunnel_version ok");
        assert_eq!(version, 2);
        server.abort();
    }

    #[tokio::test]
    async fn tunnel_create_errors_when_id_missing() {
        async fn no_id() -> Json<Value> {
            Json(json!({ "secret": "s" }))
        }
        let router = Router::new().route("/api/tunnel/v2", post(no_id));
        let (base, server) = crate::testutil::serve_router(router).await;
        let client = client_for(&base);

        let err = tokio::time::timeout(Duration::from_secs(5), client.tunnel_create())
            .await
            .expect("tunnel_create timed out")
            .expect_err("missing id must error");
        assert!(err.to_string().contains("no tunnel id"), "{err}");
        server.abort();
    }
}
