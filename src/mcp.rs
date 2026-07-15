//! Minimal stdio MCP server (JSON-RPC 2.0 over newline-delimited stdin/stdout).
//! Mirrors the tool surface registered in the plugin's `index.ts`.

use anyhow::Result;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::tools;

const PROTOCOL_VERSION: &str = "2024-11-05";
const SERVER_NAME: &str = "testsprite-rs-mcp-server";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The 18 TestSprite tools (including the deterministic `testsprite_store_test`
/// and the auth-aware `testsprite_flaky`), plus the 3 local conversational-agent tools.
fn tool_list() -> Value {
    json!({
        "tools": [
            { "name": "testsprite_bootstrap",
              "description": "[cloud] First-time project initialization. Skip if testsprite_tests/tmp/config.json already exists. Needs a TestSprite account; for local/offline use prefer testsprite_store_test / testsprite_local_generate / testsprite_local_run.",
              "inputSchema": obj_schema(&[("localPort","number"),("type","string"),("projectPath","string"),("testScope","string")]) },
            { "name": "testsprite_generate_code_summary",
              "description": "[cloud] Analyze the project repository and summarize the codebase. Needs a TestSprite account; for local/offline use prefer testsprite_store_test / testsprite_local_generate / testsprite_local_run.",
              "inputSchema": obj_schema(&[("projectRootPath","string")]) },
            { "name": "testsprite_generate_standardized_prd",
              "description": "[cloud] Generate a structured standard PRD. Needs a TestSprite account; for local/offline use prefer testsprite_store_test / testsprite_local_generate / testsprite_local_run.",
              "inputSchema": obj_schema(&[("projectPath","string")]) },
            { "name": "testsprite_generate_frontend_test_plan",
              "description": "[cloud] Generate a frontend test plan. Needs a TestSprite account; for local/offline use prefer testsprite_store_test / testsprite_local_generate / testsprite_local_run.",
              "inputSchema": obj_schema(&[("projectPath","string")]) },
            { "name": "testsprite_generate_backend_test_plan",
              "description": "[cloud] Generate a backend test plan. Needs a TestSprite account; for local/offline use prefer testsprite_store_test / testsprite_local_generate / testsprite_local_run.",
              "inputSchema": obj_schema(&[("projectPath","string")]) },
            { "name": "testsprite_generate_code_and_execute",
              "description": "[cloud] Open the tunnel, dispatch tests to the cloud, poll, and write the report. Needs a TestSprite account; for local/offline use prefer testsprite_store_test / testsprite_local_generate / testsprite_local_run.",
              "inputSchema": obj_schema(&[("projectName","string"),("projectPath","string")]) },
            { "name": "testsprite_check_account_info",
              "description": "[cloud] Check the current user's TestSprite account (plan, credits, email). Needs a TestSprite account; for local/offline use prefer testsprite_store_test / testsprite_local_generate / testsprite_local_run.",
              "inputSchema": json!({ "type": "object", "properties": {}, "additionalProperties": false }) },
            { "name": "testsprite_local_generate",
              "description": "Generate local test cases with the LLM (needs an OpenAI key).",
              "inputSchema": obj_schema(&[("instruction","string"),("from","string"),("type","string"),("model","string")]) },
            { "name": "testsprite_local_run",
              "description": "Run local tests: execute + LLM failure analysis; set fix=true to also write a repair patch.",
              "inputSchema": obj_schema(&[("id","string"),("model","string"),("fix","boolean")]) },
            { "name": "testsprite_store_test",
              "description": "Store a test YOU already wrote so testsprite can run + track it deterministically (no LLM). Provide `spec` for an HTTP assertion OR `code` for a python/rust test body. Prefer this over testsprite_local_generate when you can write the test yourself. Set kind:\"command\" with code set to a shell command (e.g. `cargo test -p mycrate --test foo`) to run your repo's OWN tests deterministically — pass on exit 0.",
              "inputSchema": obj_schema(&[("title","string"),("kind","string"),("description","string"),("code","string")]) },
            { "name": "testsprite_coverage_gaps",
              "description": "List functions in the code surface that NO stored test references yet — the uncovered set to generate next. Loop this until empty for full coverage.",
              "inputSchema": obj_schema(&[("path","string")]) },
            { "name": "testsprite_emit_test",
              "description": "Materialize a stored test's code into a repo file (e.g. crates/foo/tests/bar.rs) so cargo/CI own it — the repo-native alternative to ephemeral SQLite runs.",
              "inputSchema": obj_schema(&[("id","string"),("out","string")]) },
            { "name": "testsprite_rename_test",
              "description": "Rename a stored test's title. Use this to fix the munged/duplicate TC000 names auto-generation produces — give each test a meaningful, unique name.",
              "inputSchema": obj_schema(&[("id","string"),("title","string")]) },
            { "name": "testsprite_agent_message",
              "description": "Talk to the local test agent: it proposes ONE action (generate/run) to approve.",
              "inputSchema": obj_schema(&[("conversation_id","string"),("message","string"),("model","string")]) },
            { "name": "testsprite_agent_approve",
              "description": "Approve (or reject) a pending agent action by id; executes the pipeline.",
              "inputSchema": obj_schema(&[("conversation_id","string"),("action_id","number"),("approve","boolean"),("model","string")]) },
            { "name": "testsprite_agent_history",
              "description": "Show a conversation's messages and pending actions.",
              "inputSchema": obj_schema(&[("conversation_id","string")]) },
            { "name": "testsprite_triage",
              "description": "Group the failing tests by root cause (failureKind) into clusters so you fix the few underlying problems instead of N symptoms.",
              "inputSchema": obj_schema(&[]) },
            { "name": "testsprite_flaky",
              "description": "Replay a stored test N times (default 5) and report a stability score; blocked/auth-failure runs are excluded, not scored as flaky.",
              "inputSchema": obj_schema(&[("id","string"),("runs","number"),("model","string")]) },
            { "name": "testsprite_run_history",
              "description": "Show a stored test's full run history (append-only): every recorded run newest-first with pass/fail, verdict, failureKind, and timestamp. Use it to spot regressions and intermittent failures over time.",
              "inputSchema": obj_schema(&[("id","string")]) },
        ]
    })
}

fn obj_schema(fields: &[(&str, &str)]) -> Value {
    let mut props = serde_json::Map::new();
    for (name, ty) in fields {
        props.insert((*name).to_string(), json!({ "type": ty }));
    }
    json!({ "type": "object", "properties": props })
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
        "testsprite_generate_code_summary" => Ok(code_summary_instruction(&project_path)),
        "testsprite_bootstrap" => bootstrap(&project_path, args).await,
        "testsprite_local_generate" => {
            let model = args
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or("gpt-4o-mini");
            let root = std::env::current_dir()?;
            let kind = args
                .get("type")
                .and_then(|v| v.as_str())
                .map(crate::server::executors::TestKind::parse);
            let ids = crate::local::generate::generate(
                &root,
                args.get("from").and_then(|v| v.as_str()).map(std::path::Path::new),
                args.get("instruction").and_then(|v| v.as_str()),
                model,
                kind,
            )
            .await?;
            Ok(json!({ "generated": ids.len(), "ids": ids }))
        }
        "testsprite_local_run" => {
            let model = args
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or("gpt-4o-mini");
            let fix = args.get("fix").and_then(|v| v.as_bool()).unwrap_or(false);
            let ids: Vec<String> = match args.get("id").and_then(|v| v.as_str()) {
                Some(id) => vec![id.to_string()],
                None => vec![],
            };
            let root = std::env::current_dir()?;
            let results = crate::local::run::run_collect(&root, &ids, None, model, fix, None, 1).await?;
            Ok(json!({ "results": results }))
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
            let model = args
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or("gpt-4o-mini");
            let root = std::env::current_dir()?;
            let message = args
                .get("message")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("missing required argument: message"))?;
            let conversation_id = args.get("conversation_id").and_then(|v| v.as_str());
            crate::local::agent::message(&root, conversation_id, message, model).await
        }
        "testsprite_agent_approve" => {
            let model = args
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or("gpt-4o-mini");
            let root = std::env::current_dir()?;
            let conversation_id = args
                .get("conversation_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("missing required argument: conversation_id"))?;
            let action_id = args
                .get("action_id")
                .and_then(|v| v.as_i64())
                .ok_or_else(|| anyhow::anyhow!("missing required argument: action_id"))?;
            let approve = args.get("approve").and_then(|v| v.as_bool()).unwrap_or(true);
            crate::local::agent::resolve(&root, conversation_id, action_id, approve, model).await
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
        "testsprite_flaky" => {
            let root = std::env::current_dir()?;
            let id = args
                .get("id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("missing required argument: id"))?;
            let runs = args.get("runs").and_then(|v| v.as_u64()).unwrap_or(5) as usize;
            let model = args
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or("gpt-4o-mini");
            Ok(serde_json::to_value(
                crate::local::flaky::flaky(&root, id, runs, model).await?,
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

fn code_summary_instruction(project_path: &str) -> Value {
    let target = crate::paths::Paths::new(project_path).code_summary();
    tools::next_action(vec![
        json!({ "type": "instruction",
                "text": format!("Scan the codebase, extract tech stack + features, and write a YAML summary to {}.", target.display()) }),
        json!({ "type": "tool_use", "tool": "testsprite_generate_standardized_prd" }),
    ])
}

/// Wrap a tool result as MCP `content`.
fn as_content(v: Value) -> Value {
    json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&v).unwrap_or_default() }] })
}

/// Route a single method to its result. `None` = notification (no response).
async fn route(method: &str, req: &Value) -> Option<Result<Value>> {
    match method {
        "initialize" => Some(Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION }
        }))),
        "tools/list" => Some(Ok(tool_list())),
        "tools/call" => {
            let params = req.get("params").cloned().unwrap_or(json!({}));
            let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            Some(call_tool(name, &args).await.map(as_content))
        }
        "notifications/initialized" => None,
        _ => Some(Err(anyhow::anyhow!("method not found: {method}"))),
    }
}

/// Handle one JSON-RPC request, returning the response value (or None for notifications).
async fn handle(req: &Value) -> Option<Value> {
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let result = route(method, req).await?;
    let id = req.get("id").cloned()?; // notifications carry no id → no response
    Some(match result {
        Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
        Err(e) => json!({
            "jsonrpc": "2.0", "id": id,
            "result": { "isError": true, "content": [{ "type": "text", "text": e.to_string() }] }
        }),
    })
}

/// Run the stdio MCP server loop.
pub async fn serve() -> Result<()> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    eprintln!("[testsprite-rs] MCP server started");
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(req) = serde_json::from_str::<Value>(&line) else {
            tracing::warn!("bad JSON-RPC line");
            continue;
        };
        if let Some(resp) = handle(&req).await {
            let mut bytes = serde_json::to_vec(&resp)?;
            bytes.push(b'\n');
            stdout.write_all(&bytes).await?;
            stdout.flush().await?;
        }
    }
    Ok(())
}
