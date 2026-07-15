//! testsprite-rs — a from-scratch Rust reimplementation of the TestSprite MCP
//! client (`@testsprite/testsprite-mcp`).
//!
//! It speaks the same backend protocol (`api.testsprite.com`), opens the same
//! reverse tunnel (`*.tun.testsprite.com`, yamux v2), and exposes the same 8
//! MCP tools over stdio. See README.md for the full flow.

mod backend;
mod config;
#[cfg(feature = "discord")]
mod discord;
mod envs;
mod local;
mod mcp;
mod net;
mod paths;
mod report;
mod server;
mod tools;
mod tunnel;
mod types;

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, Subcommand};
use std::path::PathBuf;

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
    /// Diagnose the local environment (LLM key, cargo, coverage, node/playwright, gh, python).
    Doctor,
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
        /// Testing modality: backend | frontend | mcp | rust.
        #[arg(long, default_value = "backend")]
        kind: String,
    },
    /// Report test coverage: cargo llvm-cov (Rust) + a tree-sitter structural surface.
    Coverage {
        /// Root directory to scan (defaults to the current directory).
        #[arg(long)]
        path: Option<PathBuf>,
        /// Print a single JSON object instead of a human-readable report.
        #[arg(long)]
        json: bool,
        /// Report structural units NOT referenced by any stored test yet,
        /// instead of the full report.
        #[arg(long)]
        gaps: bool,
    },
    /// Run the suite as a CI gate: JUnit + JSON + best-effort gh PR comment; exit 1 on any failure.
    Gate {
        /// Override the project's target URL for this run.
        #[arg(long)]
        url: Option<String>,
        /// OpenAI model for spec-less LLM execution and failure analysis.
        #[arg(long, default_value = "gpt-4o-mini")]
        model: String,
    },
    /// Local project lifecycle — no cloud (init / show).
    Project {
        #[command(subcommand)]
        cmd: ProjectCmd,
    },
    /// Local test lifecycle — no cloud (add / list / run).
    Test {
        #[command(subcommand)]
        cmd: TestCmd,
    },
    /// Compare two PNG screenshots and report a pixel-diff ratio; exits 1 on
    /// a visual regression (see local::visual::REGRESSION_THRESHOLD).
    Visual {
        /// Baseline (known-good) screenshot.
        #[arg(long)]
        baseline: PathBuf,
        /// Current screenshot to compare against the baseline.
        #[arg(long)]
        current: PathBuf,
    },
    /// Conversational test agent — proposes ONE action (generate/run) that
    /// must be approved before it executes. No cloud; DB-backed threads.
    Agent {
        #[command(subcommand)]
        cmd: AgentCmd,
    },
    /// Run the Discord bot front-end for the agent (build with `--features discord`).
    #[cfg(feature = "discord")]
    Discord {
        /// Override the bot token (else config `[discord].token_file` or `token.key`).
        #[arg(long)]
        token: Option<String>,
    },
    /// Generate a shell completion script (bash|zsh|fish|elvish|powershell).
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
}

#[derive(Subcommand)]
enum AgentCmd {
    /// Send a message; the agent replies and may propose one pending action.
    Message {
        /// Existing conversation id to continue (omit to start a new one).
        #[arg(long)]
        conversation: Option<String>,
        /// OpenAI model for the conversational planner.
        #[arg(long, default_value = "gpt-4o-mini")]
        model: String,
        /// The message text (joined from remaining words).
        #[arg(trailing_var_arg = true, required = true)]
        message: Vec<String>,
    },
    /// Approve (default) or reject a pending action by id.
    Approve {
        /// Conversation id the action belongs to.
        conversation: String,
        /// Pending action id (from `agent message` or `agent history`).
        action_id: i64,
        /// Reject instead of approving.
        #[arg(long)]
        reject: bool,
        /// OpenAI model used when the action executes.
        #[arg(long, default_value = "gpt-4o-mini")]
        model: String,
    },
    /// Show a conversation's messages and pending actions.
    History {
        /// Conversation id.
        conversation: String,
    },
}

#[derive(Subcommand)]
enum ProjectCmd {
    /// Create testsprite_tests/project.json.
    Init {
        /// Modality: backend | frontend | mcp | rust.
        #[arg(long = "type", default_value = "backend")]
        kind: String,
        /// Project name.
        #[arg(long)]
        name: String,
        /// Target URL the tests run against (e.g. http://127.0.0.1:8080).
        #[arg(long)]
        url: Option<String>,
    },
    /// Print the current project.json.
    Show,
}

#[derive(Subcommand)]
enum TestCmd {
    /// Add a JSON test case from a file.
    Add {
        /// Path to a JSON test-plan file.
        #[arg(long)]
        file: PathBuf,
    },
    /// List stored test cases.
    List {
        /// Output format: text | json | csv | ndjson.
        #[arg(long, default_value = "text")]
        output: String,
    },
    /// Run stored tests locally through the executor seam (all, or only --id).
    Run {
        /// Run only this test id (repeatable); omit to run every test.
        #[arg(long)]
        id: Vec<String>,
        /// Override the project's target URL for this run.
        #[arg(long)]
        url: Option<String>,
        /// OpenAI model for spec-less LLM execution and failure analysis.
        #[arg(long, default_value = "gpt-4o-mini")]
        model: String,
        /// Print a single JSON array of results instead of PASS/FAIL lines.
        #[arg(long)]
        json: bool,
        /// On failure, write an LLM fix recommendation to testsprite_tests/fixes/<id>.md.
        #[arg(long)]
        fix: bool,
        /// Browser engine for frontend tests: chromium | firefox | webkit.
        /// Also enables per-case screenshots under testsprite_tests/shots/.
        #[arg(long)]
        browser: Option<String>,
    },
    /// Re-run stored tests; --heal regenerates fragility-failing LLM tests.
    Rerun {
        /// Re-run only this test id (repeatable); omit to re-run every test.
        #[arg(long)]
        id: Vec<String>,
        /// Override the project's target URL for this run.
        #[arg(long)]
        url: Option<String>,
        /// OpenAI model for failure analysis and healing.
        #[arg(long, default_value = "gpt-4o-mini")]
        model: String,
        /// Regenerate and re-run cases whose failure is diagnosed as fragility.
        #[arg(long)]
        heal: bool,
        /// Print a single JSON array of results instead of PASS/FAIL lines.
        #[arg(long)]
        json: bool,
    },
    /// Generate test cases with the LLM (needs an OpenAI key).
    Generate {
        /// Path to a code-summary JSON file.
        #[arg(long)]
        from: Option<PathBuf>,
        /// Plain-text instruction describing what to test.
        #[arg(long)]
        instruction: Option<String>,
        /// Modality to tag generated cases with: backend | frontend | mcp | rust.
        #[arg(long = "type")]
        kind: Option<String>,
        /// OpenAI model for PRD/plan generation.
        #[arg(long, default_value = "gpt-4o-mini")]
        model: String,
        /// --cover: generate a test per function under --path (default cwd).
        #[arg(long)]
        cover: bool,
        #[arg(long)]
        path: Option<PathBuf>,
    },
    /// Validate stored test JSON offline.
    Lint {
        /// Print a single CliLintReport JSON object instead of text lines.
        #[arg(long)]
        json: bool,
    },
    /// Compare two stored test results (results/<id>.json).
    Diff {
        #[arg()]
        a: String,
        #[arg()]
        b: String,
        /// Print a single CliRunDiff JSON object instead of text lines.
        #[arg(long)]
        json: bool,
    },
    /// Emit a schema-correct starter test (backend python | frontend plan).
    Scaffold {
        /// Modality: backend | frontend.
        #[arg(long = "type", value_name = "KIND")]
        kind: String,
        /// Print the scaffold as a single JSON object instead of raw text.
        #[arg(long)]
        json: bool,
    },
    /// Write a stored test's code to a file (cargo/CI can own it).
    Emit {
        #[arg()]
        id: String,
        #[arg(long)]
        out: PathBuf,
    },
    /// Rename a stored test's title (fixes munged TC000 duplicates).
    Rename {
        #[arg()]
        id: String,
        #[arg(long)]
        title: String,
    },
    /// Group failing tests by root cause (failureKind) — fix causes, not symptoms.
    Triage {
        #[arg(long)]
        json: bool,
    },
    /// Export all stored test definitions as JSON (for version control).
    Export {
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Import test definitions from a JSON file (array of test objects).
    Import {
        #[arg()]
        file: PathBuf,
    },
    /// Replay a test N times and report a stability score (auth/infra 'blocked' runs are excluded, never scored as flaky).
    Flaky {
        #[arg()]
        id: String,
        #[arg(long, default_value_t = 5)]
        runs: usize,
        #[arg(long, default_value = "gpt-4o-mini")]
        model: String,
        #[arg(long)]
        json: bool,
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
        Command::Doctor => std::process::exit(local::doctor::doctor()?),
        Command::Account | Command::Check => run_account().await,
        Command::GenerateCodeAndExecute => run_console_execute().await,
        Command::Backend { port, model, kind } => {
            server::serve(port, &model, server::executors::TestKind::parse(&kind)).await
        }
        Command::Project { cmd } => run_project(cmd).await,
        Command::Coverage { path, json, gaps } => {
            let scan = path.unwrap_or(std::env::current_dir()?);
            let code = if gaps {
                let root = std::env::current_dir()?;
                local::coverage::gaps_report(&root, &scan, json).await?
            } else {
                local::coverage::coverage(&scan, json).await?
            };
            std::process::exit(code);
        }
        Command::Gate { url, model } => {
            let root = std::env::current_dir()?;
            let code = local::gate::gate(&root, url.as_deref(), &model).await?;
            std::process::exit(code);
        }
        Command::Test { cmd } => run_test(cmd).await,
        Command::Visual { baseline, current } => {
            let diff = local::visual::diff(&baseline, &current)?;
            let regression = diff.is_regression();
            println!("same_dimensions: {}", diff.same_dimensions);
            println!("diff_ratio: {:.6}", diff.diff_ratio);
            if regression {
                println!(
                    "REGRESSION (threshold {:.4})",
                    local::visual::REGRESSION_THRESHOLD
                );
            } else {
                println!("OK (within threshold {:.4})", local::visual::REGRESSION_THRESHOLD);
            }
            std::process::exit(if regression { 1 } else { 0 });
        }
        Command::Agent { cmd } => run_agent(cmd).await,
        Command::Completions { shell } => {
            let mut cmd = <Cli as CommandFactory>::command();
            clap_complete::generate(shell, &mut cmd, "testsprite-rs", &mut std::io::stdout());
            Ok(())
        }
        #[cfg(feature = "discord")]
        Command::Discord { token } => discord::run(token).await,
    }
}

async fn run_agent(cmd: AgentCmd) -> Result<()> {
    let root = std::env::current_dir()?;
    match cmd {
        AgentCmd::Message {
            conversation,
            model,
            message,
        } => {
            let text = message.join(" ");
            let out =
                local::agent::message(&root, conversation.as_deref(), &text, &model).await?;
            println!("{}", serde_json::to_string_pretty(&out)?);
            Ok(())
        }
        AgentCmd::Approve {
            conversation,
            action_id,
            reject,
            model,
        } => {
            let out =
                local::agent::resolve(&root, &conversation, action_id, !reject, &model).await?;
            println!("{}", serde_json::to_string_pretty(&out)?);
            Ok(())
        }
        AgentCmd::History { conversation } => {
            let out = local::agent::history(&root, &conversation).await?;
            println!("{}", serde_json::to_string_pretty(&out)?);
            Ok(())
        }
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

async fn run_project(cmd: ProjectCmd) -> Result<()> {
    let root = std::env::current_dir()?;
    match cmd {
        ProjectCmd::Init { kind, name, url } => {
            let kind = server::executors::TestKind::parse(&kind);
            let path = local::project::init(&root, kind, &name, url.as_deref()).await?;
            println!("wrote {}", path.display());
            Ok(())
        }
        ProjectCmd::Show => local::project::show(&root).await,
    }
}

async fn run_test(cmd: TestCmd) -> Result<()> {
    let root = std::env::current_dir()?;
    match cmd {
        TestCmd::Add { file } => {
            let id = local::store::add(&root, &file).await?;
            println!("added test {id}");
            Ok(())
        }
        TestCmd::List { output } => {
            let tests = local::store::list(&root).await?;
            let kind_str = |t: &local::LocalTest| -> String {
                t.kind
                    .as_ref()
                    .and_then(|k| serde_json::to_value(k).ok())
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default()
            };
            match output.as_str() {
                "text" => {
                    for t in &tests {
                        println!("{}  {}", t.id, t.title);
                    }
                }
                "json" => {
                    let rows: Vec<serde_json::Value> = tests
                        .iter()
                        .map(|t| {
                            serde_json::json!({ "id": t.id, "title": t.title, "kind": kind_str(t) })
                        })
                        .collect();
                    println!("{}", serde_json::to_string_pretty(&rows)?);
                }
                "csv" => {
                    println!("id,title,kind");
                    for t in &tests {
                        println!(
                            "{},{},{}",
                            csv_field(&t.id),
                            csv_field(&t.title),
                            csv_field(&kind_str(t))
                        );
                    }
                }
                "ndjson" => {
                    for t in &tests {
                        let row =
                            serde_json::json!({ "id": t.id, "title": t.title, "kind": kind_str(t) });
                        println!("{}", serde_json::to_string(&row)?);
                    }
                }
                other => anyhow::bail!("invalid --output '{other}': expected text|json|csv|ndjson"),
            }
            Ok(())
        }
        TestCmd::Run {
            id,
            url,
            model,
            json,
            fix,
            browser,
        } => {
            let code =
                local::run::run(&root, &id, url.as_deref(), &model, json, fix, browser.as_deref())
                    .await?;
            std::process::exit(code);
        }
        TestCmd::Rerun {
            id,
            url,
            model,
            heal,
            json,
        } => {
            let code = local::rerun::rerun(&root, &id, url.as_deref(), &model, heal, json).await?;
            std::process::exit(code);
        }
        TestCmd::Generate {
            from,
            instruction,
            kind,
            model,
            cover,
            path,
        } => {
            if cover {
                let p = path.unwrap_or(std::env::current_dir()?);
                let ids = local::generate::generate_cover(&root, &p, &model).await?;
                println!("generated {} coverage test(s)", ids.len());
                for id in &ids {
                    println!("  {id}");
                }
                return Ok(());
            }
            let kind = kind.as_deref().map(server::executors::TestKind::parse);
            let ids = local::generate::generate(
                &root,
                from.as_deref(),
                instruction.as_deref(),
                &model,
                kind,
            )
            .await?;
            println!("generated {} test(s)", ids.len());
            for id in &ids {
                println!("  {id}");
            }
            Ok(())
        }
        TestCmd::Lint { json } => {
            let code = local::lint::lint(&root, json).await?;
            std::process::exit(code);
        }
        TestCmd::Diff { a, b, json } => {
            let code = local::diff::diff(&root, &a, &b, json).await?;
            std::process::exit(code);
        }
        TestCmd::Scaffold { kind, json } => {
            let code = local::scaffold::scaffold(&kind, json)?;
            std::process::exit(code);
        }
        TestCmd::Emit { id, out } => {
            local::store::emit(&root, &id, &out).await?;
            println!("wrote {}", out.display());
            Ok(())
        }
        TestCmd::Rename { id, title } => {
            local::store::rename(&root, &id, &title).await?;
            println!("renamed {id} -> {title}");
            Ok(())
        }
        TestCmd::Triage { json } => {
            let code = local::triage::triage_report(&root, json).await?;
            std::process::exit(code);
        }
        TestCmd::Export { out } => {
            let v = local::store::export_all(&root).await?;
            let s = serde_json::to_string_pretty(&v)?;
            match out {
                Some(p) => {
                    std::fs::write(&p, &s)?;
                    println!("exported {} test(s) -> {}", v.len(), p.display());
                }
                None => println!("{s}"),
            }
            Ok(())
        }
        TestCmd::Import { file } => {
            let body = std::fs::read_to_string(&file)
                .with_context(|| format!("reading {}", file.display()))?;
            let tests: Vec<serde_json::Value> = serde_json::from_str(&body)
                .with_context(|| format!("{} is not a JSON array of test objects", file.display()))?;
            let ids = local::store::import_values(&root, &tests).await?;
            println!("imported {} test(s)", ids.len());
            Ok(())
        }
        TestCmd::Flaky { id, runs, model, json } => {
            let code = local::flaky::flaky_report(&root, &id, runs, &model, json).await?;
            std::process::exit(code);
        }
    }
}

/// Quote a CSV field in double-quotes (doubling any internal quotes) when it
/// contains a comma, quote, or newline; otherwise return it unquoted.
fn csv_field(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}
