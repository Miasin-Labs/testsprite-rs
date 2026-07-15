//! OpenAI client — the "intelligence" the real `api.testsprite.com` runs.
//!
//! Reads the key from `OPENAI_API_KEY` or `~/.config/jfc/credentials.toml`
//! (`[openai].api_key`). Used to generate the PRD, the test plan, and the
//! executable Python test code — exactly the artifacts the cloud produces.

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Clone)]
pub struct LlmClient {
    http: reqwest::Client,
    api_key: String,
    pub model: String,
}

/// Resolve the OpenAI key: env first, then the jfc credentials file.
pub fn resolve_key() -> Option<String> {
    if let Ok(k) = std::env::var("OPENAI_API_KEY")
        && !k.is_empty()
    {
        return Some(k);
    }
    let path = dirs_credentials()?;
    let text = std::fs::read_to_string(path).ok()?;
    let parsed: toml::Value = toml::from_str(&text).ok()?;
    parsed
        .get("openai")?
        .get("api_key")?
        .as_str()
        .map(String::from)
}

fn dirs_credentials() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(std::path::Path::new(&home).join(".config/jfc/credentials.toml"))
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}
#[derive(Deserialize)]
struct Choice {
    message: ChatMessage,
}
#[derive(Deserialize)]
struct ChatMessage {
    content: String,
}

impl LlmClient {
    pub fn new(api_key: String, model: String) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .build()
                .expect("reqwest client"),
            api_key,
            model,
        }
    }

    /// Build from ambient config; `None` if no key is available.
    pub fn from_env(model: &str) -> Option<Self> {
        resolve_key().map(|k| Self::new(k, model.to_string()))
    }

    /// One chat turn. `json_mode` forces a JSON object response.
    async fn chat(&self, system: &str, user: &str, json_mode: bool) -> Result<String> {
        let mut body = json!({
            "model": self.model,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": user },
            ],
        });
        // Older models accept a low temperature for determinism; newer ones
        // (gpt-5*, gpt-6*, o1/o3/o4*) only support the default (1) and 400 on an
        // explicit 0.2 — so omit `temperature` for those.
        let m = self.model.as_str();
        if !(m.starts_with("gpt-5")
            || m.starts_with("gpt-6")
            || m.starts_with("o1")
            || m.starts_with("o3")
            || m.starts_with("o4"))
        {
            body["temperature"] = json!(0.2);
        }
        if json_mode {
            body["response_format"] = json!({ "type": "json_object" });
        }
        let resp = self
            .http
            .post("https://api.openai.com/v1/chat/completions")
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .context("openai request failed")?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("openai error {}: {}", status.as_u16(), text);
        }
        let parsed: ChatResponse = serde_json::from_str(&text).context("parse openai response")?;
        parsed
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .ok_or_else(|| anyhow!("openai returned no choices"))
    }

    /// Generate a structured PRD JSON from a code summary.
    pub async fn generate_prd(&self, code_summary: &Value) -> Result<Value> {
        let system = "You are TestSprite's PRD generator. Given a code summary, produce a concise \
            product requirements document as JSON with keys: meta{project,prepared_by}, \
            product_overview (string), core_goals (string[]), features (array of \
            {name, description, user_flows:string[]}). Respond with JSON only.";
        let user = format!(
            "Code summary:\n{}",
            serde_json::to_string_pretty(code_summary)?
        );
        let out = self.chat(system, &user, true).await?;
        serde_json::from_str(&out).context("PRD was not valid JSON")
    }

    /// Generate a backend test plan (array of {id,title,description}) from a PRD.
    pub async fn generate_plan(&self, prd: &Value) -> Result<Vec<Value>> {
        let system = "You are TestSprite's test planner. Given a PRD, produce backend API test \
            cases as JSON: {\"plan\":[{\"id\":\"TC001\",\"title\":...,\"description\":...}]}. \
            Cover happy paths and key error cases. Respond with JSON only.";
        let user = format!("PRD:\n{}", serde_json::to_string_pretty(prd)?);
        let out = self.chat(system, &user, true).await?;
        let v: Value = serde_json::from_str(&out).context("plan was not valid JSON")?;
        let plan = v.get("plan").cloned().unwrap_or(v);
        plan.as_array()
            .cloned()
            .ok_or_else(|| anyhow!("plan was not an array"))
    }

    /// Generate executable Python (`requests`) test code for one case.
    pub async fn generate_test_code(
        &self,
        case: &Value,
        prd: &Value,
        base_url: &str,
    ) -> Result<String> {
        let system = format!(
            "You write a single self-contained Python test using the `requests` library. \
             The service base URL is {base_url}. Use real HTTP calls, assert status codes and \
             response shape per the case, and CALL the test function at the end so running the \
             file executes it. Output ONLY Python code, no markdown fences.",
        );
        let user = format!(
            "Test case:\n{}\n\nContext PRD:\n{}",
            serde_json::to_string_pretty(case)?,
            serde_json::to_string_pretty(prd)?
        );
        let code = self.chat(&system, &user, false).await?;
        Ok(strip_code_fences(&code))
    }

    /// Generate a self-contained Playwright (Node) script for a frontend case.
    pub async fn generate_playwright(
        &self,
        case: &Value,
        prd: &Value,
        url: &str,
    ) -> Result<String> {
        let system = format!(
            "You write a single self-contained Node script using `require('playwright')`. \
             Launch chromium headless, open the page at {url}, exercise the UI flow / edge case \
             described, and `process.exit(1)` with a console.error on failure (assertion fails, \
             bad HTTP status, or any pageerror). Output ONLY JavaScript, no markdown fences.",
        );
        let user = format!(
            "Test case:\n{}\n\nContext PRD:\n{}",
            serde_json::to_string_pretty(case)?,
            serde_json::to_string_pretty(prd)?
        );
        let code = self.chat(&system, &user, false).await?;
        Ok(strip_code_fences(&code))
    }

    /// Generate a Rust integration `#[test]` for a case against a crate.
    pub async fn generate_rust_test(
        &self,
        case: &Value,
        prd: &Value,
        crate_dir: &str,
    ) -> Result<String> {
        let system = format!(
            "You write a single Rust integration test file for the crate at {crate_dir}. \
             Use the crate's PUBLIC API only (it is a dependency of this test). Cover the \
             edge/boundary/error path described with one or more `#[test]` functions and real \
             assertions. Output ONLY Rust source, no markdown fences.",
        );
        let user = format!(
            "Test case:\n{}\n\nContext PRD:\n{}",
            serde_json::to_string_pretty(case)?,
            serde_json::to_string_pretty(prd)?
        );
        let code = self.chat(&system, &user, false).await?;
        Ok(strip_code_fences(&code))
    }
}

/// Remove ```python ... ``` fences if the model added them.
fn strip_code_fences(s: &str) -> String {
    let t = s.trim();
    let t = t
        .strip_prefix("```python")
        .or_else(|| t.strip_prefix("```"))
        .unwrap_or(t);
    let t = t.strip_suffix("```").unwrap_or(t);
    t.trim().to_string()
}
