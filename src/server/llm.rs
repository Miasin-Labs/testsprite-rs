//! OpenAI client — the "intelligence" the real `api.testsprite.com` runs.
//!
//! Reads the key from `OPENAI_API_KEY` or `~/.config/jfc/credentials.toml`
//! (`[openai].api_key`). Used to generate the PRD, the test plan, and the
//! executable Python test code — exactly the artifacts the cloud produces.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::{Value, json};

/// Cumulative token/call spend across every request made through one client
/// lineage — clones (including the ones threaded through `ExecCtx`) share the
/// same ledger, so a command can meter and budget-cap its whole LLM spend.
#[derive(Debug, Default)]
pub struct UsageLedger {
    calls: AtomicU64,
    prompt_tokens: AtomicU64,
    completion_tokens: AtomicU64,
}

/// Point-in-time copy of a [`UsageLedger`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct UsageSnapshot {
    pub calls: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

impl UsageSnapshot {
    pub fn total_tokens(&self) -> u64 {
        self.prompt_tokens + self.completion_tokens
    }

    /// Tokens spent since an earlier snapshot of the same ledger.
    pub fn since(&self, earlier: &UsageSnapshot) -> UsageSnapshot {
        UsageSnapshot {
            calls: self.calls.saturating_sub(earlier.calls),
            prompt_tokens: self.prompt_tokens.saturating_sub(earlier.prompt_tokens),
            completion_tokens: self
                .completion_tokens
                .saturating_sub(earlier.completion_tokens),
        }
    }
}

#[derive(Clone)]
pub struct LlmClient {
    http: reqwest::Client,
    api_key: String,
    pub model: String,
    usage: Arc<UsageLedger>,
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

/// OpenAI-compatible API base. `OPENAI_BASE_URL` overrides it (a proxy, a
/// local stand-in, or a compatible provider); default is the real endpoint.
///
/// Callers append `/v1/chat/completions` / `/v1/responses` themselves, but the
/// official SDKs document base URLs *with* a trailing `/v1`
/// (`https://api.openai.com/v1`), so environments routinely carry that form.
/// Accept both: trim trailing slashes, then one trailing `/v1` — otherwise the
/// SDK-convention base yields `/v1/v1/responses` → 404.
fn api_base() -> String {
    std::env::var("OPENAI_BASE_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| normalize_base(&s))
        .unwrap_or_else(|| "https://api.openai.com".to_string())
}

/// Strip trailing slashes and at most one trailing `/v1` path segment.
fn normalize_base(raw: &str) -> String {
    let trimmed = raw.trim().trim_end_matches('/');
    trimmed
        .strip_suffix("/v1")
        .unwrap_or(trimmed)
        .trim_end_matches('/')
        .to_string()
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<WireUsage>,
}
#[derive(Deserialize)]
struct Choice {
    message: ChatMessage,
}
#[derive(Deserialize)]
struct ChatMessage {
    content: String,
}

/// Token accounting as the wire reports it: chat/completions uses
/// `prompt_tokens`/`completion_tokens`, the responses API uses
/// `input_tokens`/`output_tokens`. Absent fields count as zero.
#[derive(Debug, Default, Deserialize)]
struct WireUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
}

impl WireUsage {
    fn prompt(&self) -> u64 {
        self.prompt_tokens.max(self.input_tokens)
    }
    fn completion(&self) -> u64 {
        self.completion_tokens.max(self.output_tokens)
    }
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
            usage: Arc::new(UsageLedger::default()),
        }
    }

    /// Build from ambient config; `None` if no key is available.
    pub fn from_env(model: &str) -> Option<Self> {
        resolve_key().map(|k| Self::new(k, model.to_string()))
    }

    /// Cumulative spend across this client and all its clones.
    pub fn usage(&self) -> UsageSnapshot {
        UsageSnapshot {
            calls: self.usage.calls.load(Ordering::Relaxed),
            prompt_tokens: self.usage.prompt_tokens.load(Ordering::Relaxed),
            completion_tokens: self.usage.completion_tokens.load(Ordering::Relaxed),
        }
    }

    /// True once total spend has reached `budget` tokens (`None` = unlimited).
    pub fn over_budget(&self, budget: Option<u64>) -> bool {
        budget.is_some_and(|b| self.usage().total_tokens() >= b)
    }

    fn record_usage(&self, usage: Option<&WireUsage>) {
        self.usage.calls.fetch_add(1, Ordering::Relaxed);
        if let Some(u) = usage {
            self.usage
                .prompt_tokens
                .fetch_add(u.prompt(), Ordering::Relaxed);
            self.usage
                .completion_tokens
                .fetch_add(u.completion(), Ordering::Relaxed);
        }
    }

    /// One chat turn. `json_mode` forces a JSON object response.
    async fn chat(&self, system: &str, user: &str, json_mode: bool) -> Result<String> {
        if uses_responses_api(&self.model) {
            return self.responses(system, user, json_mode).await;
        }
        match self.chat_completions(system, user, json_mode).await {
            Ok(out) => Ok(out),
            Err(e)
                if e.to_string()
                    .contains("not supported in the v1/chat/completions") =>
            {
                self.responses(system, user, json_mode).await
            }
            Err(e) => Err(e),
        }
    }

    async fn chat_completions(&self, system: &str, user: &str, json_mode: bool) -> Result<String> {
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
            .post(format!("{}/v1/chat/completions", api_base()))
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
        self.record_usage(parsed.usage.as_ref());
        parsed
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .ok_or_else(|| anyhow!("openai returned no choices"))
    }

    async fn responses(&self, system: &str, user: &str, json_mode: bool) -> Result<String> {
        let mut body = json!({
            "model": self.model,
            "store": false,
            "input": [
                { "role": "system", "content": [{ "type": "input_text", "text": system }] },
                { "role": "user", "content": [{ "type": "input_text", "text": user }] },
            ],
        });
        if json_mode {
            body["text"] = json!({ "format": { "type": "json_object" } });
        }
        let resp = self
            .http
            .post(format!("{}/v1/responses", api_base()))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .context("openai responses request failed")?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("openai responses error {}: {}", status.as_u16(), text);
        }
        let parsed: Value =
            serde_json::from_str(&text).context("parse openai responses response")?;
        let usage = parsed
            .get("usage")
            .and_then(|u| serde_json::from_value::<WireUsage>(u.clone()).ok());
        self.record_usage(usage.as_ref());
        extract_responses_text(&parsed).ok_or_else(|| anyhow!("openai responses returned no text"))
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
        let system = plan_prompt();
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
    /// Fix ONLY the compilation problems in a generated test, preserving its
    /// assertions, given the exact compiler diagnostic. Used by the bounded
    /// compile-repair loop after deterministic fixups fall short.
    pub async fn repair_code(
        &self,
        language: &str,
        code: &str,
        diagnostic: &str,
    ) -> Result<String> {
        let system = format!(
            "You are TestSprite's build-repair engine. The following {language} test fails to \
             compile. Fix ONLY compilation problems (imports, types, syntax, API misuse) — do \
             NOT weaken, remove, or change the meaning of any assertion. Output ONLY the \
             corrected {language} source, no markdown fences.",
        );
        let user = format!("Test source:\n{code}\n\nCompiler diagnostic:\n{diagnostic}");
        let out = self.chat(&system, &user, false).await?;
        Ok(strip_code_fences(&out))
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
    /// `[{name,file,branches,source?}]`, targeting each function's inputs/outputs
    /// and its control-flow branches through one [`Perspective`]. Returns
    /// [{id,title,description}].
    pub async fn generate_from_functions(
        &self,
        functions: &Value,
        exemplars: &[Value],
        prevention: &str,
        perspective: Perspective,
    ) -> Result<Vec<Value>> {
        let system = format!(
            "You are TestSprite's coverage-driven planner. Given a JSON array of functions \
            {{name,file,branches,source?,branch_conditions?}}, produce one test case per function \
            that exercises its inputs/outputs and its control-flow branches. When \
            `branch_conditions` is present, choose inputs that make EACH listed condition both \
            true and false — target the branches, not just the function. {} Respond with JSON \
            only: {{\"plan\":[{{\"id\":\"{}001\",\"title\":...,\"description\":\"what to feed the \
            function and assert, covering its branches\"}}]}} — ids MUST use the {} prefix.",
            perspective.prompt_clause(),
            perspective.id_prefix(),
            perspective.id_prefix(),
        );
        // Repo's own related tests as few-shot grounding for style/setup/values.
        let exemplar_block = if exemplars.is_empty() {
            String::new()
        } else {
            format!(
                "\n\nRelated existing tests from THIS repo — mirror their setup, argument \
                 shapes, and assertion style (do not copy them verbatim):\n{}",
                serde_json::to_string_pretty(&Value::Array(exemplars.to_vec()))?
            )
        };
        let user = format!(
            "Functions to cover:\n{}{exemplar_block}{prevention}",
            serde_json::to_string_pretty(functions)?
        );
        let out = self.chat(&system, &user, true).await?;
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

/// The angle a coverage-driven generation pass takes on each function. LLMs
/// left to one framing produce almost exclusively happy-path cases (the
/// literature measures ~0% exception-path coverage vs 93% for search-based
/// tools), so callers fan the same units through several perspectives and
/// merge the results.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Perspective {
    Normal,
    Boundary,
    Exception,
}

impl Perspective {
    pub const ALL: &[Perspective] = &[
        Perspective::Normal,
        Perspective::Boundary,
        Perspective::Exception,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Perspective::Normal => "normal",
            Perspective::Boundary => "boundary",
            Perspective::Exception => "exception",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "normal" => Some(Perspective::Normal),
            "boundary" => Some(Perspective::Boundary),
            "exception" => Some(Perspective::Exception),
            _ => None,
        }
    }

    /// Distinct case-id prefix per view, so merged views can't collide on id
    /// (the store upserts by id — a collision silently overwrites).
    fn id_prefix(self) -> &'static str {
        match self {
            Perspective::Normal => "TC",
            Perspective::Boundary => "BND",
            Perspective::Exception => "EXC",
        }
    }

    fn prompt_clause(self) -> &'static str {
        match self {
            Perspective::Normal => {
                "Focus on NORMAL operation: representative valid inputs and the documented \
                 happy-path behavior."
            }
            Perspective::Boundary => {
                "Focus on BOUNDARY inputs: empty/zero/negative values, numeric extremes, \
                 off-by-one limits, empty and oversized collections/strings, and unicode — \
                 the inputs at the edges of each branch condition."
            }
            Perspective::Exception => {
                "Focus on ERROR and EXCEPTION paths: invalid inputs, unmet preconditions, \
                 failure returns (Err/None/exception), and how the function must reject or \
                 report them. Every case should target a failure-handling branch."
            }
        }
    }
}

fn adversarial_plan_prompt(context: &Value) -> Result<(String, String)> {
    let system = "You are TestSprite's adversarial QA planner. Assume the app is wrong by \
        default. Given code summary, existing tests, latest results, and coverage gaps, propose \
        high-signal tests that would catch real product defects, false-green checks, auth/scope \
        mistakes, broken payloads, missing UI behavior, and regression-prone edges. Prefer \
        deterministic runnable cases: backend `spec` or `steps`, frontend `planSteps`, command \
        `code` (shell script). Do not put `steps` on command cases; command cases run their \
        `code` field. Use \
        description-only only if no endpoint/selector can be inferred. Never embed secrets; use \
        ${VARS} placeholders from .testsprite.env/process env. Respond JSON only: \
        {\"plan\":[{\"id\":\"ADV001\",\"title\":\"...\",\"description\":\"...\",\
        \"kind\":\"backend|frontend|command\",\"category\":\"security|functional|edge|regression\",\
        \"priority\":\"critical|high|medium|low\",\"adversarialReason\":\"why this might be broken\",\
        \"spec\":{...} OR \"steps\":[...] OR \"planSteps\":[...] OR \"code\":\"...\"}]}.";
    let user = format!(
        "Project context:\n{}",
        serde_json::to_string_pretty(context)?
    );
    Ok((system.to_string(), user))
}

fn uses_responses_api(model: &str) -> bool {
    model.contains("codex") || model.starts_with("gpt-5.3") || model.starts_with("gpt-5.5")
}

fn extract_responses_text(v: &Value) -> Option<String> {
    if let Some(s) = v.get("output_text").and_then(Value::as_str) {
        return Some(s.to_string());
    }
    let mut out = String::new();
    for item in v.get("output")?.as_array()? {
        for content in item.get("content")?.as_array()? {
            if content.get("type").and_then(Value::as_str) == Some("output_text")
                && let Some(text) = content.get("text").and_then(Value::as_str)
            {
                out.push_str(text);
            }
        }
    }
    (!out.is_empty()).then_some(out)
}

fn plan_prompt() -> &'static str {
    "You are TestSprite's test planner. Given a PRD, produce backend API test \
        cases as JSON: {\"plan\":[{\"id\":\"TC001\",\"title\":...,\"description\":...,\
        \"kind\":\"backend\",\"spec\":{...}}]}. Prefer deterministic runnable cases: \
        use `spec` for one HTTP assertion and `steps` for real QA flows (login/OAuth -> \
        save token -> call protected endpoint). A step shape is {method,path,headers?,\
        auth?,body?|form?,expect_status?,expect_json?,expect_body?,expect_parses?,save?,\
        graphql?}. Use `${VAR}` placeholders for secrets from .testsprite.env/process env; \
        never invent or embed real credentials. GraphQL may use `graphql`:{query,variables?,\
        operationName?,expect_no_errors?,expect_data?}. Description-only cases are allowed \
        only when no endpoint/payload can be inferred. Cover happy paths and key error cases. \
        Respond with JSON only."
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
    use std::sync::Arc;

    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::routing::post;
    use axum::{Json, Router};
    use tokio::sync::Mutex;

    use super::*;
    use crate::testutil::{env_guard, serve_router};

    #[test]
    fn adversarial_prompt_demands_deterministic_runnable_cases() {
        let (system, user) =
            adversarial_plan_prompt(&json!({"code_summary":{"project_name":"x"}})).unwrap();
        assert!(system.contains("adversarial QA planner"));
        assert!(system.contains("Assume the app is wrong"));
        assert!(system.contains("spec"));
        assert!(system.contains("steps"));
        assert!(system.contains("planSteps"));
        assert!(system.contains("command `code`"));
        assert!(system.contains("Do not put `steps` on command cases"));
        assert!(system.contains("Never embed secrets") || system.contains("never embed secrets"));
        assert!(user.contains("code_summary"));
    }

    #[test]
    fn plan_prompt_keeps_llm_generation_flow_runnable_and_env_driven() {
        let system = plan_prompt();
        assert!(system.contains("\"plan\""));
        assert!(system.contains("steps"));
        assert!(system.contains("login/OAuth"));
        assert!(system.contains("save token"));
        assert!(system.contains("${VAR}"));
        assert!(system.contains(".testsprite.env"));
        assert!(system.contains("never invent or embed real credentials"));
        assert!(system.contains("graphql"));
    }

    #[test]
    fn responses_api_models_are_routed_off_chat_completions() {
        assert!(uses_responses_api("gpt-5.3-codex"));
        assert!(uses_responses_api("gpt-5.5"));
        assert!(!uses_responses_api("gpt-4o-mini"));
    }

    #[test]
    fn extracts_responses_api_text_shapes() {
        assert_eq!(
            extract_responses_text(&json!({"output_text":"{\"ok\":true}"})).unwrap(),
            "{\"ok\":true}"
        );
        assert_eq!(
            extract_responses_text(&json!({
                "output": [{
                    "type": "message",
                    "content": [
                        {"type": "output_text", "text": "hello"},
                        {"type": "output_text", "text": " world"}
                    ]
                }]
            }))
            .unwrap(),
            "hello world"
        );
    }

    #[test]
    fn strips_common_markdown_code_fences() {
        assert_eq!(
            strip_code_fences("```python\nprint('ok')\n```"),
            "print('ok')"
        );
        assert_eq!(strip_code_fences("```\nraw\n```"), "raw");
        assert_eq!(strip_code_fences(" plain "), "plain");
    }

    // ---- Fake OpenAI server harness ----------------------------------------
    //
    // One helper (`fake_openai`) serves POST /v1/chat/completions and
    // POST /v1/responses on an ephemeral port, records every request body into
    // per-endpoint `Arc<Mutex<Vec<Value>>>`, and replays a per-test canned
    // reply. `OPENAI_BASE_URL` (read per-request by `api_base()`) is pointed at
    // it under the global env guard, held across the client `.await`.

    /// A canned HTTP reply (status + raw body text). `api_base()`-fronted code
    /// reads `resp.text()` then `serde_json::from_str`, so a plain-text body of
    /// JSON is enough — the content-type does not matter.
    struct Reply {
        status: u16,
        body: String,
    }

    #[derive(Clone)]
    struct Fake {
        chat_reqs: Arc<Mutex<Vec<Value>>>,
        resp_reqs: Arc<Mutex<Vec<Value>>>,
        chat: Arc<Reply>,
        responses: Arc<Reply>,
    }

    /// `{"choices":[{"message":{"content": <content>}}]}` at 200.
    fn ok_chat(content: &str) -> Reply {
        Reply {
            status: 200,
            body: json!({ "choices": [{ "message": { "content": content } }] }).to_string(),
        }
    }

    /// [`ok_chat`] plus wire usage accounting.
    fn ok_chat_with_usage(content: &str, prompt: u64, completion: u64) -> Reply {
        Reply {
            status: 200,
            body: json!({
                "choices": [{ "message": { "content": content } }],
                "usage": { "prompt_tokens": prompt, "completion_tokens": completion },
            })
            .to_string(),
        }
    }

    /// `{"output_text": <text>}` at 200 (the Responses API shape).
    fn ok_resp(text: &str) -> Reply {
        Reply {
            status: 200,
            body: json!({ "output_text": text }).to_string(),
        }
    }

    /// A non-2xx reply with an arbitrary body.
    fn fail(status: u16, body: &str) -> Reply {
        Reply {
            status,
            body: body.to_string(),
        }
    }

    /// A reply for an endpoint a given test never expects to hit.
    fn unused() -> Reply {
        Reply {
            status: 599,
            body: "unused endpoint".to_string(),
        }
    }

    async fn chat_endpoint(State(s): State<Fake>, Json(body): Json<Value>) -> (StatusCode, String) {
        s.chat_reqs.lock().await.push(body);
        let code = StatusCode::from_u16(s.chat.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (code, s.chat.body.clone())
    }

    async fn responses_endpoint(
        State(s): State<Fake>,
        Json(body): Json<Value>,
    ) -> (StatusCode, String) {
        s.resp_reqs.lock().await.push(body);
        let code =
            StatusCode::from_u16(s.responses.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (code, s.responses.body.clone())
    }

    /// Serve a fake OpenAI on an ephemeral port. Returns its base URL, the
    /// shared `Fake` state (holds the captured request bodies), and the server
    /// join handle (abort it at test end).
    async fn fake_openai(
        chat: Reply,
        responses: Reply,
    ) -> (String, Fake, tokio::task::JoinHandle<()>) {
        let state = Fake {
            chat_reqs: Arc::new(Mutex::new(Vec::new())),
            resp_reqs: Arc::new(Mutex::new(Vec::new())),
            chat: Arc::new(chat),
            responses: Arc::new(responses),
        };
        let router = Router::new()
            .route("/v1/chat/completions", post(chat_endpoint))
            .route("/v1/responses", post(responses_endpoint))
            .with_state(state.clone());
        let (base, handle) = serve_router(router).await;
        (base, state, handle)
    }

    fn client(model: &str) -> LlmClient {
        LlmClient::new("test-key".to_string(), model.to_string())
    }

    #[tokio::test]
    async fn chat_completions_sends_temperature_and_json_format_for_older_models() {
        let (base, fake, handle) =
            fake_openai(ok_chat(r#"{"product_overview":"ok"}"#), unused()).await;
        let client = client("gpt-4o-mini");
        let prd = {
            let _g = env_guard(&[("OPENAI_BASE_URL", Some(base.as_str()))]);
            client
                .generate_prd(&json!({ "project_name": "demo" }))
                .await
        }
        .unwrap();
        assert_eq!(prd["product_overview"], "ok");

        let reqs = fake.chat_reqs.lock().await;
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0]["model"], "gpt-4o-mini");
        // Value equality avoids a float `==` (clippy::float_cmp); both sides are
        // the same f64 0.2, so the numbers compare equal.
        assert_eq!(reqs[0]["temperature"], json!(0.2));
        assert_eq!(reqs[0]["response_format"]["type"], "json_object");
        assert_eq!(reqs[0]["messages"][0]["role"], "system");
        assert!(fake.resp_reqs.lock().await.is_empty());
        handle.abort();
    }

    #[tokio::test]
    async fn gpt5_model_omits_temperature_and_response_format() {
        // gpt-5 (not gpt-5.3/5.5/codex) still uses chat/completions, but must
        // NOT send an explicit temperature; json_mode false omits the format.
        let (base, fake, handle) = fake_openai(ok_chat("SRC"), unused()).await;
        let client = client("gpt-5");
        let out = {
            let _g = env_guard(&[("OPENAI_BASE_URL", Some(base.as_str()))]);
            client
                .generate_test_code(&json!({ "id": "TC1" }), &json!({}), "http://svc")
                .await
        }
        .unwrap();
        assert_eq!(out, "SRC");

        let reqs = fake.chat_reqs.lock().await;
        assert_eq!(reqs.len(), 1);
        assert!(reqs[0].get("temperature").is_none());
        assert!(reqs[0].get("response_format").is_none());
        handle.abort();
    }

    #[tokio::test]
    async fn chat_completions_non_2xx_surfaces_openai_error() {
        let (base, _fake, handle) = fake_openai(fail(500, "boom"), unused()).await;
        let client = client("gpt-4o-mini");
        let err = {
            let _g = env_guard(&[("OPENAI_BASE_URL", Some(base.as_str()))]);
            client.generate_prd(&json!({})).await
        }
        .unwrap_err();
        assert!(err.to_string().contains("openai error"), "got: {err}");
        handle.abort();
    }

    #[tokio::test]
    async fn chat_falls_back_to_responses_when_param_rejected() {
        let (base, fake, handle) = fake_openai(
            fail(
                400,
                "temperature is not supported in the v1/chat/completions endpoint",
            ),
            ok_resp("print('hi')"),
        )
        .await;
        let client = client("gpt-4o-mini");
        let out = {
            let _g = env_guard(&[("OPENAI_BASE_URL", Some(base.as_str()))]);
            client
                .generate_test_code(&json!({ "id": "TC1" }), &json!({}), "http://svc")
                .await
        }
        .unwrap();
        assert_eq!(out, "print('hi')");
        // Both endpoints were exercised: chat/completions rejected the param,
        // and the fallback reached /v1/responses.
        assert_eq!(fake.chat_reqs.lock().await.len(), 1);
        assert_eq!(fake.resp_reqs.lock().await.len(), 1);
        handle.abort();
    }

    #[tokio::test]
    async fn responses_api_model_routes_directly_and_sets_json_format() {
        let (base, fake, handle) =
            fake_openai(unused(), ok_resp(r#"{"product_overview":"r"}"#)).await;
        let client = client("gpt-5.3-codex");
        let prd = {
            let _g = env_guard(&[("OPENAI_BASE_URL", Some(base.as_str()))]);
            client.generate_prd(&json!({ "project_name": "x" })).await
        }
        .unwrap();
        assert_eq!(prd["product_overview"], "r");

        assert!(fake.chat_reqs.lock().await.is_empty());
        let reqs = fake.resp_reqs.lock().await;
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0]["model"], "gpt-5.3-codex");
        assert_eq!(reqs[0]["store"], json!(false));
        assert_eq!(reqs[0]["text"]["format"]["type"], "json_object");
        assert_eq!(reqs[0]["input"][0]["role"], "system");
        handle.abort();
    }

    #[tokio::test]
    async fn generate_prd_rejects_invalid_json() {
        let (base, _f, handle) = fake_openai(ok_chat("not json at all"), unused()).await;
        let client = client("gpt-4o-mini");
        let err = {
            let _g = env_guard(&[("OPENAI_BASE_URL", Some(base.as_str()))]);
            client.generate_prd(&json!({})).await
        }
        .unwrap_err();
        assert!(
            err.to_string().contains("PRD was not valid JSON"),
            "got: {err}"
        );
        handle.abort();
    }

    #[tokio::test]
    async fn generate_prd_from_doc_normalizes_and_sends_document() {
        let (base, fake, handle) =
            fake_openai(ok_chat(r#"{"product_overview":"norm"}"#), unused()).await;
        let client = client("gpt-4o-mini");
        let prd = {
            let _g = env_guard(&[("OPENAI_BASE_URL", Some(base.as_str()))]);
            client
                .generate_prd_from_doc("# My README\nDoes things.")
                .await
        }
        .unwrap();
        assert_eq!(prd["product_overview"], "norm");

        let reqs = fake.chat_reqs.lock().await;
        let sys = reqs[0]["messages"][0]["content"].as_str().unwrap();
        let user = reqs[0]["messages"][1]["content"].as_str().unwrap();
        assert!(sys.contains("PRD normalizer"));
        assert!(user.contains("Document:"));
        assert!(user.contains("My README"));
        handle.abort();
    }

    #[tokio::test]
    async fn generate_plan_accepts_array_and_wrapper_and_rejects_non_array() {
        let client = client("gpt-4o-mini");

        // Bare array response.
        let (base, _f, h1) = fake_openai(ok_chat(r#"[{"id":"TC001"}]"#), unused()).await;
        let bare = {
            let _g = env_guard(&[("OPENAI_BASE_URL", Some(base.as_str()))]);
            client.generate_plan(&json!({})).await
        }
        .unwrap();
        assert_eq!(bare.len(), 1);
        assert_eq!(bare[0]["id"], "TC001");
        h1.abort();

        // {"plan":[...]} wrapper.
        let (base, _f, h2) = fake_openai(
            ok_chat(r#"{"plan":[{"id":"TC002"},{"id":"TC003"}]}"#),
            unused(),
        )
        .await;
        let wrapped = {
            let _g = env_guard(&[("OPENAI_BASE_URL", Some(base.as_str()))]);
            client.generate_plan(&json!({})).await
        }
        .unwrap();
        assert_eq!(wrapped.len(), 2);
        assert_eq!(wrapped[1]["id"], "TC003");
        h2.abort();

        // A JSON object that is neither an array nor a {"plan":[...]} wrapper.
        let (base, _f, h3) = fake_openai(ok_chat(r#"{"nope":1}"#), unused()).await;
        let err = {
            let _g = env_guard(&[("OPENAI_BASE_URL", Some(base.as_str()))]);
            client.generate_plan(&json!({})).await
        }
        .unwrap_err();
        assert!(
            err.to_string().contains("plan was not an array"),
            "got: {err}"
        );
        h3.abort();
    }

    #[tokio::test]
    async fn generate_adversarial_tests_returns_plan_from_adversarial_prompt() {
        let (base, fake, handle) = fake_openai(
            ok_chat(r#"{"plan":[{"id":"ADV001","kind":"backend"}]}"#),
            unused(),
        )
        .await;
        let client = client("gpt-4o-mini");
        let plan = {
            let _g = env_guard(&[("OPENAI_BASE_URL", Some(base.as_str()))]);
            client
                .generate_adversarial_tests(&json!({ "code_summary": { "project_name": "x" } }))
                .await
        }
        .unwrap();
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0]["id"], "ADV001");

        let reqs = fake.chat_reqs.lock().await;
        assert!(
            reqs[0]["messages"][0]["content"]
                .as_str()
                .unwrap()
                .contains("adversarial QA planner")
        );
        handle.abort();
    }

    #[tokio::test]
    async fn generate_test_code_strips_python_fences_and_omits_json_format() {
        let (base, fake, handle) =
            fake_openai(ok_chat("```python\nprint('ok')\n```"), unused()).await;
        let client = client("gpt-4o-mini");
        let code = {
            let _g = env_guard(&[("OPENAI_BASE_URL", Some(base.as_str()))]);
            client
                .generate_test_code(&json!({ "id": "TC1" }), &json!({ "x": 1 }), "http://svc:9")
                .await
        }
        .unwrap();
        assert_eq!(code, "print('ok')");

        let reqs = fake.chat_reqs.lock().await;
        assert!(reqs[0].get("response_format").is_none());
        assert!(
            reqs[0]["messages"][0]["content"]
                .as_str()
                .unwrap()
                .contains("http://svc:9")
        );
        handle.abort();
    }

    #[tokio::test]
    async fn generate_playwright_returns_body_and_prompts_for_page() {
        let (base, fake, handle) = fake_openai(ok_chat("await page.click('#go');"), unused()).await;
        let client = client("gpt-4o-mini");
        let code = {
            let _g = env_guard(&[("OPENAI_BASE_URL", Some(base.as_str()))]);
            client
                .generate_playwright(&json!({ "id": "F1" }), &json!({}), "http://ui.local")
                .await
        }
        .unwrap();
        assert_eq!(code, "await page.click('#go');");

        let reqs = fake.chat_reqs.lock().await;
        let sys = reqs[0]["messages"][0]["content"].as_str().unwrap();
        assert!(sys.contains("Playwright"));
        assert!(sys.contains("http://ui.local"));
        handle.abort();
    }

    #[tokio::test]
    async fn generate_rust_test_returns_source_for_crate_dir() {
        let (base, fake, handle) =
            fake_openai(ok_chat("fn it_works() { assert_eq!(1, 1); }"), unused()).await;
        let client = client("gpt-4o-mini");
        let code = {
            let _g = env_guard(&[("OPENAI_BASE_URL", Some(base.as_str()))]);
            client
                .generate_rust_test(&json!({ "id": "R1" }), &json!({}), "/crate/dir")
                .await
        }
        .unwrap();
        assert_eq!(code, "fn it_works() { assert_eq!(1, 1); }");
        assert!(
            fake.chat_reqs.lock().await[0]["messages"][0]["content"]
                .as_str()
                .unwrap()
                .contains("/crate/dir")
        );
        handle.abort();
    }

    #[tokio::test]
    async fn analyze_failure_returns_classification_json() {
        let (base, fake, handle) = fake_openai(
            ok_chat(r#"{"fixKind":"code","verdict":"bug","cause":"c","fix":"f"}"#),
            unused(),
        )
        .await;
        let client = client("gpt-4o-mini");
        let v = {
            let _g = env_guard(&[("OPENAI_BASE_URL", Some(base.as_str()))]);
            client
                .analyze_failure(&json!({ "id": "T" }), "code body", "boom error")
                .await
        }
        .unwrap();
        assert_eq!(v["verdict"], "bug");

        let reqs = fake.chat_reqs.lock().await;
        assert!(
            reqs[0]["messages"][0]["content"]
                .as_str()
                .unwrap()
                .contains("failure analyst")
        );
        assert!(
            reqs[0]["messages"][1]["content"]
                .as_str()
                .unwrap()
                .contains("boom error")
        );
        handle.abort();
    }

    #[tokio::test]
    async fn propose_fix_system_prompt_differs_by_source_presence() {
        let client = client("gpt-4o-mini");

        // With source: the model is asked for a REAL, git-appliable unified diff.
        let (base, with, h1) =
            fake_openai(ok_chat(r#"{"explanation":"e","patch":"p"}"#), unused()).await;
        {
            let _g = env_guard(&[("OPENAI_BASE_URL", Some(base.as_str()))]);
            client
                .propose_fix(&json!({ "id": "T" }), "code", "err", Some("12: let x = 1;"))
                .await
                .unwrap();
        }
        let with_sys = with.chat_reqs.lock().await[0]["messages"][0]["content"]
            .as_str()
            .unwrap()
            .to_string();
        h1.abort();
        assert!(with_sys.contains("REAL unified diff"), "got: {with_sys}");

        // Without source: an ILLUSTRATIVE sketch, explicitly not git-appliable.
        let (base, without, h2) =
            fake_openai(ok_chat(r#"{"explanation":"e","patch":"p"}"#), unused()).await;
        {
            let _g = env_guard(&[("OPENAI_BASE_URL", Some(base.as_str()))]);
            client
                .propose_fix(&json!({ "id": "T" }), "code", "err", None)
                .await
                .unwrap();
        }
        let without_sys = without.chat_reqs.lock().await[0]["messages"][0]["content"]
            .as_str()
            .unwrap()
            .to_string();
        h2.abort();
        assert!(without_sys.contains("ILLUSTRATIVE"), "got: {without_sys}");
        assert!(!without_sys.contains("REAL unified diff"));
    }

    #[tokio::test]
    async fn heal_test_returns_improved_case_json() {
        let (base, fake, handle) =
            fake_openai(ok_chat(r#"{"title":"better","description":"d"}"#), unused()).await;
        let client = client("gpt-4o-mini");
        let v = {
            let _g = env_guard(&[("OPENAI_BASE_URL", Some(base.as_str()))]);
            client
                .heal_test(&json!({ "title": "old" }), "code", "err")
                .await
        }
        .unwrap();
        assert_eq!(v["title"], "better");
        assert!(
            fake.chat_reqs.lock().await[0]["messages"][0]["content"]
                .as_str()
                .unwrap()
                .contains("auto-heal")
        );
        handle.abort();
    }

    #[tokio::test]
    async fn generate_from_functions_returns_plan() {
        let (base, fake, handle) = fake_openai(
            ok_chat(r#"{"plan":[{"id":"TC001","title":"t"}]}"#),
            unused(),
        )
        .await;
        let client = client("gpt-4o-mini");
        let plan = {
            let _g = env_guard(&[("OPENAI_BASE_URL", Some(base.as_str()))]);
            client
                .generate_from_functions(
                    &json!([{ "name": "foo", "file": "a.rs", "branches": 2 }]),
                    &[],
                    "",
                    Perspective::Normal,
                )
                .await
        }
        .unwrap();
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0]["id"], "TC001");
        assert!(
            fake.chat_reqs.lock().await[0]["messages"][0]["content"]
                .as_str()
                .unwrap()
                .contains("coverage-driven planner")
        );
        handle.abort();
    }

    #[tokio::test]
    async fn plan_action_returns_action_json() {
        let (base, fake, handle) = fake_openai(
            ok_chat(r#"{"assistant":"hi","action":{"kind":"none"}}"#),
            unused(),
        )
        .await;
        let client = client("gpt-4o-mini");
        let v = {
            let _g = env_guard(&[("OPENAI_BASE_URL", Some(base.as_str()))]);
            client
                .plan_action("prev turns", "hello there", &json!([]))
                .await
        }
        .unwrap();
        assert_eq!(v["action"]["kind"], "none");

        let reqs = fake.chat_reqs.lock().await;
        assert!(
            reqs[0]["messages"][0]["content"]
                .as_str()
                .unwrap()
                .contains("conversational test agent")
        );
        assert!(
            reqs[0]["messages"][1]["content"]
                .as_str()
                .unwrap()
                .contains("hello there")
        );
        handle.abort();
    }

    #[test]
    fn api_base_accepts_sdk_convention_v1_suffix() {
        // SDK-style base (with /v1) and repo-style base (without) must both
        // land on ONE /v1 in the final URL — the /v1/v1 double-up was a real
        // 404 in the field.
        assert_eq!(
            normalize_base("https://api.openai.com/v1"),
            "https://api.openai.com"
        );
        assert_eq!(
            normalize_base("https://api.openai.com/v1/"),
            "https://api.openai.com"
        );
        assert_eq!(
            normalize_base("https://api.openai.com"),
            "https://api.openai.com"
        );
        // Proxy with a path: /v1 is stripped from the end only.
        assert_eq!(
            normalize_base("https://proxy.example/openai/v1"),
            "https://proxy.example/openai"
        );
        // Only ONE /v1 is stripped, and inner /v1 segments are untouched.
        assert_eq!(
            normalize_base("https://h.example/v1/v1"),
            "https://h.example/v1"
        );
        assert_eq!(
            normalize_base("https://v1.example/api"),
            "https://v1.example/api"
        );
    }

    #[test]
    fn api_base_env_override_normalizes() {
        let _g = env_guard(&[("OPENAI_BASE_URL", Some("https://api.openai.com/v1"))]);
        assert_eq!(api_base(), "https://api.openai.com");
    }

    #[test]
    fn resolve_key_prefers_env_var() {
        let _g = env_guard(&[("OPENAI_API_KEY", Some("sk-env"))]);
        assert_eq!(resolve_key(), Some("sk-env".to_string()));
    }

    #[test]
    fn resolve_key_reads_credentials_file_when_env_empty() {
        let home = crate::local::tmp_root();
        let cfg = home.join(".config/jfc");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(
            cfg.join("credentials.toml"),
            "[openai]\napi_key = \"from-file\"\n",
        )
        .unwrap();
        let got = {
            let _g = env_guard(&[
                ("OPENAI_API_KEY", Some("")),
                ("HOME", Some(home.to_str().unwrap())),
            ]);
            resolve_key()
        };
        std::fs::remove_dir_all(&home).ok();
        assert_eq!(got, Some("from-file".to_string()));
    }

    #[test]
    fn resolve_key_returns_none_when_absent() {
        let home = crate::local::tmp_root();
        let got = {
            let _g = env_guard(&[
                ("OPENAI_API_KEY", None),
                ("HOME", Some(home.to_str().unwrap())),
            ]);
            resolve_key()
        };
        std::fs::remove_dir_all(&home).ok();
        assert_eq!(got, None);
    }

    #[tokio::test]
    async fn usage_ledger_accumulates_across_clones_and_gates_budget() {
        let (base, _fake, server) =
            fake_openai(ok_chat_with_usage("hi", 120, 30), ok_resp("unused")).await;
        let _g = env_guard(&[("OPENAI_BASE_URL", Some(base.as_str()))]);
        let client = LlmClient::new("k".into(), "gpt-4o-mini".into());
        let clone = client.clone();

        clone.chat("s", "u", false).await.unwrap();
        client.chat("s", "u", false).await.unwrap();

        // Both calls landed on ONE shared ledger, visible from either handle.
        let usage = client.usage();
        assert_eq!(usage.calls, 2);
        assert_eq!(usage.prompt_tokens, 240);
        assert_eq!(usage.completion_tokens, 60);
        assert_eq!(usage.total_tokens(), 300);

        // `since` isolates a span; `over_budget` trips at the cap, not below.
        let before = clone.usage();
        clone.chat("s", "u", false).await.unwrap();
        let delta = clone.usage().since(&before);
        assert_eq!(delta.calls, 1);
        assert_eq!(delta.total_tokens(), 150);
        assert!(!client.over_budget(None));
        assert!(!client.over_budget(Some(451)));
        assert!(client.over_budget(Some(450)));
        assert!(client.over_budget(Some(10)));
        server.abort();
    }

    #[tokio::test]
    async fn perspectives_shape_the_cover_prompt_and_id_prefix() {
        let (base, fake, server) = fake_openai(
            ok_chat(r#"{"plan":[{"id":"BND001","title":"boundary case"}]}"#),
            ok_resp("unused"),
        )
        .await;
        let _g = env_guard(&[("OPENAI_BASE_URL", Some(base.as_str()))]);
        let client = LlmClient::new("k".into(), "gpt-4o-mini".into());

        let functions = json!([{ "name": "f", "file": "a.rs", "branches": 3 }]);
        let exemplars = vec![json!({ "title": "existing related test", "code": "f();" })];
        let plan = client
            .generate_from_functions(&functions, &exemplars, "", Perspective::Boundary)
            .await
            .unwrap();
        assert_eq!(plan[0]["id"], "BND001");

        let sent = fake.chat_reqs.lock().await;
        let system = sent[0]["messages"][0]["content"].as_str().unwrap();
        assert!(system.contains("BOUNDARY"), "{system}");
        assert!(system.contains("BND"), "{system}");
        assert!(
            !system.contains("ERROR and EXCEPTION"),
            "boundary view must not carry the exception clause: {system}"
        );
        // The retrieved exemplar rides along in the user prompt.
        let user = sent[0]["messages"][1]["content"].as_str().unwrap();
        assert!(user.contains("existing related test"), "{user}");
        assert!(user.contains("Related existing tests"), "{user}");
        server.abort();
    }

    #[test]
    fn perspective_parse_and_labels_round_trip() {
        for p in Perspective::ALL {
            assert_eq!(Perspective::parse(p.label()), Some(*p));
        }
        assert_eq!(Perspective::parse("nope"), None);
    }
}
