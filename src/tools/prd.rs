//! `testsprite_generate_standardized_prd`. Mirrors `tools/generateStandardPRD.ts`.

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::backend::{self, BackendClient};
use crate::config::read_config;
use crate::paths::Paths;
use crate::tools::{next_action, tool_use};
use crate::types::TestType;

pub async fn generate_standard_prd(project_path: &str) -> Result<Value> {
    let paths = Paths::new(project_path);
    let config = read_config(project_path).await;
    let test_type = config.r#type.unwrap_or(TestType::Backend);

    // Collect any raw PRD files the user dropped in tmp/prd_files.
    let mut prd_files = Vec::new();
    if let Ok(mut rd) = tokio::fs::read_dir(paths.raw_prd_dir()).await {
        while let Ok(Some(entry)) = rd.next_entry().await {
            prd_files.push(entry.path());
        }
    }

    let code_summary_path = paths.code_summary();
    if !code_summary_path.exists() {
        bail!(
            "[ACTION REQUIRED] Code summary not found. Save it as {:?} (run generate_code_summary first).",
            code_summary_path
        );
    }
    let code_summary_json = backend::code_summary_to_json(&code_summary_path).await?;

    let client = BackendClient::from_env()?;
    let mut prd = client
        .generate_standard_prd(&prd_files, &code_summary_json, test_type)
        .await
        .context("generate-prd failed")?;

    // Attach the code summary, as the original does.
    if let (Value::Object(m), Ok(cs)) =
        (&mut prd, serde_json::from_str::<Value>(&code_summary_json))
    {
        m.insert("code_summary".into(), cs);
    }

    tokio::fs::write(paths.standard_prd(), serde_json::to_string_pretty(&prd)?).await?;

    let next_tool = match test_type {
        TestType::Frontend => "testsprite_generate_frontend_test_plan",
        TestType::Backend => "testsprite_generate_backend_test_plan",
    };
    Ok(next_action(vec![tool_use(next_tool)]))
}
