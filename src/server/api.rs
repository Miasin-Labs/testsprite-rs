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

use super::engine;
use super::llm::LlmClient;
use super::store::{self, Runnable, Store};

const LOCAL_USER_ID: &str = "00000000-0000-0000-0000-000000000000";

#[derive(Clone)]
pub struct AppState {
    pub store: Store,
    /// Maps a planned case id -> how to run it (deterministic spec or LLM Python).
    pub plans: Arc<RwLock<HashMap<String, Runnable>>>,
    /// Maps a planned case id -> the case JSON (for LLM code generation on /run).
    pub cases: Arc<RwLock<HashMap<String, Value>>>,
    /// The PRD most recently generated (LLM code-gen context).
    pub prd: Arc<RwLock<Value>>,
    /// Where the local app under test listens (for direct execution).
    pub target_base: Arc<RwLock<Option<String>>>,
    /// Optional OpenAI client. When present, the backend behaves like the real
    /// cloud (LLM PRD/plan/code-gen). When absent, the deterministic engine runs.
    pub llm: Option<LlmClient>,
}

impl AppState {
    pub fn new(llm: Option<LlmClient>) -> Self {
        Self {
            store: Store::new(),
            plans: Arc::new(RwLock::new(HashMap::new())),
            cases: Arc::new(RwLock::new(HashMap::new())),
            prd: Arc::new(RwLock::new(json!({}))),
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

/// Build a plan from a PRD, storing per-case runnables (deterministic) or case
/// JSON (LLM, code generated lazily on /run). Returns the client-facing cases.
async fn build_plan(state: &AppState, prd: &Value) -> Vec<Value> {
    *state.prd.write().await = prd.clone();

    if let Some(llm) = &state.llm {
        if let Ok(plan) = llm.generate_plan(prd).await {
            let mut cases_store = state.cases.write().await;
            let mut out = Vec::new();
            for case in &plan {
                if let Some(id) = case.get("id").and_then(|v| v.as_str()) {
                    cases_store.insert(id.to_string(), case.clone());
                    out.push(case.clone());
                }
            }
            return out;
        }
        tracing::warn!("LLM plan failed; using deterministic engine");
    }

    // Deterministic fallback.
    let cs = code_summary_from_prd(prd);
    let plan = engine::plan_from_code_summary(&cs);
    let mut plans = state.plans.write().await;
    plan.iter()
        .map(|c| {
            plans.insert(c.id.clone(), Runnable::Spec(c.spec.clone()));
            json!({ "id": c.id, "title": c.title, "description": c.description })
        })
        .collect()
}

// --- run ---

/// `POST /mcp/backend-test/run` — create test entities, kick off direct
/// execution, return the test ids the client will poll.
async fn backend_run(State(state): State<AppState>, Json(body): Json<Value>) -> impl IntoResponse {
    let project_id = Uuid::new_v4().to_string();
    let base_url = resolve_base(&state, &body).await;

    // The client sends the (possibly filtered) testPlan back to us.
    let plan = body
        .get("testPlan")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let prd = state.prd.read().await.clone();

    let mut to_run: Vec<(String, Runnable)> = Vec::new();
    let mut ids: Vec<String> = Vec::new();
    for case in &plan {
        let case_id = case.get("id").and_then(|v| v.as_str()).unwrap_or("TC");
        let title = case.get("title").and_then(|v| v.as_str()).unwrap_or("test");
        let description = case
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let Some(runnable) = resolve_runnable(&state, case_id, case, &prd, &base_url).await else {
            continue;
        };
        let test_id = Uuid::new_v4().to_string();
        let entity =
            store::new_running_entity(&project_id, &test_id, LOCAL_USER_ID, title, description);
        state.store.insert(test_id.clone(), entity).await;
        to_run.push((test_id.clone(), runnable));
        ids.push(test_id);
    }

    store::spawn_execution(state.store.clone(), base_url, to_run);
    (StatusCode::CREATED, Json(json!({ "testIds": ids })))
}

/// Resolve how to run a case: a stored deterministic spec, or LLM-generated
/// Python (generated on demand using the case + PRD).
async fn resolve_runnable(
    state: &AppState,
    case_id: &str,
    case: &Value,
    prd: &Value,
    base_url: &str,
) -> Option<Runnable> {
    if let Some(r) = state.plans.read().await.get(case_id) {
        return Some(r.clone());
    }
    let llm = state.llm.as_ref()?;
    match llm.generate_test_code(case, prd, base_url).await {
        Ok(code) => Some(Runnable::Python(code)),
        Err(e) => {
            tracing::warn!("LLM code-gen failed for {case_id}: {e}");
            None
        }
    }
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

// --- misc sinks ---

async fn log_sink() -> impl IntoResponse {
    Json(json!({ "ok": true }))
}

async fn test_summary() -> impl IntoResponse {
    Json(json!({ "summary": "local run" }))
}
