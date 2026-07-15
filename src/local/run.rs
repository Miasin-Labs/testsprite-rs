//! Run local test cases through the existing Executor seam.

use std::path::Path;
use std::sync::Arc;

use serde_json::Value;

use crate::server::executors::ExecCtx;

use super::{project, store};

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
) -> anyhow::Result<i32> {
    let report = run_collect(root, ids, url_override, model, fix, browser).await?;

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
            let passed = entry.get("passed").and_then(Value::as_bool).unwrap_or(false);
            let error = entry.get("error").and_then(Value::as_str).unwrap_or("");
            if passed {
                println!("PASS  {id}  {title}");
            } else {
                println!("FAIL  {id}  {title}");
                if !error.is_empty() {
                    println!("      {error}");
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

    let llm = crate::server::llm::LlmClient::from_env(model);
    let ctx = ExecCtx {
        target: target.clone(),
        llm: llm.clone(),
        prd: Arc::new(serde_json::json!({})),
        browser: browser.map(str::to_string),
        shots_dir: browser.map(|_| super::ts_dir(root).join("shots")),
        root: root.to_path_buf(),
    };

    let mut report = Vec::with_capacity(tests.len());
    for t in &tests {
        let kind = t
            .kind
            .unwrap_or_else(|| project.as_ref().map(|p| p.kind).unwrap_or_default());
        let ex = crate::server::executors::for_kind(kind);
        let case = serde_json::to_value(t)?;
        let outcome = ex.run(&case, &ctx).await;

        let analysis: Option<Value> = if !outcome.passed {
            match &llm {
                Some(c) => match c.analyze_failure(&case, &outcome.code, &outcome.error).await {
                    Ok(a) => Some(a),
                    Err(e) => {
                        tracing::warn!("failure analysis failed for {}: {e}", t.id);
                        None
                    }
                },
                None => None,
            }
        } else {
            None
        };

        let fix_path: Option<String> = if fix && !outcome.passed {
            match &llm {
                Some(c) => match c.propose_fix(&case, &outcome.code, &outcome.error).await {
                    Ok(f) => match store::write_fix(root, &t.id, &t.title, analysis.as_ref(), &f) {
                        Ok(p) => Some(p.display().to_string()),
                        Err(e) => {
                            tracing::warn!("writing fix for {} failed: {e}", t.id);
                            None
                        }
                    },
                    Err(e) => {
                        tracing::warn!("fix proposal for {} failed: {e}", t.id);
                        None
                    }
                },
                None => None,
            }
        } else {
            None
        };

        let mut entry = serde_json::json!({
            "id": t.id,
            "title": t.title,
            "passed": outcome.passed,
            "error": outcome.error,
        });
        let (verdict, fk) = super::verdict::classify(outcome.passed, &outcome.error);
        entry["verdict"] = serde_json::json!(verdict.as_str());
        entry["failureKind"] = match fk {
            Some(k) => serde_json::json!(k),
            None => Value::Null,
        };
        if let Some(analysis) = &analysis {
            entry["analysis"] = analysis.clone();
        }
        if let Some(p) = &fix_path {
            entry["fixPath"] = serde_json::json!(p);
        }
        report.push(entry);

        store::write_result(root, &t.id, &outcome, analysis.as_ref()).await?;
    }

    Ok(report)
}
