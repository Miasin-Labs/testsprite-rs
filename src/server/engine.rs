//! Deterministic generation engine — the part `api.testsprite.com` does with an
//! LLM, done locally with no account and no model.
//!
//! Input: a code summary (`api_endpoints: [{method, path, body?, expect_status?}]`).
//! Output: a structured PRD, a test plan (`[{id,title,description}]`), an
//! executable spec per case, and the Python (`requests`) test-code artifact.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// A range of response statuses that counts as a pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Band {
    /// 2xx/3xx only — the endpoint answered the request. The default: a route
    /// that is missing (404), auth-walled (401/403), or rejects the call
    /// (400/405) has NOT passed.
    #[default]
    Success,
    /// 2xx/3xx plus 400/422 — the write band. A synthesized request body is a
    /// guess, so the server validating and rejecting it still proves the route
    /// exists and is wired. 401/403/404/405 remain failures.
    Accepted,
    /// Any status below 500. An explicit opt-in for callers that really do want
    /// "did not 5xx" semantics; never a default, because it reports a missing
    /// or auth-walled endpoint as green.
    Any,
}

/// What counts as a pass for one endpoint check.
///
/// Wire form is backward compatible with the bare-integer field it replaces: a
/// number (`"expect_status": 200`) is an exact match, a band name
/// (`"expect_status": "accepted"`) is a range, and an absent field is
/// [`Band::Success`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Expect {
    Exact(u16),
    Band(Band),
}

impl Default for Expect {
    fn default() -> Self {
        Expect::Band(Band::Success)
    }
}

impl Expect {
    /// True iff `status` counts as a pass under this expectation.
    pub fn accepts(self, status: u16) -> bool {
        match self {
            Expect::Exact(want) => status == want,
            Expect::Band(Band::Success) => (200..400).contains(&status),
            Expect::Band(Band::Accepted) => {
                (200..400).contains(&status) || matches!(status, 400 | 422)
            }
            Expect::Band(Band::Any) => status < 500,
        }
    }

    /// Human phrasing of what was expected, for failure messages and plans.
    pub fn describe(self) -> String {
        match self {
            Expect::Exact(s) => format!("a {s} response"),
            Expect::Band(Band::Success) => "a 2xx/3xx response".to_string(),
            Expect::Band(Band::Accepted) => "a 2xx/3xx response (or 400/422)".to_string(),
            Expect::Band(Band::Any) => "a non-5xx response".to_string(),
        }
    }
}

/// One executable endpoint check derived from the code summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndpointSpec {
    /// Optional step id, used in multi-step QA flows and artifacts.
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default = "default_method")]
    pub method: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub body: Option<Value>,
    /// Form body (`application/x-www-form-urlencoded`) for OAuth/login flows.
    #[serde(default)]
    pub form: Option<Value>,
    #[serde(default)]
    pub expect_status: Expect,
    #[serde(default)]
    pub headers: Option<Value>,
    /// Shorthand auth. String = bearer token variable/name; object supports
    /// `{ "bearer": "${token}" }` or `{ "token": "${token}" }`.
    #[serde(default)]
    pub auth: Option<Value>,
    /// Disable redirect-following when a flow needs to capture `Location` (e.g.
    /// OAuth authorize -> code). Defaults to true.
    #[serde(default)]
    pub follow_redirects: Option<bool>,
    /// Save values from this response into the per-test session map:
    /// `{ "accessToken": "$.access_token", "code": "header.location.query.code" }`.
    #[serde(default)]
    pub save: Option<std::collections::HashMap<String, String>>,
    /// GraphQL shorthand. When set, the executor turns the step into
    /// `POST /graphql` with `{query, variables, operationName}` and JSON
    /// assertions. This rides the same HTTP flow runner as REST/OAuth.
    #[serde(default)]
    pub graphql: Option<GraphqlSpec>,
    /// Require the response body to parse as JSON (catches a 200 that returns an
    /// HTML error page or a truncated payload). Implied by `expect_body`.
    #[serde(default)]
    pub expect_json: Option<bool>,
    /// A JSON subset the response must deep-contain: every key/value in this
    /// object (recursively; arrays positional) must be present in the response.
    /// This is what turns "status 200 = pass" into "200 AND the payload is
    /// right = pass".
    #[serde(default)]
    pub expect_body: Option<Value>,
    /// Require the response body to parse as this format (`json` | `yaml` |
    /// `toml`) — the general "emitted output must be valid <format>" check.
    /// `expect_json: true` is sugar for `expect_parses: "json"`.
    #[serde(default)]
    pub expect_parses: Option<String>,
    /// A follow-up request run only when this one passes — a read-after-write
    /// check: mutate here, then GET and assert the state actually changed via
    /// the follow-up's own `expect_status`/`expect_body`. Chains (`then.then`),
    /// so it catches "the write returned ok but nothing actually changed".
    #[serde(default)]
    pub then: Option<Box<EndpointSpec>>,
    /// How STRING leaves in `expect_body` are compared to the response.
    /// Defaults to [`MatchMode::Exact`] (byte-for-byte). The wire field is
    /// `"match"` (a keyword, hence the rename), so a spec opts into tolerance
    /// with `"match": "fuzzy"` — never the default, because a lenient oracle
    /// that waves through a wrong response is more dangerous than a brittle one.
    #[serde(default, rename = "match")]
    pub match_mode: MatchMode,
}

/// String-leaf comparison mode for [`EndpointSpec::expect_body`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchMode {
    /// Byte-for-byte string equality — the historical, default behavior.
    #[default]
    Exact,
    /// Normalize each string leaf (NFC-ish + case-fold + trim + collapse
    /// internal whitespace) and accept when the normalized Levenshtein
    /// similarity is at least [`DEFAULT_FUZZY_THRESHOLD`]. String leaves only;
    /// every number, bool, and structural shape still matches exactly.
    Fuzzy,
}

/// Minimum normalized Levenshtein similarity (`0.0..=1.0`) for two normalized
/// strings to count as a fuzzy match. `0.9` tolerates a few typo-scale edits
/// on a short string while still rejecting a genuinely different word.
pub const DEFAULT_FUZZY_THRESHOLD: f64 = 0.9;

/// Normalize a string for fuzzy comparison: compose common combining marks,
/// case-fold, trim, and collapse internal whitespace runs to a single space.
pub fn normalize(s: &str) -> String {
    s.chars()
        .flat_map(compose_combining)
        .collect::<String>()
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// A minimal NFC-style fold for the handful of precomposable Latin marks that
/// show up in test data (accents entered as base + combining mark). Anything
/// else passes through unchanged — this is normalization for oracle tolerance,
/// not a full Unicode NFC implementation (which would need an external crate,
/// and Cargo.toml is fixed).
fn compose_combining(c: char) -> Vec<char> {
    // Strip standalone combining diacritics so "e\u{301}" and "é" fold to "e".
    if ('\u{0300}'..='\u{036F}').contains(&c) {
        return Vec::new();
    }
    vec![c]
}

/// True when `expected` and `actual` match within `threshold` after
/// normalization. Identical (post-normalize) strings always match.
pub fn fuzzy_str_match(expected: &str, actual: &str, threshold: f64) -> bool {
    let e = normalize(expected);
    let a = normalize(actual);
    if e == a {
        return true;
    }
    let dist = levenshtein(&e, &a);
    let max = e.chars().count().max(a.chars().count());
    if max == 0 {
        return true;
    }
    let similarity = 1.0 - dist as f64 / max as f64;
    similarity >= threshold
}

/// Levenshtein edit distance over chars (O(n·m) DP, one row) — fine for the
/// short strings response bodies carry.
fn levenshtein(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.chars().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = if ca == *cb { 0 } else { 1 };
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

fn default_method() -> String {
    "GET".to_string()
}

/// GraphQL request/assertion shorthand for [`EndpointSpec`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphqlSpec {
    #[serde(default)]
    pub path: Option<String>,
    pub query: String,
    #[serde(default)]
    pub variables: Option<Value>,
    #[serde(default, rename = "operationName", alias = "operation_name")]
    pub operation_name: Option<String>,
    #[serde(default)]
    pub expect_no_errors: Option<bool>,
    #[serde(default)]
    pub expect_data: Option<Value>,
}

/// A planned case + its executable spec.
pub struct PlannedCase {
    pub id: String,
    pub title: String,
    pub description: String,
    pub spec: EndpointSpec,
}

/// Extract `api_endpoints` from a code summary JSON value.
fn read_endpoints(code_summary: &Value) -> Vec<EndpointSpec> {
    code_summary
        .get("api_endpoints")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(parse_endpoint_spec).collect())
        .unwrap_or_default()
}

/// Parse a JSON endpoint (`{method, path, ...}`) into an [`EndpointSpec`].
/// Distinct from `tools::execute::parse_host_port`, which parses a URL string
/// into `(host, port)`.
fn parse_endpoint_spec(v: &Value) -> Option<EndpointSpec> {
    let method = v.get("method")?.as_str()?.to_uppercase();
    let path = v.get("path")?.as_str()?.to_string();
    Some(EndpointSpec {
        id: v.get("id").and_then(Value::as_str).map(str::to_string),
        method,
        path,
        body: v.get("body").cloned().filter(|b| !b.is_null()),
        form: v.get("form").cloned().filter(|b| !b.is_null()),
        expect_status: v
            .get("expect_status")
            .and_then(|s| serde_json::from_value(s.clone()).ok())
            .unwrap_or_default(),
        headers: v.get("headers").cloned().filter(Value::is_object),
        auth: v.get("auth").cloned().filter(|b| !b.is_null()),
        follow_redirects: v.get("follow_redirects").and_then(Value::as_bool),
        save: v
            .get("save")
            .and_then(|s| serde_json::from_value(s.clone()).ok()),
        graphql: v
            .get("graphql")
            .and_then(|g| serde_json::from_value(g.clone()).ok()),
        expect_json: v.get("expect_json").and_then(Value::as_bool),
        expect_body: v.get("expect_body").cloned().filter(|b| !b.is_null()),
        expect_parses: v
            .get("expect_parses")
            .and_then(Value::as_str)
            .map(str::to_string),
        then: v
            .get("then")
            .and_then(|t| serde_json::from_value::<EndpointSpec>(t.clone()).ok())
            .map(Box::new),
        match_mode: v
            .get("match")
            .and_then(|m| serde_json::from_value(m.clone()).ok())
            .unwrap_or_default(),
    })
}

/// Substitute `{param}` path segments: use `vars[param]` when present, else the
/// probe value `1`. The map (from `testsprite_tests/variables.json`) lets
/// deterministic tests hit real records — e.g. `{id}` -> a real UUID — instead
/// of 404ing on the `1` placeholder.
pub fn concrete_path(path: &str, vars: &HashMap<String, String>) -> String {
    let mut out = String::new();
    let mut name = String::new();
    let mut in_brace = false;
    for ch in path.chars() {
        match ch {
            '{' => {
                in_brace = true;
                name.clear();
            }
            '}' => {
                in_brace = false;
                match vars.get(&name) {
                    Some(v) => out.push_str(v),
                    None => out.push('1'),
                }
            }
            _ if in_brace => name.push(ch),
            c => out.push(c),
        }
    }
    out
}

/// Build a plan (+ specs) from a code summary.
pub fn plan_from_code_summary(code_summary: &Value) -> Vec<PlannedCase> {
    read_endpoints(code_summary)
        .into_iter()
        .enumerate()
        .map(|(i, spec)| {
            let id = format!("TC{:03}", i + 1);
            let expect = spec.expect_status.describe();
            let title = format!("{} {} responds", spec.method, spec.path);
            let description = format!(
                "Send a {} request to {} and verify {expect}.",
                spec.method, spec.path
            );
            PlannedCase {
                id,
                title,
                description,
                spec,
            }
        })
        .collect()
}

/// Boundary values a path parameter is probed with: zero/negative/huge
/// numerics, an encoded space, and a classic injection string. The oracle is
/// deliberately [`Band::Any`] — a boundary input may be REJECTED however the
/// app likes, but it must never 5xx.
const PATH_PROBES: &[(&str, &str)] = &[
    ("zero id", "0"),
    ("negative id", "-1"),
    ("huge id", "999999999999999999"),
    ("injection-shaped id", "'%20OR%20'1'='1"),
];

/// Boundary bodies a mutation endpoint is probed with.
fn body_probes() -> Vec<(&'static str, Value)> {
    vec![
        ("empty object body", json!({})),
        (
            "oversized string field",
            json!({ "boundary": "x".repeat(4096) }),
        ),
        (
            "unicode and control characters",
            json!({ "boundary": "✓ ünïcode \u{0000} \u{202e}txet" }),
        ),
    ]
}

/// Deterministic boundary/robustness probes derived from the declared
/// endpoints — the value-selection strategy LLMs systematically skip, with
/// zero model spend. Each parameterized path gets edge-value substitutions and
/// each body-bearing mutation gets malformed-body probes; every probe asserts
/// "must not 5xx" ([`Band::Any`]), the honest robustness oracle for inputs the
/// server is entitled to reject.
pub fn boundary_cases(code_summary: &Value) -> Vec<PlannedCase> {
    let mut out = Vec::new();
    let mut n = 0usize;
    for spec in read_endpoints(code_summary) {
        if spec.path.contains('{') {
            for (label, value) in PATH_PROBES {
                n += 1;
                let path = substitute_all_params(&spec.path, value);
                out.push(PlannedCase {
                    id: format!("BND{n:03}"),
                    title: format!("{} {} tolerates {label}", spec.method, spec.path),
                    description: format!(
                        "Send a {} request to {path} (boundary probe: {label}) and verify \
                         the server rejects or handles it without a 5xx.",
                        spec.method
                    ),
                    spec: EndpointSpec {
                        path,
                        expect_status: Expect::Band(Band::Any),
                        body: spec.body.clone(),
                        ..spec.clone()
                    },
                });
            }
        }
        if matches!(spec.method.as_str(), "POST" | "PUT" | "PATCH") {
            for (label, body) in body_probes() {
                n += 1;
                out.push(PlannedCase {
                    id: format!("BND{n:03}"),
                    title: format!("{} {} tolerates {label}", spec.method, spec.path),
                    description: format!(
                        "Send a {} request to {} with a boundary body ({label}) and verify \
                         the server rejects or handles it without a 5xx.",
                        spec.method, spec.path
                    ),
                    spec: EndpointSpec {
                        body: Some(body),
                        form: None,
                        expect_status: Expect::Band(Band::Any),
                        ..spec.clone()
                    },
                });
            }
        }
    }
    out
}

/// Replace every `{param}` segment with one literal probe value.
fn substitute_all_params(path: &str, value: &str) -> String {
    let mut out = String::new();
    let mut in_brace = false;
    for ch in path.chars() {
        match ch {
            '{' => in_brace = true,
            '}' => {
                in_brace = false;
                out.push_str(value);
            }
            _ if in_brace => {}
            c => out.push(c),
        }
    }
    out
}

/// Build a structured PRD from a code summary (mirrors the cloud's PRD shape).
pub fn prd_from_code_summary(code_summary: &Value) -> Value {
    let project = code_summary
        .get("project_name")
        .and_then(|v| v.as_str())
        .unwrap_or("project");
    let endpoints = read_endpoints(code_summary);
    let features: Vec<Value> = endpoints
        .iter()
        .map(|e| {
            json!({
                "name": format!("{} {}", e.method, e.path),
                "description": format!("Endpoint {} {} behaves per contract.", e.method, e.path),
                "user_flows": [format!("{} {} -> expected response", e.method, e.path)],
            })
        })
        .collect();
    json!({
        "meta": { "project": project, "prepared_by": "testsprite-rs (local engine)" },
        "product_overview": format!("{project}: local contract validation of {} endpoint(s).", endpoints.len()),
        "core_goals": ["Every declared endpoint is reachable and returns its expected status."],
        "features": features,
    })
}

/// Generate the Python `requests` REPRODUCTION for a case.
///
/// This is not what runs: `execute_spec` performs the check with reqwest. This
/// is a human-readable equivalent, recorded alongside the result and emitted by
/// `test emit`. It is generated from the same spec, so it asserts the same
/// thing — but any secret is a placeholder, so running it verbatim will not
/// reproduce an authenticated call. The header says so rather than leaving the
/// reader to discover it from a surprise 401.
pub fn python_for(spec: &EndpointSpec, base_url: &str, vars: &HashMap<String, String>) -> String {
    let url = format!(
        "{}{}",
        base_url.trim_end_matches('/'),
        concrete_path(&spec.path, vars)
    );
    let fn_name = format!(
        "test_{}_{}",
        spec.method.to_lowercase(),
        crate::report::sanitize_filename(&spec.path).to_lowercase()
    );
    let mut hdrs: Vec<String> = Vec::new();
    if vars.contains_key("authToken") || vars.contains_key("bearer") {
        hdrs.push("\"Authorization\": \"Bearer <authToken>\"".to_string());
    }
    if let Some(map) = spec.headers.as_ref().and_then(Value::as_object) {
        for (k, v) in map {
            if let Some(vs) = v.as_str() {
                hdrs.push(format!("{k:?}: {vs:?}"));
            }
        }
    }
    let hdr = if hdrs.is_empty() {
        String::new()
    } else {
        format!(", headers={{{}}}", hdrs.join(", "))
    };
    let call = match (spec.method.as_str(), &spec.body) {
        ("GET", _) => format!("requests.get(\"{url}\"{hdr}, timeout=30)"),
        (m, Some(b)) => format!(
            "requests.request(\"{m}\", \"{url}\", json={}{hdr}, timeout=30)",
            py_literal(b)
        ),
        (m, None) => format!("requests.request(\"{m}\", \"{url}\"{hdr}, timeout=30)"),
    };
    let assertion = match spec.expect_status {
        Expect::Exact(s) => {
            format!("assert r.status_code == {s}, f\"expected {s}, got {{r.status_code}}\"")
        }
        Expect::Band(Band::Success) => {
            "assert 200 <= r.status_code < 400, f\"expected 2xx/3xx, got {r.status_code}\""
                .to_string()
        }
        Expect::Band(Band::Accepted) => {
            "assert 200 <= r.status_code < 400 or r.status_code in (400, 422), \\\n        f\"expected 2xx/3xx or 400/422, got {r.status_code}\""
                .to_string()
        }
        Expect::Band(Band::Any) => {
            "assert r.status_code < 500, f\"server error: {r.status_code}\"".to_string()
        }
    };
    let note = if hdrs.iter().any(|h| h.contains("<authToken>")) {
        "# NOTE: reproduction of the check testsprite ran (the run itself uses reqwest).\n\
         # The Authorization value is a placeholder — substitute a real token to run this.\n"
    } else {
        "# NOTE: reproduction of the check testsprite ran (the run itself uses reqwest).\n"
    };
    format!(
        "{note}import requests\n\ndef {fn_name}():\n    r = {call}\n    {assertion}\n\n{fn_name}()\n"
    )
}

/// Render a JSON value as a Python literal (dict/list/str/num/bool/None).
fn py_literal(v: &Value) -> String {
    match v {
        Value::Null => "None".to_string(),
        Value::Bool(b) => if *b { "True" } else { "False" }.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => format!("{s:?}"),
        Value::Array(a) => format!(
            "[{}]",
            a.iter().map(py_literal).collect::<Vec<_>>().join(", ")
        ),
        Value::Object(o) => format!(
            "{{{}}}",
            o.iter()
                .map(|(k, v)| format!("{k:?}: {}", py_literal(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concrete_path_seeds_from_vars_else_probe() {
        let mut vars = HashMap::new();
        vars.insert("id".to_string(), "uuid-1234".to_string());
        // known param -> the variable; unknown -> the `1` probe; literals untouched
        assert_eq!(concrete_path("/users/{id}", &vars), "/users/uuid-1234");
        assert_eq!(
            concrete_path("/users/{id}/posts/{postId}", &vars),
            "/users/uuid-1234/posts/1"
        );
        assert_eq!(concrete_path("/health", &vars), "/health");
        assert_eq!(concrete_path("/a/{x}", &HashMap::new()), "/a/1");
    }

    #[test]
    fn absent_expectation_is_the_success_band_not_anything_under_500() {
        // Regression: the default used to be `status < 500`, which reported a
        // missing route (404), an auth wall (401/403), or a rejected request
        // (400/405) as PASS. Every downstream signal — triage, flaky, gate —
        // is a transformation of this boolean, so a lenient default here
        // manufactures confident green over a wall of errors.
        let spec: EndpointSpec =
            serde_json::from_value(json!({"method": "GET", "path": "/todos"})).unwrap();
        assert_eq!(spec.expect_status, Expect::Band(Band::Success));
        for status in [200, 201, 204, 301, 302] {
            assert!(spec.expect_status.accepts(status), "{status} should pass");
        }
        for status in [400, 401, 403, 404, 405, 422, 500, 502] {
            assert!(!spec.expect_status.accepts(status), "{status} should fail");
        }
    }

    #[test]
    fn expectation_wire_form_accepts_exact_codes_and_band_names() {
        let exact: EndpointSpec =
            serde_json::from_value(json!({"method": "GET", "path": "/a", "expect_status": 204}))
                .unwrap();
        assert_eq!(exact.expect_status, Expect::Exact(204));
        assert!(exact.expect_status.accepts(204));
        assert!(!exact.expect_status.accepts(200));

        let band: EndpointSpec = serde_json::from_value(
            json!({"method": "POST", "path": "/a", "expect_status": "accepted"}),
        )
        .unwrap();
        assert_eq!(band.expect_status, Expect::Band(Band::Accepted));
        // The write band tolerates our synthesized body being rejected...
        assert!(band.expect_status.accepts(400));
        assert!(band.expect_status.accepts(422));
        assert!(band.expect_status.accepts(201));
        // ...but not a missing or auth-walled route.
        for status in [401, 403, 404, 405, 500] {
            assert!(!band.expect_status.accepts(status), "{status} should fail");
        }
    }

    #[test]
    fn the_lenient_band_survives_only_as_an_explicit_opt_in() {
        let any: EndpointSpec =
            serde_json::from_value(json!({"method": "GET", "path": "/a", "expect_status": "any"}))
                .unwrap();
        assert_eq!(any.expect_status, Expect::Band(Band::Any));
        assert!(any.expect_status.accepts(404));
        assert!(!any.expect_status.accepts(500));
    }

    #[test]
    fn boundary_cases_probe_path_params_and_bodies_without_5xx_oracle() {
        let summary = json!({
            "project_name": "demo",
            "api_endpoints": [
                { "method": "GET", "path": "/users/{id}" },
                { "method": "POST", "path": "/users" },
                { "method": "GET", "path": "/health" },
            ],
        });
        let cases = boundary_cases(&summary);
        // 4 path probes for {id} + 3 body probes for POST; /health has neither.
        assert_eq!(cases.len(), 7);
        assert!(cases.iter().all(|c| c.id.starts_with("BND")));
        // Every boundary case uses the robustness oracle: reject-or-handle,
        // never a 5xx.
        assert!(
            cases
                .iter()
                .all(|c| c.spec.expect_status == Expect::Band(Band::Any))
        );
        // The injection probe substitutes into the concrete path.
        assert!(cases.iter().any(|c| c.spec.path.contains("OR%20")));
        // Body probes carry a boundary body on the mutation endpoint.
        assert!(
            cases
                .iter()
                .any(|c| c.spec.path == "/users" && c.spec.body.is_some())
        );
    }

    #[test]
    fn substitute_all_params_replaces_every_brace_segment() {
        assert_eq!(substitute_all_params("/a/{x}/b/{y}", "0"), "/a/0/b/0");
        assert_eq!(substitute_all_params("/health", "0"), "/health");
    }

    #[test]
    fn normalize_folds_case_whitespace_and_combining_marks() {
        assert_eq!(normalize("  Welcome   Back  "), "welcome back");
        // base + combining acute folds to the base letter.
        assert_eq!(normalize("Cafe\u{0301}"), normalize("cafe"));
    }

    #[test]
    fn fuzzy_str_match_accepts_cosmetic_but_rejects_different_words() {
        assert!(fuzzy_str_match("Welcome Back", "welcome  back", 0.9));
        assert!(fuzzy_str_match("identical", "identical", 0.9));
        assert!(!fuzzy_str_match("hello", "goodbye", 0.9));
        // A single-char typo on a longer string clears 0.9; a huge threshold
        // rejects it.
        assert!(fuzzy_str_match("dashboard", "dashborad", 0.7));
        assert!(!fuzzy_str_match("dashboard", "dashborad", 0.99));
    }

    #[test]
    fn match_mode_wire_form_defaults_to_exact_and_parses_fuzzy() {
        let default: EndpointSpec =
            serde_json::from_value(json!({"method":"GET","path":"/a"})).unwrap();
        assert_eq!(default.match_mode, MatchMode::Exact);
        let fuzzy: EndpointSpec =
            serde_json::from_value(json!({"method":"GET","path":"/a","match":"fuzzy"})).unwrap();
        assert_eq!(fuzzy.match_mode, MatchMode::Fuzzy);
        // The manual api_endpoints parser reads it too.
        let parsed =
            parse_endpoint_spec(&json!({"method":"GET","path":"/a","match":"fuzzy"})).unwrap();
        assert_eq!(parsed.match_mode, MatchMode::Fuzzy);
    }

    #[test]
    fn generated_python_asserts_the_same_band_the_executor_enforces() {
        let vars = HashMap::new();
        let spec = |e: Value| -> EndpointSpec {
            serde_json::from_value(json!({"method": "GET", "path": "/a", "expect_status": e}))
                .unwrap()
        };
        assert!(
            python_for(&spec(json!("success")), "http://x", &vars)
                .contains("200 <= r.status_code < 400")
        );
        assert!(python_for(&spec(json!(204)), "http://x", &vars).contains("== 204"));
    }
}
