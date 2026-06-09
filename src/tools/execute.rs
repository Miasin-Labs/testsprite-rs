//! `testsprite_generate_code_and_execute` orchestrator.
//! Mirrors `tools/generateCodeAndExecute.ts`.
//!
//! Steps: open the reverse tunnel → verify connectivity through it → dispatch
//! the test plan to the cloud runner → poll until done → write Python test
//! files + results + a raw markdown report → close the tunnel.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use crate::backend::BackendClient;
use crate::config::read_config;
use crate::paths::Paths;
use crate::tunnel::Tunnel;
use crate::types::{Config, ServerMode, TestCase, TestEntity, TestType};
use crate::{net, report};

const MAX_DEV_MODE_TESTS: usize = 15;
const MAX_PROD_MODE_TESTS: usize = 30;

/// Removes the execution lock file on drop (mirrors `forceRemoveLock`).
struct LockGuard(std::path::PathBuf);
impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub struct ExecuteArgs {
    pub project_name: String,
    pub project_path: String,
    pub test_ids: Vec<String>,
    pub additional_instruction: String,
    pub server_mode: ServerMode,
}

/// Load + select the test plan, applying dev/prod caps and explicit id filters.
async fn load_test_plan(
    paths: &Paths,
    test_type: TestType,
    test_ids: &[String],
    server_mode: ServerMode,
) -> Result<Vec<TestCase>> {
    let plan_path = match test_type {
        TestType::Frontend => paths.frontend_test_plan(),
        TestType::Backend => paths.backend_test_plan(),
    };
    let raw = tokio::fs::read_to_string(&plan_path)
        .await
        .with_context(|| format!("read test plan {plan_path:?}"))?;
    let mut plan: Vec<TestCase> = serde_json::from_str(&raw).context("parse test plan")?;

    if !test_ids.is_empty() {
        plan.retain(|tc| test_ids.contains(&tc.id));
    }

    // Frontend test-count cap (matches the original priority-sorted truncation).
    if test_type == TestType::Frontend {
        let cap = if server_mode == ServerMode::Production {
            MAX_PROD_MODE_TESTS
        } else {
            MAX_DEV_MODE_TESTS
        };
        if plan.len() > cap {
            plan.sort_by_key(|tc| match tc.priority.as_deref() {
                Some("High") => 0,
                Some("Medium") => 1,
                _ => 2,
            });
            plan.truncate(cap);
        }
    }
    Ok(plan)
}

/// Append auth/login info to the instruction, as the original does.
fn instruction_with_auth(config: &Config, test_type: TestType, base: &str) -> String {
    let mut out = base.to_string();
    match test_type {
        TestType::Frontend => {
            if config.login_user.is_some() || config.login_password.is_some() {
                let auth =
                    json!({ "username": config.login_user, "password": config.login_password });
                out.push('\n');
                out.push_str(&auth.to_string());
            }
        }
        TestType::Backend => {
            if config.backend_auth_type.as_deref().unwrap_or("public") != "public" {
                let auth = json!({
                    "authType": config.backend_auth_type,
                    "credential": config.backend_credential,
                });
                out.push('\n');
                out.push_str(&auth.to_string());
            }
        }
    }
    out
}

/// Persist results: write each test's Python code + a results JSON + raw report.
async fn write_outputs(
    paths: &Paths,
    project_name: &str,
    test_type: TestType,
    plan: &[TestCase],
    results: &[TestEntity],
) -> Result<()> {
    // test_results.json
    let results_json =
        serde_json::to_string_pretty(&results.iter().map(entity_to_value).collect::<Vec<_>>())?;
    tokio::fs::write(paths.test_results(), results_json).await?;

    // Python test files.
    let code_dir = paths.test_code_dir();
    tokio::fs::create_dir_all(&code_dir).await?;
    for (idx, r) in results.iter().enumerate() {
        let Some(code) = r.code.as_deref().filter(|c| !c.is_empty()) else {
            continue;
        };
        let id = plan.get(idx).map(|t| t.id.as_str()).unwrap_or("TC");
        let title = r.title.clone().unwrap_or_default();
        let filename = format!("{id}_{}.py", report::sanitize_filename(&title));
        tokio::fs::write(code_dir.join(filename), code).await?;
    }

    // raw_report.md
    let report = report::build_raw_report(project_name, test_type, results);
    tokio::fs::write(paths.raw_report(), report).await?;
    Ok(())
}

fn entity_to_value(e: &TestEntity) -> Value {
    json!({
        "projectId": e.project_id,
        "testId": e.test_id,
        "userId": e.user_id,
        "title": e.title,
        "description": e.description,
        "code": e.code,
        "testStatus": e.test_status,
        "testError": e.test_error,
        "modified": e.modified,
    })
}

/// Run the full execute flow. Returns the finished test entities.
pub async fn run(args: ExecuteArgs) -> Result<Vec<TestEntity>> {
    let paths = Paths::new(&args.project_path);
    let config = read_config(&args.project_path).await;
    let test_type = config
        .r#type
        .ok_or_else(|| anyhow!("config has no test type"))?;
    let local_endpoint = config
        .local_endpoint
        .clone()
        .ok_or_else(|| anyhow!("config has no localEndpoint"))?;
    let (local_host, local_port) = parse_endpoint(&local_endpoint)?;

    // Single-flight guard: refuse to run if a previous run is still in progress.
    let lock_path = paths.execution_lock();
    if lock_path.exists() {
        bail!("Tests are already running. Remove the lock to override: {lock_path:?}");
    }
    if let Some(dir) = lock_path.parent() {
        tokio::fs::create_dir_all(dir).await.ok();
    }
    tokio::fs::write(&lock_path, "running").await.ok();
    let _lock = LockGuard(lock_path);

    let plan = load_test_plan(&paths, test_type, &args.test_ids, args.server_mode).await?;
    if plan.is_empty() {
        bail!("no test cases selected");
    }
    let prd_content = tokio::fs::read_to_string(paths.standard_prd())
        .await
        .context("read standard_prd.json")?;

    let backend = BackendClient::from_env()?;

    // 1. Open tunnel + verify connectivity.
    tracing::info!("starting tunnel...");
    let tunnel = Tunnel::start(&backend).await?;
    tracing::info!("proxy: {}", redact(&tunnel.proxy_url));

    if !net::check_port_listening(&local_host, local_port, Duration::from_secs(2)).await {
        tracing::warn!("local app not detected on {local_host}:{local_port}");
    }
    if test_type == TestType::Frontend {
        match net::probe_local_endpoint(&local_endpoint).await {
            Ok(status) => tracing::info!("local endpoint reachable (status={status})"),
            Err(e) => tracing::warn!("local endpoint probe failed: {e}"),
        }
    }
    match net::probe_through_tunnel(&local_endpoint, &tunnel.proxy_url).await {
        Ok(status) => tracing::info!("tunnel connectivity verified (status={status})"),
        Err(e) => tracing::warn!("tunnel probe failed: {e}"),
    }

    // 2. Dispatch.
    let instruction = instruction_with_auth(&config, test_type, &args.additional_instruction);
    let payload = json!({
        "name": args.project_name,
        "instruction": instruction,
        "testPlan": plan,
        "prdContent": prd_content,
        "endpoint": local_endpoint,
        "maxAttempts": 3,
        "autoFix": true,
        "proxy": tunnel.proxy_url,
    });
    let dispatch = async {
        match test_type {
            TestType::Frontend => backend.run_frontend_test(payload).await,
            TestType::Backend => backend.run_backend_test(payload).await,
        }
    };
    let test_ids = match dispatch.await {
        Ok(ids) => ids,
        Err(e) => {
            tunnel.stop().await;
            return Err(e);
        }
    };
    tracing::info!("dispatched {} test(s); polling...", test_ids.len());

    // 3. Poll.
    let poll = backend.poll_test_status(&test_ids, |done, _| {
        tracing::info!("progress: {done}/{} completed", test_ids.len());
    });
    let results = match poll.await {
        Ok(r) => r,
        Err(e) => {
            tunnel.stop().await;
            return Err(e);
        }
    };

    // 4. Write outputs + close tunnel.
    write_outputs(&paths, &args.project_name, test_type, &plan, &results).await?;
    tunnel.stop().await;
    tracing::info!("tunnel closed; {} result(s) written", results.len());
    Ok(results)
}

fn parse_endpoint(endpoint: &str) -> Result<(String, u16)> {
    let url = endpoint.trim_end_matches('/');
    let after = url.split("://").nth(1).unwrap_or(url);
    let authority = after.split('/').next().unwrap_or(after);
    if let Some((h, p)) = authority.rsplit_once(':') {
        return Ok((h.to_string(), p.parse().unwrap_or(80)));
    }
    let port = if url.starts_with("https") { 443 } else { 80 };
    Ok((authority.to_string(), port))
}

/// Redact credentials in a proxy URL for logging.
fn redact(url: &str) -> String {
    match (url.split_once("://"), url.rsplit_once('@')) {
        (Some((scheme, _)), Some((_, host))) => format!("{scheme}://[REDACTED]@{host}"),
        _ => url.to_string(),
    }
}

/// Build the next-action the MCP tool returns: spawn the console execution in a
/// terminal, then have the host LLM fill in the report (mirrors the original).
pub fn mcp_next_action(project_path: &str, exe: &str) -> Value {
    let paths = Paths::new(project_path);
    crate::tools::next_action(vec![
        json!({
            "type": "tool",
            "tool": "Run in Terminal",
            "input": {
                "inline_execution": true,
                "command": format!("cd {project_path} && {exe} generate-code-and-execute")
            },
            "mode": "terminal_only"
        }),
        json!({
            "type": "llm.generate",
            "input": {
                "prompt": format!(
                    "The test is complete. Raw report at `{}`. Complete it and write the final report to `{}`.",
                    paths.raw_report().display(),
                    paths.test_report().display()
                )
            }
        }),
    ])
}
