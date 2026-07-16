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

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, Subcommand};

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
        /// Report structural units nothing covers yet, instead of the full
        /// report. Prefers real execution data (cargo llvm-cov); where that is
        /// unavailable it falls back to matching function NAMES against stored
        /// tests and labels the report `evidence: named` — a worklist, not a
        /// measurement. Don't chase it to zero.
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
        #[arg(long, default_value = "gpt-4o-mini")]
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
        #[arg(long, default_value = "gpt-4o-mini")]
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
        /// Only list tests in this group/list.
        #[arg(long)]
        group: Option<String>,
    },
    /// Show one stored test definition as JSON.
    Get {
        #[arg()]
        id: String,
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
        /// Run only tests tagged with this group/list.
        #[arg(long)]
        group: Option<String>,
        /// Run independent tests in the same dependency wave concurrently (default 1 = sequential).
        #[arg(long, default_value_t = 1)]
        jobs: usize,
        /// Start the target app before running (uses `project set-start`), stop it after.
        #[arg(long)]
        serve: bool,
        /// Refuse to run tests stamped with prdId unless that PRD was approved.
        #[arg(long)]
        require_approved_prd: bool,
        /// Code Diff Mode: run only tests affected by files changed since --since.
        #[arg(long)]
        changed: bool,
        /// Git ref to diff against for --changed (default: HEAD = uncommitted changes).
        #[arg(long)]
        since: Option<String>,
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
        /// Re-run only tests whose most recent run FAILED (replaces --id).
        #[arg(long)]
        failed: bool,
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
        /// A Postman/OpenAPI/HAR doc → deterministic spec cases (no key), or a README/notes doc → LLM PRD. Accepts a file path OR an http(s) URL (e.g. a utoipa app's /api-docs/openapi.json).
        #[arg(long)]
        doc: Option<PathBuf>,
        /// Code Diff Mode: generate tests only for functions changed since --since.
        #[arg(long)]
        changed: bool,
        /// Git ref to diff against for --changed (default: HEAD).
        #[arg(long)]
        since: Option<String>,
    },
    /// Explore a live frontend page and generate deterministic planSteps candidates.
    Explore {
        /// URL to explore (defaults to project targetUrl).
        #[arg(long)]
        url: Option<String>,
        /// Same-origin crawl depth (0 = current page only).
        #[arg(long, default_value_t = 1)]
        depth: usize,
        /// Maximum pages to inventory.
        #[arg(long, default_value_t = 8)]
        limit: usize,
        /// Opt-in: click visible controls on fresh pages and generate action+assertion probes.
        #[arg(long)]
        interactions: bool,
        /// Store generated candidates into the local test DB.
        #[arg(long)]
        store: bool,
        /// Write the exploration report JSON to a file.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Use the TestSprite LLM to adversarially propose high-signal QA tests.
    Audit {
        /// Path to scan for coverage gaps (default cwd).
        #[arg(long)]
        path: Option<PathBuf>,
        /// OpenAI model for adversarial planning.
        #[arg(long, default_value = "gpt-4o-mini")]
        model: String,
        /// Store proposed cases in the local DB.
        #[arg(long)]
        store: bool,
        /// Write proposed cases JSON to a file.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Code Diff Mode: show which functions changed (git) and which tests they affect.
    Changed {
        /// Git ref to diff against (default: HEAD = uncommitted changes).
        #[arg(long)]
        since: Option<String>,
        /// Print a single JSON object instead of human lines.
        #[arg(long)]
        json: bool,
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
    /// Download one run's artifact bundle (request/response evidence, code, screenshots).
    Artifact {
        #[command(subcommand)]
        cmd: ArtifactCmd,
    },
    /// Export a latest-results report (Markdown by default; .pdf writes PDF; JSON with --json).
    Report {
        /// Output path.
        #[arg(long, default_value = "testsprite_tests/testsprite-report.md")]
        out: PathBuf,
        /// Write JSON instead of Markdown.
        #[arg(long)]
        json: bool,
    },
    /// Export a static local dashboard HTML.
    Dashboard {
        /// Output path.
        #[arg(long, default_value = "testsprite_tests/dashboard.html")]
        out: PathBuf,
    },
    /// Replace a frontend test's planSteps from a JSON array file.
    Plan {
        #[command(subcommand)]
        cmd: PlanCmd,
    },
    /// Write a visual replay HTML for a frontend test's stored steps/screenshots.
    Replay {
        #[arg()]
        id: String,
        #[arg(long, default_value = "testsprite_tests/replay.html")]
        out: PathBuf,
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
    /// Delete a stored test and its run history (storing is an upsert, so
    /// without this the suite only ever grows).
    Delete {
        #[arg()]
        id: String,
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
        /// Start the target app before each replay; without it a backend test
        /// with nothing listening scores every run blocked ("inconclusive").
        #[arg(long)]
        serve: bool,
        #[arg(long)]
        json: bool,
    },
    /// Show a test's full run history (append-only runs table, newest first).
    History {
        #[arg()]
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Show prior definitions of a test, newest first — what it asserted before
    /// an automated rewrite (`rerun --heal`) replaced it.
    Revisions {
        #[arg()]
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Prune run history, keeping the latest N runs per test (bounds testsprite.db).
    Prune {
        /// Keep the latest N runs per test (0 = keep all).
        #[arg(long, default_value_t = 200)]
        keep: usize,
        /// Only prune this test id (omit to prune every test).
        #[arg(long)]
        id: Option<String>,
    },
}

#[derive(Subcommand)]
enum ArtifactCmd {
    /// Write a run bundle directory for `run_id`.
    Get {
        /// Numeric `runs.run_id` (see `test history <id>`).
        run_id: i64,
        /// Output directory.
        #[arg(long)]
        out: PathBuf,
    },
}

#[derive(Subcommand)]
enum PlanCmd {
    /// Replace `planSteps` with the JSON array in `file`.
    Put {
        #[arg()]
        id: String,
        #[arg(long)]
        file: PathBuf,
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
        Command::Loop {
            changed,
            since,
            generate,
            model,
            fix,
            serve,
            require_approved_prd,
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
            };
            let code = local::cycle::cycle_report(&root, opts, json).await?;
            std::process::exit(code);
        }
        Command::Test { cmd } => run_test(cmd).await,
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
                    local::visual::REGRESSION_THRESHOLD
                );
            } else {
                println!(
                    "OK (within threshold {:.4})",
                    local::visual::REGRESSION_THRESHOLD
                );
            }
            std::process::exit(if regression { 1 } else { 0 });
        }
        Command::Agent { cmd } => run_agent(cmd).await,
        Command::Schedule { cmd } => run_schedule(cmd).await,
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
            let code = local::run::run(
                &root,
                &ids,
                None,
                "gpt-4o-mini",
                false,
                false,
                None,
                1,
                false,
                false,
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

async fn run_test(cmd: TestCmd) -> Result<()> {
    let root = std::env::current_dir()?;
    match cmd {
        TestCmd::Add { file } => {
            let id = local::store::add(&root, &file).await?;
            println!("added test {id}");
            Ok(())
        }
        TestCmd::List { output, group } => {
            let tests = local::store::list(&root).await?;
            let tests: Vec<local::LocalTest> = match group.as_deref() {
                Some(g) => tests.into_iter().filter(|t| t.group() == Some(g)).collect(),
                None => tests,
            };
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
                        let row = serde_json::json!({ "id": t.id, "title": t.title, "kind": kind_str(t) });
                        println!("{}", serde_json::to_string(&row)?);
                    }
                }
                other => anyhow::bail!("invalid --output '{other}': expected text|json|csv|ndjson"),
            }
            Ok(())
        }
        TestCmd::Get { id } => {
            let value = local::store::get_value(&root, &id).await?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
        TestCmd::Run {
            id,
            url,
            model,
            json,
            fix,
            browser,
            group,
            jobs,
            changed,
            since,
            serve,
            require_approved_prd,
        } => {
            let ids = if changed {
                let since = since.as_deref().unwrap_or("HEAD");
                let cs = local::changed::changed_surface(&root, since)?;
                match local::changed::select(&root, &cs).await? {
                    local::changed::Selection::NoChanges => {
                        println!("no source changes since {since} — nothing to verify");
                        return Ok(());
                    }
                    local::changed::Selection::Affected(ids) => ids,
                    local::changed::Selection::Unattributable { changed_units } => {
                        // Not "nothing to do" — "I cannot tell". Reporting success
                        // here would greenlight an unverified change, so fall back
                        // to the full suite instead.
                        eprintln!(
                            "warning: {changed_units} function(s) changed since {since} but no \
                             stored test could be attributed to them (tests are matched by \
                             function-name mention, which cannot see through spec/command \
                             tests) — running the full suite instead of reporting success"
                        );
                        Vec::new()
                    }
                }
            } else {
                match group.as_deref() {
                    Some(g) => {
                        let matched: Vec<String> = local::store::list(&root)
                            .await?
                            .into_iter()
                            .filter(|t| t.group() == Some(g))
                            .map(|t| t.id)
                            .collect();
                        if matched.is_empty() {
                            println!("no tests in group '{g}'");
                            return Ok(());
                        }
                        matched
                    }
                    None => id,
                }
            };
            let code = local::run::run(
                &root,
                &ids,
                url.as_deref(),
                &model,
                json,
                fix,
                browser.as_deref(),
                jobs,
                serve,
                require_approved_prd,
            )
            .await?;
            std::process::exit(code);
        }
        TestCmd::Rerun {
            id,
            url,
            model,
            heal,
            json,
            failed,
        } => {
            let ids = if failed {
                let reds = local::store::last_failed_ids(&root).await?;
                if reds.is_empty() {
                    println!("no failed tests to rerun");
                    return Ok(());
                }
                reds
            } else {
                id
            };
            let code = local::rerun::rerun(&root, &ids, url.as_deref(), &model, heal, json).await?;
            std::process::exit(code);
        }
        TestCmd::Generate {
            from,
            instruction,
            kind,
            model,
            cover,
            path,
            doc,
            changed,
            since,
        } => {
            if cover {
                let p = path.unwrap_or(std::env::current_dir()?);
                let out = local::generate::generate_cover(&root, &p, &model).await?;
                println!("generated {} coverage test(s)", out.test_ids.len());
                for id in &out.test_ids {
                    println!("  {id}");
                }
                return Ok(());
            }
            if changed {
                let since = since.as_deref().unwrap_or("HEAD");
                let out = local::generate::generate_changed(&root, since, &model).await?;
                if out.test_ids.is_empty() {
                    println!(
                        "no changed functions need new tests (nothing changed, or all covered)"
                    );
                } else {
                    println!(
                        "generated {} test(s) for changed functions",
                        out.test_ids.len()
                    );
                    for id in &out.test_ids {
                        println!("  {id}");
                    }
                }
                return Ok(());
            }
            let kind = kind.as_deref().map(server::executors::TestKind::parse);
            let out = local::generate::generate(
                &root,
                from.as_deref(),
                instruction.as_deref(),
                doc.as_deref(),
                &model,
                kind,
            )
            .await?;
            if let Some(prd_id) = &out.prd_id {
                println!("PRD {prd_id}  (inspect: testsprite-rs prd show {prd_id})");
            }
            println!("generated {} test(s)", out.test_ids.len());
            for id in &out.test_ids {
                println!("  {id}");
            }
            Ok(())
        }
        TestCmd::Explore {
            url,
            depth,
            limit,
            interactions,
            store,
            out,
        } => {
            let target = match url {
                Some(u) => u,
                None => local::project::load(&root)
                    .await?
                    .target_url
                    .ok_or_else(|| anyhow::anyhow!("no --url and project has no targetUrl"))?,
            };
            let report = local::explore::explore(
                &root,
                local::explore::ExploreOpts {
                    url: &target,
                    store,
                    depth,
                    limit,
                    interactions,
                },
            )
            .await?;
            if let Some(out) = out {
                if let Some(parent) = out.parent()
                    && !parent.as_os_str().is_empty()
                {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&out, serde_json::to_string_pretty(&report)?)?;
                println!("wrote {}", out.display());
            } else {
                println!("{}", serde_json::to_string_pretty(&report)?);
            }
            Ok(())
        }
        TestCmd::Audit {
            path,
            model,
            store,
            out,
        } => {
            let scan = path.unwrap_or(std::env::current_dir()?);
            let audit = local::generate::adversarial(&root, &scan, &model, store).await?;
            if let Some(out) = out {
                if let Some(parent) = out.parent()
                    && !parent.as_os_str().is_empty()
                {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&out, serde_json::to_string_pretty(&audit.cases)?)?;
                println!("wrote {}", out.display());
            }
            println!("adversarial proposed {} test(s)", audit.cases.len());
            if store {
                for id in &audit.test_ids {
                    println!("  {id}");
                }
            }
            Ok(())
        }
        TestCmd::Changed { since, json } => {
            let since = since.as_deref().unwrap_or("HEAD");
            let code = local::changed::changed_report(&root, since, json).await?;
            std::process::exit(code);
        }
        TestCmd::Lint { json } => {
            let code = local::lint::lint(&root, json).await?;
            std::process::exit(code);
        }
        TestCmd::Diff { a, b, json } => {
            let code = local::diff::diff(&root, &a, &b, json).await?;
            std::process::exit(code);
        }
        TestCmd::Artifact { cmd } => match cmd {
            ArtifactCmd::Get { run_id, out } => {
                let dir = local::artifact::get(&root, run_id, &out).await?;
                println!("wrote {}", dir.display());
                Ok(())
            }
        },
        TestCmd::Report { out, json } => {
            let path = local::artifact::write_report(&root, &out, json).await?;
            println!("wrote {}", path.display());
            Ok(())
        }
        TestCmd::Dashboard { out } => {
            let path = local::artifact::write_dashboard(&root, &out).await?;
            println!("wrote {}", path.display());
            Ok(())
        }
        TestCmd::Plan { cmd } => match cmd {
            PlanCmd::Put { id, file } => {
                let body = std::fs::read_to_string(&file)?;
                let steps: serde_json::Value = serde_json::from_str(&body)
                    .or_else(|_| serde_yaml::from_str(&body))
                    .map_err(|e| anyhow::anyhow!("parsing {} as JSON/YAML: {e}", file.display()))?;
                local::store::put_plan_steps(&root, &id, steps).await?;
                println!("updated planSteps for {id}");
                Ok(())
            }
        },
        TestCmd::Replay { id, out } => {
            let path = local::artifact::write_replay(&root, &id, &out).await?;
            println!("wrote {}", path.display());
            Ok(())
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
        TestCmd::Delete { id } => {
            if local::store::delete(&root, &id).await? {
                println!("deleted {id}");
                Ok(())
            } else {
                anyhow::bail!("no test {id}")
            }
        }
        TestCmd::Revisions { id, json } => {
            let revs = local::store::revisions(&root, &id).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&revs)?);
            } else if revs.is_empty() {
                println!("no prior revisions of {id}");
            } else {
                for r in &revs {
                    let title = r["body"]["title"].as_str().unwrap_or("");
                    println!(
                        "rev {}  {}  [{}]  {title}",
                        r["revId"],
                        r["createdAt"].as_str().unwrap_or(""),
                        r["reason"].as_str().unwrap_or("")
                    );
                }
            }
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
            let tests: Vec<serde_json::Value> = serde_json::from_str(&body).with_context(|| {
                format!("{} is not a JSON array of test objects", file.display())
            })?;
            let ids = local::store::import_values(&root, &tests).await?;
            println!("imported {} test(s)", ids.len());
            Ok(())
        }
        TestCmd::Flaky {
            id,
            runs,
            model,
            serve,
            json,
        } => {
            let code = local::flaky::flaky_report(&root, &id, runs, &model, serve, json).await?;
            std::process::exit(code);
        }
        TestCmd::History { id, json } => {
            let runs = local::store::run_history(&root, &id).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&runs)?);
            } else if runs.is_empty() {
                println!("no runs recorded for {id}");
            } else {
                println!("{} run(s) for {id} (newest first):", runs.len());
                for r in &runs {
                    let run_id = r.get("run_id").and_then(|v| v.as_i64()).unwrap_or(0);
                    let mark = if r.get("passed").and_then(|v| v.as_bool()).unwrap_or(false) {
                        "PASS"
                    } else {
                        "FAIL"
                    };
                    let verdict = r.get("verdict").and_then(|v| v.as_str()).unwrap_or("-");
                    let fk = r.get("failureKind").and_then(|v| v.as_str()).unwrap_or("-");
                    let when = r.get("created_at").and_then(|v| v.as_str()).unwrap_or("");
                    println!("  #{run_id}  {when}  {mark}  verdict={verdict}  failureKind={fk}");
                }
            }
            Ok(())
        }
        TestCmd::Prune { keep, id } => {
            let deleted = match id {
                Some(id) => local::store::prune_runs(&root, &id, keep).await?,
                None => local::store::prune_all(&root, keep).await?,
            };
            println!("pruned {deleted} run row(s), keeping latest {keep} per test");
            Ok(())
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
