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
