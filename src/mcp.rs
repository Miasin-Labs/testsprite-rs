//! Stdio MCP server built on the official `rmcp` SDK. The SDK owns the
//! JSON-RPC framing, the initialize/version handshake, ping, and error
//! envelope; this module supplies the tool list ([`tool_list`]) and the
//! dispatch ([`call_tool`]). The tool surface mirrors the plugin's `index.ts`.

use anyhow::Result;
use rmcp::model::{
    CallToolRequestParams,
    CallToolResponse,
    CallToolResult,
    ContentBlock,
    ErrorData,
    Implementation,
    ListToolsResult,
    PaginatedRequestParams,
    ProtocolVersion,
    ServerCapabilities,
    ServerInfo,
    Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::transport::stdio;
use rmcp::{ServerHandler, ServiceExt};
use serde_json::{Value, json};

use crate::tools;

const SERVER_NAME: &str = "testsprite-rs-mcp-server";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The local TestSprite MCP tools: generate/run/store/list/delete test cases,
/// the one-call `loop`, coverage, emit/rename/triage/flaky/history, plus the 3
/// conversational-agent tools. (Cloud tools that need a TestSprite account are
/// intentionally not advertised.)
fn tool_list() -> Value {
    let mut tools = base_tool_list();
    tools.extend(local_extension_tools());
    if flow_backend_configured() {
        tools.extend(cloud_flow_tools());
    }
    json!({ "tools": tools })
}

/// Whether a TestSprite backend is configured — cloud (an API key is set) or
/// the local `testsprite-rs backend` stand-in (a non-production `API_URL`).
/// When neither is present the account-gated official-flow tools stay hidden,
/// so a local no-account client only sees tools that actually work. This is
/// the discoverable version of the old "cloud tools intentionally not
/// advertised" policy: the moment a backend exists, the whole flow appears in
/// tools/list with real schemas instead of being callable only by name.
fn flow_backend_configured() -> bool {
    crate::envs::api_key().is_some()
        || std::env::var("API_URL").is_ok_and(|u| !u.contains("api.testsprite.com"))
}

/// The always-available local tool surface (no TestSprite account required).
fn base_tool_list() -> Vec<Value> {
    let Value::Array(tools) = json!([
            { "name": "testsprite_generate_code_summary",
              "description": "Scan the repo and write TestSprite's code summary (tech_stack, features/files, api_endpoints) to testsprite_tests/tmp/code_summary.yaml by default. This is the official first step before normalized PRD/test-plan generation.",
              "inputSchema": obj_schema(&[("path","string"),("out","string")]) },
            { "name": "testsprite_generate",
              "description": "Generate local test cases. from=<file>: a runnable code summary (top-level api_endpoints) → deterministic backend spec cases with NO OpenAI key; OR any real/loose TestSprite standard_prd.json → tolerant ingestion that recovers endpoints hidden under code_summary.features / security.*_endpoints / apis and auto-seeds testCredentials + test_environment into variables.json (non-clobbering). doc=<file-or-URL>: a Postman collection, OpenAPI/Swagger spec (incl. a utoipa/served /api-docs/openapi.json URL), or HAR → deterministic spec cases with NO OpenAI key; README/notes/Jira → LLM PRD. changed=true (since, default HEAD) generates only for functions changed since a git ref. cover=true generates one test per uncovered function under path (default repo root); iterate>1 re-measures coverage each round.",
              "inputSchema": obj_schema(&[("instruction","string"),("from","string"),("doc","string"),("type","string"),("model","string"),("changed","boolean"),("since","string"),("cover","boolean"),("path","string"),("iterate","number")]) },
            { "name": "testsprite_explore",
              "description": "Autonomous exploratory frontend QA: open a live page with Playwright, inventory visible inputs/buttons/links/headings, and generate deterministic frontend planSteps candidates. Set store=true to add them to the local test DB. interactions=true also clicks visible controls on fresh pages and generates action+assertion candidates (opt-in because clicks can mutate state).",
              "inputSchema": obj_schema(&[("url","string"),("store","boolean"),("depth","number"),("limit","number"),("interactions","boolean")]) },
            { "name": "testsprite_audit",
              "description": "Use TestSprite's LLM to adversarially propose high-signal QA tests from code summary, stored tests, latest results, and coverage gaps. Set store=true to persist proposed cases. `model` may be comma-separated (e.g. gpt-5.3-codex,gpt-5.5) to run side-by-side and merge.",
              "inputSchema": obj_schema(&[("path","string"),("model","string"),("store","boolean")]) },
            { "name": "testsprite_run",
              "description": "Run local tests: execute + LLM failure analysis; set fix=true to also write a repair patch. Set changed=true to run ONLY the tests affected by files changed since a git ref (since, default HEAD). Set serve=true to start the target app (`project set-start`) before running so backend/spec cases hit a live server. url overrides the project target URL for this run; browser picks the Playwright browser (chromium|firefox|webkit); group runs only a named list; jobs sets run concurrency.",
              "inputSchema": obj_schema(&[("id","string"),("model","string"),("fix","boolean"),("changed","boolean"),("since","string"),("serve","boolean"),("require_approved_prd","boolean"),("url","string"),("browser","string"),("group","string"),("jobs","number")]) },
            { "name": "testsprite_loop",
              "description": "The regression loop in ONE call — the agent-facing 'run the whole surface after every change and hand the breaks back'. Optionally generates tests for changed functions (generate:true + changed:true), runs the suite (the changed subset when changed:true, else all — and on an unattributable change it runs everything rather than reporting an empty green), triages failures into root-cause clusters, and returns one actionable report: {selection, total, passed, failed, blocked, failures:[{id,title,verdict,failureKind,cause}], clusters, next_action, green}. `blocked` (auth/network/infra) is counted apart from real `failed`. Prefer this over calling generate/run/triage separately.",
              "inputSchema": obj_schema(&[("changed","boolean"),("since","string"),("generate","boolean"),("model","string"),("fix","boolean"),("serve","boolean"),("require_approved_prd","boolean")]) },
            { "name": "testsprite_store_test",
              "description": "Store a test YOU already wrote so testsprite can run + track it deterministically (no LLM). Backend: provide `spec` or `steps` for HTTP/OAuth/GraphQL QA flows. Frontend: provide `planSteps` (fill/click/assert/navigate actions) for Playwright-driven UI QA with screenshots. Or provide `code` for python/rust/command tests. Prefer this over testsprite_generate when you can write the test yourself.",
              "inputSchema": json!({ "type": "object", "properties": {
                  "title": {"type": "string"},
                  "kind": {"type": "string", "enum": ["backend","frontend","mcp","rust","command"]},
                  "description": {"type": "string"},
                  "code": {"type": "string"},
                  "spec": {"type": "object", "description": "One HTTP QA step: {method,path,expect_status?,headers?,auth?,body?|form?,expect_json?,expect_body?,expect_parses?,save?,then?,graphql?}. expect_status is an exact code (200) or a band: \"success\" (2xx/3xx default), \"accepted\" (2xx/3xx or 400/422), or \"any\" (<500). auth:{bearer:\"${accessToken}\"} attaches a bearer from the per-test session; project variables come from .testsprite.env (gitignored), testsprite_tests/variables.json, or process env. save:{accessToken:\"$.access_token\", code:\"header.location.query.code\"} captures values for later ${var} interpolation. graphql:{query,variables?,operationName?,expect_no_errors?,expect_data?} is shorthand for POST /graphql and fails on GraphQL errors by default. then:{...} chains a follow-up read-after-write check."},
                  "steps": {"type": "array", "description": "Multi-step QA flow: array of the same HTTP step shape as spec. Steps share a session map, so OAuth/login can save a token and later REST/GraphQL steps can use ${accessToken}. Run artifacts record sanitized request/response evidence for every step."},
                  "planSteps": {"type": "array", "description": "Frontend UI steps for kind:\"frontend\". Strings like \"Input Email: ${EMAIL}\", \"Input Password: ${PASSWORD}\", \"Click Sign In\", \"Verify: Dashboard\" or objects like {action:\"fill\",selector:\"#email\",value:\"${EMAIL}\"}, {action:\"click\",text:\"Sign In\"}, {action:\"assert_text\",text:\"Dashboard\"}. Values interpolate from .testsprite.env/variables/process env. The browser executor turns them into Playwright and captures screenshots."}
              }}) },
            { "name": "testsprite_list_tests",
              "description": "List every stored test as {id,title,kind}. Use this to map the opaque ids other tools return back to what they actually are — no need to run anything or shell out to the CLI.",
              "inputSchema": obj_schema(&[]) },
            { "name": "testsprite_get_test",
              "description": "Get one stored test definition as the exact JSON shape the executor consumes (spec/steps/planSteps flattened at top level).",
              "inputSchema": obj_schema(&[("id","string")]) },
            { "name": "testsprite_delete_test",
              "description": "Delete a stored test and its run history by id. Use it to prune duplicate/munged auto-generated cases; storing is an upsert, so without this the suite only grows.",
              "inputSchema": obj_schema(&[("id","string")]) },
            { "name": "testsprite_coverage_gaps",
              "description": "List functions in the code surface that no stored test references yet. NOTE: matching is by function-NAME mention in a stored test's text, not by execution — it cannot see your repo's own cargo/pytest tests, and a name-drop counts. Treat it as a to-write worklist, not a coverage measurement, and do not chase it to zero.",
              "inputSchema": obj_schema(&[("path","string")]) },
            { "name": "testsprite_emit_test",
              "description": "Materialize a stored test into a repo file (e.g. crates/foo/tests/bar.rs, or tests/test_api.py for a spec case) so cargo/CI own it — the repo-native alternative to ephemeral SQLite runs. `command` tests cannot be emitted (their code is a shell line, not source).",
              "inputSchema": obj_schema(&[("id","string"),("out","string")]) },
            { "name": "testsprite_materialize_tests",
              "description": "Bulk-write stored tests into TestSprite-style cwd files under testsprite_tests/ by default: TC001_Title.py/js/json/sh. Use this after generation when the user expects visible files, not only SQLite/run history.",
              "inputSchema": obj_schema(&[("id","array"),("out","string")]) },
            { "name": "testsprite_rename_test",
              "description": "Rename a stored test's title. Use this to fix the munged/duplicate TC000 names auto-generation produces — give each test a meaningful, unique name.",
              "inputSchema": obj_schema(&[("id","string"),("title","string")]) },
            { "name": "testsprite_agent_message",
              "description": "Talk to the local test agent: it proposes ONE action (generate/run) to approve. Set auto_approve:true to execute the proposed action immediately (no separate approve step).",
              "inputSchema": obj_schema(&[("conversation_id","string"),("message","string"),("model","string"),("auto_approve","boolean")]) },
            { "name": "testsprite_agent_approve",
              "description": "Approve (or reject) a pending agent action by id; executes the pipeline.",
              "inputSchema": obj_schema(&[("conversation_id","string"),("action_id","number"),("approve","boolean"),("model","string")]) },
            { "name": "testsprite_agent_history",
              "description": "Show a conversation's messages and pending actions.",
              "inputSchema": obj_schema(&[("conversation_id","string")]) },
            { "name": "testsprite_triage",
              "description": "Group the failing tests by root cause (failureKind) into clusters so you fix the few underlying problems instead of N symptoms. Clusters are ranked most-debuggable-first by a divergence score (sharp \"expected X got Y\" failures above diffuse ones).",
              "inputSchema": obj_schema(&[]) },
            { "name": "testsprite_guidelines",
              "description": "Distill this project's recurring failures (from run history) into deterministic do/don't guidelines. These are also auto-injected into coverage/changed generation prompts to stop the model repeating past mistakes.",
              "inputSchema": obj_schema(&[]) },
            { "name": "testsprite_bench",
              "description": "Per-model telemetry scoreboard from run history: pass rate, average latency, and token spend per model, plus drift vs a saved baseline. Set save_baseline:true to snapshot the current scoreboard as the drift baseline.",
              "inputSchema": obj_schema(&[("save_baseline","boolean")]) },
            { "name": "testsprite_flaky",
              "description": "Replay a stored test N times (default 5) and report a stability score; blocked runs (auth/network/infra) are excluded from the denominator, not scored as flaky. Set serve:true to start the target app first — otherwise a backend test with nothing listening scores every run blocked and reports \"inconclusive\".",
              "inputSchema": obj_schema(&[("id","string"),("runs","number"),("model","string"),("serve","boolean")]) },
            { "name": "testsprite_run_history",
              "description": "Show a stored test's full run history (append-only): every recorded run newest-first with pass/fail, verdict, failureKind, and timestamp. Use it to spot regressions and intermittent failures over time.",
              "inputSchema": obj_schema(&[("id","string")]) },
            { "name": "testsprite_artifact_get",
              "description": "Export one run's evidence bundle by numeric run_id (from testsprite_run_history): run.json, qa-artifact.json/executed-artifact.txt, and screenshots when present.",
              "inputSchema": obj_schema(&[("run_id","number"),("out","string")]) },
            { "name": "testsprite_report",
              "description": "Write a latest-results report in TestSprite's official format: a Requirement Validation Summary grouped by each result's requirement, per-failure Severity (HIGH/MEDIUM/LOW derived from failureKind), a per-requirement Coverage & Matching Metrics matrix, and a Key Gaps / Risks section. Markdown by default, PDF when out ends with .pdf, JSON when json=true.",
              "inputSchema": obj_schema(&[("out","string"),("json","boolean")]) },
            { "name": "testsprite_dashboard",
              "description": "Write a static local dashboard HTML: pass counts, stored test list, latest status, and failure clusters.",
              "inputSchema": obj_schema(&[("out","string")]) },
            { "name": "testsprite_prd_review",
              "description": "Write an HTML review page for a stored PRD + generated test plan. Use before approving/running large generated suites.",
              "inputSchema": obj_schema(&[("id","string"),("out","string")]) },
            { "name": "testsprite_prd_approve",
              "description": "Mark a stored PRD/test plan as reviewed and approved; records approvedAt in the local DB.",
              "inputSchema": obj_schema(&[("id","string")]) },
            { "name": "testsprite_plan_put",
              "description": "Replace a frontend test's planSteps with a JSON array. Use this like the dashboard's Update/Re-generate Steps control.",
              "inputSchema": obj_schema(&[("id","string"),("steps","array")]) },
            { "name": "testsprite_replay",
              "description": "Write a visual replay HTML for a frontend test's stored planSteps and screenshots.",
              "inputSchema": obj_schema(&[("id","string"),("out","string")]) },
    ]) else {
        unreachable!("base_tool_list literal is a JSON array")
    };
    tools
}

/// Local tools that need no TestSprite account and are always advertised:
/// Wave D tolerant PRD ingestion plus the project-config surface an MCP-only
/// agent needs to bootstrap a project (create project.json, seed route/credential
/// variables, set the app start command) — capabilities that previously existed
/// only on the CLI.
fn local_extension_tools() -> Vec<Value> {
    let Value::Array(tools) = json!([
            { "name": "testsprite_ingest_prd",
              "description": "Ingest a loose/real TestSprite standard_prd.json of ANY shape: normalize it, recover endpoints hidden under code_summary.features / security.*_endpoints / apis / per-feature api_doc, and seed testCredentials ({role}_username/password/role) + test_environment (frontend_url/backend_api) into testsprite_tests/variables.json WITHOUT overwriting existing values. persist=true also stores the normalized PRD + recovered deterministic plan so test run/report work against it immediately. This is the local, no-account version of the official generate-PRD step for third-party PRDs.",
              "inputSchema": obj_schema(&[("file","string"),("persist","boolean")]) },
            { "name": "testsprite_project_init",
              "description": "Create/update testsprite_tests/project.json: the modality (type: backend|frontend|mcp|rust), project name, and target URL tests run against. Required before test run/loop can resolve a base URL.",
              "inputSchema": obj_schema(&[("type","string"),("name","string"),("url","string")]) },
            { "name": "testsprite_project_show",
              "description": "Print the current testsprite_tests/project.json (name, kind, targetUrl, startCommand).",
              "inputSchema": obj_schema(&[]) },
            { "name": "testsprite_project_set_var",
              "description": "Set a variable in testsprite_tests/variables.json — a path-param ({id}->value), a credential, or any ${VAR} a spec/planStep interpolates. Values here override .testsprite.env. Use this to seed auth tokens or route ids an MCP-driven suite needs.",
              "inputSchema": obj_schema(&[("key","string"),("value","string")]) },
            { "name": "testsprite_project_set_start",
              "description": "Set the shell command that starts the target app, used by testsprite_run/testsprite_loop with serve=true to bring up a live server before backend/spec cases run.",
              "inputSchema": obj_schema(&[("command","string")]) },
            { "name": "testsprite_gate",
              "description": "Run the stored suite as a CI gate: writes junit.xml + gate-summary.json, best-effort posts a PR comment via gh, and returns {total,passed,failed,exit_code,mutation?,log}. smoke=true runs one representative case per group first and only escalates on green. min_mutation=<0-100> also fails the gate when the mutation kill score is below that floor (weak oracles, not just failing tests).",
              "inputSchema": obj_schema(&[("url","string"),("model","string"),("smoke","boolean"),("min_mutation","number")]) },
            { "name": "testsprite_lint",
              "description": "Validate every stored test offline (no network/LLM): flags malformed backend specs, empty/unrunnable cases, and vacuous oracles. Returns {checked,valid,issues:[{file,field,reason}]}.",
              "inputSchema": obj_schema(&[]) },
            { "name": "testsprite_changed",
              "description": "Code Diff Mode report: which functions changed since a git ref (since, default HEAD) and which stored tests they affect. Returns {since,changedUnits,selection,affected}. Read-only — does not run anything.",
              "inputSchema": obj_schema(&[("since","string")]) },
            { "name": "testsprite_diff",
              "description": "Compare two stored test results (latest run per id) offline: returns {runA,runB,verdictChanged,failureKindChanged}. Use it to check whether a fix changed a test's verdict.",
              "inputSchema": obj_schema(&[("a","string"),("b","string")]) },
            { "name": "testsprite_coverage",
              "description": "Structural (tree-sitter) + Rust (cargo llvm-cov) coverage surface: returns {languages,functions,uncovered,rust}. This is the full report; testsprite_coverage_gaps is the name-matched worklist subset.",
              "inputSchema": obj_schema(&[("path","string")]) },
            { "name": "testsprite_mutation",
              "description": "ORACLE STRENGTH: run cargo-mutants over the Rust crate at path and return the kill score {caught,missed,unviable,timeout,kill_score,survivors}. A green, high-coverage suite can still catch zero seeded bugs — surviving mutants name the assertions to strengthen.",
              "inputSchema": obj_schema(&[("path","string")]) },
            { "name": "testsprite_scaffold",
              "description": "Emit a schema-correct starter test (kind: backend → a pytest requests file; frontend → a plan-input JSON) so you have a valid shape to fill in. Returns the scaffold object.",
              "inputSchema": obj_schema(&[("kind","string")]) },
            { "name": "testsprite_release",
              "description": "Reinstate a quarantined test (clear its suspect-oracle marker) so it runs with the whole suite again. The counterpart to the acceptance gate's quarantine.",
              "inputSchema": obj_schema(&[("id","string")]) },
            { "name": "testsprite_revisions",
              "description": "Show a stored test's prior definitions (pre-heal rewrites), newest-first: {revId,createdAt,reason,body}. Use it to see how --heal or re-generation changed a test.",
              "inputSchema": obj_schema(&[("id","string")]) },
            { "name": "testsprite_prune",
              "description": "Prune run history, keeping the latest `keep` runs per test (bounds testsprite.db). Pass id to prune one test's history, omit it to prune every test. Returns the number of run rows deleted.",
              "inputSchema": obj_schema(&[("id","string"),("keep","number")]) },
            { "name": "testsprite_export",
              "description": "Export every stored test definition as a JSON array (for committing the suite to version control). Returns {count,tests}.",
              "inputSchema": obj_schema(&[]) },
            { "name": "testsprite_import",
              "description": "Import test definitions (upsert by id) from an inline `tests` array or a `file` path to a JSON array. Returns the imported ids.",
              "inputSchema": obj_schema(&[("file","string"),("tests","array")]) },
            { "name": "testsprite_prd_list",
              "description": "List generated PRDs newest-first: {id,features,cases,createdAt,approvedAt,source}. Map a prdId stamped on a generated test back to the PRD it came from.",
              "inputSchema": obj_schema(&[]) },
            { "name": "testsprite_prd_show",
              "description": "Show one stored PRD's full JSON (requirements + attached plan). id optional — defaults to the latest PRD.",
              "inputSchema": obj_schema(&[("id","string")]) },
    ]) else {
        unreachable!("local_extension_tools literal is a JSON array")
    };
    tools
}

/// The official TestSprite FLOW tools. They route through the backend
/// (cloud api.testsprite.com, or the local `testsprite-rs backend` stand-in),
/// so they are advertised only when [`flow_backend_configured`] is true. The
/// canonical order is bootstrap -> generate_code_summary -> generate_standardized_prd
/// -> generate_{frontend,backend}_test_plan -> generate_code_and_execute -> report.
fn cloud_flow_tools() -> Vec<Value> {
    let Value::Array(tools) = json!([
            { "name": "testsprite_bootstrap",
              "description": "FLOW step 1: initialize a project for the official TestSprite pipeline — write testsprite_tests/tmp/config.json (status/scope/type/localEndpoint) and the .gitignore entry. Returns a next_action to generate the code summary. localPort is the port the app under test listens on; type is frontend|backend; testScope is codebase|diff.",
              "inputSchema": obj_schema(&[("localPort","number"),("pathname","string"),("type","string"),("testScope","string"),("projectPath","string")]) },
            { "name": "testsprite_check_account_info",
              "description": "Verify the configured TestSprite backend/account (GET /api/me). Returns firstName/lastName/email/subPlan/credits; the local stand-in returns a fixed local account. Use it to confirm the FLOW backend is reachable before running the pipeline.",
              "inputSchema": obj_schema(&[]) },
            { "name": "testsprite_generate_standardized_prd",
              "description": "FLOW step 3: generate the normalized standard_prd.json from the code summary (+ any raw PRDs dropped in testsprite_tests/tmp/prd_files). Requires the code summary to exist (run testsprite_generate_code_summary first). Returns a next_action to the matching test-plan generator.",
              "inputSchema": obj_schema(&[("projectPath","string")]) },
            { "name": "testsprite_generate_frontend_test_plan",
              "description": "FLOW step 4 (frontend): generate testsprite_frontend_test_plan.json (an array of UI test cases) from the standardized PRD via the backend.",
              "inputSchema": obj_schema(&[("projectPath","string")]) },
            { "name": "testsprite_generate_backend_test_plan",
              "description": "FLOW step 4 (backend): generate testsprite_backend_test_plan.json (an array of API test cases with per-endpoint specs) from the standardized PRD via the backend.",
              "inputSchema": obj_schema(&[("projectPath","string")]) },
            { "name": "testsprite_generate_code_and_execute",
              "description": "FLOW step 5: run the generated test plan through the backend (tunnel -> dispatch -> poll), writing testsprite_tests/tmp/test_results.json + raw_report.md. Returns a next_action steering the host to run the execute subcommand. Follow with testsprite_report for the requirement-grouped report.",
              "inputSchema": obj_schema(&[("projectPath","string")]) },
    ]) else {
        unreachable!("cloud_flow_tools literal is a JSON array")
    };
    tools
}

fn obj_schema(fields: &[(&str, &str)]) -> Value {
    let mut props = serde_json::Map::new();
    for (name, ty) in fields {
        props.insert((*name).to_string(), json!({ "type": ty }));
    }
    json!({ "type": "object", "properties": props })
}

fn arg_model(args: &Value) -> std::borrow::Cow<'_, str> {
    args.get("model")
        .and_then(|v| v.as_str())
        .map(std::borrow::Cow::Borrowed)
        .unwrap_or_else(|| std::borrow::Cow::Owned(crate::envs::default_model()))
}

/// Dispatch a `tools/call` to the matching implementation.
async fn call_tool(name: &str, args: &Value) -> Result<Value> {
    let project_path = args
        .get("projectPath")
        .or_else(|| args.get("projectRootPath"))
        .and_then(|v| v.as_str())
        .unwrap_or(".")
        .to_string();

    match name {
        "testsprite_check_account_info" => Ok(tools::account::check_account_info().await),
        "testsprite_generate_standardized_prd" => {
            tools::prd::generate_standard_prd(&project_path).await
        }
        "testsprite_generate_frontend_test_plan" => {
            tools::plan::generate_frontend_test_plan(&project_path).await
        }
        "testsprite_generate_backend_test_plan" => {
            tools::plan::generate_backend_test_plan(&project_path).await
        }
        "testsprite_generate_code_and_execute" => {
            let exe = std::env::current_exe()?.to_string_lossy().to_string();
            Ok(tools::execute::mcp_next_action(&project_path, &exe))
        }
        "testsprite_generate_code_summary" => {
            let root = std::env::current_dir()?;
            let scan = args
                .get("path")
                .and_then(|v| v.as_str())
                .map(std::path::PathBuf::from)
                .unwrap_or(root);
            let out = args
                .get("out")
                .and_then(|v| v.as_str())
                .map(std::path::PathBuf::from);
            let (path, summary) = crate::local::summary::write(&scan, out.as_deref())?;
            Ok(json!({ "path": path, "summary": summary }))
        }
        "testsprite_bootstrap" => bootstrap(&project_path, args).await,
        "testsprite_ingest_prd" => {
            let root = std::env::current_dir()?;
            let file = args
                .get("file")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("missing required argument: file"))?;
            let persist = args
                .get("persist")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let r =
                crate::local::generate::ingest_prd_file(&root, std::path::Path::new(file), persist)
                    .await?;
            Ok(json!({
                "endpoints": r.endpoints,
                "requirements": r.requirements,
                "credentials": r.credentials,
                "timingRules": r.timing_rules,
                "hasTestDataStrategy": r.has_test_data_strategy,
                "seededVars": r.seeded_vars,
                "prdId": r.prd_id,
                "planIds": r.plan_ids,
            }))
        }
        "testsprite_project_init" => {
            let root = std::env::current_dir()?;
            let kind = args
                .get("type")
                .and_then(|v| v.as_str())
                .map(crate::server::executors::TestKind::parse)
                .unwrap_or(crate::server::executors::TestKind::Backend);
            let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("local");
            let url = args.get("url").and_then(|v| v.as_str());
            let path = crate::local::project::init(&root, kind, name, url).await?;
            Ok(json!({ "wrote": path }))
        }
        "testsprite_project_show" => {
            let root = std::env::current_dir()?;
            let project = crate::local::project::load(&root).await?;
            Ok(serde_json::to_value(project)?)
        }
        "testsprite_project_set_var" => {
            let root = std::env::current_dir()?;
            let key = args
                .get("key")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("missing required argument: key"))?;
            let value = args
                .get("value")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("missing required argument: value"))?;
            let vars = crate::local::project::set_variable(&root, key, value)?;
            Ok(json!({ "key": key, "value": value, "total": vars.len() }))
        }
        "testsprite_project_set_start" => {
            let root = std::env::current_dir()?;
            let command = args
                .get("command")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("missing required argument: command"))?;
            crate::local::project::set_start(&root, command).await?;
            Ok(json!({ "startCommand": command }))
        }
        "testsprite_gate" => {
            let root = std::env::current_dir()?;
            let model = arg_model(args);
            let out = crate::local::gate::gate_run(
                &root,
                crate::local::gate::GateOpts {
                    url: args.get("url").and_then(|v| v.as_str()),
                    model: model.as_ref(),
                    smoke: args.get("smoke").and_then(|v| v.as_bool()).unwrap_or(false),
                    min_mutation: args.get("min_mutation").and_then(|v| v.as_f64()),
                },
            )
            .await?;
            Ok(serde_json::to_value(out)?)
        }
        "testsprite_lint" => {
            let root = std::env::current_dir()?;
            crate::local::lint::lint_data(&root).await
        }
        "testsprite_changed" => {
            let root = std::env::current_dir()?;
            let since = args.get("since").and_then(|v| v.as_str()).unwrap_or("HEAD");
            let cs = crate::local::changed::changed_surface(&root, since)?;
            let affected = crate::local::changed::affected_test_ids(&root, &cs).await?;
            let units: Vec<&str> = cs.units.iter().map(|u| u.name.as_str()).collect();
            Ok(json!({
                "since": cs.since,
                "changedFiles": cs.files,
                "changedUnits": units,
                "fileLevel": cs.file_level,
                "affected": affected,
            }))
        }
        "testsprite_diff" => {
            let root = std::env::current_dir()?;
            let a = args
                .get("a")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("missing required argument: a"))?;
            let b = args
                .get("b")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("missing required argument: b"))?;
            crate::local::diff::diff_data(&root, a, b).await
        }
        "testsprite_coverage" => {
            let root = std::env::current_dir()?;
            let scan = args
                .get("path")
                .and_then(|v| v.as_str())
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| root.clone());
            crate::local::coverage::coverage_data(&scan).await
        }
        "testsprite_mutation" => {
            let root = std::env::current_dir()?;
            let scan = args
                .get("path")
                .and_then(|v| v.as_str())
                .map(std::path::PathBuf::from)
                .unwrap_or(root);
            let report =
                tokio::task::spawn_blocking(move || crate::local::mutation::run_rust(&scan, 300))
                    .await?;
            Ok(serde_json::to_value(report)?)
        }
        "testsprite_scaffold" => {
            let kind = args
                .get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or("backend");
            crate::local::scaffold::scaffold_data(kind)
        }
        "testsprite_release" => {
            let root = std::env::current_dir()?;
            let id = args
                .get("id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("missing required argument: id"))?;
            crate::local::store::set_quarantine(&root, id, None).await?;
            Ok(json!({ "id": id, "released": true }))
        }
        "testsprite_revisions" => {
            let root = std::env::current_dir()?;
            let id = args
                .get("id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("missing required argument: id"))?;
            Ok(json!({
                "id": id,
                "revisions": crate::local::store::revisions(&root, id).await?,
            }))
        }
        "testsprite_prune" => {
            let root = std::env::current_dir()?;
            let keep = args.get("keep").and_then(|v| v.as_u64()).unwrap_or(200) as usize;
            let deleted = match args.get("id").and_then(|v| v.as_str()) {
                Some(id) => crate::local::store::prune_runs(&root, id, keep).await?,
                None => crate::local::store::prune_all(&root, keep).await?,
            };
            Ok(json!({ "deleted": deleted, "keep": keep }))
        }
        "testsprite_export" => {
            let root = std::env::current_dir()?;
            let tests = crate::local::store::export_all(&root).await?;
            Ok(json!({ "count": tests.len(), "tests": tests }))
        }
        "testsprite_import" => {
            let root = std::env::current_dir()?;
            let tests: Vec<Value> = if let Some(arr) = args.get("tests").and_then(|v| v.as_array())
            {
                arr.clone()
            } else if let Some(file) = args.get("file").and_then(|v| v.as_str()) {
                let body = std::fs::read_to_string(file)
                    .map_err(|e| anyhow::anyhow!("reading {file}: {e}"))?;
                serde_json::from_str(&body)
                    .map_err(|e| anyhow::anyhow!("{file} is not a JSON array of tests: {e}"))?
            } else {
                anyhow::bail!(
                    "testsprite_import needs `tests` (inline array) or `file` (path to a JSON array)"
                )
            };
            let ids = crate::local::store::import_values(&root, &tests).await?;
            Ok(json!({ "imported": ids.len(), "ids": ids }))
        }
        "testsprite_prd_list" => {
            let root = std::env::current_dir()?;
            let prds = crate::local::store::list_prds(&root).await?;
            Ok(json!({ "count": prds.len(), "prds": prds }))
        }
        "testsprite_prd_show" => {
            let root = std::env::current_dir()?;
            let id = match args.get("id").and_then(|v| v.as_str()) {
                Some(id) => id.to_string(),
                None => crate::local::store::latest_prd_id(&root)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("no PRDs yet — generate one first"))?,
            };
            let prd = crate::local::store::load_prd(&root, &id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("no PRD with id {id}"))?;
            Ok(json!({ "id": id, "prd": prd }))
        }
        "testsprite_generate" => {
            let model = arg_model(args);
            let root = std::env::current_dir()?;
            let opts = crate::local::generate::GenOpts {
                budget: args.get("budget").and_then(|v| v.as_u64()),
                gate: args.get("gate").and_then(|v| v.as_bool()).unwrap_or(true),
                iterate: args
                    .get("iterate")
                    .and_then(|v| v.as_u64())
                    .map(|n| (n as usize).max(1))
                    .unwrap_or(1),
                ..Default::default()
            };
            let out = if args.get("cover").and_then(|v| v.as_bool()) == Some(true) {
                // One test per uncovered function under `path` (repo root by
                // default); `iterate` re-measures coverage each round.
                let path = args
                    .get("path")
                    .and_then(|v| v.as_str())
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| root.clone());
                crate::local::generate::generate_cover(&root, &path, model.as_ref(), &opts).await?
            } else if args.get("changed").and_then(|v| v.as_bool()) == Some(true) {
                let since = args.get("since").and_then(|v| v.as_str()).unwrap_or("HEAD");
                if args.get("fault_check").and_then(|v| v.as_bool()) == Some(true) {
                    crate::local::generate::generate_changed_fault_checked(
                        &root,
                        since,
                        model.as_ref(),
                        &opts,
                    )
                    .await?
                } else {
                    crate::local::generate::generate_changed(&root, since, model.as_ref(), &opts)
                        .await?
                }
            } else {
                let kind = args
                    .get("type")
                    .and_then(|v| v.as_str())
                    .map(crate::server::executors::TestKind::parse);
                crate::local::generate::generate(
                    &root,
                    args.get("from")
                        .and_then(|v| v.as_str())
                        .map(std::path::Path::new),
                    args.get("instruction").and_then(|v| v.as_str()),
                    args.get("doc")
                        .and_then(|v| v.as_str())
                        .map(std::path::Path::new),
                    model.as_ref(),
                    kind,
                    &opts,
                )
                .await?
            };
            Ok(json!({
                "generated": out.test_ids.len(),
                "ids": out.test_ids,
                "quarantined": out.quarantined,
                "prdId": out.prd_id,
            }))
        }
        "testsprite_explore" => {
            let root = std::env::current_dir()?;
            let url = match args.get("url").and_then(|v| v.as_str()) {
                Some(u) => u.to_string(),
                None => crate::local::project::load(&root)
                    .await?
                    .target_url
                    .ok_or_else(|| anyhow::anyhow!("missing url and project has no targetUrl"))?,
            };
            crate::local::explore::explore(
                &root,
                crate::local::explore::ExploreOpts {
                    url: &url,
                    store: args.get("store").and_then(|v| v.as_bool()).unwrap_or(false),
                    depth: args.get("depth").and_then(|v| v.as_u64()).unwrap_or(1) as usize,
                    limit: args.get("limit").and_then(|v| v.as_u64()).unwrap_or(8) as usize,
                    interactions: args
                        .get("interactions")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                },
            )
            .await
        }
        "testsprite_audit" => {
            let root = std::env::current_dir()?;
            let model = arg_model(args);
            let scan = args
                .get("path")
                .and_then(|v| v.as_str())
                .map(std::path::PathBuf::from)
                .unwrap_or(root.clone());
            let opts = crate::local::generate::GenOpts {
                budget: args.get("budget").and_then(|v| v.as_u64()),
                gate: args.get("gate").and_then(|v| v.as_bool()).unwrap_or(true),
                ..Default::default()
            };
            let out = crate::local::generate::adversarial(
                &root,
                &scan,
                model.as_ref(),
                args.get("store").and_then(|v| v.as_bool()).unwrap_or(false),
                &opts,
            )
            .await?;
            Ok(json!({ "proposed": out.cases.len(), "cases": out.cases, "stored": out.test_ids }))
        }
        "testsprite_run" => {
            let model = arg_model(args);
            let fix = args.get("fix").and_then(|v| v.as_bool()).unwrap_or(false);
            let root = std::env::current_dir()?;
            let changed_mode = args.get("changed").and_then(|v| v.as_bool()) == Some(true);
            let mut selection_note: Option<Value> = None;
            let ids: Vec<String> = if changed_mode {
                let since = args.get("since").and_then(|v| v.as_str()).unwrap_or("HEAD");
                let cs = crate::local::changed::changed_surface(&root, since)?;
                match crate::local::changed::select(&root, &cs).await? {
                    crate::local::changed::Selection::NoChanges => {
                        return Ok(json!({
                            "results": [],
                            "selection": "no_changes",
                            "verified": true,
                            "note": format!("no source changes since {since} — nothing to verify"),
                        }));
                    }
                    crate::local::changed::Selection::Affected(ids) => ids,
                    crate::local::changed::Selection::Unattributable { changed_units } => {
                        // Do NOT return an empty green result: the agent would read
                        // it as "my change is verified". Say so, and run everything.
                        selection_note = Some(json!({
                            "selection": "unattributable",
                            "changedUnits": changed_units,
                            "note": format!(
                                "{changed_units} function(s) changed but no stored test \
                                 mentions them; tests are matched by function-name mention, \
                                 which cannot see through spec/command tests. Ran the full \
                                 suite instead of reporting an empty success."
                            ),
                        }));
                        Vec::new()
                    }
                }
            } else if let Some(group) = args.get("group").and_then(|v| v.as_str()) {
                // Run only a named list/group (else fall back to id / all).
                crate::local::store::list(&root)
                    .await?
                    .into_iter()
                    .filter(|t| t.group() == Some(group))
                    .map(|t| t.id)
                    .collect()
            } else {
                match args.get("id").and_then(|v| v.as_str()) {
                    Some(id) => vec![id.to_string()],
                    None => vec![],
                }
            };
            let serve = args.get("serve").and_then(|v| v.as_bool()).unwrap_or(false);
            if args
                .get("require_approved_prd")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                crate::local::store::assert_prds_approved(&root, &ids).await?;
            }
            let jobs = args.get("jobs").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
            let results = crate::local::run::run_collect(
                &root,
                &ids,
                args.get("url").and_then(|v| v.as_str()),
                model.as_ref(),
                fix,
                args.get("browser").and_then(|v| v.as_str()),
                jobs.max(1),
                serve,
            )
            .await?;
            match selection_note {
                Some(mut note) => {
                    note["results"] = json!(results);
                    Ok(note)
                }
                None => Ok(json!({ "results": results })),
            }
        }
        "testsprite_loop" => {
            let root = std::env::current_dir()?;
            let model = arg_model(args);
            let opts = crate::local::cycle::CycleOpts {
                changed: args
                    .get("changed")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                since: args.get("since").and_then(|v| v.as_str()),
                generate: args
                    .get("generate")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                model: model.as_ref(),
                fix: args.get("fix").and_then(|v| v.as_bool()).unwrap_or(false),
                serve: args.get("serve").and_then(|v| v.as_bool()).unwrap_or(false),
                budget: args.get("budget").and_then(|v| v.as_u64()),
                require_approved_prd: args
                    .get("require_approved_prd")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
            };
            let report = crate::local::cycle::cycle(&root, opts).await?;
            Ok(serde_json::to_value(report)?)
        }
        "testsprite_store_test" => {
            let root = std::env::current_dir()?;
            let mut case = serde_json::Map::new();
            for key in ["title", "kind", "description", "code", "spec"] {
                if let Some(v) = args.get(key) {
                    case.insert(key.to_string(), v.clone());
                }
            }
            if case.is_empty() {
                anyhow::bail!("testsprite_store_test needs at least a title + (spec or code)");
            }
            let id = crate::local::store::add_value(&root, serde_json::Value::Object(case)).await?;
            Ok(json!({ "id": id, "stored": true }))
        }
        "testsprite_coverage_gaps" => {
            let root = std::env::current_dir()?;
            let scan = match args.get("path").and_then(|v| v.as_str()) {
                Some(p) => std::path::PathBuf::from(p),
                None => root.clone(),
            };
            let report = crate::local::coverage::gaps(&root, &scan).await?;
            Ok(serde_json::to_value(report)?)
        }
        "testsprite_list_tests" => {
            let root = std::env::current_dir()?;
            let tests = crate::local::store::list(&root).await?;
            let rows: Vec<Value> = tests
                .iter()
                .map(|t| {
                    json!({
                        "id": t.id,
                        "title": t.title,
                        "kind": t.kind.map(|k| format!("{k:?}").to_lowercase()),
                    })
                })
                .collect();
            Ok(json!({ "count": rows.len(), "tests": rows }))
        }
        "testsprite_get_test" => {
            let root = std::env::current_dir()?;
            let id = args
                .get("id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("missing required argument: id"))?;
            crate::local::store::get_value(&root, id).await
        }
        "testsprite_delete_test" => {
            let root = std::env::current_dir()?;
            let id = args
                .get("id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("missing required argument: id"))?;
            let deleted = crate::local::store::delete(&root, id).await?;
            Ok(json!({ "id": id, "deleted": deleted }))
        }
        "testsprite_emit_test" => {
            let root = std::env::current_dir()?;
            let id = args
                .get("id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("missing required argument: id"))?;
            let out = args
                .get("out")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("missing required argument: out"))?;
            crate::local::store::emit(&root, id, std::path::Path::new(out)).await?;
            Ok(json!({ "wrote": out }))
        }
        "testsprite_materialize_tests" => {
            let root = std::env::current_dir()?;
            let ids: Vec<String> = args
                .get("id")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let out = args.get("out").and_then(|v| v.as_str());
            let paths =
                crate::local::store::materialize(&root, &ids, out.map(std::path::Path::new))
                    .await?;
            Ok(json!({ "materialized": paths.len(), "paths": paths }))
        }
        "testsprite_rename_test" => {
            let root = std::env::current_dir()?;
            let id = args
                .get("id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("missing required argument: id"))?;
            let title = args
                .get("title")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("missing required argument: title"))?;
            crate::local::store::rename(&root, id, title).await?;
            Ok(json!({ "id": id, "title": title, "renamed": true }))
        }
        "testsprite_agent_message" => {
            let model = arg_model(args);
            let root = std::env::current_dir()?;
            let message = args
                .get("message")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("missing required argument: message"))?;
            let conversation_id = args.get("conversation_id").and_then(|v| v.as_str());
            let auto_approve = args
                .get("auto_approve")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            crate::local::agent::message(
                &root,
                conversation_id,
                message,
                model.as_ref(),
                auto_approve,
            )
            .await
        }
        "testsprite_agent_approve" => {
            let model = arg_model(args);
            let root = std::env::current_dir()?;
            let conversation_id = args
                .get("conversation_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("missing required argument: conversation_id"))?;
            let action_id = args
                .get("action_id")
                .and_then(|v| v.as_i64())
                .ok_or_else(|| anyhow::anyhow!("missing required argument: action_id"))?;
            let approve = args
                .get("approve")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            crate::local::agent::resolve(&root, conversation_id, action_id, approve, model.as_ref())
                .await
        }
        "testsprite_agent_history" => {
            let root = std::env::current_dir()?;
            let conversation_id = args
                .get("conversation_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("missing required argument: conversation_id"))?;
            crate::local::agent::history(&root, conversation_id).await
        }
        "testsprite_triage" => Ok(serde_json::json!({
            "clusters": crate::local::triage::triage(&std::env::current_dir()?).await?
        })),
        "testsprite_guidelines" => Ok(serde_json::json!({
            "guidelines": crate::local::guidelines::mine(&std::env::current_dir()?, 20).await?
        })),
        "testsprite_bench" => {
            // Never print here — MCP owns stdout for JSON-RPC. Return the
            // scoreboard; `save_baseline` persists it without any stdout.
            let root = std::env::current_dir()?;
            let board = crate::local::bench::scoreboard(&root).await?;
            if args.get("save_baseline").and_then(|v| v.as_bool()) == Some(true) {
                crate::local::bench::save_baseline(&root, &board)?;
            }
            Ok(serde_json::json!({ "scoreboard": board }))
        }
        "testsprite_flaky" => {
            let root = std::env::current_dir()?;
            let id = args
                .get("id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("missing required argument: id"))?;
            let runs = args.get("runs").and_then(|v| v.as_u64()).unwrap_or(5) as usize;
            let model = arg_model(args);
            let serve = args.get("serve").and_then(|v| v.as_bool()).unwrap_or(false);
            Ok(serde_json::to_value(
                crate::local::flaky::flaky(&root, id, runs, model.as_ref(), serve).await?,
            )?)
        }
        "testsprite_run_history" => {
            let root = std::env::current_dir()?;
            let id = args
                .get("id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("missing required argument: id"))?;
            Ok(serde_json::json!({
                "id": id,
                "runs": crate::local::store::run_history(&root, id).await?
            }))
        }
        "testsprite_artifact_get" => {
            let root = std::env::current_dir()?;
            let run_id = args
                .get("run_id")
                .and_then(|v| v.as_i64())
                .ok_or_else(|| anyhow::anyhow!("missing required argument: run_id"))?;
            let out = args
                .get("out")
                .and_then(|v| v.as_str())
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| {
                    crate::local::ts_dir(&root)
                        .join("artifacts")
                        .join(run_id.to_string())
                });
            let dir = crate::local::artifact::get(&root, run_id, &out).await?;
            Ok(json!({ "path": dir }))
        }
        "testsprite_report" => {
            let root = std::env::current_dir()?;
            let out = args
                .get("out")
                .and_then(|v| v.as_str())
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| crate::local::ts_dir(&root).join("testsprite-report.md"));
            let json_out = args.get("json").and_then(|v| v.as_bool()).unwrap_or(false);
            let path = crate::local::artifact::write_report(&root, &out, json_out).await?;
            Ok(json!({ "path": path }))
        }
        "testsprite_dashboard" => {
            let root = std::env::current_dir()?;
            let out = args
                .get("out")
                .and_then(|v| v.as_str())
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| crate::local::ts_dir(&root).join("dashboard.html"));
            let path = crate::local::artifact::write_dashboard(&root, &out).await?;
            Ok(json!({ "path": path }))
        }
        "testsprite_prd_review" => {
            let root = std::env::current_dir()?;
            let id = match args.get("id").and_then(|v| v.as_str()) {
                Some(id) => id.to_string(),
                None => crate::local::store::latest_prd_id(&root)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("no PRDs yet — generate one first"))?,
            };
            let out = args
                .get("out")
                .and_then(|v| v.as_str())
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| crate::local::ts_dir(&root).join("prd-review.html"));
            let path = crate::local::artifact::write_prd_review(&root, &id, &out).await?;
            Ok(json!({ "path": path, "id": id }))
        }
        "testsprite_prd_approve" => {
            let root = std::env::current_dir()?;
            let id = match args.get("id").and_then(|v| v.as_str()) {
                Some(id) => id.to_string(),
                None => crate::local::store::latest_prd_id(&root)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("no PRDs yet — generate one first"))?,
            };
            let approved_at = crate::local::store::approve_prd(&root, &id).await?;
            Ok(json!({ "id": id, "approvedAt": approved_at }))
        }
        "testsprite_plan_put" => {
            let root = std::env::current_dir()?;
            let id = args
                .get("id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("missing required argument: id"))?;
            let steps = args
                .get("steps")
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("missing required argument: steps"))?;
            crate::local::store::put_plan_steps(&root, id, steps).await?;
            Ok(json!({ "updated": id }))
        }
        "testsprite_replay" => {
            let root = std::env::current_dir()?;
            let id = args
                .get("id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("missing required argument: id"))?;
            let out = args
                .get("out")
                .and_then(|v| v.as_str())
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| crate::local::ts_dir(&root).join(format!("{id}-replay.html")));
            let path = crate::local::artifact::write_replay(&root, id, &out).await?;
            Ok(json!({ "path": path }))
        }
        other => anyhow::bail!("Unknown tool: {other}"),
    }
}

async fn bootstrap(project_path: &str, args: &Value) -> Result<Value> {
    use crate::types::{TargetScope, TestType};
    let init_args = tools::init::InitArgs {
        local_port: args
            .get("localPort")
            .and_then(|v| v.as_u64())
            .unwrap_or(5173) as u16,
        pathname: args
            .get("pathname")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        test_type: match args.get("type").and_then(|v| v.as_str()) {
            Some("frontend") => TestType::Frontend,
            _ => TestType::Backend,
        },
        project_path: project_path.to_string(),
        test_scope: match args.get("testScope").and_then(|v| v.as_str()) {
            Some("diff") => TargetScope::Diff,
            _ => TargetScope::Codebase,
        },
    };
    tools::init::initialization(init_args).await
}

/// Convert the JSON tool definitions ([`tool_list`]) into rmcp [`Tool`]s for
/// the SDK's `tools/list` response — schema and all.
fn rmcp_tools() -> Vec<Tool> {
    tool_list()["tools"]
        .as_array()
        .map(|arr| arr.iter().filter_map(json_to_tool).collect())
        .unwrap_or_default()
}

fn json_to_tool(v: &Value) -> Option<Tool> {
    let name = v.get("name")?.as_str()?.to_string();
    let description = v
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let schema = v
        .get("inputSchema")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    Some(Tool::new(name, description, std::sync::Arc::new(schema)))
}

/// The testsprite-rs MCP server on the official `rmcp` SDK. `list_tools` is
/// gated by [`flow_backend_configured`] (via [`tool_list`]), while `call_tool`
/// still dispatches names that may be absent from the listing — preserving the
/// "advertised locally, callable once a backend is configured" policy that the
/// hand-rolled server had.
#[derive(Clone)]
struct TestSpriteServer;

impl ServerHandler for TestSpriteServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(SERVER_NAME, SERVER_VERSION))
            .with_protocol_version(ProtocolVersion::V_2024_11_05)
            .with_instructions(
                "Local, no-account TestSprite: generate/run/report tests offline. The official \
                 cloud flow (bootstrap -> code_summary -> standardized_prd -> test_plan -> \
                 code_and_execute -> report) becomes discoverable once a backend (an API key, or \
                 the local `testsprite-rs backend` stand-in) is configured."
                    .to_string(),
            )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(rmcp_tools()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let args = request
            .arguments
            .map(Value::Object)
            .unwrap_or_else(|| json!({}));
        // Preserve the original envelope: success -> one pretty-printed JSON
        // text block; failure -> an isError result carrying the message (a
        // tool error, never a protocol error, so the caller reads the reason).
        let result = match call_tool(request.name.as_ref(), &args).await {
            Ok(v) => CallToolResult::success(vec![ContentBlock::text(
                serde_json::to_string_pretty(&v).unwrap_or_default(),
            )]),
            Err(e) => CallToolResult::error(vec![ContentBlock::text(e.to_string())]),
        };
        Ok(result.into())
    }
}

/// Run the stdio MCP server on the rmcp transport (handshake, framing, and the
/// read/dispatch/write loop are the SDK's; logs go to stderr so stdout stays
/// pure JSON-RPC).
pub async fn serve() -> Result<()> {
    eprintln!("[testsprite-rs] MCP server started");
    let service = TestSpriteServer.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    /// The account-gated official-flow tools, advertised only when a backend
    /// (cloud key or local stand-in `API_URL`) is configured.
    const CLOUD_FLOW: &[&str] = &[
        "testsprite_bootstrap",
        "testsprite_check_account_info",
        "testsprite_generate_standardized_prd",
        "testsprite_generate_frontend_test_plan",
        "testsprite_generate_backend_test_plan",
        "testsprite_generate_code_and_execute",
    ];

    fn advertised() -> Vec<String> {
        super::tool_list()["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str().map(str::to_string))
            .collect()
    }

    #[test]
    fn local_surface_always_advertised_including_wave_d_and_project_tools() {
        let _g = crate::testutil::env_guard(&[
            ("API_KEY", None),
            ("TSMCP_API_KEY", None),
            ("API_URL", None),
        ]);
        let names = advertised();
        // Pre-existing local tools.
        assert!(names.contains(&"testsprite_materialize_tests".to_string()));
        // Wave D + project-config tools are unconditional (no account needed).
        for t in [
            "testsprite_ingest_prd",
            "testsprite_project_init",
            "testsprite_project_show",
            "testsprite_project_set_var",
            "testsprite_project_set_start",
            // Backfilled CLI-parity tools (all local, no account).
            "testsprite_gate",
            "testsprite_lint",
            "testsprite_changed",
            "testsprite_diff",
            "testsprite_coverage",
            "testsprite_mutation",
            "testsprite_scaffold",
            "testsprite_release",
            "testsprite_revisions",
            "testsprite_prune",
            "testsprite_export",
            "testsprite_import",
            "testsprite_prd_list",
            "testsprite_prd_show",
        ] {
            assert!(
                names.contains(&t.to_string()),
                "{t} must always be advertised"
            );
        }
    }

    #[test]
    fn cloud_flow_hidden_without_a_backend() {
        let _g = crate::testutil::env_guard(&[
            ("API_KEY", None),
            ("TSMCP_API_KEY", None),
            ("API_URL", None),
        ]);
        let names = advertised();
        for t in CLOUD_FLOW {
            assert!(
                !names.contains(&t.to_string()),
                "{t} should be hidden when no backend is configured"
            );
        }
    }

    #[test]
    fn cloud_flow_advertised_with_api_key() {
        let _g = crate::testutil::env_guard(&[("API_KEY", Some("sk-user-test"))]);
        let names = advertised();
        for t in CLOUD_FLOW {
            assert!(
                names.contains(&t.to_string()),
                "{t} should be advertised once a backend is configured"
            );
        }
    }

    #[test]
    fn local_stand_in_api_url_advertises_the_flow() {
        let _g = crate::testutil::env_guard(&[
            ("API_KEY", None),
            ("TSMCP_API_KEY", None),
            ("API_URL", Some("http://127.0.0.1:8787")),
        ]);
        let names = advertised();
        assert!(names.contains(&"testsprite_generate_standardized_prd".to_string()));
    }

    #[test]
    fn production_api_url_alone_does_not_unhide_flow() {
        // A production API_URL with no key is not a usable backend.
        let _g = crate::testutil::env_guard(&[
            ("API_KEY", None),
            ("TSMCP_API_KEY", None),
            ("API_URL", Some("https://api.testsprite.com")),
        ]);
        assert!(!advertised().contains(&"testsprite_bootstrap".to_string()));
    }
}
