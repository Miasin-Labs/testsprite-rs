//! Test-plan tools. Mirrors `tools/generate{Frontend,Backend}TestPlan.ts`.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::backend::BackendClient;
use crate::config::read_config;
use crate::paths::Paths;
use crate::tools::next_action;
use crate::types::{TargetScope, TestType};

/// Build the "now run code_and_execute" next-action with context.
fn execute_next_action(project_path: &str, plan_path: &str) -> Value {
    next_action(vec![json!({
        "type": "tool_use",
        "tool": "testsprite_generate_code_and_execute",
        "context": { "projectPath": project_path, "testPlanFilePath": plan_path }
    })])
}

pub async fn generate_frontend_test_plan(project_path: &str) -> Result<Value> {
    let paths = Paths::new(project_path);
    let config = read_config(project_path).await;

    let standard_prd_path = paths.standard_prd();
    if !standard_prd_path.exists() {
        return Ok(next_action(vec![json!({
            "type": "tool_use",
            "tool": "testsprite_generate_standardized_prd",
            "input": { "projectPath": project_path }
        })]));
    }

    let standard_prd: Value =
        serde_json::from_str(&tokio::fs::read_to_string(&standard_prd_path).await?)?;
    let client = BackendClient::from_env()?;
    let plan = client
        .generate_frontend_test_plan(&standard_prd, config.scope)
        .await
        .context("frontend generate-plan failed")?;

    let plan_path = paths.frontend_test_plan();
    if let Some(dir) = plan_path.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    tokio::fs::write(&plan_path, serde_json::to_string_pretty(&plan)?).await?;
    Ok(execute_next_action(
        project_path,
        &plan_path.to_string_lossy(),
    ))
}

pub async fn generate_backend_test_plan(project_path: &str) -> Result<Value> {
    let paths = Paths::new(project_path);
    let config = read_config(project_path).await;
    if config.r#type != Some(TestType::Backend) {
        bail!("This tool only supports backend tests. Set type to \"backend\".");
    }
    let scope = config.scope.unwrap_or(TargetScope::Codebase);

    let prd_content = tokio::fs::read_to_string(paths.standard_prd())
        .await
        .context("read standard_prd.json")?;
    if prd_content.is_empty() {
        bail!("PRD file is empty");
    }

    let client = BackendClient::from_env()?;
    let plan = client
        .generate_backend_test_plan(&prd_content, scope)
        .await
        .context("backend test plan failed")?;

    let plan_path = paths.backend_test_plan();
    tokio::fs::write(&plan_path, serde_json::to_string_pretty(&plan)?).await?;
    Ok(execute_next_action(
        project_path,
        &plan_path.to_string_lossy(),
    ))
}
