//! testsprite-rs — a from-scratch Rust reimplementation of the TestSprite MCP
//! client (`@testsprite/testsprite-mcp`).
//!
//! It speaks the same backend protocol (`api.testsprite.com`), opens the same
//! reverse tunnel (`*.tun.testsprite.com`, yamux v2), and exposes the same 8
//! MCP tools over stdio. See README.md for the full flow.

mod backend;
mod cli;
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
#[cfg(test)]
mod testutil;
mod tools;
mod tunnel;
mod types;

use std::path::PathBuf;

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};

use cli::test::TestCmd;

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
    /// Make a repo testsprite-rs-aware for coding agents: install the onboard +
    /// verify skills into .claude/skills/ (local, no cloud, no account).
    Setup {
        /// Repo to set up (defaults to the current directory).
        #[arg(long)]
        path: Option<PathBuf>,
        /// Overwrite existing skill files instead of leaving them untouched.
        #[arg(long)]
        force: bool,
    },
    /// Diagnose the local environment (LLM key, cargo, coverage, node/playwright, docker, gh, python).
    Doctor {
        /// Emit a DoctorReport JSON object ({checks:[{name,status,detail}]}) instead of text.
        #[arg(long)]
        json: bool,
    },
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
        #[arg(long, default_value_t = crate::envs::default_model())]
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
        /// Report structural units nothing covers yet, instead of the full
        /// report. Prefers real execution data (cargo llvm-cov); where that is
        /// unavailable it falls back to matching function NAMES against stored
        /// tests and labels the report `evidence: named` — a worklist, not a
        /// measurement. Don't chase it to zero.
        #[arg(long)]
        gaps: bool,
        /// Measure ORACLE STRENGTH via mutation testing (cargo-mutants for Rust
        /// crates): seed faults and report the kill rate. Coverage says a line
        /// ran; this says a test would catch a bug in it.
        #[arg(long)]
        mutation: bool,
    },
    /// Run the suite as a CI gate: JUnit + JSON + best-effort gh PR comment; exit 1 on any failure.
    Gate {
        /// Override the project's target URL for this run.
        #[arg(long)]
        url: Option<String>,
        /// OpenAI model for spec-less LLM execution and failure analysis.
        #[arg(long, default_value_t = crate::envs::default_model())]
        model: String,
        /// Fast pre-gate: run one representative case per group/failure-cluster
        /// first; only escalate to the full suite if the smoke tier passes.
        #[arg(long)]
        smoke: bool,
        /// Fail the gate if the mutation kill score is below this percent
        /// (0-100). Off by default; requires cargo-mutants for a Rust target.
        #[arg(long)]
        min_mutation: Option<f64>,
    },
    /// The regression loop in one call: (optionally) generate tests for changed
    /// functions, run the suite, triage the failures, and surface one actionable
    /// result. Exit 1 unless green (nothing failed or left unverified).
    Loop {
        /// Run only the tests affected by changes since --since (else the whole
        /// suite); on an unattributable change, runs everything rather than
        /// reporting an empty success.
        #[arg(long)]
        changed: bool,
        /// Git ref to diff against for --changed (default: HEAD = uncommitted).
        #[arg(long)]
        since: Option<String>,
        /// Before running, generate tests for changed functions that no stored
        /// test covers yet (needs an OpenAI key; only acts with --changed).
        #[arg(long)]
        generate: bool,
        /// OpenAI model for generation, spec-less execution, and failure analysis.
        #[arg(long, default_value_t = crate::envs::default_model())]
        model: String,
        /// On failure, also write a fix recommendation to testsprite_tests/fixes/.
        #[arg(long)]
        fix: bool,
        /// Start the target app (via `project set-start`) before running.
        #[arg(long)]
        serve: bool,
        /// Refuse to run tests stamped with prdId unless that PRD was approved.
        #[arg(long)]
        require_approved_prd: bool,
        /// Total LLM token cap for the generation stage.
        #[arg(long)]
        budget: Option<u64>,
        /// Print a single JSON CycleReport instead of human lines.
        #[arg(long)]
        json: bool,
    },
    /// Emit CI config — a GitHub Actions workflow that runs the gate on every PR.
    Ci {
        #[command(subcommand)]
        cmd: CiCmd,
    },
    /// Inspect generated PRDs + test plans (the doc/summary -> PRD -> plan trail).
    Prd {
        #[command(subcommand)]
        cmd: PrdCmd,
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
    /// Manage local re-verification schedules (group + cadence); emit crontab lines.
    Schedule {
        #[command(subcommand)]
        cmd: ScheduleCmd,
    },
    /// Per-model telemetry scoreboard (pass rate, latency, tokens) from run
    /// history, with drift detection against a saved baseline.
    Bench {
        /// Print a single JSON object instead of human lines.
        #[arg(long)]
        json: bool,
        /// Overwrite the drift baseline with the current scoreboard.
        #[arg(long)]
        save_baseline: bool,
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
enum CiCmd {
    /// Write .github/workflows/testsprite.yml (gate on pull_request).
    Init {
        /// Overwrite an existing workflow file.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
enum PrdCmd {
    /// List generated PRDs, newest first.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Show a PRD's requirements + test plan (omit id for the latest).
    Show { id: Option<String> },
    /// Write an HTML review page for a PRD + generated test plan.
    Review {
        id: Option<String>,
        #[arg(long, default_value = "testsprite_tests/prd-review.html")]
        out: PathBuf,
    },
    /// Mark a PRD/test plan as reviewed and approved.
    Approve { id: Option<String> },
}

#[derive(Subcommand)]
enum AgentCmd {
    /// Send a message; the agent replies and may propose one pending action.
    Message {
        /// Existing conversation id to continue (omit to start a new one).
        #[arg(long)]
        conversation: Option<String>,
        /// OpenAI model for the conversational planner.
        #[arg(long, default_value_t = crate::envs::default_model())]
        model: String,
        /// Auto-execute the proposed action immediately (no separate approve step).
        #[arg(long)]
        auto_approve: bool,
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
        #[arg(long, default_value_t = crate::envs::default_model())]
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
    /// Generate TestSprite-style code summary (tech stack, features/files, endpoints).
    Summarize {
        /// Directory to scan (defaults to current repo).
        #[arg(long)]
        path: Option<PathBuf>,
        /// Output file (default: testsprite_tests/tmp/code_summary.yaml).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Print the summary JSON to stdout too.
        #[arg(long)]
        json: bool,
    },
    /// Set a path-param variable ({id} -> value) in testsprite_tests/variables.json.
    SetVar {
        /// Variable name (the {name} in a route, e.g. id).
        key: String,
        /// Value to substitute (e.g. a real UUID or record id).
        value: String,
    },
    /// Set the command that starts the target app (for `test run --serve`).
    SetStart {
        /// Shell command that starts the app on the project's target URL.
        command: String,
    },
    /// Ingest a loose `standard_prd.json` (any real TestSprite shape): normalize
    /// it, recover endpoints, and seed `testCredentials` / `test_environment`
    /// into variables.json (existing values are never overwritten).
    IngestPrd {
        /// Path to a standard_prd.json (or any PRD-shaped JSON).
        file: PathBuf,
        /// Persist the normalized PRD + recovered plan (like `test generate`).
        #[arg(long)]
        persist: bool,
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
        Command::Setup { path, force } => {
            let root = path.unwrap_or(std::env::current_dir()?);
            let code = local::setup::setup(&root, force)?;
            std::process::exit(code);
        }
        Command::Doctor { json } => std::process::exit(local::doctor::doctor(json)?),
        Command::Account | Command::Check => run_account().await,
        Command::GenerateCodeAndExecute => run_console_execute().await,
        Command::Backend { port, model, kind } => {
            server::serve(port, &model, server::executors::TestKind::parse(&kind)).await
        }
        Command::Project { cmd } => run_project(cmd).await,
        Command::Coverage {
            path,
            json,
            gaps,
            mutation,
        } => {
            let scan = path.unwrap_or(std::env::current_dir()?);
            let code = if mutation {
                // Mutation shells out to cargo-mutants (blocking); keep it off
                // the async reactor.
                let scan = scan.clone();
                tokio::task::spawn_blocking(move || {
                    local::mutation::mutation_report(&scan, json, 300)
                })
                .await??
            } else if gaps {
                let root = std::env::current_dir()?;
                local::coverage::gaps_report(&root, &scan, json).await?
            } else {
                local::coverage::coverage(&scan, json).await?
            };
            std::process::exit(code);
        }
        Command::Gate {
            url,
            model,
            smoke,
            min_mutation,
        } => {
            let root = std::env::current_dir()?;
            let opts = local::gate::GateOpts {
                url: url.as_deref(),
                model: &model,
                smoke,
                min_mutation,
            };
            let code = local::gate::gate(&root, opts).await?;
            std::process::exit(code);
        }
        Command::Loop {
            changed,
            since,
            generate,
            model,
            fix,
            serve,
            require_approved_prd,
            budget,
            json,
        } => {
            let root = std::env::current_dir()?;
            let opts = local::cycle::CycleOpts {
                changed,
                since: since.as_deref(),
                generate,
                model: &model,
                fix,
                serve,
                require_approved_prd,
                budget,
            };
            let code = local::cycle::cycle_report(&root, opts, json).await?;
            std::process::exit(code);
        }
        Command::Test { cmd } => cli::test::dispatch(cmd).await,
        Command::Ci { cmd } => match cmd {
            CiCmd::Init { force } => {
                let root = std::env::current_dir()?;
                let path = local::ci::init(&root, force)?;
                println!("wrote {}", path.display());
                Ok(())
            }
        },
        Command::Prd { cmd } => run_prd(cmd).await,
        Command::Visual { baseline, current } => {
            let diff = local::visual::diff(&baseline, &current)?;
            let regression = diff.is_regression();
            println!("same_dimensions: {}", diff.same_dimensions);
            println!("diff_ratio: {:.6}", diff.diff_ratio);
            if regression {
                println!(
                    "REGRESSION (threshold {:.4})",
                    local::visual::regression_threshold()
                );
            } else {
                println!(
                    "OK (within threshold {:.4})",
                    local::visual::regression_threshold()
                );
            }
            std::process::exit(if regression { 1 } else { 0 });
        }
        Command::Agent { cmd } => run_agent(cmd).await,
        Command::Schedule { cmd } => run_schedule(cmd).await,
        Command::Bench {
            json,
            save_baseline,
        } => {
            let root = std::env::current_dir()?;
            let code = local::bench::bench_report(&root, json, save_baseline).await?;
            std::process::exit(code);
        }
        Command::Completions { shell } => {
            let mut cmd = <Cli as CommandFactory>::command();
            clap_complete::generate(shell, &mut cmd, "testsprite-rs", &mut std::io::stdout());
            Ok(())
        }
        #[cfg(feature = "discord")]
        Command::Discord { token } => discord::run(token).await,
    }
}

#[derive(Subcommand)]
enum ScheduleCmd {
    /// Add or update a named schedule (group + cadence: hourly|daily|weekly|monthly).
    Add {
        /// Schedule name.
        name: String,
        /// The test group/list to run.
        #[arg(long)]
        group: String,
        /// Cadence: hourly | daily | weekly | monthly.
        #[arg(long, default_value = "daily")]
        cadence: String,
    },
    /// List stored schedules.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Remove a named schedule.
    Remove {
        /// Schedule name.
        name: String,
    },
    /// Run a schedule's group now.
    Run {
        /// Schedule name.
        name: String,
    },
    /// Emit crontab lines for all schedules (pipe to `crontab -`).
    Crontab,
}

async fn run_schedule(cmd: ScheduleCmd) -> Result<()> {
    let root = std::env::current_dir()?;
    match cmd {
        ScheduleCmd::Add {
            name,
            group,
            cadence,
        } => {
            local::schedule::add(&root, &name, &group, &cadence)?;
            println!("scheduled '{name}' -> group '{group}' ({cadence})");
            Ok(())
        }
        ScheduleCmd::List { json } => {
            let schedules = local::schedule::list(&root)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&schedules)?);
            } else if schedules.is_empty() {
                println!("no schedules");
            } else {
                for s in &schedules {
                    println!("{}  group={}  cadence={}", s.name, s.group, s.cadence);
                }
            }
            Ok(())
        }
        ScheduleCmd::Remove { name } => {
            if local::schedule::remove(&root, &name)? {
                println!("removed schedule '{name}'");
            } else {
                println!("no schedule named '{name}'");
            }
            Ok(())
        }
        ScheduleCmd::Run { name } => {
            let Some(s) = local::schedule::get(&root, &name)? else {
                anyhow::bail!("no schedule named '{name}'");
            };
            let ids: Vec<String> = local::store::list(&root)
                .await?
                .into_iter()
                .filter(|t| t.group() == Some(s.group.as_str()))
                .map(|t| t.id)
                .collect();
            if ids.is_empty() {
                println!("schedule '{name}': no tests in group '{}'", s.group);
                return Ok(());
            }
            let model = crate::envs::default_model();
            let code = local::run::run(
                &root, &ids, None, &model, false, false, None, 1, false, false,
            )
            .await?;
            std::process::exit(code);
        }
        ScheduleCmd::Crontab => {
            let bin = std::env::current_exe()?.to_string_lossy().to_string();
            print!("{}", local::schedule::crontab(&root, &bin)?);
            Ok(())
        }
    }
}
async fn run_agent(cmd: AgentCmd) -> Result<()> {
    let root = std::env::current_dir()?;
    match cmd {
        AgentCmd::Message {
            conversation,
            model,
            auto_approve,
            message,
        } => {
            let text = message.join(" ");
            let out =
                local::agent::message(&root, conversation.as_deref(), &text, &model, auto_approve)
                    .await?;
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
        ProjectCmd::Summarize { path, out, json } => {
            let scan = path.unwrap_or(root);
            let (written, summary) = local::summary::write(&scan, out.as_deref())?;
            if json {
                println!("{}", serde_json::to_string_pretty(&summary)?);
            } else {
                println!("wrote {}", written.display());
            }
            Ok(())
        }
        ProjectCmd::SetVar { key, value } => {
            let vars = local::project::set_variable(&root, &key, &value)?;
            println!("set {key} = {value}  ({} variable(s) total)", vars.len());
            Ok(())
        }
        ProjectCmd::SetStart { command } => {
            local::project::set_start(&root, &command).await?;
            println!("start command set — `testsprite-rs test run --serve` will use it");
            Ok(())
        }
        ProjectCmd::IngestPrd { file, persist } => {
            let r = local::generate::ingest_prd_file(&root, &file, persist).await?;
            println!(
                "ingested {}: {} endpoint(s), {} requirement(s), {} credential role(s){}",
                file.display(),
                r.endpoints,
                r.requirements,
                r.credentials,
                if r.timing_rules > 0 {
                    format!(", {} timing rule(s)", r.timing_rules)
                } else {
                    String::new()
                }
            );
            if r.has_test_data_strategy {
                println!("  test_data_strategy carried into the PRD (seeds per-case at run time)");
            }
            if r.seeded_vars > 0 {
                println!(
                    "  seeded {} variable(s) into variables.json (existing values untouched)",
                    r.seeded_vars
                );
            }
            if let Some(id) = &r.prd_id {
                println!(
                    "  persisted PRD {id} + {} recovered test case(s)",
                    r.plan_ids.len()
                );
            } else {
                println!("  (dry run — pass --persist to store the normalized PRD + plan)");
            }
            Ok(())
        }
    }
}

async fn run_prd(cmd: PrdCmd) -> Result<()> {
    let root = std::env::current_dir()?;
    match cmd {
        PrdCmd::List { json } => {
            let prds = local::store::list_prds(&root).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&prds)?);
            } else if prds.is_empty() {
                println!(
                    "no PRDs yet — run `testsprite-rs test generate --doc <file>` (or --instruction)"
                );
            } else {
                for p in &prds {
                    println!(
                        "{}  {} feature(s), {} case(s)  [{}]  {}",
                        p["id"].as_str().unwrap_or(""),
                        p["features"],
                        p["cases"],
                        p["createdAt"].as_str().unwrap_or(""),
                        p["source"].as_str().unwrap_or(""),
                    );
                }
            }
            Ok(())
        }
        PrdCmd::Show { id } => {
            let id = match id {
                Some(id) => id,
                None => local::store::latest_prd_id(&root)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("no PRDs yet — generate one first"))?,
            };
            let prd = local::store::load_prd(&root, &id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("no PRD with id {id}"))?;
            println!("{}", serde_json::to_string_pretty(&prd)?);
            Ok(())
        }
        PrdCmd::Review { id, out } => {
            let id = match id {
                Some(id) => id,
                None => local::store::latest_prd_id(&root)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("no PRDs yet — generate one first"))?,
            };
            let path = local::artifact::write_prd_review(&root, &id, &out).await?;
            println!("wrote {}", path.display());
            Ok(())
        }
        PrdCmd::Approve { id } => {
            let id = match id {
                Some(id) => id,
                None => local::store::latest_prd_id(&root)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("no PRDs yet — generate one first"))?,
            };
            let ts = local::store::approve_prd(&root, &id).await?;
            println!("approved {id} at {ts}");
            Ok(())
        }
    }
}


#[cfg(test)]
mod cli_unit_tests {
    use super::*;

    // ---- Cli::try_parse_from -----------------------------------------------

    #[test]
    fn no_args_parses_to_no_subcommand() {
        let cli = Cli::try_parse_from(["testsprite-rs"]).expect("bare invocation parses");
        assert!(
            cli.command.is_none(),
            "no subcommand => None (defaults to Serve)"
        );
    }

    #[test]
    fn test_list_output_json_parses_into_list_variant() {
        let cli = Cli::try_parse_from(["testsprite-rs", "test", "list", "--output", "json"])
            .expect("`test list --output json` parses");
        match cli.command {
            Some(Command::Test {
                cmd: TestCmd::List { output, group },
            }) => {
                assert_eq!(output, "json");
                assert!(group.is_none());
            }
            _ => panic!("expected Command::Test(TestCmd::List)"),
        }
    }

    #[test]
    fn coverage_gaps_flag_parses() {
        let cli = Cli::try_parse_from(["testsprite-rs", "coverage", "--gaps"])
            .expect("`coverage --gaps` parses");
        match cli.command {
            Some(Command::Coverage {
                path, json, gaps, ..
            }) => {
                assert!(gaps, "--gaps sets gaps=true");
                assert!(!json, "json defaults false");
                assert!(path.is_none(), "path defaults None");
            }
            _ => panic!("expected Command::Coverage"),
        }
    }

    #[test]
    fn schedule_add_parses_name_group_and_default_cadence() {
        let cli = Cli::try_parse_from([
            "testsprite-rs",
            "schedule",
            "add",
            "nightly",
            "--group",
            "g",
        ])
        .expect("`schedule add nightly --group g` parses");
        match cli.command {
            Some(Command::Schedule {
                cmd:
                    ScheduleCmd::Add {
                        name,
                        group,
                        cadence,
                    },
            }) => {
                assert_eq!(name, "nightly");
                assert_eq!(group, "g");
                assert_eq!(cadence, "daily", "cadence defaults to daily");
            }
            _ => panic!("expected Command::Schedule(ScheduleCmd::Add)"),
        }
    }

    #[test]
    fn unknown_flag_is_a_parse_error() {
        let err = Cli::try_parse_from(["testsprite-rs", "--definitely-not-a-flag"]);
        assert!(err.is_err(), "an unknown top-level flag must error");
    }
}
