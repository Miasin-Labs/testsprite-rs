//! Tolerant `standard_prd.json` ingester.
//!
//! The real TestSprite PRD is a loose bag — across public repos it takes 50+
//! shapes (`features` vs `key_features` vs `functional_requirements` vs
//! `user_stories` vs `requirements`; endpoints embedded under `code_summary`,
//! `security`, or per-feature `api_doc`; credentials/environment/timing blocks
//! that come and go). This module normalizes ANY such object into testsprite-rs's
//! internal model so a third-party PRD can be re-planned deterministically, and
//! surfaces the first-class operational blocks (`testCredentials`,
//! `test_environment`, `timing_rules`, `validation_rules`).
//!
//! Everything here is pure and total: any JSON object yields a result, missing
//! or mis-typed fields are skipped, nothing panics — matching the best-effort
//! ethos of [`super::project::load_variables`].

use serde_json::{Value, json};

/// A normalized PRD ready for testsprite-rs's planning + seeding pipeline.
#[derive(Debug, Clone)]
pub struct IngestedPrd {
    /// Canonical PRD, key-identical to `engine::prd_from_code_summary` output:
    /// `{meta{project,prepared_by}, product_overview, core_goals[], features[]}`.
    pub prd: Value,
    /// Code-summary-shaped `{project_name, api_endpoints:[{method,path}]}` — feed
    /// directly to `plan_from_code_summary` / `boundary_cases`.
    pub summary: Value,
    /// Flattened requirement statements (for report grouping / traceability).
    pub requirements: Vec<String>,
    /// Seeded test credentials by role (`adminUser`, `staffUser`, …).
    pub credentials: Vec<(String, Role)>,
    /// Test environment hints.
    pub environment: Option<TestEnvironment>,
    /// Explicit timing rules (waits/settle expectations) from the PRD.
    pub timing_rules: Vec<String>,
    /// The raw `test_data_strategy` block (parsed downstream by the run).
    pub test_data_strategy: Option<Value>,
}

/// One seeded role's credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Role {
    pub username: Option<String>,
    pub password: Option<String>,
    pub role: Option<String>,
}

/// Test environment block.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TestEnvironment {
    pub frontend_url: Option<String>,
    pub backend_api: Option<String>,
    pub browsers: Vec<String>,
    pub viewport_priority: Vec<String>,
    pub network_condition: Option<String>,
}

/// Ingest a loose PRD `Value`. Returns `None` when the value is a code summary
/// (it already carries a non-empty top-level `api_endpoints`, which the existing
/// deterministic generation path owns) or isn't a JSON object — so callers stay
/// byte-for-byte backward-compatible for today's code-summary inputs.
pub fn ingest(prd: &Value) -> Option<IngestedPrd> {
    if !prd.is_object() {
        return None;
    }
    if prd
        .get("api_endpoints")
        .and_then(Value::as_array)
        .is_some_and(|a| !a.is_empty())
    {
        return None;
    }

    let endpoints = recover_endpoints(prd);
    let project = project_name(prd);
    let summary = json!({
        "project_name": project,
        "api_endpoints": endpoints,
    });

    let requirements = collect_requirements(prd);
    let timing_rules = string_array(prd.get("timing_rules"));
    let test_data_strategy = prd
        .get("test_data_strategy")
        .cloned()
        .filter(Value::is_object);

    // Fold the operational blocks into the canonical PRD so they PERSIST with
    // it (via `persist_prd`) and are inspectable — and so the run path picks up
    // `test_data_strategy` automatically: `run::inject_strategy_vars` reads it
    // off the latest stored PRD. Anything absent is simply not attached.
    let mut canonical = build_prd(prd, &project);
    if let Some(tds) = &test_data_strategy {
        canonical["test_data_strategy"] = tds.clone();
    }
    if !timing_rules.is_empty() {
        canonical["timing_rules"] = json!(timing_rules);
    }
    if let Some(vr) = prd.get("validation_rules").filter(|v| !v.is_null()) {
        canonical["validation_rules"] = vr.clone();
    }
    if !requirements.is_empty() {
        canonical["requirements"] = json!(requirements);
    }

    Some(IngestedPrd {
        prd: canonical,
        summary,
        requirements,
        credentials: collect_credentials(prd),
        environment: parse_environment(prd),
        timing_rules,
        test_data_strategy,
    })
}

impl IngestedPrd {
    /// Flatten credentials + environment into `key=value` variable pairs for
    /// non-clobbering seeding into `variables.json`. Credentials become
    /// `{role}_username` / `{role}_password` / `{role}_role` (so a Playwright
    /// step can reference `${adminUser_username}`); the environment contributes
    /// `frontend_url` / `backend_api`. Order is deterministic.
    pub fn variable_seeds(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for (role_key, role) in &self.credentials {
            if let Some(u) = &role.username {
                out.push((format!("{role_key}_username"), u.clone()));
            }
            if let Some(p) = &role.password {
                out.push((format!("{role_key}_password"), p.clone()));
            }
            if let Some(r) = &role.role {
                out.push((format!("{role_key}_role"), r.clone()));
            }
        }
        if let Some(env) = &self.environment {
            if let Some(f) = &env.frontend_url {
                out.push(("frontend_url".to_string(), f.clone()));
            }
            if let Some(b) = &env.backend_api {
                out.push(("backend_api".to_string(), b.clone()));
            }
        }
        out
    }
}

/// Resolve a project name across the many aliases real PRDs use.
fn project_name(prd: &Value) -> String {
    for path in [
        &["meta", "project"][..],
        &["projectName"],
        &["project_name"],
        &["productName"],
        &["product_name"],
        &["product", "name"],
        &["name"],
    ] {
        if let Some(s) = dig(prd, path)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            return s.to_string();
        }
    }
    "project".to_string()
}

/// Build the canonical PRD (same key shape as `engine::prd_from_code_summary`).
fn build_prd(prd: &Value, project: &str) -> Value {
    let overview = [
        "product_overview",
        "description",
        "project_description",
        "summary",
        "overview",
    ]
    .iter()
    .find_map(|k| prd.get(*k).and_then(Value::as_str))
    .unwrap_or("")
    .to_string();
    let core_goals = first_string_array(prd, &["core_goals", "success_criteria", "objectives"]);
    let prepared_by = dig(prd, &["meta", "prepared_by"])
        .and_then(Value::as_str)
        .unwrap_or("testsprite-rs (ingested)");
    json!({
        "meta": { "project": project, "prepared_by": prepared_by },
        "product_overview": overview,
        "core_goals": core_goals,
        "features": normalize_features(prd),
    })
}

/// Normalize the feature list from whichever shape the PRD uses, into
/// `[{name, description, user_flows}]`.
fn normalize_features(prd: &Value) -> Vec<Value> {
    // Prefer a top-level `features` array of objects.
    if let Some(arr) = prd.get("features").and_then(Value::as_array)
        && arr.iter().any(|f| f.get("name").is_some())
    {
        return arr
            .iter()
            .filter_map(|f| {
                let name = f.get("name").and_then(Value::as_str)?;
                Some(json!({
                    "name": name,
                    "description": f.get("description").and_then(Value::as_str).unwrap_or(""),
                    "user_flows": feature_flows(f),
                }))
            })
            .collect();
    }
    // `key_features`: strings or objects.
    if let Some(arr) = prd.get("key_features").and_then(Value::as_array) {
        return arr
            .iter()
            .map(|f| match f {
                Value::String(s) => json!({"name": s, "description": "", "user_flows": []}),
                Value::Object(_) => json!({
                    "name": f.get("name").and_then(Value::as_str).unwrap_or(""),
                    "description": f.get("description").and_then(Value::as_str).unwrap_or(""),
                    "user_flows": string_array(f.get("user_flows")),
                }),
                _ => json!({"name": "", "description": "", "user_flows": []}),
            })
            .collect();
    }
    // Derive from an embedded code_summary's features.
    if let Some(arr) = dig(prd, &["code_summary", "features"]).and_then(Value::as_array) {
        return arr
            .iter()
            .filter_map(|f| {
                let name = f.get("name").and_then(Value::as_str)?;
                Some(json!({
                    "name": name,
                    "description": f.get("description").and_then(Value::as_str).unwrap_or(""),
                    "user_flows": string_array(f.get("user_interactions")),
                }))
            })
            .collect();
    }
    Vec::new()
}

/// A feature's user flows: its explicit `user_flows`, else its `requirements`
/// (real PRDs express per-feature acceptance criteria there), so the downstream
/// planner always has something concrete to drive steps from.
fn feature_flows(f: &Value) -> Vec<String> {
    let flows = string_array(f.get("user_flows"));
    if !flows.is_empty() {
        return flows;
    }
    string_array(f.get("requirements"))
}

const HTTP_VERBS: &[&str] = &[
    "GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS", "QUERY",
];

/// Recover endpoints from wherever the PRD embeds them, deduped by
/// `"METHOD path"`.
fn recover_endpoints(prd: &Value) -> Vec<Value> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    let mut push = |method: &str, path: &str| {
        if !path.starts_with('/') {
            return;
        }
        let key = format!("{} {}", method.to_uppercase(), path);
        if seen.insert(key) {
            out.push(json!({"method": method.to_uppercase(), "path": path}));
        }
    };

    // (a) features[].endpoints — string or object. Scanned both nested under
    // `code_summary` (full PRD) and at the top level (an unwrapped code summary,
    // e.g. what the serve stand-in's `code_summary_from_prd` produces).
    for features_path in [
        &["code_summary", "features"][..],
        &["features"],
        &["key_features"],
    ] {
        if let Some(features) = dig(prd, features_path).and_then(Value::as_array) {
            for f in features {
                if let Some(eps) = f.get("endpoints").and_then(Value::as_array) {
                    for e in eps {
                        if let Some((m, p)) = endpoint_entry(e) {
                            push(&m, &p);
                        }
                    }
                }
            }
        }
    }
    // (b) code_summary.api_endpoints[] and top-level api_endpoints[].
    for arr_path in [
        &["code_summary", "api_endpoints"][..],
        &["api_endpoints"],
        &["apis"],
    ] {
        if let Some(arr) = dig(prd, arr_path).and_then(Value::as_array) {
            for e in arr {
                if let Some((m, p)) = endpoint_entry(e) {
                    push(&m, &p);
                }
            }
        }
    }
    // (c) security.{protected,public}_endpoints[] — paths only, method GET.
    for key in ["protected_endpoints", "public_endpoints"] {
        if let Some(arr) = dig(prd, &["code_summary", "security", key])
            .or_else(|| dig(prd, &["security", key]))
            .and_then(Value::as_array)
        {
            for p in arr.iter().filter_map(Value::as_str) {
                push("GET", p);
            }
        }
    }
    out
}

/// Parse one endpoint entry (string `"POST /api/x"` or object `{method,path}` /
/// `{method,endpoint}`) to `(method, path)`.
fn endpoint_entry(e: &Value) -> Option<(String, String)> {
    match e {
        Value::String(s) => {
            let mut it = s.split_whitespace();
            let first = it.next()?;
            if HTTP_VERBS.contains(&first.to_uppercase().as_str()) {
                let path = it.next()?;
                path.starts_with('/')
                    .then(|| (first.to_uppercase(), path.to_string()))
            } else if first.starts_with('/') {
                Some(("GET".to_string(), first.to_string()))
            } else {
                None
            }
        }
        Value::Object(o) => {
            let path = o
                .get("path")
                .or_else(|| o.get("endpoint"))
                .and_then(Value::as_str)?;
            let method = o
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or("GET")
                .to_uppercase();
            path.starts_with('/').then(|| (method, path.to_string()))
        }
        _ => None,
    }
}

/// Flatten requirement statements from the various array shapes.
fn collect_requirements(prd: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut add = |s: &str| {
        let t = s.trim();
        if !t.is_empty() && seen.insert(t.to_string()) {
            out.push(t.to_string());
        }
    };
    for key in ["validation_criteria", "user_flow_summary"] {
        for s in string_array(prd.get(key)) {
            add(&s);
        }
    }
    // Per-feature acceptance criteria (`features[].requirements`) — the shape
    // real TestSprite PRDs most commonly use.
    for arr_key in ["features", "key_features"] {
        if let Some(feats) = prd.get(arr_key).and_then(Value::as_array) {
            for f in feats {
                for s in string_array(f.get("requirements")) {
                    add(&s);
                }
            }
        }
    }
    for key in ["user_stories", "functional_requirements", "requirements"] {
        if let Some(arr) = prd.get(key).and_then(Value::as_array) {
            for item in arr {
                match item {
                    Value::String(s) => add(s),
                    Value::Object(_) => {
                        for f in ["story", "description", "name", "title", "userStory"] {
                            if let Some(s) = item.get(f).and_then(Value::as_str) {
                                add(s);
                                break;
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    out
}

/// Collect `testCredentials` roles (any `*User` key), sorted for deterministic
/// seeding order.
fn collect_credentials(prd: &Value) -> Vec<(String, Role)> {
    let Some(obj) = prd.get("testCredentials").and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut out: Vec<(String, Role)> = obj
        .iter()
        .filter_map(|(k, v)| {
            let o = v.as_object()?;
            let s = |key: &str| o.get(key).and_then(Value::as_str).map(str::to_string);
            Some((
                k.clone(),
                Role {
                    username: s("username").or_else(|| s("email")),
                    password: s("password"),
                    role: s("role"),
                },
            ))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Parse the environment block. Prefers an explicit `test_environment`, and
/// falls back to a top-level `endpoints: {baseUrl, apiBaseUrl}` object (the
/// shape real frontend PRDs use to carry the app + API roots). Parenthetical
/// noise is stripped from URLs.
fn parse_environment(prd: &Value) -> Option<TestEnvironment> {
    let clean = |v: Option<&Value>| {
        v.and_then(Value::as_str)
            .map(|s| s.split_whitespace().next().unwrap_or(s).to_string())
    };
    if let Some(obj) = prd.get("test_environment").and_then(Value::as_object) {
        return Some(TestEnvironment {
            frontend_url: clean(obj.get("frontend_url")),
            backend_api: clean(obj.get("backend_api")),
            browsers: string_array(obj.get("browsers")),
            viewport_priority: string_array(obj.get("viewport_priority")),
            network_condition: obj
                .get("network_condition")
                .and_then(Value::as_str)
                .map(str::to_string),
        });
    }
    // Fallback: `endpoints: {baseUrl, apiBaseUrl}`.
    if let Some(obj) = prd.get("endpoints").and_then(Value::as_object) {
        let frontend = clean(obj.get("baseUrl"));
        let backend = clean(obj.get("apiBaseUrl"));
        if frontend.is_some() || backend.is_some() {
            return Some(TestEnvironment {
                frontend_url: frontend,
                backend_api: backend,
                ..TestEnvironment::default()
            });
        }
    }
    None
}

// --- small tolerant helpers -------------------------------------------------

/// Follow a key path through nested objects.
fn dig<'a>(v: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut cur = v;
    for key in path {
        cur = cur.get(key)?;
    }
    Some(cur)
}

/// A `Vec<String>` from a JSON string-array (or `[]` for anything else).
fn string_array(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The first non-empty string array among `keys`.
fn first_string_array(prd: &Value, keys: &[&str]) -> Vec<String> {
    for k in keys {
        let a = string_array(prd.get(*k));
        if !a.is_empty() {
            return a;
        }
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_summary_with_api_endpoints_is_not_ingested() {
        // Backward-compat guard: the existing deterministic path owns this.
        assert!(ingest(&json!({"api_endpoints": [{"method":"GET","path":"/x"}]})).is_none());
        assert!(ingest(&json!([1, 2, 3])).is_none());
        assert!(ingest(&json!("x")).is_none());
    }

    #[test]
    fn canonical_prd_shape_matches_engine_output() {
        let prd = json!({
            "projectName": "Demo",
            "description": "A demo app.",
            "key_features": ["Auth", {"name": "Billing", "description": "invoices"}],
        });
        let out = ingest(&prd).unwrap();
        assert_eq!(out.prd["meta"]["project"], "Demo");
        assert_eq!(out.prd["product_overview"], "A demo app.");
        let f0 = &out.prd["features"][0];
        // Exactly the engine's feature shape.
        assert!(
            f0.get("name").is_some()
                && f0.get("description").is_some()
                && f0.get("user_flows").is_some()
        );
        assert_eq!(out.prd["features"][1]["name"], "Billing");
    }

    #[test]
    fn recovers_endpoints_from_mixed_shapes_deduped() {
        let prd = json!({
            "code_summary": {
                "features": [{
                    "name": "F", "endpoints": ["POST /api/attack/fire", {"method":"get","path":"/health"}]
                }],
                "security": { "protected_endpoints": ["/api/attack/fire", "/api/me"] }
            }
        });
        let out = ingest(&prd).unwrap();
        let eps = out.summary["api_endpoints"].as_array().unwrap();
        let pairs: Vec<(String, String)> = eps
            .iter()
            .map(|e| {
                (
                    e["method"].as_str().unwrap().to_string(),
                    e["path"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        assert!(pairs.contains(&("POST".to_string(), "/api/attack/fire".to_string())));
        assert!(pairs.contains(&("GET".to_string(), "/health".to_string())));
        assert!(pairs.contains(&("GET".to_string(), "/api/me".to_string())));
        // POST /api/attack/fire (features) vs GET /api/attack/fire (security) are
        // distinct method+path — both present, no accidental collapse.
        assert!(pairs.contains(&("GET".to_string(), "/api/attack/fire".to_string())));
    }

    #[test]
    fn endpoint_string_without_verb_defaults_to_get() {
        assert_eq!(
            endpoint_entry(&json!("/health")),
            Some(("GET".to_string(), "/health".to_string()))
        );
        assert_eq!(
            endpoint_entry(&json!("delete /x")),
            Some(("DELETE".to_string(), "/x".to_string()))
        );
        assert_eq!(endpoint_entry(&json!("not a path")), None);
    }

    #[test]
    fn credentials_and_environment_and_requirements_are_extracted() {
        let prd = json!({
            "testCredentials": {
                "adminUser": {"username":"admin","password":"a123","role":"admin"},
                "staffUser": {"username":"staff","password":"s123"},
            },
            "test_environment": {
                "frontend_url": "http://localhost:3000",
                "backend_api": "http://localhost:5000 (optional)",
                "browsers": ["chromium","firefox"],
            },
            "functional_requirements": [
                {"name":"Auth","description":"login/logout"},
            ],
            "user_stories": ["As a user I can log in"],
        });
        let out = ingest(&prd).unwrap();
        // Deterministic role order (adminUser before staffUser).
        assert_eq!(out.credentials[0].0, "adminUser");
        assert_eq!(out.credentials[0].1.username.as_deref(), Some("admin"));
        // Parenthetical stripped from the backend URL.
        let env = out.environment.unwrap();
        assert_eq!(env.backend_api.as_deref(), Some("http://localhost:5000"));
        assert_eq!(env.browsers, vec!["chromium", "firefox"]);
        // Requirements flattened across shapes.
        assert!(out.requirements.iter().any(|r| r.contains("login/logout")));
        assert!(out.requirements.iter().any(|r| r.contains("log in")));
    }

    #[test]
    fn real_frontend_prd_shape_features_requirements_and_endpoints_env() {
        // Mirrors the mknoufi/stock_last frontend standard_prd.json in the
        // sample corpus: `features[].requirements`, `endpoints:{baseUrl,..}`.
        let prd = json!({
            "projectName": "Stock Last",
            "description": "Inventory app.",
            "features": [{
                "id": "F001",
                "name": "User Authentication",
                "description": "Login system.",
                "requirements": [
                    "Users can login with username and password",
                    "Invalid credentials show an error",
                ],
            }],
            "testCredentials": {
                "staffUser": {"username":"staff","password":"staff123","role":"staff"},
                "adminUser": {"username":"admin","password":"admin123","role":"admin"},
            },
            "endpoints": {"baseUrl":"http://localhost:8082","apiBaseUrl":"http://localhost:8001/api"},
        });
        let out = ingest(&prd).unwrap();
        // Feature requirements become both user_flows and global requirements.
        assert_eq!(
            out.prd["features"][0]["user_flows"][0],
            "Users can login with username and password"
        );
        assert!(
            out.requirements
                .iter()
                .any(|r| r.contains("login with username"))
        );
        // `endpoints` dict → environment (not treated as endpoint paths).
        assert!(out.summary["api_endpoints"].as_array().unwrap().is_empty());
        let env = out.environment.as_ref().unwrap();
        assert_eq!(env.frontend_url.as_deref(), Some("http://localhost:8082"));
        assert_eq!(
            env.backend_api.as_deref(),
            Some("http://localhost:8001/api")
        );
        // Deterministic, role-prefixed variable seeds (adminUser sorts first).
        let seeds = out.variable_seeds();
        assert_eq!(
            seeds[0],
            ("adminUser_username".to_string(), "admin".to_string())
        );
        assert!(seeds.contains(&("staffUser_password".to_string(), "staff123".to_string())));
        assert!(seeds.contains(&(
            "frontend_url".to_string(),
            "http://localhost:8082".to_string()
        )));
    }
}
