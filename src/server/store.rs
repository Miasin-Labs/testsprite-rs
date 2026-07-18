//! In-memory test store + executor — the part the cloud runs in a sandbox.
//!
//! Because the local backend runs on the same machine as the target app, the
//! executor hits the app **directly** (no tunnel data plane needed). Each test
//! transitions RUNNING -> PASSED/FAILED and is stored so the client's
//! `GET /mcp/project/test/{id}` polling observes completion.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
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

/// Execute a spec: the primary request, then — only if it passes — each `then`
/// follow-up in order (a read-after-write check that the state actually
/// changed). Returns (passed, error, code); the code artifact is the primary's.
pub async fn execute_spec(
    spec: &EndpointSpec,
    base_url: &str,
    vars: &HashMap<String, String>,
) -> (bool, String, String) {
    let mut steps = vec![spec.clone()];
    let mut cur = spec.then.as_deref();
    while let Some(next) = cur {
        steps.push(next.clone());
        cur = next.then.as_deref();
    }
    execute_flow(&steps, base_url, vars).await
}

/// Execute a multi-step QA flow. Each step can interpolate variables saved by
/// earlier steps (`${accessToken}`), save values from body/headers, and assert
/// response status/body. This is the TestSprite-style "session" substrate:
/// OAuth/login, REST, and GraphQL all ride the same path.
pub async fn execute_flow(
    steps: &[EndpointSpec],
    base_url: &str,
    vars: &HashMap<String, String>,
) -> (bool, String, String) {
    let mut session = vars.clone();
    seed_runtime_vars(&mut session);
    let mut artifacts = Vec::new();
    for (i, step) in steps.iter().enumerate() {
        let (ok, err, artifact) = execute_one(step, base_url, &mut session, i + 1).await;
        artifacts.push(artifact);
        if !ok {
            return (
                false,
                if i == 0 {
                    err
                } else {
                    format!("step {} failed: {err}", i + 1)
                },
                artifact_json(&artifacts),
            );
        }
    }
    (true, String::new(), artifact_json(&artifacts))
}

/// Execute ONE request for a spec (ignoring `then`); returns
/// (passed, error, sanitized artifact).
async fn execute_one(
    spec: &EndpointSpec,
    base_url: &str,
    session: &mut HashMap<String, String>,
    step_no: usize,
) -> (bool, String, Value) {
    let effective = effective_spec(spec, session);
    let path = interpolate_str(&effective.path, session);
    let url = format!(
        "{}{}",
        base_url.trim_end_matches('/'),
        engine::concrete_path(&path, session)
    );
    let client = match reqwest::Client::builder()
        .redirect(if effective.follow_redirects == Some(false) {
            reqwest::redirect::Policy::none()
        } else {
            reqwest::redirect::Policy::limited(10)
        })
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return (
                false,
                format!("request client failed: {e}"),
                json!({"step": step_no, "error": "client build failed"}),
            );
        }
    };

    let mut req = match effective.method.as_str() {
        "GET" => client.get(&url),
        "DELETE" => client.delete(&url),
        m => {
            let builder = client.request(m.parse().unwrap_or(reqwest::Method::POST), &url);
            match (&effective.form, &effective.body) {
                (Some(f), _) => builder
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(urlencoded(&form_pairs(&interpolate_value(f, session)))),
                (None, Some(b)) => builder.json(&interpolate_value(b, session)),
                (None, None) => builder,
            }
        }
    };

    if let Some(tok) = auth_token(&effective, session)
        .or_else(|| session.get("authToken").cloned())
        .or_else(|| session.get("bearer").cloned())
    {
        req = req.bearer_auth(tok);
    }
    let mut artifact_headers = serde_json::Map::new();
    if let Some(headers) = effective.headers.as_ref().and_then(Value::as_object) {
        for (k, v) in headers {
            if let Some(vs) = interpolate_value(v, session).as_str() {
                req = req.header(k.as_str(), vs);
                artifact_headers.insert(k.clone(), json!(redact_header(k, vs)));
            }
        }
    }

    match req.timeout(std::time::Duration::from_secs(30)).send().await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let headers = resp.headers().clone();
            let body = resp.text().await.unwrap_or_default();
            let saved = save_from_response(&effective, &body, &headers, status, session);
            let (ok, err) = check_response(&effective, status, &body);
            let artifact = json!({
                "step": step_no,
                "id": effective.id,
                "request": {
                    "method": effective.method,
                    "url": sanitize_url(&url),
                    "headers": artifact_headers,
                    "body": redact_json(&effective.body),
                    "form": redact_json(&effective.form),
                    "graphql": effective.graphql.as_ref().map(|_| true).unwrap_or(false),
                },
                "response": {
                    "status": status,
                    "headers": sanitize_headers(&headers),
                    "bodySnippet": response_snippet(&body),
                },
                "saved": saved,
                "passed": ok,
                "error": err,
            });
            (ok, err, artifact)
        }
        Err(e) => (
            false,
            format!("request failed: {e}"),
            json!({"step": step_no, "request": {"method": effective.method, "url": sanitize_url(&url)}, "error": e.to_string()}),
        ),
    }
}

fn artifact_json(steps: &[Value]) -> String {
    serde_json::to_string_pretty(&json!({
        "kind": "testsprite-qa-artifact",
        "steps": steps,
    }))
    .unwrap_or_else(|_| "{\"kind\":\"testsprite-qa-artifact\"}".to_string())
}

/// Normalize shorthands (currently GraphQL) into the generic HTTP spec fields.
fn effective_spec(spec: &EndpointSpec, session: &HashMap<String, String>) -> EndpointSpec {
    let mut s = spec.clone();
    if let Some(g) = &spec.graphql {
        s.method = "POST".to_string();
        s.path = g.path.clone().unwrap_or_else(|| "/graphql".to_string());
        let mut body = serde_json::Map::new();
        body.insert(
            "query".to_string(),
            Value::String(interpolate_str(&g.query, session)),
        );
        if let Some(vars) = &g.variables {
            body.insert("variables".to_string(), interpolate_value(vars, session));
        }
        if let Some(op) = &g.operation_name {
            body.insert("operationName".to_string(), Value::String(op.clone()));
        }
        s.body = Some(Value::Object(body));
        s.expect_json = Some(true);
        if g.expect_no_errors.unwrap_or(true) {
            let mut expected = serde_json::Map::new();
            let data = g.expect_data.clone().unwrap_or_else(|| json!({}));
            expected.insert("data".to_string(), data);
            s.expect_body = Some(Value::Object(expected));
        } else if let Some(data) = &g.expect_data {
            s.expect_body = Some(json!({ "data": data }));
        }
    }
    s
}

fn seed_runtime_vars(session: &mut HashMap<String, String>) {
    session
        .entry("state".to_string())
        .or_insert_with(|| Uuid::new_v4().to_string());
    session
        .entry("pkceVerifier".to_string())
        .or_insert_with(|| format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple()));
    if !session.contains_key("pkceChallenge")
        && let Some(verifier) = session.get("pkceVerifier")
    {
        let digest = Sha256::digest(verifier.as_bytes());
        session.insert("pkceChallenge".to_string(), base64url(&digest));
    }
    seed_dynamic(session);
}

/// Seed the parallel-safe dynamic tokens and expand any stashed
/// `test_data_strategy` templates — the modality-agnostic half of runtime
/// seeding, shared by the backend flow, the browser executor, and the command
/// executor so every kind of test draws from the same per-case data vocabulary.
///
/// Fresh unique tokens (`${uuid}` etc.) mean every test — and every concurrent
/// run — gets a distinct account/record and cannot collide. Idempotent within
/// one map (`or_insert`), so a login step and a later step in the SAME case see
/// the SAME `${uuid}`, and a user-set value is never overwritten.
pub(crate) fn seed_dynamic(session: &mut HashMap<String, String>) {
    let tokens: HashMap<String, String> = dynamic_tokens().into_iter().collect();
    for (k, v) in &tokens {
        session.entry(k.clone()).or_insert_with(|| v.clone());
    }
    // Expand `__tsdata_tpl__<name>` (e.g. `testuser_{uuid}@x.com`) into the
    // concrete variable `<name>` using THIS case's fresh tokens.
    let templates: Vec<(String, String)> = session
        .iter()
        .filter_map(|(k, v)| {
            k.strip_prefix(TPL_PREFIX)
                .map(|name| (name.to_string(), v.clone()))
        })
        .collect();
    for (name, tpl) in templates {
        let expanded = expand_template(&tpl, &tokens);
        session.entry(name).or_insert(expanded);
    }
}

/// Variables-map key prefix under which a `test_data_strategy` template is
/// stashed so [`seed_runtime_vars`] can expand it fresh per test case. The PRD
/// ingester writes `__tsdata_tpl__email` = `testuser_{uuid}@x.com`.
pub(crate) const TPL_PREFIX: &str = "__tsdata_tpl__";

/// A resolved `test_data_strategy` entry: a fixed value, or a template with
/// `{uuid}`/`{ts}`/`{rand}` tokens to expand fresh per case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StrategyVar {
    Literal(String),
    Template(String),
}

/// Parse a PRD's `test_data_strategy` block into named variables. Keys ending
/// `_template` become [`StrategyVar::Template`] under the stripped name; other
/// string values become [`StrategyVar::Literal`]. Prose/config keys
/// (`CRITICAL`, `uuid_generation`, non-strings) are skipped. Each snake_case
/// name also gets a camelCase alias so `${app_url}` and `${appUrl}` both work.
pub(crate) fn strategy_vars(prd: &Value) -> Vec<(String, StrategyVar)> {
    let Some(obj) = prd.get("test_data_strategy").and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (k, v) in obj {
        let Some(s) = v.as_str() else { continue };
        if matches!(k.as_str(), "CRITICAL" | "uuid_generation") {
            continue;
        }
        let (name, var) = match k.strip_suffix("_template") {
            Some(base) => (base.to_string(), StrategyVar::Template(s.to_string())),
            None => (k.clone(), StrategyVar::Literal(s.to_string())),
        };
        let alias = snake_to_camel(&name);
        let alias_differs = alias != name;
        out.push((name, var.clone()));
        if alias_differs {
            out.push((alias, var));
        }
    }
    out
}

/// `app_url` -> `appUrl`. Unchanged when there are no underscores.
fn snake_to_camel(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut upper = false;
    for c in s.chars() {
        if c == '_' {
            upper = true;
        } else if upper {
            out.extend(c.to_uppercase());
            upper = false;
        } else {
            out.push(c);
        }
    }
    out
}

/// Expand ONLY the dynamic tokens `{uuid}`/`{uuid8}`/`{ts}`/`{rand}` in a
/// `test_data_strategy` template; every other `{...}` (e.g. a `{id}` path
/// param) is left verbatim so [`engine::concrete_path`] can still resolve it.
pub(crate) fn expand_template(template: &str, tokens: &HashMap<String, String>) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('}') {
            Some(end) => {
                let key = &after[..end];
                match tokens.get(key) {
                    Some(v) => out.push_str(v),
                    None => {
                        out.push('{');
                        out.push_str(key);
                        out.push('}');
                    }
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push_str(&rest[start..]);
                return out;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Fresh, unique substitution tokens for one test execution. Exposed to the
/// command / browser executors so every modality shares the same parallel-safe
/// data vocabulary. `uuid` (full v4), `uuid8` (short), `ts` (unix seconds),
/// `rand` (short alphanumeric).
pub(crate) fn dynamic_tokens() -> Vec<(String, String)> {
    let full = Uuid::new_v4();
    let short: String = full.simple().to_string().chars().take(8).collect();
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        .to_string();
    let rand: String = Uuid::new_v4()
        .simple()
        .to_string()
        .chars()
        .take(10)
        .collect();
    vec![
        ("uuid".to_string(), full.to_string()),
        ("uuid8".to_string(), short),
        ("ts".to_string(), ts),
        ("rand".to_string(), rand),
    ]
}

fn base64url(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    let mut i = 0;
    while i + 3 <= bytes.len() {
        let n = ((bytes[i] as u32) << 16) | ((bytes[i + 1] as u32) << 8) | bytes[i + 2] as u32;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(TABLE[((n >> 6) & 63) as usize] as char);
        out.push(TABLE[(n & 63) as usize] as char);
        i += 3;
    }
    match bytes.len() - i {
        1 => {
            let n = (bytes[i] as u32) << 16;
            out.push(TABLE[((n >> 18) & 63) as usize] as char);
            out.push(TABLE[((n >> 12) & 63) as usize] as char);
        }
        2 => {
            let n = ((bytes[i] as u32) << 16) | ((bytes[i + 1] as u32) << 8);
            out.push(TABLE[((n >> 18) & 63) as usize] as char);
            out.push(TABLE[((n >> 12) & 63) as usize] as char);
            out.push(TABLE[((n >> 6) & 63) as usize] as char);
        }
        _ => {}
    }
    out
}

fn interpolate_str(s: &str, session: &HashMap<String, String>) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let key = &after[..end];
        let val = session
            .get(key)
            .cloned()
            .or_else(|| std::env::var(key).ok())
            .unwrap_or_default();
        out.push_str(&val);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

fn interpolate_value(v: &Value, session: &HashMap<String, String>) -> Value {
    match v {
        Value::String(s) => Value::String(interpolate_str(s, session)),
        Value::Array(a) => Value::Array(a.iter().map(|v| interpolate_value(v, session)).collect()),
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, v)| (k.clone(), interpolate_value(v, session)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn form_pairs(v: &Value) -> Vec<(String, String)> {
    v.as_object()
        .map(|o| {
            o.iter()
                .map(|(k, v)| {
                    let s = v
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| v.to_string());
                    (k.clone(), s)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn urlencoded(pairs: &[(String, String)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char);
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn auth_token(spec: &EndpointSpec, session: &HashMap<String, String>) -> Option<String> {
    let auth = spec.auth.as_ref()?;
    match auth {
        Value::String(s) => session
            .get(s)
            .cloned()
            .or_else(|| Some(interpolate_str(s, session)))
            .filter(|s| !s.is_empty()),
        Value::Object(o) => o
            .get("bearer")
            .or_else(|| o.get("token"))
            .and_then(Value::as_str)
            .map(|s| interpolate_str(s, session))
            .filter(|s| !s.is_empty()),
        _ => None,
    }
}

fn save_from_response(
    spec: &EndpointSpec,
    body: &str,
    headers: &reqwest::header::HeaderMap,
    status: u16,
    session: &mut HashMap<String, String>,
) -> Vec<String> {
    let Some(save) = &spec.save else {
        return Vec::new();
    };
    let json_body = serde_json::from_str::<Value>(body).ok();
    let mut saved = Vec::new();
    for (key, selector) in save {
        if let Some(value) = extract_selector(selector, json_body.as_ref(), headers, status) {
            session.insert(key.clone(), value);
            saved.push(key.clone());
        }
    }
    saved
}

fn extract_selector(
    selector: &str,
    body: Option<&Value>,
    headers: &reqwest::header::HeaderMap,
    status: u16,
) -> Option<String> {
    if selector == "status" {
        return Some(status.to_string());
    }
    if let Some(path) = selector.strip_prefix("$.") {
        return json_path(body?, path).map(value_to_session_string);
    }
    if let Some(name) = selector.strip_prefix("header.") {
        if let Some((header, query_key)) = name.split_once(".query.") {
            let value = header_value(headers, header)?;
            return query_param(&value, query_key);
        }
        return header_value(headers, name);
    }
    None
}

fn json_path<'a>(mut v: &'a Value, path: &str) -> Option<&'a Value> {
    for part in path.split('.') {
        let (key, idx) = match part.split_once('[') {
            Some((k, rest)) => (k, rest.strip_suffix(']')?.parse::<usize>().ok()),
            None => (part, None),
        };
        if !key.is_empty() {
            v = v.get(key)?;
        }
        if let Some(i) = idx {
            v = v.as_array()?.get(i)?;
        }
    }
    Some(v)
}

fn value_to_session_string(v: &Value) -> String {
    v.as_str()
        .map(str::to_string)
        .unwrap_or_else(|| v.to_string())
}

fn header_value(headers: &reqwest::header::HeaderMap, name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(k, _)| k.as_str().eq_ignore_ascii_case(name))
        .and_then(|(_, v)| v.to_str().ok())
        .map(str::to_string)
}

fn query_param(url: &str, key: &str) -> Option<String> {
    let q = url
        .split_once('?')?
        .1
        .split_once('#')
        .map(|(q, _)| q)
        .unwrap_or_else(|| url.split_once('?').map(|(_, q)| q).unwrap_or(""));
    for pair in q.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if k == key {
            return Some(percent_decode(v));
        }
    }
    None
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(hex) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(hex);
            i += 3;
        } else if bytes[i] == b'+' {
            out.push(b' ');
            i += 1;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn sanitize_url(url: &str) -> String {
    match (url.split_once("://"), url.rsplit_once('@')) {
        (Some((scheme, _)), Some((_, host))) => format!("{scheme}://[REDACTED]@{host}"),
        _ => url.to_string(),
    }
}

fn sanitize_headers(headers: &reqwest::header::HeaderMap) -> Value {
    let mut out = serde_json::Map::new();
    for (k, v) in headers {
        if let Ok(vs) = v.to_str() {
            out.insert(k.as_str().to_string(), json!(redact_header(k.as_str(), vs)));
        }
    }
    Value::Object(out)
}

fn response_snippet(body: &str) -> String {
    let redacted = serde_json::from_str::<Value>(body)
        .map(|v| redact_value(&v).to_string())
        .unwrap_or_else(|_| body.to_string());
    redacted.chars().take(500).collect()
}

fn redact_header(k: &str, v: &str) -> String {
    if k.eq_ignore_ascii_case("authorization")
        || k.eq_ignore_ascii_case("cookie")
        || k.eq_ignore_ascii_case("set-cookie")
    {
        "[REDACTED]".to_string()
    } else {
        v.to_string()
    }
}

fn redact_json(v: &Option<Value>) -> Value {
    v.as_ref().map(redact_value).unwrap_or(Value::Null)
}

fn redact_value(v: &Value) -> Value {
    match v {
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, v)| {
                    let lk = k.to_ascii_lowercase();
                    let redacted = lk.contains("password")
                        || lk.contains("secret")
                        || lk.contains("token")
                        || lk.contains("authorization")
                        || lk.contains("cookie");
                    (
                        k.clone(),
                        if redacted {
                            Value::String("[REDACTED]".to_string())
                        } else {
                            redact_value(v)
                        },
                    )
                })
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(redact_value).collect()),
        other => other.clone(),
    }
}

/// Does this spec assert anything about the response body (so it must be read)?
fn response_body_asserted(spec: &EndpointSpec) -> bool {
    spec.expect_body.is_some() || spec.expect_json == Some(true) || spec.expect_parses.is_some()
}

/// Validate a response against a spec: the status band first, then — when the
/// spec asks — that the body parses as its declared format and deep-contains
/// `expect_body`. Pure (no I/O) so the whole verdict logic is unit-testable.
pub(crate) fn check_response(spec: &EndpointSpec, status: u16, body: &str) -> (bool, String) {
    if !spec.expect_status.accepts(status) {
        return (
            false,
            format!("expected {}, got {status}", spec.expect_status.describe()),
        );
    }
    if !response_body_asserted(spec) {
        return (true, String::new());
    }
    // "Emitted output must be valid <format>" — the round-trip / YAML-bug class.
    if let Some(fmt) = spec.expect_parses.as_deref()
        && let Err(msg) = parses_as(fmt, body)
    {
        return (false, format!("status {status} ok but {msg}"));
    }
    // JSON structural checks.
    if spec.expect_json == Some(true) || spec.expect_body.is_some() {
        let parsed: Value = match serde_json::from_str(body) {
            Ok(v) => v,
            Err(e) => {
                let preview: String = body.chars().take(120).collect();
                return (
                    false,
                    format!(
                        "status {status} ok but response body is not valid JSON ({e}): {preview}"
                    ),
                );
            }
        };
        if spec
            .graphql
            .as_ref()
            .and_then(|g| g.expect_no_errors)
            .unwrap_or(spec.graphql.is_some())
            && parsed
                .get("errors")
                .is_some_and(|e| !e.as_array().is_some_and(Vec::is_empty))
        {
            return (
                false,
                format!("status {status} ok but GraphQL errors were returned"),
            );
        }
        if let Some(expected) = &spec.expect_body
            && let Some(path) = json_mismatch(&parsed, expected, "$", spec.match_mode)
        {
            return (
                false,
                format!("status {status} ok but response body {path}"),
            );
        }
    }
    (true, String::new())
}

/// `Ok` if `body` parses as the named format (`json` | `yaml` | `toml`), else a
/// human-readable reason.
fn parses_as(fmt: &str, body: &str) -> Result<(), String> {
    let preview = || body.chars().take(120).collect::<String>();
    match fmt.to_ascii_lowercase().as_str() {
        "json" => serde_json::from_str::<Value>(body)
            .map(|_| ())
            .map_err(|e| format!("response body is not valid JSON ({e}): {}", preview())),
        "yaml" => serde_yaml::from_str::<serde_yaml::Value>(body)
            .map(|_| ())
            .map_err(|e| format!("response body is not valid YAML ({e}): {}", preview())),
        "toml" => toml::from_str::<toml::Value>(body)
            .map(|_| ())
            .map_err(|e| format!("response body is not valid TOML ({e}): {}", preview())),
        other => Err(format!(
            "unknown expect_parses format {other:?} (use json|yaml|toml)"
        )),
    }
}

/// `None` if `actual` deep-contains `expected`; otherwise the JSON path of the
/// first mismatch with a reason. Objects match as a subset (every expected key
/// must be present and match), arrays positionally (actual at least as long,
/// each expected element contained), scalars by equality.
fn json_mismatch(
    actual: &Value,
    expected: &Value,
    path: &str,
    mode: crate::server::engine::MatchMode,
) -> Option<String> {
    use crate::server::engine::{DEFAULT_FUZZY_THRESHOLD, MatchMode, fuzzy_str_match};
    match expected {
        Value::Object(exp) => {
            let Some(act) = actual.as_object() else {
                return Some(format!("at {path}: expected an object"));
            };
            for (k, ev) in exp {
                let child = format!("{path}.{k}");
                match act.get(k) {
                    Some(av) => {
                        if let Some(m) = json_mismatch(av, ev, &child, mode) {
                            return Some(m);
                        }
                    }
                    None => return Some(format!("missing {child}")),
                }
            }
            None
        }
        Value::Array(exp) => {
            let Some(act) = actual.as_array() else {
                return Some(format!("at {path}: expected an array"));
            };
            if act.len() < exp.len() {
                return Some(format!(
                    "at {path}: array has {} element(s), expected at least {}",
                    act.len(),
                    exp.len()
                ));
            }
            for (i, ev) in exp.iter().enumerate() {
                let child = format!("{path}[{i}]");
                if let Some(m) = json_mismatch(&act[i], ev, &child, mode) {
                    return Some(m);
                }
            }
            None
        }
        // String leaves may match fuzzily when the spec opts in; every other
        // scalar (number, bool, null) always compares exactly.
        Value::String(exp) if mode == MatchMode::Fuzzy => match actual.as_str() {
            Some(a) if fuzzy_str_match(exp, a, DEFAULT_FUZZY_THRESHOLD) => None,
            _ => Some(format!("at {path}: expected ~{expected}, got {actual}")),
        },
        _ => (actual != expected).then(|| format!("at {path}: expected {expected}, got {actual}")),
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
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    rfc3339_from_epoch(secs as i64)
}

/// Format Unix epoch `secs` (UTC) as RFC3339.
///
/// Hand-rolled to keep the original's dependency-free intent, but actually
/// parseable: the previous form was `1970-01-01T00:00:00Z+<secs>`, where a `Z`
/// — which already means +00:00 — is followed by a numeric offset. No ISO
/// parser accepts that, and read literally it claims every entity was created
/// in 1970.
fn rfc3339_from_epoch(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, mo, d) = civil_from_days(days);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Days since 1970-01-01 → (year, month, day), proleptic Gregorian.
/// Howard Hinnant's `civil_from_days`.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 }.div_euclid(146_097);
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (y + i64::from(m <= 2), m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(json: serde_json::Value) -> EndpointSpec {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn status_only_spec_ignores_the_body() {
        let s = spec(serde_json::json!({"method": "GET", "path": "/x", "expect_status": 200}));
        assert!(!response_body_asserted(&s));
        // A 200 passes regardless of body; a 404 fails on status alone.
        assert!(check_response(&s, 200, "anything, even not-json").0);
        assert!(!check_response(&s, 404, "").0);
    }

    #[test]
    fn expect_json_requires_a_parseable_body() {
        let s = spec(serde_json::json!({"method": "GET", "path": "/x", "expect_json": true}));
        // 200 with a valid JSON body passes…
        assert!(check_response(&s, 200, r#"{"ok":true}"#).0);
        // …but a 200 that returns an HTML error page fails (the classic
        // "green but the payload is broken").
        let (ok, err) = check_response(&s, 200, "<html>500 oops</html>");
        assert!(!ok);
        assert!(err.contains("not valid JSON"), "{err}");
        // Status is still checked first.
        assert!(!check_response(&s, 500, r#"{"ok":true}"#).0);
    }

    #[test]
    fn expect_body_deep_contains_the_response() {
        let s = spec(serde_json::json!({
            "method": "GET", "path": "/user",
            "expect_body": {"user": {"id": 7, "active": true}, "roles": ["admin"]}
        }));
        // Extra fields in the response are fine (subset match).
        assert!(
            check_response(
                &s,
                200,
                r#"{"user":{"id":7,"active":true,"name":"Ada"},"roles":["admin","ops"],"extra":1}"#,
            )
            .0
        );
        // A wrong nested value fails with a precise path.
        let (ok, err) = check_response(
            &s,
            200,
            r#"{"user":{"id":8,"active":true},"roles":["admin"]}"#,
        );
        assert!(!ok);
        assert!(err.contains("$.user.id"), "{err}");
        // A missing field fails.
        let (ok, err) = check_response(&s, 200, r#"{"user":{"id":7},"roles":["admin"]}"#);
        assert!(!ok);
        assert!(err.contains("missing $.user.active"), "{err}");
    }

    #[test]
    fn expect_parses_validates_the_body_format() {
        // The YAML-frontmatter bug class: emitted output that must be valid YAML.
        let y = spec(serde_json::json!({"method": "GET", "path": "/cfg", "expect_parses": "yaml"}));
        assert!(check_response(&y, 200, "name: ok\nlist:\n  - a\n").0);
        // `name: ok: broken` is the exact unquoted-colon YAML defect.
        let (ok, err) = check_response(&y, 200, "name: ok: broken");
        assert!(!ok);
        assert!(err.contains("not valid YAML"), "{err}");

        let j = spec(serde_json::json!({"method": "GET", "path": "/x", "expect_parses": "json"}));
        assert!(check_response(&j, 200, r#"{"a":1}"#).0);
        assert!(!check_response(&j, 200, "<html>").0);

        // An unknown format is reported, never silently passed.
        let u = spec(serde_json::json!({"method": "GET", "path": "/x", "expect_parses": "xml"}));
        assert!(!check_response(&u, 200, "<x/>").0);
    }

    #[test]
    fn interpolation_recurses_through_json_and_env() {
        let _guard = crate::testutil::env_guard(&[("FROM_ENV", Some("env-value"))]);
        let mut session = HashMap::new();
        session.insert("token".to_string(), "abc123".to_string());
        let value = interpolate_value(
            &json!({
                "auth": "Bearer ${token}",
                "nested": ["${FROM_ENV}", {"missing": "x${NOPE}y"}],
                "n": 3
            }),
            &session,
        );
        assert_eq!(value["auth"], "Bearer abc123");
        assert_eq!(value["nested"][0], "env-value");
        assert_eq!(value["nested"][1]["missing"], "xy");
        assert_eq!(value["n"], 3);
    }

    #[test]
    fn dynamic_tokens_are_unique_per_call_and_wellformed() {
        let a: HashMap<String, String> = dynamic_tokens().into_iter().collect();
        let b: HashMap<String, String> = dynamic_tokens().into_iter().collect();
        for k in ["uuid", "uuid8", "ts", "rand"] {
            assert!(a.contains_key(k), "missing {k}");
        }
        // Two calls yield distinct uuids (parallel-safety guarantee).
        assert_ne!(a["uuid"], b["uuid"], "each test must get a fresh uuid");
        assert_ne!(a["uuid8"], b["uuid8"]);
        assert_eq!(a["uuid8"].len(), 8);
        assert!(a["ts"].chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn expand_template_expands_only_dynamic_tokens_leaving_path_params() {
        let mut tokens = HashMap::new();
        tokens.insert("uuid".to_string(), "U1".to_string());
        tokens.insert("ts".to_string(), "T1".to_string());
        // uuid/ts expand; the {id} path-param is left for concrete_path.
        assert_eq!(
            expand_template("user_{uuid}@x.com/{id}?t={ts}", &tokens),
            "user_U1@x.com/{id}?t=T1"
        );
        // An unterminated brace is passed through unharmed.
        assert_eq!(expand_template("a{uuid", &tokens), "a{uuid");
    }

    #[test]
    fn strategy_vars_splits_templates_from_literals_with_camel_aliases() {
        let prd = json!({
            "test_data_strategy": {
                "CRITICAL": "Tests run in PARALLEL. You MUST use UUID",
                "uuid_generation": "fresh per test",
                "email_template": "testuser_{uuid}@x.com",
                "app_url_template": "https://app-{uuid}.example.com",
                "password": "Test@1234",
            }
        });
        let vars: HashMap<String, StrategyVar> = strategy_vars(&prd).into_iter().collect();
        // Prose/config keys are skipped.
        assert!(!vars.contains_key("CRITICAL"));
        assert!(!vars.contains_key("uuid_generation"));
        // Templates keep their {uuid}; literals are literal.
        assert_eq!(
            vars["email"],
            StrategyVar::Template("testuser_{uuid}@x.com".to_string())
        );
        assert_eq!(
            vars["password"],
            StrategyVar::Literal("Test@1234".to_string())
        );
        // snake → camel alias present.
        assert_eq!(vars["appUrl"], vars["app_url"]);
        // No strategy block → empty, no panic.
        assert!(strategy_vars(&json!({})).is_empty());
    }

    #[test]
    fn stashed_strategy_template_expands_fresh_per_case_via_seeding() {
        // The run injects a template stash; each case's seeding expands it to a
        // unique concrete value that ${email} then resolves to.
        let mut base = HashMap::new();
        base.insert(
            format!("{TPL_PREFIX}email"),
            "testuser_{uuid}@x.com".to_string(),
        );
        let mut case_a = base.clone();
        seed_runtime_vars(&mut case_a);
        let mut case_b = base.clone();
        seed_runtime_vars(&mut case_b);
        let email_a = interpolate_str("${email}", &case_a);
        let email_b = interpolate_str("${email}", &case_b);
        assert!(email_a.starts_with("testuser_") && email_a.ends_with("@x.com"));
        assert_ne!(email_a, email_b, "each case gets a distinct address");
    }

    #[test]
    fn seeded_dynamic_tokens_interpolate_into_a_spec() {
        // A spec body written with ${uuid} resolves to the per-flow seeded
        // value, and the SAME value across two references in one flow.
        let mut session = HashMap::new();
        seed_runtime_vars(&mut session);
        let out = interpolate_value(
            &json!({"email": "user_${uuid}@x.com", "again": "${uuid}"}),
            &session,
        );
        let email = out["email"].as_str().unwrap();
        let again = out["again"].as_str().unwrap();
        assert!(email.starts_with("user_") && email.ends_with("@x.com"));
        assert!(
            email.contains(again),
            "same uuid within one flow: {email} vs {again}"
        );
        // A caller-supplied ${uuid} (static var) is NOT overwritten by seeding.
        let mut fixed = HashMap::new();
        fixed.insert("uuid".to_string(), "FIXED".to_string());
        seed_runtime_vars(&mut fixed);
        assert_eq!(fixed["uuid"], "FIXED");
    }

    #[test]
    fn form_pairs_urlencode_and_query_param_decode() {
        let form = json!({"grant_type":"authorization code", "n": 7, "weird":"a&b"});
        let pairs = form_pairs(&form);
        let encoded = urlencoded(&pairs);
        assert!(
            encoded.contains("grant_type=authorization+code"),
            "{encoded}"
        );
        assert!(encoded.contains("n=7"), "{encoded}");
        assert!(encoded.contains("weird=a%26b"), "{encoded}");

        assert_eq!(
            query_param("https://app/cb?code=a%2Bb+c&state=ok#frag", "code"),
            Some("a+b c".to_string())
        );
        assert_eq!(
            query_param("https://app/cb?empty", "empty"),
            Some(String::new())
        );
        assert_eq!(query_param("https://app/cb", "code"), None);
    }

    #[test]
    fn header_lookup_is_case_insensitive_and_redacts_sensitive_values() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("Location", "/cb?code=abc".parse().unwrap());
        headers.insert("Authorization", "Bearer secret".parse().unwrap());
        assert_eq!(
            header_value(&headers, "location"),
            Some("/cb?code=abc".to_string())
        );
        assert_eq!(
            query_param(&header_value(&headers, "LOCATION").unwrap(), "code"),
            Some("abc".to_string())
        );
        let sanitized = sanitize_headers(&headers);
        assert_eq!(sanitized["authorization"], "[REDACTED]");
        assert_eq!(sanitized["location"], "/cb?code=abc");
    }

    #[test]
    fn graphql_shorthand_fails_when_errors_are_present() {
        let s = spec(serde_json::json!({
            "graphql": {
                "query": "{ me { id } }",
                "expect_no_errors": true,
                "expect_data": {"me": {}}
            }
        }));
        let effective = effective_spec(&s, &HashMap::new());
        assert_eq!(effective.method, "POST");
        assert_eq!(effective.path, "/graphql");
        assert!(effective.expect_json.unwrap());

        let (ok, err) = check_response(
            &effective,
            200,
            r#"{"data":{"me":null},"errors":[{"message":"nope"}]}"#,
        );
        assert!(!ok);
        assert!(err.contains("GraphQL errors"), "{err}");
    }

    #[tokio::test]
    async fn flow_saves_token_interpolates_bearer_and_redacts_artifact() {
        use axum::http::{HeaderMap, StatusCode};
        use axum::routing::{get, post};
        use axum::{Json, Router};

        async fn token() -> Json<Value> {
            Json(json!({"access_token":"secret-token"}))
        }
        async fn me(headers: HeaderMap) -> (StatusCode, Json<Value>) {
            match headers.get("authorization").and_then(|h| h.to_str().ok()) {
                Some("Bearer secret-token") => (StatusCode::OK, Json(json!({"ok": true}))),
                _ => (StatusCode::UNAUTHORIZED, Json(json!({"ok": false}))),
            }
        }

        let app = Router::new()
            .route("/token", post(token))
            .route("/me", get(me));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let steps: Vec<EndpointSpec> = serde_json::from_value(json!([
            {
                "id": "login",
                "method": "POST",
                "path": "/token",
                "expect_body": {"access_token": "secret-token"},
                "save": {"accessToken": "$.access_token"}
            },
            {
                "id": "me",
                "method": "GET",
                "path": "/me",
                "auth": {"bearer": "${accessToken}"},
                "expect_body": {"ok": true}
            }
        ]))
        .unwrap();

        let (ok, err, artifact) =
            execute_flow(&steps, &format!("http://{addr}"), &HashMap::new()).await;
        assert!(ok, "{err}\n{artifact}");
        let parsed: Value = serde_json::from_str(&artifact).unwrap();
        assert_eq!(parsed["kind"], "testsprite-qa-artifact");
        assert_eq!(parsed["steps"][0]["saved"][0], "accessToken");
        assert!(!artifact.contains("secret-token"), "{artifact}");
        assert!(artifact.contains("[REDACTED]"), "{artifact}");
    }

    #[test]
    fn json_mismatch_reports_array_and_type_problems() {
        use crate::server::engine::MatchMode;
        // Array shorter than expected.
        let short = json_mismatch(
            &serde_json::json!({"xs": [1]}),
            &serde_json::json!({"xs": [1, 2]}),
            "$",
            MatchMode::Exact,
        );
        assert!(short.unwrap().contains("$.xs"));
        // Expected object, got scalar.
        let wrong_type = json_mismatch(
            &serde_json::json!({"u": 5}),
            &serde_json::json!({"u": {"id": 1}}),
            "$",
            MatchMode::Exact,
        );
        assert!(wrong_type.unwrap().contains("expected an object"));
        // Full deep-contains success returns None.
        assert!(
            json_mismatch(
                &serde_json::json!({"a": 1, "b": [1, 2, 3]}),
                &serde_json::json!({"a": 1, "b": [1, 2]}),
                "$",
                MatchMode::Exact,
            )
            .is_none()
        );
    }

    #[test]
    fn fuzzy_match_mode_tolerates_cosmetic_string_leaves_but_not_wrong_values() {
        use crate::server::engine::MatchMode;
        let actual = serde_json::json!({"message": "Welcome  Back", "n": 3});
        // Exact mode: a whitespace/case difference is a mismatch.
        let exact = json_mismatch(
            &actual,
            &serde_json::json!({"message": "welcome back"}),
            "$",
            MatchMode::Exact,
        );
        assert!(exact.is_some());
        // Fuzzy mode: the cosmetic difference is tolerated on the string leaf.
        let fuzzy = json_mismatch(
            &actual,
            &serde_json::json!({"message": "welcome back"}),
            "$",
            MatchMode::Fuzzy,
        );
        assert!(fuzzy.is_none(), "{fuzzy:?}");
        // But a genuinely wrong value still fails, even in fuzzy mode.
        let wrong = json_mismatch(
            &actual,
            &serde_json::json!({"message": "goodbye forever"}),
            "$",
            MatchMode::Fuzzy,
        );
        assert!(wrong.is_some());
        // And a non-string leaf is never fuzzed.
        let num = json_mismatch(&actual, &serde_json::json!({"n": 4}), "$", MatchMode::Fuzzy);
        assert!(num.is_some());
    }

    #[test]
    fn timestamps_are_real_rfc3339() {
        // Regression: every entity carried `1970-01-01T00:00:00Z+<secs>` — a Z
        // followed by an offset, which no ISO parser accepts, claiming 1970.
        assert_eq!(rfc3339_from_epoch(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_from_epoch(1), "1970-01-01T00:00:01Z");
        assert_eq!(rfc3339_from_epoch(1_000_000_000), "2001-09-09T01:46:40Z");
        assert_eq!(rfc3339_from_epoch(1_700_000_000), "2023-11-14T22:13:20Z");
        // Leap-year boundaries.
        assert_eq!(rfc3339_from_epoch(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339_from_epoch(1_709_164_800), "2024-02-29T00:00:00Z");
    }

    #[test]
    fn now_iso_is_parseable_and_not_in_1970() {
        let now = now_iso();
        assert!(now.ends_with('Z'), "{now}");
        assert!(!now.contains("Z+"), "{now}");
        assert_eq!(now.len(), 20, "{now}");
        let year: i32 = now[..4].parse().unwrap();
        assert!(year >= 2024, "{now}");
    }
}
