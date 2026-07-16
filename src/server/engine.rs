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
    pub method: String,
    pub path: String,
    #[serde(default)]
    pub body: Option<Value>,
    #[serde(default)]
    pub expect_status: Expect,
    #[serde(default)]
    pub headers: Option<Value>,
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
        method,
        path,
        body: v.get("body").cloned().filter(|b| !b.is_null()),
        expect_status: v
            .get("expect_status")
            .and_then(|s| serde_json::from_value(s.clone()).ok())
            .unwrap_or_default(),
        headers: v.get("headers").cloned().filter(Value::is_object),
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
