//! Deterministic generation engine — the part `api.testsprite.com` does with an
//! LLM, done locally with no account and no model.
//!
//! Input: a code summary (`api_endpoints: [{method, path, body?, expect_status?}]`).
//! Output: a structured PRD, a test plan (`[{id,title,description}]`), an
//! executable spec per case, and the Python (`requests`) test-code artifact.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;

/// One executable endpoint check derived from the code summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndpointSpec {
    pub method: String,
    pub path: String,
    #[serde(default)]
    pub body: Option<Value>,
    #[serde(default)]
    pub expect_status: Option<u16>,
    #[serde(default)]
    pub headers: Option<Value>,
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
        .map(|arr| arr.iter().filter_map(parse_endpoint).collect())
        .unwrap_or_default()
}

fn parse_endpoint(v: &Value) -> Option<EndpointSpec> {
    let method = v.get("method")?.as_str()?.to_uppercase();
    let path = v.get("path")?.as_str()?.to_string();
    Some(EndpointSpec {
        method,
        path,
        body: v.get("body").cloned().filter(|b| !b.is_null()),
        expect_status: v
            .get("expect_status")
            .and_then(|s| s.as_u64())
            .map(|s| s as u16),
        headers: v.get("headers").cloned().filter(Value::is_object),
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
            let expect = spec
                .expect_status
                .map(|s| format!("a {s} response"))
                .unwrap_or_else(|| "a non-5xx response".to_string());
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

/// Generate the Python `requests` test-code artifact for a case.
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
        Some(s) => format!("assert r.status_code == {s}, f\"expected {s}, got {{r.status_code}}\""),
        None => "assert r.status_code < 500, f\"server error: {r.status_code}\"".to_string(),
    };
    format!("import requests\n\ndef {fn_name}():\n    r = {call}\n    {assertion}\n\n{fn_name}()\n")
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
}
