//! Parse structured API docs — Postman collections, OpenAPI/Swagger specs, and
//! HAR captures — into deterministic `spec` test cases (`{method, path,
//! expect_status?}`) plus a synthesized PRD. These run via `execute_spec`
//! (reqwest) with NO OpenAI key and NO per-run codegen, and hit the project's
//! live target URL (`concrete_path` seeds `{id}` from `variables.json`). When
//! the input isn't a recognized structured format, callers fall back to the LLM
//! doc path.

use serde_json::{Value, json};

/// One extracted HTTP endpoint (path only — the host comes from the target URL).
struct Endpoint {
    method: String,
    path: String,
    body: Option<Value>,
    expect_status: Option<u16>,
    name: String,
}

/// The result of parsing a structured API doc.
pub struct Extracted {
    pub format: &'static str,
    /// Deterministic `{id,title,description,kind:backend,spec}` test cases.
    pub cases: Vec<Value>,
    /// A synthesized PRD (same shape the LLM PRD uses).
    pub prd: Value,
}

/// Try to parse `text` as a Postman collection, OpenAPI/Swagger spec, or HAR.
/// `None` = not a recognized structured API doc (caller should LLM-normalize).
pub fn extract(text: &str) -> Option<Extracted> {
    let v: Value = serde_json::from_str(text)
        .ok()
        .or_else(|| serde_yaml::from_str(text).ok())?;

    let (format, endpoints) = if is_postman(&v) {
        ("postman", postman_endpoints(&v))
    } else if is_openapi(&v) {
        ("openapi", openapi_endpoints(&v))
    } else if is_har(&v) {
        ("har", har_endpoints(&v))
    } else {
        return None;
    };

    if endpoints.is_empty() {
        return None;
    }
    Some(build(format, &endpoints))
}

fn is_postman(v: &Value) -> bool {
    v.get("item").and_then(Value::as_array).is_some()
        && (v.get("info").is_some() || v.get("item").is_some())
}
fn is_openapi(v: &Value) -> bool {
    (v.get("openapi").is_some() || v.get("swagger").is_some()) && v.get("paths").is_some()
}
fn is_har(v: &Value) -> bool {
    v.get("log")
        .and_then(|l| l.get("entries"))
        .and_then(Value::as_array)
        .is_some()
}

/// `{{var}}` -> `{var}`, `:seg` -> `{seg}`; ensure a leading `/`.
fn normalize_params(p: &str) -> String {
    let braced = p.replace("{{", "{").replace("}}", "}");
    let joined = braced
        .split('/')
        .map(|seg| match seg.strip_prefix(':') {
            Some(name) => format!("{{{name}}}"),
            None => seg.to_string(),
        })
        .collect::<Vec<_>>()
        .join("/");
    if joined.starts_with('/') {
        joined
    } else {
        format!("/{joined}")
    }
}

/// The path portion of a URL string (host/scheme/query stripped).
fn url_string_path(raw: &str) -> String {
    let no_qf = raw.split(['?', '#']).next().unwrap_or(raw);
    if let Some(pos) = no_qf.find("://") {
        let after = &no_qf[pos + 3..];
        return match after.find('/') {
            Some(i) => after[i..].to_string(),
            None => "/".to_string(),
        };
    }
    if no_qf.starts_with("{{") {
        return match no_qf.find('/') {
            Some(i) => no_qf[i..].to_string(),
            None => "/".to_string(),
        };
    }
    if no_qf.starts_with('/') {
        return no_qf.to_string();
    }
    match no_qf.find('/') {
        Some(i) => no_qf[i..].to_string(),
        None => "/".to_string(),
    }
}

fn postman_path(url: &Value) -> Option<String> {
    if let Some(segs) = url.get("path").and_then(Value::as_array) {
        let joined = segs
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("/");
        return Some(normalize_params(&joined));
    }
    let raw = url
        .as_str()
        .or_else(|| url.get("raw").and_then(Value::as_str))?;
    Some(normalize_params(&url_string_path(raw)))
}

fn body_from(raw: Option<&str>) -> Option<Value> {
    raw.and_then(|t| serde_json::from_str::<Value>(t).ok())
}

fn postman_endpoints(v: &Value) -> Vec<Endpoint> {
    fn walk(items: &[Value], out: &mut Vec<Endpoint>) {
        for it in items {
            if let Some(children) = it.get("item").and_then(Value::as_array) {
                walk(children, out);
                continue;
            }
            let Some(req) = it.get("request") else { continue };
            let method = req
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or("GET")
                .to_uppercase();
            let Some(path) = req.get("url").and_then(postman_path) else {
                continue;
            };
            let body = body_from(req.get("body").and_then(|b| b.get("raw")).and_then(Value::as_str));
            let name = it.get("name").and_then(Value::as_str).unwrap_or("").to_string();
            out.push(Endpoint {
                method,
                path,
                body,
                expect_status: None,
                name,
            });
        }
    }
    let mut out = Vec::new();
    if let Some(items) = v.get("item").and_then(Value::as_array) {
        walk(items, &mut out);
    }
    out
}

const HTTP_METHODS: &[&str] = &["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS"];

fn openapi_endpoints(v: &Value) -> Vec<Endpoint> {
    let mut out = Vec::new();
    let Some(paths) = v.get("paths").and_then(Value::as_object) else {
        return out;
    };
    for (path, methods) in paths {
        let Some(mobj) = methods.as_object() else { continue };
        for (method, op) in mobj {
            let m = method.to_uppercase();
            if !HTTP_METHODS.contains(&m.as_str()) {
                continue;
            }
            let expect_status = op
                .get("responses")
                .and_then(Value::as_object)
                .and_then(|r| {
                    r.keys()
                        .filter_map(|k| k.parse::<u16>().ok())
                        .filter(|s| (200..300).contains(s))
                        .min()
                });
            let name = op
                .get("summary")
                .and_then(Value::as_str)
                .or_else(|| op.get("operationId").and_then(Value::as_str))
                .unwrap_or("")
                .to_string();
            out.push(Endpoint {
                method: m,
                path: normalize_params(path),
                body: None,
                expect_status,
                name,
            });
        }
    }
    out
}

fn har_endpoints(v: &Value) -> Vec<Endpoint> {
    let mut out = Vec::new();
    let Some(entries) = v
        .get("log")
        .and_then(|l| l.get("entries"))
        .and_then(Value::as_array)
    else {
        return out;
    };
    let mut seen = std::collections::BTreeSet::new();
    for e in entries {
        let Some(req) = e.get("request") else { continue };
        let method = req
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("GET")
            .to_uppercase();
        let Some(url) = req.get("url").and_then(Value::as_str) else {
            continue;
        };
        let path = normalize_params(&url_string_path(url));
        if !seen.insert(format!("{method} {path}")) {
            continue;
        }
        let body = body_from(
            req.get("postData")
                .and_then(|p| p.get("text"))
                .and_then(Value::as_str),
        );
        out.push(Endpoint {
            method,
            path,
            body,
            expect_status: None,
            name: String::new(),
        });
    }
    out
}

fn build(format: &'static str, endpoints: &[Endpoint]) -> Extracted {
    let cases = endpoints
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let mut spec = json!({ "method": e.method, "path": e.path });
            if let Some(b) = &e.body {
                spec["body"] = b.clone();
            }
            if let Some(s) = e.expect_status {
                spec["expect_status"] = json!(s);
            }
            let label = if e.name.is_empty() {
                format!("{} {}", e.method, e.path)
            } else {
                e.name.clone()
            };
            json!({
                "id": format!("TC{:03}", i + 1),
                "title": format!("{} {}", e.method, e.path),
                "description": format!("{label} — send {} {} and verify a non-error response.", e.method, e.path),
                "kind": "backend",
                "spec": spec,
            })
        })
        .collect();
    Extracted {
        format,
        cases,
        prd: synth_prd(format, endpoints),
    }
}

/// Synthesize a PRD from the endpoints, grouping by first path segment.
fn synth_prd(format: &str, endpoints: &[Endpoint]) -> Value {
    use std::collections::BTreeMap;
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for e in endpoints {
        let seg = e.path.trim_start_matches('/').split('/').next().unwrap_or("");
        let feature = if seg.is_empty() { "root" } else { seg };
        groups
            .entry(feature.to_string())
            .or_default()
            .push(format!("{} {}", e.method, e.path));
    }
    let features: Vec<Value> = groups
        .into_iter()
        .map(|(name, flows)| {
            json!({
                "name": name,
                "description": format!("Endpoints under /{name}"),
                "user_flows": flows,
            })
        })
        .collect();
    json!({
        "meta": { "project": "local", "prepared_by": format!("testsprite-rs ({format} import)") },
        "product_overview": format!("API surface imported from a {format} document: {} endpoint(s).", endpoints.len()),
        "core_goals": [
            "every endpoint responds without a server error",
            "auth-required endpoints reject unauthenticated calls",
        ],
        "features": features,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_postman_collection() {
        let doc = r#"{
          "info": {"name": "API"},
          "item": [
            {"name": "List users", "request": {"method": "GET", "url": {"raw": "{{base}}/users", "path": ["users"]}}},
            {"name": "Get user", "request": {"method": "GET", "url": "{{base}}/users/:id"}},
            {"name": "Admin", "item": [
              {"name": "Create", "request": {"method": "POST", "url": {"path": ["users"]}, "body": {"raw": "{\"n\":1}"}}}
            ]}
          ]
        }"#;
        let ex = extract(doc).unwrap();
        assert_eq!(ex.format, "postman");
        assert_eq!(ex.cases.len(), 3);
        assert_eq!(ex.cases[0]["spec"]["method"], "GET");
        assert_eq!(ex.cases[0]["spec"]["path"], "/users");
        assert_eq!(ex.cases[1]["spec"]["path"], "/users/{id}");
        assert_eq!(ex.cases[2]["spec"]["method"], "POST");
        assert_eq!(ex.cases[2]["spec"]["body"], json!({"n": 1}));
    }

    #[test]
    fn extracts_openapi_with_expected_status() {
        let doc = r#"{
          "openapi": "3.0.0",
          "paths": {
            "/health": {"get": {"responses": {"200": {}}}},
            "/users/{id}": {
              "get": {"summary": "Get user", "responses": {"200": {}, "404": {}}},
              "delete": {"responses": {"204": {}}}
            }
          }
        }"#;
        let ex = extract(doc).unwrap();
        assert_eq!(ex.format, "openapi");
        assert_eq!(ex.cases.len(), 3);
        let health = ex.cases.iter().find(|c| c["spec"]["path"] == "/health").unwrap();
        assert_eq!(health["spec"]["expect_status"], 200);
        let del = ex.cases.iter().find(|c| c["spec"]["method"] == "DELETE").unwrap();
        assert_eq!(del["spec"]["expect_status"], 204);
        // PRD is synthesized with features grouped by first segment.
        assert!(ex.prd["features"].as_array().unwrap().len() >= 2);
    }

    #[test]
    fn non_api_doc_returns_none() {
        assert!(extract("# just a readme\n\nsome prose").is_none());
        assert!(extract(r#"{"random": "json"}"#).is_none());
    }
}
