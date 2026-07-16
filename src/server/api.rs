//! Local reimplementation of `api.testsprite.com`.
//!
//! Implements the exact endpoint contract the client calls, with no account,
//! no LLM, and no cloud. The deterministic engine derives PRD/plan/test-code
//! from the code summary; the executor runs the tests directly against the
//! local app (the tunnel becomes a no-op accept-only control socket).

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Multipart, Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use tokio::sync::RwLock;
use uuid::Uuid;

use super::executors::{self, ExecCtx, TestKind};
use super::llm::LlmClient;
use super::store::{self, Store};
use super::{coverage, engine};

const LOCAL_USER_ID: &str = "00000000-0000-0000-0000-000000000000";

#[derive(Clone)]
pub struct AppState {
    pub store: Store,
    /// Planned cases keyed by id. A case is JSON `{id,title,description,...}`;
    /// deterministic cases embed a `spec`. This is the single source of truth —
    /// no modality-specific branching lives here.
    pub cases: Arc<RwLock<HashMap<String, Value>>>,
    /// The PRD most recently generated (executor codegen context).
    pub prd: Arc<RwLock<Value>>,
    /// The modality this backend instance serves; routes /run to one
    /// [`executors::Executor`]. Selected at startup (`--kind`), overridable by a
    /// `testKind` field in the run body.
    pub kind: TestKind,
    /// Target surface: HTTP/browser base URL, mcp command, or rust crate path.
    pub target_base: Arc<RwLock<Option<String>>>,
    /// Optional OpenAI client. When present, the backend behaves like the real
    /// cloud (LLM PRD/plan/code-gen). When absent, the deterministic engine runs.
    pub llm: Option<LlmClient>,
}

impl AppState {
    pub fn new(llm: Option<LlmClient>, kind: TestKind) -> Self {
        Self {
            store: Store::new(),
            cases: Arc::new(RwLock::new(HashMap::new())),
            prd: Arc::new(RwLock::new(json!({}))),
            kind,
            target_base: Arc::new(RwLock::new(None)),
            llm,
        }
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route(
            "/health",
            get(|| async { Json(json!({ "env": "local", "version": "v0.0.0" })) }),
        )
        .route("/api/me", get(me))
        .route("/api/tunnel/v2", post(tunnel_create))
        .route("/api/tunnel/v2/version", get(tunnel_version))
        .route("/mcp/common/generate-prd", post(generate_prd))
        .route("/mcp/backend-test/plan", post(backend_plan))
        .route("/mcp/backend-test/run", post(backend_run))
        .route("/mcp/frontend-test/generate-plan", post(frontend_plan))
        .route("/mcp/frontend-test/run", post(backend_run))
        .route("/mcp/project/test/{test_id}", get(get_test))
        .route("/mcp/coverage", get(coverage_report))
        .route("/mcp/common/log", post(log_sink))
        .route("/mcp/common/log-batch", post(log_sink))
        .route("/mcp/common/generate-test-summary", post(test_summary))
        .with_state(state)
}

// --- account / tunnel (gating no-ops) ---

async fn me() -> impl IntoResponse {
    Json(json!({
        "id": LOCAL_USER_ID, "sub": LOCAL_USER_ID, "user": "local@localhost",
        "firstName": "local", "lastName": null, "subPlan": "Local",
        "credits": 999999, "totalTests": { "frontend": 0, "backend": 0 },
    }))
}

async fn tunnel_version() -> impl IntoResponse {
    Json(json!({ "version": 2 }))
}

async fn tunnel_create() -> impl IntoResponse {
    // The local executor doesn't need a data plane; still mint creds so the
    // client's control handshake has an id/secret.
    Json(json!({ "id": Uuid::new_v4().to_string(), "secret": Uuid::new_v4().to_string() }))
}

// --- PRD ---

/// `POST /mcp/common/generate-prd` (multipart). We only need `codeSummary`.
async fn generate_prd(State(state): State<AppState>, mut mp: Multipart) -> impl IntoResponse {
    let mut code_summary: Option<Value> = None;
    while let Ok(Some(field)) = mp.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        let text = field.text().await.unwrap_or_default();
        if name == "codeSummary" {
            code_summary = serde_json::from_str(&text).ok();
        }
    }
    let Some(cs) = code_summary else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "message": "missing codeSummary" })),
        )
            .into_response();
    };
    // Remember the app's base URL from the summary if present (for direct exec).
    if let Some(base) = cs.get("base_url").and_then(|v| v.as_str()) {
        *state.target_base.write().await = Some(base.to_string());
    }

    let prd = match &state.llm {
        Some(llm) => match llm.generate_prd(&cs).await {
            Ok(prd) => prd,
            Err(e) => {
                tracing::warn!("LLM PRD failed ({e}); using deterministic engine");
                engine::prd_from_code_summary(&cs)
            }
        },
        None => engine::prd_from_code_summary(&cs),
    };
    *state.prd.write().await = prd.clone();
    (StatusCode::CREATED, Json(prd)).into_response()
}

// --- plans ---

/// Reconstruct a code summary from the PRD's attached `code_summary` (the client
/// attaches it) — falling back to the PRD features.
fn code_summary_from_prd(prd: &Value) -> Value {
    prd.get("code_summary")
        .cloned()
        .unwrap_or_else(|| prd.clone())
}

async fn backend_plan(State(state): State<AppState>, Json(body): Json<Value>) -> impl IntoResponse {
    // body: { prdContent: <stringified PRD json>, targetScope }
    let prd: Value = body
        .get("prdContent")
        .and_then(|v| v.as_str())
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_else(|| json!({}));
    let cases = build_plan(&state, &prd).await;
    (StatusCode::CREATED, Json(json!({ "plan": cases })))
}

async fn frontend_plan(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let prd = body
        .get("standard_prd")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let cases = build_plan(&state, &prd).await;
    (StatusCode::CREATED, Json(cases))
}

/// Build a plan from a PRD and store the cases (the single source of truth that
/// `/run` and the coverage guard both read). LLM mode produces richer cases
/// (incl. adversarial edge cases); deterministic mode synthesizes one case per
/// endpoint with an embedded `spec`. Either way a case is just JSON.
async fn build_plan(state: &AppState, prd: &Value) -> Vec<Value> {
    *state.prd.write().await = prd.clone();

    let cases = match &state.llm {
        Some(llm) => match llm.generate_plan(prd).await {
            Ok(plan) => plan,
            Err(e) => {
                tracing::warn!("LLM plan failed ({e}); using deterministic engine");
                deterministic_cases(prd)
            }
        },
        None => deterministic_cases(prd),
    };

    let mut store = state.cases.write().await;
    let mut out = Vec::new();
    for case in cases {
        if let Some(id) = case.get("id").and_then(|v| v.as_str()) {
            store.insert(id.to_string(), case.clone());
            out.push(case);
        }
    }
    out
}

/// Deterministic cases: one per declared endpoint, with an embedded `spec` the
/// HTTP executor runs directly (no LLM).
fn deterministic_cases(prd: &Value) -> Vec<Value> {
    let cs = code_summary_from_prd(prd);
    engine::plan_from_code_summary(&cs)
        .into_iter()
        .map(|c| {
            json!({
                "id": c.id,
                "title": c.title,
                "description": c.description,
                "spec": serde_json::to_value(&c.spec).unwrap_or(Value::Null),
            })
        })
        .collect()
}

// --- run ---

/// `POST /mcp/{backend,frontend}-test/run` — create RUNNING entities, dispatch
/// every case through the modality executor, return the test ids to poll.
async fn backend_run(State(state): State<AppState>, Json(body): Json<Value>) -> impl IntoResponse {
    let project_id = Uuid::new_v4().to_string();
    let target = resolve_base(&state, &body).await;
    // Kind is fixed at startup but may be overridden per-run via `testKind`.
    let kind = body
        .get("testKind")
        .and_then(|v| v.as_str())
        .map(TestKind::parse)
        .unwrap_or(state.kind);

    // The client sends back the (possibly filtered) testPlan; fall back to the
    // full stored plan so the run still works even if it's empty.
    let mut plan = body
        .get("testPlan")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if plan.is_empty() {
        plan = state.cases.read().await.values().cloned().collect();
    }

    let mut to_run: Vec<(String, Value)> = Vec::new();
    let mut ids: Vec<String> = Vec::new();
    for case in &plan {
        let title = case.get("title").and_then(|v| v.as_str()).unwrap_or("test");
        let description = case
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let test_id = Uuid::new_v4().to_string();
        let entity =
            store::new_running_entity(&project_id, &test_id, LOCAL_USER_ID, title, description);
        state.store.insert(test_id.clone(), entity).await;
        to_run.push((test_id.clone(), case.clone()));
        ids.push(test_id);
    }

    // One execution path for every modality: pick the executor for the kind,
    // build the shared context, and let the store drive each case.
    let executor = executors::for_kind(kind);
    let ctx = ExecCtx {
        target,
        llm: state.llm.clone(),
        prd: Arc::new(state.prd.read().await.clone()),
        browser: None,
        shots_dir: None,
        root: std::env::current_dir().unwrap_or_default(),
        variables: std::collections::HashMap::new(),
    };
    store::spawn_execution(state.store.clone(), executor, ctx, to_run);
    (StatusCode::CREATED, Json(json!({ "testIds": ids })))
}

/// Determine the app base URL: prefer the run payload's `endpoint`, then the
/// summary-derived base, then localhost.
async fn resolve_base(state: &AppState, body: &Value) -> String {
    if let Some(ep) = body.get("endpoint").and_then(|v| v.as_str()) {
        return ep.to_string();
    }
    if let Some(b) = state.target_base.read().await.clone() {
        return b;
    }
    "http://localhost:8080".to_string()
}

// --- poll ---

async fn get_test(State(state): State<AppState>, Path(test_id): Path<String>) -> impl IntoResponse {
    match state.store.get(&test_id).await {
        Some(entity) => (StatusCode::OK, Json(entity)),
        None => (
            StatusCode::OK,
            Json(json!({ "error": "Test not found or no permission" })),
        ),
    }
}

// --- coverage guard ---

/// `GET /mcp/coverage` — the Coverage Guard gate: declared surface vs. the cases
/// that were planned + executed. Sibling to Slop Guard, but for test coverage.
async fn coverage_report(State(state): State<AppState>) -> impl IntoResponse {
    let prd = state.prd.read().await.clone();
    let code_summary = code_summary_from_prd(&prd);
    let declared = coverage::declared_surface(&code_summary);

    // Coverage measures what was *executed*, not just planned: pull the test
    // entities the store recorded (only run cases are present), and only count a
    // surface element as covered when a PASSED test exercised it.
    let case_texts: Vec<String> = state
        .store
        .all()
        .await
        .iter()
        .filter(|e| e.get("testStatus").and_then(|s| s.as_str()) == Some("PASSED"))
        .map(|e| {
            format!(
                "{} {} {}",
                e.get("title").and_then(|v| v.as_str()).unwrap_or(""),
                e.get("description").and_then(|v| v.as_str()).unwrap_or(""),
                e.get("code").and_then(|v| v.as_str()).unwrap_or(""),
            )
        })
        .collect();

    let report = coverage::evaluate(&declared, &case_texts);
    Json(json!({
        "coveragePercent": report.percent(),
        "hasGaps": report.has_findings(),
        "declared": report.declared,
        "covered": report.covered,
        "findings": report.findings.iter().map(|f| json!({
            "rule": f.rule, "message": f.message, "target": f.target,
        })).collect::<Vec<_>>(),
        "report": coverage::format_report(&report),
        "mermaid": coverage::mermaid_diagram(&declared, &case_texts),
    }))
}

// --- misc sinks ---

async fn log_sink() -> impl IntoResponse {
    Json(json!({ "ok": true }))
}

async fn test_summary() -> impl IntoResponse {
    Json(json!({ "summary": "local run" }))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    // --- test harness (no committed helper exists on this base) ---------------

    /// A reqwest client with a hard per-request timeout so no await can hang.
    fn client() -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("reqwest client")
    }

    /// Serve any axum router on an ephemeral loopback port; returns its base URL.
    /// The serve task is detached (dropped handle) and torn down when the test's
    /// runtime shuts down.
    async fn spawn_app(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let _task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    /// The real API router over a fresh deterministic (no-LLM) backend state.
    async fn spawn_backend() -> String {
        spawn_app(router(AppState::new(None, TestKind::Backend))).await
    }

    /// A code summary declaring a single `GET /health` endpoint expecting 200.
    fn health_summary(base_url: &str) -> Value {
        json!({
            "project_name": "todo",
            "base_url": base_url,
            "api_endpoints": [
                { "method": "GET", "path": "/health", "expect_status": 200 }
            ]
        })
    }

    /// Poll `GET /mcp/project/test/{id}` until it leaves RUNNING or the bounded
    /// budget (50 * 50ms = 2.5s worst case) is exhausted.
    async fn poll_until_done(base: &str, id: &str) -> Value {
        let c = client();
        for _ in 0..50 {
            let resp = c
                .get(format!("{base}/mcp/project/test/{id}"))
                .send()
                .await
                .expect("poll get");
            let body: Value = resp.json().await.expect("poll json");
            let status = body
                .get("testStatus")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if status != "RUNNING" {
                return body;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("test {id} never left RUNNING within budget");
    }

    // --- account / tunnel gating no-ops -------------------------------------

    #[tokio::test]
    async fn health_me_and_tunnel_endpoints() {
        let base = spawn_backend().await;
        let c = client();

        let health: Value = c
            .get(format!("{base}/health"))
            .send()
            .await
            .expect("health")
            .json()
            .await
            .expect("health json");
        assert_eq!(health["env"], "local");
        assert_eq!(health["version"], "v0.0.0");

        let me: Value = c
            .get(format!("{base}/api/me"))
            .send()
            .await
            .expect("me")
            .json()
            .await
            .expect("me json");
        assert_eq!(me["id"], LOCAL_USER_ID);
        assert_eq!(me["subPlan"], "Local");

        let ver: Value = c
            .get(format!("{base}/api/tunnel/v2/version"))
            .send()
            .await
            .expect("tunnel version")
            .json()
            .await
            .expect("tunnel version json");
        assert_eq!(ver["version"], 2);

        let tunnel: Value = c
            .post(format!("{base}/api/tunnel/v2"))
            .send()
            .await
            .expect("tunnel create")
            .json()
            .await
            .expect("tunnel json");
        let id = tunnel["id"].as_str().expect("id str");
        let secret = tunnel["secret"].as_str().expect("secret str");
        assert!(Uuid::parse_str(id).is_ok(), "id is a uuid: {id}");
        assert!(
            Uuid::parse_str(secret).is_ok(),
            "secret is a uuid: {secret}"
        );
        assert_ne!(id, secret);
    }

    // --- generate-prd (multipart) -------------------------------------------

    #[tokio::test]
    async fn generate_prd_without_code_summary_is_400() {
        let base = spawn_backend().await;
        let form = reqwest::multipart::Form::new().text("other", "ignored");
        let resp = client()
            .post(format!("{base}/mcp/common/generate-prd"))
            .multipart(form)
            .send()
            .await
            .expect("prd post");
        assert_eq!(resp.status().as_u16(), 400);
        let body: Value = resp.json().await.expect("prd err json");
        assert_eq!(body["message"], "missing codeSummary");
    }

    #[tokio::test]
    async fn generate_prd_with_code_summary_is_201() {
        let base = spawn_backend().await;
        let summary = health_summary("http://127.0.0.1:9");
        let form = reqwest::multipart::Form::new()
            .text("codeSummary", serde_json::to_string(&summary).unwrap());
        let resp = client()
            .post(format!("{base}/mcp/common/generate-prd"))
            .multipart(form)
            .send()
            .await
            .expect("prd post");
        assert_eq!(resp.status().as_u16(), 201);
        let prd: Value = resp.json().await.expect("prd json");
        assert_eq!(prd["meta"]["project"], "todo");
        let features = prd["features"].as_array().expect("features array");
        assert_eq!(features.len(), 1);
        assert_eq!(features[0]["name"], "GET /health");
        assert!(
            prd["product_overview"]
                .as_str()
                .unwrap()
                .contains("endpoint")
        );
    }

    // --- plans ---------------------------------------------------------------

    #[tokio::test]
    async fn backend_plan_returns_cases_with_specs() {
        let base = spawn_backend().await;
        // A PRD carrying the attached code_summary the client sends back.
        let prd = json!({
            "meta": { "project": "todo" },
            "code_summary": {
                "api_endpoints": [
                    { "method": "get", "path": "/health", "expect_status": 200 },
                    { "method": "POST", "path": "/api/todos", "body": { "title": "x" }, "expect_status": 201 }
                ]
            }
        });
        let resp = client()
            .post(format!("{base}/mcp/backend-test/plan"))
            .json(&json!({ "prdContent": serde_json::to_string(&prd).unwrap() }))
            .send()
            .await
            .expect("plan post");
        assert_eq!(resp.status().as_u16(), 201);
        let body: Value = resp.json().await.expect("plan json");
        let plan = body["plan"].as_array().expect("plan array");
        assert_eq!(plan.len(), 2);
        let first = &plan[0];
        assert_eq!(first["id"], "TC001");
        assert!(first["title"].as_str().unwrap().contains("/health"));
        assert_eq!(first["spec"]["method"], "GET");
        assert_eq!(first["spec"]["path"], "/health");
        assert_eq!(first["spec"]["expect_status"], 200);
        assert_eq!(plan[1]["spec"]["method"], "POST");
        assert_eq!(plan[1]["spec"]["path"], "/api/todos");
    }

    #[tokio::test]
    async fn frontend_plan_returns_array() {
        let base = spawn_backend().await;
        let resp = client()
            .post(format!("{base}/mcp/frontend-test/generate-plan"))
            .json(&json!({
                "standard_prd": {
                    "api_endpoints": [
                        { "method": "GET", "path": "/health", "expect_status": 200 }
                    ]
                }
            }))
            .send()
            .await
            .expect("frontend plan post");
        assert_eq!(resp.status().as_u16(), 201);
        let plan: Value = resp.json().await.expect("frontend plan json");
        let arr = plan.as_array().expect("plan is an array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["id"], "TC001");
        assert_eq!(arr[0]["spec"]["path"], "/health");
    }

    // --- run + poll + coverage (end to end against a real fake app) ----------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn backend_run_executes_and_coverage_reflects_pass() {
        // A real target app: GET /health -> 200.
        let app_base = spawn_app(
            Router::new().route("/health", get(|| async { Json(json!({ "ok": true })) })),
        )
        .await;
        let base = spawn_backend().await;
        let c = client();

        // Plan against a PRD whose code_summary declares GET /health. This both
        // returns the cases (with embedded spec) and stores them + the PRD.
        let prd = json!({
            "meta": { "project": "todo" },
            "code_summary": {
                "api_endpoints": [
                    { "method": "GET", "path": "/health", "expect_status": 200 }
                ]
            }
        });
        let plan_body: Value = c
            .post(format!("{base}/mcp/backend-test/plan"))
            .json(&json!({ "prdContent": serde_json::to_string(&prd).unwrap() }))
            .send()
            .await
            .expect("plan post")
            .json()
            .await
            .expect("plan json");
        let plan = plan_body["plan"].as_array().expect("plan array").clone();
        assert_eq!(plan.len(), 1);

        // Run the plan against the real app via `endpoint`.
        let run: Value = c
            .post(format!("{base}/mcp/backend-test/run"))
            .json(&json!({ "endpoint": app_base, "testPlan": plan }))
            .send()
            .await
            .expect("run post")
            .json()
            .await
            .expect("run json");
        let ids = run["testIds"].as_array().expect("testIds array");
        assert_eq!(ids.len(), 1);
        let id = ids[0].as_str().expect("id str");

        let entity = poll_until_done(&base, id).await;
        assert_eq!(
            entity["testStatus"], "PASSED",
            "GET /health against the fake app should pass; entity={entity}"
        );
        assert_eq!(entity["testError"], "");
        // The recorded `code` is the qa-artifact JSON (request/response
        // evidence), not python source — assert it names the executed call.
        let code = entity["code"].as_str().unwrap();
        assert!(code.contains("GET"), "artifact records the method: {code}");
        assert!(code.contains("/health"), "artifact records the URL: {code}");

        // Coverage should now report the declared surface as fully covered.
        let cov: Value = c
            .get(format!("{base}/mcp/coverage"))
            .send()
            .await
            .expect("coverage get")
            .json()
            .await
            .expect("coverage json");
        for k in [
            "coveragePercent",
            "hasGaps",
            "declared",
            "covered",
            "findings",
            "report",
            "mermaid",
        ] {
            assert!(cov.get(k).is_some(), "coverage missing key {k}");
        }
        assert_eq!(cov["declared"].as_u64().unwrap(), 1);
        assert!(
            cov["covered"].as_u64().unwrap() >= 1,
            "a PASSED run covers the declared surface: {cov}"
        );
        assert_eq!(cov["hasGaps"], false);
        assert!((cov["coveragePercent"].as_f64().unwrap() - 100.0).abs() < 1e-9);
        assert!(cov["mermaid"].as_str().unwrap().contains("graph LR"));
    }

    #[tokio::test]
    async fn get_missing_test_returns_error_body() {
        let base = spawn_backend().await;
        let resp = client()
            .get(format!("{base}/mcp/project/test/missing-id"))
            .send()
            .await
            .expect("get missing");
        assert_eq!(resp.status().as_u16(), 200);
        let body: Value = resp.json().await.expect("missing json");
        assert!(body.get("error").and_then(|v| v.as_str()).is_some());
    }

    // --- misc sinks ----------------------------------------------------------

    #[tokio::test]
    async fn log_sinks_and_test_summary() {
        let base = spawn_backend().await;
        let c = client();

        for path in ["/mcp/common/log", "/mcp/common/log-batch"] {
            let body: Value = c
                .post(format!("{base}{path}"))
                .json(&json!({ "level": "info", "message": "hi" }))
                .send()
                .await
                .expect("log post")
                .json()
                .await
                .expect("log json");
            assert_eq!(body["ok"], true);
        }

        let summary: Value = c
            .post(format!("{base}/mcp/common/generate-test-summary"))
            .json(&json!({ "tests": [] }))
            .send()
            .await
            .expect("summary post")
            .json()
            .await
            .expect("summary json");
        assert_eq!(summary["summary"], "local run");
    }

    // --- private-fn unit coverage -------------------------------------------

    #[test]
    fn code_summary_from_prd_prefers_embedded_then_falls_back() {
        let with = json!({ "code_summary": { "api_endpoints": [] }, "meta": { "x": 1 } });
        assert_eq!(code_summary_from_prd(&with), json!({ "api_endpoints": [] }));

        let without = json!({ "features": [], "meta": { "x": 1 } });
        assert_eq!(code_summary_from_prd(&without), without);
    }

    #[test]
    fn deterministic_cases_builds_specs_from_endpoints() {
        let prd = json!({
            "code_summary": {
                "api_endpoints": [
                    { "method": "get", "path": "/health", "expect_status": 200 },
                    { "method": "POST", "path": "/api/todos", "body": { "title": "x" }, "expect_status": 201 }
                ]
            }
        });
        let cases = deterministic_cases(&prd);
        assert_eq!(cases.len(), 2);
        assert_eq!(cases[0]["id"], "TC001");
        assert_eq!(cases[0]["spec"]["method"], "GET");
        assert_eq!(cases[0]["spec"]["path"], "/health");
        assert_eq!(cases[1]["id"], "TC002");
        assert_eq!(cases[1]["spec"]["method"], "POST");
        assert_eq!(cases[1]["spec"]["body"], json!({ "title": "x" }));

        // Top-level api_endpoints (no code_summary key) work via the fallback.
        let flat = json!({ "api_endpoints": [ { "method": "GET", "path": "/ping" } ] });
        let flat_cases = deterministic_cases(&flat);
        assert_eq!(flat_cases.len(), 1);
        assert_eq!(flat_cases[0]["spec"]["path"], "/ping");

        // No endpoints -> no cases.
        assert!(deterministic_cases(&json!({})).is_empty());
    }

    #[tokio::test]
    async fn resolve_base_prefers_endpoint_then_target_then_default() {
        let state = AppState::new(None, TestKind::Backend);

        // Endpoint in the run body wins outright.
        assert_eq!(
            resolve_base(&state, &json!({ "endpoint": "http://ep:1" })).await,
            "http://ep:1"
        );

        // No default target -> localhost fallback.
        assert_eq!(
            resolve_base(&state, &json!({})).await,
            "http://localhost:8080"
        );

        // With a target_base set, it beats the localhost default...
        *state.target_base.write().await = Some("http://tb:2".to_string());
        assert_eq!(resolve_base(&state, &json!({})).await, "http://tb:2");

        // ...but the run body's endpoint still wins over target_base.
        assert_eq!(
            resolve_base(&state, &json!({ "endpoint": "http://ep:1" })).await,
            "http://ep:1"
        );
    }
}
