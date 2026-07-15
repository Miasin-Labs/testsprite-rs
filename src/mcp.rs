//! Minimal stdio MCP server (JSON-RPC 2.0 over newline-delimited stdin/stdout).
//! Mirrors the tool surface registered in the plugin's `index.ts`.

use anyhow::Result;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::tools;

const PROTOCOL_VERSION: &str = "2024-11-05";
const SERVER_NAME: &str = "testsprite-rs-mcp-server";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The 8 TestSprite tools, with names + descriptions matching the original.
fn tool_list() -> Value {
    json!({
        "tools": [
            { "name": "testsprite_bootstrap",
              "description": "First-time project initialization. Skip if testsprite_tests/tmp/config.json already exists.",
              "inputSchema": obj_schema(&[("localPort","number"),("type","string"),("projectPath","string"),("testScope","string")]) },
            { "name": "testsprite_generate_code_summary",
              "description": "Analyze the project repository and summarize the codebase.",
              "inputSchema": obj_schema(&[("projectRootPath","string")]) },
            { "name": "testsprite_generate_standardized_prd",
              "description": "Generate a structured standard PRD.",
              "inputSchema": obj_schema(&[("projectPath","string")]) },
            { "name": "testsprite_generate_frontend_test_plan",
              "description": "Generate a frontend test plan.",
              "inputSchema": obj_schema(&[("projectPath","string")]) },
            { "name": "testsprite_generate_backend_test_plan",
              "description": "Generate a backend test plan.",
              "inputSchema": obj_schema(&[("projectPath","string")]) },
            { "name": "testsprite_generate_code_and_execute",
              "description": "Open the tunnel, dispatch tests to the cloud, poll, and write the report.",
              "inputSchema": obj_schema(&[("projectName","string"),("projectPath","string")]) },
            { "name": "testsprite_check_account_info",
              "description": "Check the current user's TestSprite account (plan, credits, email).",
              "inputSchema": json!({ "type": "object", "properties": {}, "additionalProperties": false }) },
            { "name": "testsprite_local_generate",
              "description": "Generate local test cases with the LLM (needs an OpenAI key).",
              "inputSchema": obj_schema(&[("instruction","string"),("from","string"),("type","string"),("model","string")]) },
            { "name": "testsprite_local_run",
              "description": "Run local tests: execute + LLM failure analysis; set fix=true to also write a repair patch.",
              "inputSchema": obj_schema(&[("id","string"),("model","string"),("fix","boolean")]) },
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
            let results = crate::local::run::run_collect(&root, &ids, None, model, fix, None).await?;
            Ok(json!({ "results": results }))
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
