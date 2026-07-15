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
) -> anyhow::Result<i32> {
    let project = project::load(root)?;

    let target = url_override
        .map(str::to_string)
        .or_else(|| project.target_url.clone())
        .unwrap_or_else(|| DEFAULT_TARGET.to_string());

    let tests = if ids.is_empty() {
        store::list(root)?
    } else {
        ids.iter()
            .map(|id| store::load_one(root, id))
            .collect::<anyhow::Result<Vec<_>>>()?
    };

    if tests.is_empty() {
        println!("no tests found; run `testsprite-rs test add <file>` first");
        return Ok(0);
    }

    let llm = crate::server::llm::LlmClient::from_env(model);
    let ctx = ExecCtx {
        target: target.clone(),
        llm: llm.clone(),
        prd: Arc::new(serde_json::json!({})),
    };

    let mut failed = 0;
    let total = tests.len();
    let mut report = Vec::with_capacity(total);
    for t in &tests {
        let kind = t.kind.unwrap_or(project.kind);
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

        if !outcome.passed {
            failed += 1;
        }

        if json {
            let mut entry = serde_json::json!({
                "id": t.id,
                "title": t.title,
                "passed": outcome.passed,
                "error": outcome.error,
            });
            if let Some(analysis) = &analysis {
                entry["analysis"] = analysis.clone();
            }
            if let Some(p) = &fix_path {
                entry["fixPath"] = serde_json::json!(p);
            }
            report.push(entry);
        } else if outcome.passed {
            println!("PASS  {}  {}", t.id, t.title);
        } else {
            println!("FAIL  {}  {}", t.id, t.title);
            if !outcome.error.is_empty() {
                println!("      {}", outcome.error);
            }
            if let Some(analysis) = &analysis {
                let verdict = analysis
                    .get("verdict")
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                let cause = analysis.get("cause").and_then(Value::as_str).unwrap_or("?");
                let fix = analysis.get("fix").and_then(Value::as_str).unwrap_or("?");
                println!("      [{verdict}] {cause} — fix: {fix}");
            }
            if let Some(p) = &fix_path {
                println!("      fix → {p}");
            }
        }

        store::write_result(root, &t.id, &outcome, analysis.as_ref())?;
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        let passed = total - failed;
        println!("\n{passed}/{total} passed");
    }

    Ok(if failed == 0 { 0 } else { 1 })
}
