//! Run local test cases through the existing Executor seam.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::StreamExt;
use serde_json::Value;

use super::{LocalTest, project, store};
use crate::server::executors::{ExecCtx, Outcome, TestKind};

const DEFAULT_TARGET: &str = "http://127.0.0.1:8080";

/// Run the given test ids (all tests if `ids` is empty) against the local
/// project's target, printing pass/fail per test and a summary line (or a
/// single JSON array when `json` is set). Uses the LLM for cases without a
/// `spec` and for failure analysis when a key is available (`model`); falls
/// back to the deterministic engine otherwise.
/// Returns `0` if every test passed, `1` otherwise.
pub async fn run(
    root: &Path,
    ids: &[String],
    url_override: Option<&str>,
    model: &str,
    json: bool,
    fix: bool,
    browser: Option<&str>,
    jobs: usize,
    serve: bool,
) -> anyhow::Result<i32> {
    let report = run_collect(root, ids, url_override, model, fix, browser, jobs, serve).await?;

    if report.is_empty() {
        println!("no tests found; run `testsprite-rs test add <file>` first");
        return Ok(0);
    }

    let total = report.len();
    let failed = report
        .iter()
        .filter(|e| !e.get("passed").and_then(Value::as_bool).unwrap_or(false))
        .count();

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        for entry in &report {
            let id = entry.get("id").and_then(Value::as_str).unwrap_or("");
            let title = entry.get("title").and_then(Value::as_str).unwrap_or("");
            let passed = entry
                .get("passed")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let error = entry.get("error").and_then(Value::as_str).unwrap_or("");
            if passed {
                println!("PASS  {id}  {title}");
            } else {
                println!("FAIL  {id}  {title}");
                if !error.is_empty() {
                    println!("      {error}");
                }
                let el = error.to_lowercase();
                if el.contains("connection refused")
                    || el.contains("error sending request")
                    || el.contains("max retries")
                    || el.contains("failed to establish")
                {
                    println!(
                        "      → target not reachable; start the app or run `testsprite-rs test run --serve`"
                    );
                }
                if let Some(analysis) = entry.get("analysis") {
                    let verdict = analysis
                        .get("verdict")
                        .and_then(Value::as_str)
                        .unwrap_or("?");
                    let cause = analysis.get("cause").and_then(Value::as_str).unwrap_or("?");
                    let fx = analysis.get("fix").and_then(Value::as_str).unwrap_or("?");
                    println!("      [{verdict}] {cause} — fix: {fx}");
                }
                if let Some(p) = entry.get("fixPath").and_then(Value::as_str) {
                    println!("      fix → {p}");
                }
            }
        }
        let passed = total - failed;
        println!("\n{passed}/{total} passed");
    }

    Ok(if failed == 0 { 0 } else { 1 })
}

/// Run the given test ids (all tests if `ids` is empty), executing each case,
/// running LLM failure analysis on failures, optionally proposing a fix, and
/// writing the result to disk — without printing or exiting. Returns one JSON
/// object per test: `{id,title,passed,error,analysis?,fixPath?}`.
pub async fn run_collect(
    root: &Path,
    ids: &[String],
    url_override: Option<&str>,
    model: &str,
    fix: bool,
    browser: Option<&str>,
    jobs: usize,
    serve: bool,
) -> anyhow::Result<Vec<Value>> {
    let project = project::load(root).await.ok();

    let target = url_override
        .map(str::to_string)
        .or_else(|| project.as_ref().and_then(|p| p.target_url.clone()))
        .unwrap_or_else(|| DEFAULT_TARGET.to_string());

    let tests = if ids.is_empty() {
        store::list(root).await?
    } else {
        let mut loaded = Vec::with_capacity(ids.len());
        for id in ids {
            loaded.push(store::load_one(root, id).await?);
        }
        loaded
    };

    if tests.is_empty() {
        return Ok(Vec::new());
    }

    // Optionally bring the target app up so backend/spec cases hit a LIVE server
    // instead of env-failing on a dead URL. RAII: the handle is killed on drop,
    // so the app stops even on early return / panic / SIGINT.
    let _served = if serve {
        let Some(cmd) = project.as_ref().and_then(|p| p.start_command.clone()) else {
            anyhow::bail!(
                "test run --serve needs a start command; set one with `testsprite-rs project set-start \"<cmd>\"`"
            );
        };
        crate::local::serve::start_and_wait(&cmd, &target, crate::envs::serve_ready_secs()).await?
    } else {
        None
    };

    let llm = crate::server::llm::LlmClient::from_env(model);
    let variables = project::load_variables(root);
    let ctx = ExecCtx {
        target: target.clone(),
        llm: llm.clone(),
        prd: Arc::new(serde_json::json!({})),
        browser: browser.map(str::to_string),
        shots_dir: browser.map(|_| super::ts_dir(root).join("shots")),
        root: root.to_path_buf(),
        variables,
    };
    let default_kind = project.as_ref().map(|p| p.kind).unwrap_or_default();

    // Dependency waves: producers before consumers, teardown last. Each level is
    // a set of independent tests that MAY run concurrently (opt-in via `jobs`).
    let (levels, teardown) = crate::local::waves::waves(tests);

    // Trap Ctrl-C: stop launching new waves on interrupt, but still run teardown
    // so a scheduled/long run doesn't leave orphaned resources behind.
    let interrupted = Arc::new(AtomicBool::new(false));
    // Register the SIGINT handler SYNCHRONOUSLY (before any test runs) so an
    // interrupt sets the flag instead of killing the process; remaining waves
    // are then skipped but teardown still runs. (`ctrl_c()` registers lazily on
    // first poll, which can miss an early signal.)
    let watcher = interrupt_watcher(interrupted.clone());

    let mut report = Vec::new();
    // Caps whose producer failed or was skipped → block downstream consumers.
    let mut failed_caps: HashSet<String> = HashSet::new();

    for level in levels {
        if interrupted.load(Ordering::SeqCst) {
            tracing::warn!("interrupted — skipping remaining tests, running teardown");
            break;
        }

        // Split the level into runnable tests and dependency skips.
        let mut to_run = Vec::new();
        for t in level {
            match t.needs().into_iter().find(|c| failed_caps.contains(c)) {
                Some(cap) => {
                    // Skip: mark blocked; its own outputs cascade as failed.
                    for p in t.produces() {
                        failed_caps.insert(p);
                    }
                    let outcome = Outcome::fail(
                        format!(
                            "skipped: dependency '{cap}' unmet (upstream producer failed or was skipped)"
                        ),
                        String::new(),
                    );
                    let kind = t.kind.unwrap_or(default_kind);
                    store::write_result(root, &t.id, &outcome, None, kind).await?;
                    report.push(build_entry(&t, &outcome, None, None, kind));
                }
                None => to_run.push(t),
            }
        }

        let outcomes = run_wave(to_run, &ctx, default_kind, jobs).await;
        for (t, outcome) in outcomes {
            if !outcome.passed {
                for p in t.produces() {
                    failed_caps.insert(p);
                }
            }
            let (analysis, fix_path) = post_process(&t, &outcome, &llm, fix, root).await;
            let kind = t.kind.unwrap_or(default_kind);
            store::write_result(root, &t.id, &outcome, analysis.as_ref(), kind).await?;
            report.push(build_entry(
                &t,
                &outcome,
                analysis.as_ref(),
                fix_path.as_deref(),
                kind,
            ));
        }
    }

    // Teardown always runs (cleanup), sequentially, regardless of failures.
    for t in teardown {
        let outcome = run_one(&t, &ctx, default_kind).await;
        let (analysis, fix_path) = post_process(&t, &outcome, &llm, fix, root).await;
        let kind = t.kind.unwrap_or(default_kind);
        store::write_result(root, &t.id, &outcome, analysis.as_ref(), kind).await?;
        report.push(build_entry(
            &t,
            &outcome,
            analysis.as_ref(),
            fix_path.as_deref(),
            kind,
        ));
    }

    if let Some(w) = watcher {
        w.abort();
    }
    Ok(report)
}

/// Spawn a task that flips `flag` on SIGINT. The handler is registered at call
/// time (not lazily) so an early interrupt is caught rather than fatal. Returns
/// `None` on non-Unix or if the handler can't be installed (default Ctrl-C then).
fn interrupt_watcher(flag: Arc<AtomicBool>) -> Option<tokio::task::JoinHandle<()>> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::interrupt()) {
            Ok(mut sigint) => Some(tokio::spawn(async move {
                if sigint.recv().await.is_some() {
                    flag.store(true, Ordering::SeqCst);
                }
            })),
            Err(_) => None,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = flag;
        None
    }
}

/// Resolve the executor for `t` and run one case.
async fn run_one(
    t: &LocalTest,
    ctx: &ExecCtx,
    default_kind: crate::server::executors::TestKind,
) -> Outcome {
    let kind = t.kind.unwrap_or(default_kind);
    let ex = crate::server::executors::for_kind(kind);
    let case = match serde_json::to_value(t) {
        Ok(c) => c,
        Err(e) => return Outcome::fail(format!("could not serialize case: {e}"), String::new()),
    };
    ex.run(&case, ctx).await
}

/// Run one dependency level: sequentially when `jobs <= 1`, else up to `jobs`
/// concurrently. Executors share the Arc-backed `ctx` read-only, so concurrency
/// is safe here; target-state races are the user's call (hence opt-in).
async fn run_wave(
    tests: Vec<LocalTest>,
    ctx: &ExecCtx,
    default_kind: crate::server::executors::TestKind,
    jobs: usize,
) -> Vec<(LocalTest, Outcome)> {
    if jobs <= 1 || tests.len() <= 1 {
        let mut out = Vec::with_capacity(tests.len());
        for t in tests {
            let o = run_one(&t, ctx, default_kind).await;
            out.push((t, o));
        }
        return out;
    }
    futures::stream::iter(tests)
        .map(|t| async move {
            let o = run_one(&t, ctx, default_kind).await;
            (t, o)
        })
        .buffer_unordered(jobs)
        .collect()
        .await
}

/// LLM failure analysis + optional fix-file for a completed outcome. No-op when
/// the test passed or no key is configured.
async fn post_process(
    t: &LocalTest,
    outcome: &Outcome,
    llm: &Option<crate::server::llm::LlmClient>,
    fix: bool,
    root: &Path,
) -> (Option<Value>, Option<String>) {
    if outcome.passed {
        return (None, None);
    }
    let Some(client) = llm else {
        return (None, None);
    };
    let case = serde_json::to_value(t).unwrap_or_else(|_| serde_json::json!({ "id": t.id }));

    let analysis = match client
        .analyze_failure(&case, &outcome.code, &outcome.error)
        .await
    {
        Ok(a) => Some(a),
        Err(e) => {
            tracing::warn!("failure analysis failed for {}: {e}", t.id);
            None
        }
    };

    let fix_path = if fix {
        // Ground the fix in real source when the failure points at repo files;
        // then a returned patch that passes `git apply --check` is labelled
        // appliable, otherwise it stays an illustrative sketch.
        let source = super::fix_context::source_context(root, &outcome.error);
        match client
            .propose_fix(&case, &outcome.code, &outcome.error, source.as_deref())
            .await
        {
            Ok(f) => {
                let appliable = source.is_some()
                    && f.get("patch")
                        .and_then(Value::as_str)
                        .is_some_and(|p| super::fix_context::patch_applies(root, p));
                match store::write_fix(root, &t.id, &t.title, analysis.as_ref(), &f, appliable) {
                    Ok(p) => Some(p.display().to_string()),
                    Err(e) => {
                        tracing::warn!("writing fix for {} failed: {e}", t.id);
                        None
                    }
                }
            }
            Err(e) => {
                tracing::warn!("fix proposal for {} failed: {e}", t.id);
                None
            }
        }
    } else {
        None
    };

    (analysis, fix_path)
}

/// Build the per-test report entry.
fn build_entry(
    t: &LocalTest,
    outcome: &Outcome,
    analysis: Option<&Value>,
    fix_path: Option<&str>,
    kind: TestKind,
) -> Value {
    let mut entry = serde_json::json!({
        "id": t.id,
        "title": t.title,
        "passed": outcome.passed,
        "error": outcome.error,
    });
    let (verdict, fk) = super::verdict::classify(outcome.passed, &outcome.error, kind);
    entry["verdict"] = serde_json::json!(verdict.as_str());
    entry["failureKind"] = match fk {
        Some(k) => serde_json::json!(k),
        None => Value::Null,
    };
    if let Some(analysis) = analysis {
        entry["analysis"] = analysis.clone();
    }
    if let Some(p) = fix_path {
        entry["fixPath"] = serde_json::json!(p);
    }
    entry
}
