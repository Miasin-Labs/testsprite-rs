//! Dispatch for the `test` subcommand group.
//!
//! The `TestCmd` clap tree and its ~460-line dispatch used to live inline in
//! `main.rs`; they are the largest command surface by far, so they live here to
//! keep the binary root readable. This is glue only — every arm delegates to a
//! `local::*` module (the real logic) exactly as it did before.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Subcommand;

use crate::{local, server};

#[derive(Subcommand)]
pub enum TestCmd {
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
        #[arg(long, default_value_t = crate::envs::default_model())]
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
        #[arg(long, default_value_t = crate::envs::default_model())]
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
        #[arg(long, default_value_t = crate::envs::default_model())]
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
        /// Total LLM token cap; generation stops early once reached.
        #[arg(long)]
        budget: Option<u64>,
        /// Skip the acceptance gate (LLM cases are otherwise run once against
        /// the current baseline; a fail-on-green oracle is quarantined).
        #[arg(long)]
        no_gate: bool,
        /// Perspectives for --cover/--changed generation (comma-separated:
        /// normal,boundary,exception). Default: all three.
        #[arg(long)]
        views: Option<String>,
        /// --cover coverage-feedback rounds: after each round re-measure real
        /// coverage and regenerate only for still-uncovered functions, until a
        /// plateau or this many rounds. 1 = single-shot (default).
        #[arg(long, default_value_t = 1)]
        iterate: usize,
        /// --changed only: keep a generated regression test only if it FAILS on
        /// the base revision and PASSES on HEAD (proves it detects the change).
        #[arg(long)]
        fault_check: bool,
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
        /// OpenAI model(s) for adversarial planning. Comma-separate to run side-by-side and merge.
        #[arg(long, default_value_t = crate::envs::default_model())]
        model: String,
        /// Store proposed cases in the local DB.
        #[arg(long)]
        store: bool,
        /// Write proposed cases JSON to a file.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Total LLM token cap across the audited models.
        #[arg(long)]
        budget: Option<u64>,
        /// Skip the acceptance gate for stored cases.
        #[arg(long)]
        no_gate: bool,
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
    /// Write stored tests into testsprite_tests/TC001_Title.{py,js,json,sh}.
    Materialize {
        /// Materialize only this test id (repeatable); omit to materialize all.
        #[arg(long)]
        id: Vec<String>,
        /// Output directory (default: testsprite_tests/).
        #[arg(long)]
        out: Option<PathBuf>,
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
    /// Reinstate a quarantined test (clear the acceptance-gate suspect-oracle
    /// marker so it runs with the whole suite again).
    Release {
        #[arg()]
        id: String,
    },
    /// Group failing tests by root cause (failureKind) — fix causes, not symptoms.
    Triage {
        #[arg(long)]
        json: bool,
    },
    /// Distill recurring failures from run history into do/don't guidelines
    /// (fed into generation prompts to prevent repeating them).
    Guidelines {
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
        #[arg(long, default_value_t = crate::envs::default_model())]
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
pub enum ArtifactCmd {
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
pub enum PlanCmd {
    /// Replace `planSteps` with the JSON array in `file`.
    Put {
        #[arg()]
        id: String,
        #[arg(long)]
        file: PathBuf,
    },
}

pub async fn dispatch(cmd: TestCmd) -> Result<()> {
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
                        let mark = if local::accept::is_quarantined_test(t) {
                            "  [quarantined]"
                        } else {
                            ""
                        };
                        println!("{}  {}{mark}", t.id, t.title);
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
            budget,
            no_gate,
            views,
            iterate,
            fault_check,
        } => {
            let mut opts = gen_opts(budget, no_gate, views.as_deref())?;
            opts.iterate = iterate.max(1);
            if cover {
                let p = path.unwrap_or(std::env::current_dir()?);
                let out = local::generate::generate_cover(&root, &p, &model, &opts).await?;
                println!("generated {} coverage test(s)", out.test_ids.len());
                print_generated(&out);
                return Ok(());
            }
            if changed {
                let since = since.as_deref().unwrap_or("HEAD");
                let out = if fault_check {
                    local::generate::generate_changed_fault_checked(&root, since, &model, &opts)
                        .await?
                } else {
                    local::generate::generate_changed(&root, since, &model, &opts).await?
                };
                if out.test_ids.is_empty() {
                    println!(
                        "no changed functions need new tests (nothing changed, or all covered)"
                    );
                } else {
                    println!(
                        "generated {} test(s) for changed functions",
                        out.test_ids.len()
                    );
                    print_generated(&out);
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
                &opts,
            )
            .await?;
            if let Some(prd_id) = &out.prd_id {
                println!("PRD {prd_id}  (inspect: testsprite-rs prd show {prd_id})");
            }
            println!("generated {} test(s)", out.test_ids.len());
            print_generated(&out);
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
            budget,
            no_gate,
        } => {
            let scan = path.unwrap_or(std::env::current_dir()?);
            let opts = gen_opts(budget, no_gate, None)?;
            let audit = local::generate::adversarial(&root, &scan, &model, store, &opts).await?;
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
        TestCmd::Materialize { id, out } => {
            let paths = local::store::materialize(&root, &id, out.as_deref()).await?;
            for p in &paths {
                println!("wrote {}", p.display());
            }
            println!("materialized {} test artifact(s)", paths.len());
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
        TestCmd::Release { id } => {
            local::store::set_quarantine(&root, &id, None).await?;
            println!("released {id} — it runs with the whole suite again");
            Ok(())
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
        TestCmd::Guidelines { json } => {
            let code = local::guidelines::guidelines_report(&root, json).await?;
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

/// Parse the shared generation knobs (`--budget`, `--no-gate`, `--views`).
fn gen_opts(
    budget: Option<u64>,
    no_gate: bool,
    views: Option<&str>,
) -> Result<local::generate::GenOpts> {
    let mut opts = local::generate::GenOpts {
        budget,
        gate: !no_gate,
        ..Default::default()
    };
    if let Some(spec) = views {
        let parsed: Vec<_> = spec
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| {
                server::llm::Perspective::parse(s).ok_or_else(|| {
                    anyhow::anyhow!("invalid --views '{s}': expected normal|boundary|exception")
                })
            })
            .collect::<Result<_>>()?;
        if parsed.is_empty() {
            anyhow::bail!("--views must name at least one of normal|boundary|exception");
        }
        opts.views = parsed;
    }
    Ok(opts)
}

/// Print generated ids, marking the ones the acceptance gate quarantined.
fn print_generated(out: &local::generate::GenSummary) {
    for id in &out.test_ids {
        if out.quarantined.contains(id) {
            println!("  {id}  [quarantined: suspect oracle]");
        } else {
            println!("  {id}");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_field_plain_value_is_returned_unquoted() {
        assert_eq!(csv_field("TC001"), "TC001");
        assert_eq!(csv_field(""), "");
        assert_eq!(csv_field("simple title"), "simple title");
    }

    #[test]
    fn csv_field_with_comma_is_wrapped_in_quotes() {
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_field("one, two, three"), "\"one, two, three\"");
    }

    #[test]
    fn csv_field_doubles_embedded_double_quotes() {
        // A field containing a quote must be wrapped AND every inner quote
        // doubled, per RFC 4180.
        assert_eq!(csv_field("a\"b"), "\"a\"\"b\"");
        assert_eq!(csv_field("\""), "\"\"\"\"");
    }

    #[test]
    fn csv_field_with_newline_is_wrapped_in_quotes() {
        assert_eq!(csv_field("a\nb"), "\"a\nb\"");
    }
}
