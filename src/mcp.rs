//! Minimal stdio MCP server (JSON-RPC 2.0 over newline-delimited stdin/stdout).
//! Mirrors the tool surface registered in the plugin's `index.ts`.

use anyhow::Result;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::tools;

const PROTOCOL_VERSION: &str = "2024-11-05";
const SERVER_NAME: &str = "testsprite-rs-mcp-server";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The local TestSprite MCP tools: generate/run/store/list/delete test cases,
/// the one-call `loop`, coverage, emit/rename/triage/flaky/history, plus the 3
/// conversational-agent tools. (Cloud tools that need a TestSprite account are
/// intentionally not advertised.)
fn tool_list() -> Value {
    json!({
        "tools": [
            { "name": "testsprite_generate_code_summary",
              "description": "Scan the repo and write TestSprite's code summary (tech_stack, features/files, api_endpoints) to testsprite_tests/tmp/code_summary.yaml by default. This is the official first step before normalized PRD/test-plan generation.",
              "inputSchema": obj_schema(&[("path","string"),("out","string")]) },
            { "name": "testsprite_generate",
              "description": "Generate local test cases. doc=<file-or-URL>: a Postman collection, OpenAPI/Swagger spec (incl. a utoipa/served /api-docs/openapi.json URL), or HAR → deterministic spec cases with NO OpenAI key; README/notes/Jira → LLM PRD. changed=true (since, default HEAD) generates only for functions changed since a git ref.",
              "inputSchema": obj_schema(&[("instruction","string"),("from","string"),("doc","string"),("type","string"),("model","string"),("changed","boolean"),("since","string")]) },
            { "name": "testsprite_explore",
              "description": "Autonomous exploratory frontend QA: open a live page with Playwright, inventory visible inputs/buttons/links/headings, and generate deterministic frontend planSteps candidates. Set store=true to add them to the local test DB. interactions=true also clicks visible controls on fresh pages and generates action+assertion candidates (opt-in because clicks can mutate state).",
              "inputSchema": obj_schema(&[("url","string"),("store","boolean"),("depth","number"),("limit","number"),("interactions","boolean")]) },
            { "name": "testsprite_audit",
              "description": "Use TestSprite's LLM to adversarially propose high-signal QA tests from code summary, stored tests, latest results, and coverage gaps. Set store=true to persist proposed cases.",
              "inputSchema": obj_schema(&[("path","string"),("model","string"),("store","boolean")]) },
            { "name": "testsprite_run",
              "description": "Run local tests: execute + LLM failure analysis; set fix=true to also write a repair patch. Set changed=true to run ONLY the tests affected by files changed since a git ref (since, default HEAD). Set serve=true to start the target app (`project set-start`) before running so backend/spec cases hit a live server.",
              "inputSchema": obj_schema(&[("id","string"),("model","string"),("fix","boolean"),("changed","boolean"),("since","string"),("serve","boolean"),("require_approved_prd","boolean")]) },
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
              "description": "Group the failing tests by root cause (failureKind) into clusters so you fix the few underlying problems instead of N symptoms.",
              "inputSchema": obj_schema(&[]) },
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
              "description": "Write a latest-results report summarizing pass/fail, failures, and clusters. Markdown by default, PDF when out ends with .pdf, JSON when json=true.",
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
        "testsprite_generate" => {
            let model = args
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or("gpt-4o-mini");
            let root = std::env::current_dir()?;
            let out = if args.get("changed").and_then(|v| v.as_bool()) == Some(true) {
                let since = args.get("since").and_then(|v| v.as_str()).unwrap_or("HEAD");
                crate::local::generate::generate_changed(&root, since, model).await?
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
                    model,
                    kind,
                )
                .await?
            };
            Ok(json!({ "generated": out.test_ids.len(), "ids": out.test_ids, "prdId": out.prd_id }))
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
            let model = args
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or("gpt-4o-mini");
            let scan = args
                .get("path")
                .and_then(|v| v.as_str())
                .map(std::path::PathBuf::from)
                .unwrap_or(root.clone());
            let out = crate::local::generate::adversarial(
                &root,
                &scan,
                model,
                args.get("store").and_then(|v| v.as_bool()).unwrap_or(false),
            )
            .await?;
            Ok(json!({ "proposed": out.cases.len(), "cases": out.cases, "stored": out.test_ids }))
        }
        "testsprite_run" => {
            let model = args
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or("gpt-4o-mini");
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
            let results =
                crate::local::run::run_collect(&root, &ids, None, model, fix, None, 1, serve)
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
            let model = args
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or("gpt-4o-mini");
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
                model,
                fix: args.get("fix").and_then(|v| v.as_bool()).unwrap_or(false),
                serve: args.get("serve").and_then(|v| v.as_bool()).unwrap_or(false),
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
            let auto_approve = args
                .get("auto_approve")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            crate::local::agent::message(&root, conversation_id, message, model, auto_approve).await
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
            let approve = args
                .get("approve")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
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
            let serve = args.get("serve").and_then(|v| v.as_bool()).unwrap_or(false);
            Ok(serde_json::to_value(
                crate::local::flaky::flaky(&root, id, runs, model, serve).await?,
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
