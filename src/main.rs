//! testsprite-rs — a from-scratch Rust reimplementation of the TestSprite MCP
//! client (`@testsprite/testsprite-mcp`).
//!
//! It speaks the same backend protocol (`api.testsprite.com`), opens the same
//! reverse tunnel (`*.tun.testsprite.com`, yamux v2), and exposes the same 8
//! MCP tools over stdio. See README.md for the full flow.

mod backend;
mod config;
mod envs;
mod mcp;
mod net;
mod paths;
mod report;
mod server;
mod tools;
mod tunnel;
mod types;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "testsprite-rs",
    version,
    about = "TestSprite MCP client (Rust)"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run as a stdio MCP server (default when no subcommand is given).
    Serve,
    /// Show the current account (plan, credits, email).
    Account,
    /// Alias for `account` — verify the API key.
    Check,
    /// Console execution path: read config.executionArgs and run the full
    /// tunnel → dispatch → poll → report flow. Mirrors the plugin CLI.
    GenerateCodeAndExecute,
    /// Run a LOCAL stand-in for api.testsprite.com (no account needed).
    /// Uses OpenAI (key from ~/.config/jfc/credentials.toml or OPENAI_API_KEY)
    /// when available, else a deterministic engine. Point the client at it via
    /// API_URL / TSEMCP_TUNNEL_CONTROL_URL.
    Backend {
        /// Port to listen on.
        #[arg(long, default_value_t = 8787)]
        port: u16,
        /// OpenAI model for PRD/plan/test-code generation.
        #[arg(long, default_value = "gpt-4o-mini")]
        model: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "testsprite_rs=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => mcp::serve().await,
        Command::Account | Command::Check => run_account().await,
        Command::GenerateCodeAndExecute => run_console_execute().await,
        Command::Backend { port, model } => server::serve(port, &model).await,
    }
}

async fn run_account() -> Result<()> {
    let info = tools::account::check_account_info().await;
    println!("{}", serde_json::to_string_pretty(&info)?);
    Ok(())
}

async fn run_console_execute() -> Result<()> {
    let project_path = std::env::current_dir()?.to_string_lossy().to_string();
    let config = config::read_config(&project_path).await;
    let exec = config.execution_args.ok_or_else(|| {
        anyhow::anyhow!("config.executionArgs missing; run generate_code_and_execute first")
    })?;

    let args = tools::execute::ExecuteArgs {
        project_name: exec.project_name,
        project_path: exec.project_path,
        test_ids: exec.test_ids,
        additional_instruction: exec.additional_instruction,
        server_mode: exec.server_mode,
    };
    let results = tools::execute::run(args).await?;
    let passed = results.iter().filter(|r| r.passed()).count();
    println!(
        "Test execution completed: {passed}/{} passed",
        results.len()
    );
    Ok(())
}
