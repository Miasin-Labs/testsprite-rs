//! `testsprite_bootstrap`. Mirrors `tools/initialize.ts`.
//!
//! Seeds `testsprite_tests/tmp/config.json`, makes sure the config is
//! git-ignored, and returns a next-action telling the host agent to generate
//! the code summary.

use anyhow::Result;
use serde_json::{Value, json};

use crate::config::{ensure_gitignore_entry, read_config, save_config};
use crate::tools::{next_action, tool_use};
use crate::types::{Config, TargetScope, TestType};

pub struct InitArgs {
    pub local_port: u16,
    pub pathname: String,
    pub test_type: TestType,
    pub project_path: String,
    pub test_scope: TargetScope,
}

pub async fn initialization(args: InitArgs) -> Result<Value> {
    let mut config = read_config(&args.project_path).await;
    config.status = "init".into();
    config.scope = Some(args.test_scope);
    config.r#type = Some(args.test_type);

    let path = args.pathname.trim_start_matches('/');
    config.local_endpoint = Some(format!("http://localhost:{}/{}", args.local_port, path));

    save_config(&args.project_path, &config).await?;
    ensure_gitignore_entry(&args.project_path).await;
    tracing::info!("config seeded, gitignore updated");

    Ok(prompt_for(&config, args.local_port))
}

fn prompt_for(config: &Config, local_port: u16) -> Value {
    let running_hint = match config.r#type {
        Some(TestType::Frontend) => format!(
            "Build and START the project in PRODUCTION mode on port {local_port} (prod caps tests at 30)."
        ),
        _ => format!("Start the project on port {local_port}."),
    };
    next_action(vec![
        json!({ "type": "instruction", "text": running_hint }),
        json!({ "type": "instruction",
                "text": "Then generate code_summary.yaml via testsprite_generate_code_summary." }),
        tool_use("testsprite_generate_code_summary"),
    ])
}
