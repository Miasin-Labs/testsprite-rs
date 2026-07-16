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

    /// Normalize an arbitrary product document (README, notes, a Jira ticket, a
    /// design doc, or a spec — any format) into the standard PRD JSON shape.
    pub async fn generate_prd_from_doc(&self, doc: &str) -> Result<Value> {
        let system = "You are TestSprite's PRD normalizer. Given an arbitrary product document \
            (README, notes, a Jira ticket, a design doc, or a spec) in ANY format, extract and \
            produce a concise PRD as JSON with keys: meta{project,prepared_by}, product_overview \
            (string), core_goals (string[]), features (array of {name, description, \
            user_flows:string[]}). Infer sensibly from whatever is present. Respond with JSON only.";
        let out = self
            .chat(system, &format!("Document:\n{doc}"), true)
            .await?;
        serde_json::from_str(&out).context("normalized PRD was not valid JSON")
    }

    /// Generate a backend test plan from a PRD. Prefer deterministic `spec` or
    /// `steps` cases when enough endpoint detail exists; description-only cases
    /// remain the fallback.
    pub async fn generate_plan(&self, prd: &Value) -> Result<Vec<Value>> {
        let system = "You are TestSprite's test planner. Given a PRD, produce backend API test \
            cases as JSON: {\"plan\":[{\"id\":\"TC001\",\"title\":...,\"description\":...,\
            \"kind\":\"backend\",\"spec\":{...}}]}. Prefer deterministic runnable cases: \
            use `spec` for one HTTP assertion and `steps` for real QA flows (login/OAuth -> \
            save token -> call protected endpoint). A step shape is {method,path,headers?,\
            auth?,body?|form?,expect_status?,expect_json?,expect_body?,expect_parses?,save?,\
            graphql?}. Use `${VAR}` placeholders for secrets from .testsprite.env/process env; \
            never invent or embed real credentials. GraphQL may use `graphql`:{query,variables?,\
            operationName?,expect_no_errors?,expect_data?}. Description-only cases are allowed \
            only when no endpoint/payload can be inferred. Cover happy paths and key error cases. \
            Respond with JSON only.";
        let user = format!("PRD:\n{}", serde_json::to_string_pretty(prd)?);
        let out = self.chat(system, &user, true).await?;
        let v: Value = serde_json::from_str(&out).context("plan was not valid JSON")?;
        let plan = v.get("plan").cloned().unwrap_or(v);
        plan.as_array()
            .cloned()
            .ok_or_else(|| anyhow!("plan was not an array"))
    }

    /// Generate adversarial QA cases from the current project context:
    /// code-summary, stored suite, latest results, and coverage gaps. This is
    /// TestSprite-style "assume it is broken; prove otherwise" planning, and it
    /// should prefer deterministic `spec` / `steps` / `planSteps` over
    /// description-only cases.
    pub async fn generate_adversarial_tests(&self, context: &Value) -> Result<Vec<Value>> {
        let (system, user) = adversarial_plan_prompt(context)?;
        let out = self.chat(&system, &user, true).await?;
        let v: Value = serde_json::from_str(&out).context("adversarial plan was not valid JSON")?;
        let plan = v.get("plan").cloned().unwrap_or(v);
        plan.as_array()
            .cloned()
            .ok_or_else(|| anyhow!("adversarial plan was not an array"))
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

    /// Generate the BODY of a Playwright test — the statements that exercise the
    /// flow, nothing else. An executor-owned harness (see `browser::wrap_script`)
    /// launches the browser, performs the initial navigation to `url`, captures
    /// the screenshot, and closes — so the screenshot is deterministic and does
    /// not depend on the model remembering to call it.
    pub async fn generate_playwright(
        &self,
        case: &Value,
        prd: &Value,
        url: &str,
    ) -> Result<String> {
        let system = format!(
            "You write the BODY of a Playwright test — ONLY the statements that exercise the UI \
             flow / edge case described. A `page` (Playwright Page) already exists and is \
             navigated to {url}; an outer harness owns launching the browser, the initial \
             navigation, the screenshot, and closing. Operate on `page` (click, fill, \
             waitForSelector, check text/values) and `throw new Error(<why>)` on any failure. Do \
             NOT `require('playwright')`, launch a browser, navigate initially, screenshot, or \
             close the browser. Output ONLY JavaScript statements, no function wrapper, no \
             markdown fences.",
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
    /// Classify a failed test (real bug vs test/env fragility) with root cause + fix.
    pub async fn analyze_failure(&self, case: &Value, code: &str, error: &str) -> Result<Value> {
        let system = "You are TestSprite's failure analyst. Given a test case, the code that ran, \
            and the failure output, classify the failure. Respond with JSON only: \
            {\"fixKind\":\"code|selector|data|env|unknown\",\"verdict\":\"bug|fragility|env\",\
            \"cause\":\"one-sentence root cause\",\"fix\":\"one concrete fix\"}.";
        let user = format!(
            "Case:\n{}\n\nTest code:\n{}\n\nFailure output:\n{}",
            serde_json::to_string_pretty(case)?,
            code,
            error
        );
        let out = self.chat(system, &user, true).await?;
        serde_json::from_str(&out).context("failure analysis was not valid JSON")
    }

    /// Propose the smallest fix to the CODE UNDER TEST that would make this case
    /// pass — returns `{explanation, patch}`.
    ///
    /// When `source` is `Some` (the failure pointed at real repo files and the
    /// caller read them), the model is asked for a REAL unified diff grounded in
    /// that source, which the caller then verifies with `git apply --check`.
    /// When `None`, it produces an illustrative sketch a human applies by hand —
    /// the engine cannot know real paths/line numbers, and `store::write_fix`
    /// labels the two cases differently so a sketch is never mistaken for a
    /// ready-to-apply patch.
    pub async fn propose_fix(
        &self,
        case: &Value,
        code: &str,
        error: &str,
        source: Option<&str>,
    ) -> Result<Value> {
        let system = if source.is_some() {
            "You are TestSprite's fix engine. You are given the failing test case, the test code, \
             the failure output, AND the ACTUAL source at the referenced locations (line-numbered). \
             Propose the SMALLEST change to the code UNDER TEST (never the test) that makes it pass, \
             as a REAL unified diff grounded in the provided source. Respond with JSON only: \
             {\"explanation\":\"what to change and why, 1-3 sentences\",\"patch\":\"a unified diff \
             (--- a/<path>, +++ b/<path>, @@ hunks) using the EXACT paths and line numbers from the \
             provided source so it applies with `git apply`\"}."
        } else {
            "You are TestSprite's fix-recommendation engine. Given a failing test case, the test \
             code that ran, and the failure output, propose the SMALLEST change to the code UNDER \
             TEST (never the test) that would make it pass. You do NOT have the repository, so you \
             cannot know exact file paths or line numbers. Respond with JSON only: \
             {\"explanation\":\"what to change and why, 1-3 sentences\",\"patch\":\"an ILLUSTRATIVE \
             code sketch of the change (diff-style is fine) — a guide a human applies by hand, NOT \
             a patch that will git apply, since the real file/line context is unknown\"}."
        };
        let user = match source {
            Some(src) => format!(
                "Case:\n{}\n\nTest code:\n{}\n\nFailure output:\n{}\n\n\
                 Actual source at the referenced locations:\n{}",
                serde_json::to_string_pretty(case)?,
                code,
                error,
                src
            ),
            None => format!(
                "Case:\n{}\n\nTest code:\n{}\n\nFailure output:\n{}",
                serde_json::to_string_pretty(case)?,
                code,
                error
            ),
        };
        let out = self.chat(system, &user, true).await?;
        serde_json::from_str(&out).context("fix proposal was not valid JSON")
    }
    /// Regenerate an IMPROVED test case for a FRAGILITY failure (selector/wait/data
    /// drift) so it passes without masking a real bug. Returns an improved case JSON
    /// object (same shape as the input case: {title,description,kind?,spec?}).
    pub async fn heal_test(&self, case: &Value, code: &str, error: &str) -> Result<Value> {
        let system = "You are TestSprite's auto-heal engine. The given test failed due to TEST \
            FRAGILITY (a selector/wait/data/spec drift), NOT a real product bug. Return an IMPROVED \
            version of the SAME test case as JSON only (same keys as the input: title, description, \
            kind, and spec if present) that adapts to what the app actually does WITHOUT weakening \
            the assertion into something that would pass even for a broken app. Respond with the \
            case JSON object only.";
        let user = format!(
            "Failing case:\n{}\n\nGenerated code:\n{}\n\nFailure:\n{}",
            serde_json::to_string_pretty(case)?,
            code,
            error
        );
        let out = self.chat(system, &user, true).await?;
        serde_json::from_str(&out).context("healed case was not valid JSON")
    }

    /// Generate a test plan (one case per function) from a list of function units
    /// `[{name,file,branches}]`, targeting each function's inputs/outputs and its
    /// control-flow branches. Returns [{id,title,description}].
    pub async fn generate_from_functions(&self, functions: &Value) -> Result<Vec<Value>> {
        let system = "You are TestSprite's coverage-driven planner. Given a JSON array of functions \
            {name,file,branches}, produce one test case per function that exercises its inputs/outputs \
            and every control-flow branch. Respond with JSON only: {\"plan\":[{\"id\":\"TC001\",\
            \"title\":...,\"description\":\"what to feed the function and assert, covering its branches\"}]}.";
        let user = format!(
            "Functions to cover:\n{}",
            serde_json::to_string_pretty(functions)?
        );
        let out = self.chat(system, &user, true).await?;
        let v: Value = serde_json::from_str(&out).context("cover plan was not valid JSON")?;
        let plan = v.get("plan").cloned().unwrap_or(v);
        plan.as_array()
            .cloned()
            .ok_or_else(|| anyhow!("cover plan was not an array"))
    }
    /// Propose the next single conversational action: `generate`, `run`, or `none`.
    /// Returns the raw JSON `{assistant, action{kind,instruction?,cover?,ids?,summary}}`.
    pub async fn plan_action(&self, history: &str, user_msg: &str, tests: &Value) -> Result<Value> {
        let system = "You are TestSprite's conversational test agent, embedded in a coding tool via MCP. \
            Based on the conversation and the user's latest message, choose exactly ONE next action to \
            PROPOSE (the user approves before it runs). Actions: `generate` (synthesize new test cases \
            from an instruction; set cover=true to instead target currently-uncovered functions), `run` \
            (execute stored tests; empty ids = all), or `none` (greeting/question needing no test action). \
            Respond JSON only: {\"assistant\":\"one or two sentences to the user\",\"action\":{\"kind\":\
            \"generate|run|none\",\"instruction\":\"...\",\"cover\":false,\"ids\":[],\"summary\":\
            \"imperative one-line description of what approving does\"}}.";
        let user = format!(
            "Conversation so far:\n{history}\n\nStored tests:\n{tests}\n\nUser: {user_msg}"
        );
        let out = self.chat(system, &user, true).await?;
        serde_json::from_str(&out).context("plan_action was not valid JSON")
    }
}

fn adversarial_plan_prompt(context: &Value) -> Result<(String, String)> {
    let system = "You are TestSprite's adversarial QA planner. Assume the app is wrong by \
        default. Given code summary, existing tests, latest results, and coverage gaps, propose \
        high-signal tests that would catch real product defects, false-green checks, auth/scope \
        mistakes, broken payloads, missing UI behavior, and regression-prone edges. Prefer \
        deterministic runnable cases: backend `spec` or `steps`, frontend `planSteps`; use \
        description-only only if no endpoint/selector can be inferred. Never embed secrets; use \
        ${VARS} placeholders from .testsprite.env/process env. Respond JSON only: \
        {\"plan\":[{\"id\":\"ADV001\",\"title\":\"...\",\"description\":\"...\",\
        \"kind\":\"backend|frontend|command\",\"category\":\"security|functional|edge|regression\",\
        \"priority\":\"critical|high|medium|low\",\"adversarialReason\":\"why this might be broken\",\
        \"spec\":{...} OR \"steps\":[...] OR \"planSteps\":[...]}]}.";
    let user = format!(
        "Project context:\n{}",
        serde_json::to_string_pretty(context)?
    );
    Ok((system.to_string(), user))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adversarial_prompt_demands_deterministic_runnable_cases() {
        let (system, user) =
            adversarial_plan_prompt(&json!({"code_summary":{"project_name":"x"}})).unwrap();
        assert!(system.contains("adversarial QA planner"));
        assert!(system.contains("Assume the app is wrong"));
        assert!(system.contains("spec"));
        assert!(system.contains("steps"));
        assert!(system.contains("planSteps"));
        assert!(system.contains("Never embed secrets") || system.contains("never embed secrets"));
        assert!(user.contains("code_summary"));
    }
}
