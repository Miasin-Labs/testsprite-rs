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
    use super::*;

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn dashboard_url_uses_testsprite_base() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var("TESTSPRITE_URL", "https://dash.example");
        }
        assert_eq!(
            dashboard_url("p1", "t1"),
            "https://dash.example/dashboard/mcp/tests/p1/t1"
        );
        unsafe {
            std::env::remove_var("TESTSPRITE_URL");
        }
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
}
