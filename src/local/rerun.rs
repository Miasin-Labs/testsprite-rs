//! Re-run stored test cases through the existing Executor seam, optionally
//! auto-healing fragility failures via the LLM.

use std::path::Path;
use std::sync::Arc;

use serde_json::Value;

use crate::server::executors::ExecCtx;

use super::{project, store};

const DEFAULT_TARGET: &str = "http://127.0.0.1:8080";

/// Re-run the given test ids (all tests if `ids` is empty) against the local
/// project's target. When `heal` is set and a failure is analyzed as
/// **fragility** (never a real `bug` or `env` defect), asks the LLM for an
/// improved case, stores it in place of the old one, and re-runs it once.
/// Returns `0` if every test ends up passing, `1` otherwise.
pub async fn rerun(
    root: &Path,
    ids: &[String],
    url_override: Option<&str>,
    model: &str,
    heal: bool,
    json: bool,
) -> anyhow::Result<i32> {
    let project = project::load(root).await.ok();

    let target = url_override
        .map(str::to_string)
        .or_else(|| project.as_ref().and_then(|p| p.target_url.clone()))
        .unwrap_or_else(|| DEFAULT_TARGET.to_string());

    let tests = if ids.is_empty() {
        store::list(root).await?
    } else {
        {
            let mut loaded = Vec::with_capacity(ids.len());
            for id in ids {
                loaded.push(store::load_one(root, id).await?);
            }
            loaded
        }
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
        browser: None,
        shots_dir: None,
        root: root.to_path_buf(),
    };

    let mut failed = 0;
    let total = tests.len();
    let mut report = Vec::with_capacity(total);

    for t in &tests {
        let kind = t
            .kind
            .unwrap_or_else(|| project.as_ref().map(|p| p.kind).unwrap_or_default());
        let ex = crate::server::executors::for_kind(kind);
        let case = serde_json::to_value(t)?;
        let mut outcome = ex.run(&case, &ctx).await;

        let mut healed = false;
        let mut verdict: Option<String> = None;
        let mut analysis: Option<Value> = None;

        if !outcome.passed {
            analysis = match &llm {
                Some(c) => match c.analyze_failure(&case, &outcome.code, &outcome.error).await {
                    Ok(a) => Some(a),
                    Err(e) => {
                        tracing::warn!("failure analysis failed for {}: {e}", t.id);
                        None
                    }
                },
                None => None,
            };

            let v = analysis
                .as_ref()
                .and_then(|a| a.get("verdict"))
                .and_then(Value::as_str)
                .map(str::to_string);
            verdict = v.clone();

            if heal && v.as_deref() == Some("fragility")
                && let Some(c) = &llm
            {
                match c.heal_test(&case, &outcome.code, &outcome.error).await {
                    Ok(Value::Object(mut improved)) => {
                        improved.insert("id".to_string(), Value::String(t.id.clone()));
                        match store::add_value(root, Value::Object(improved.clone())).await {
                            Ok(_) => {
                                let healed_case = Value::Object(improved);
                                let retry = ex.run(&healed_case, &ctx).await;
                                if retry.passed {
                                    healed = true;
                                }
                                outcome = retry;
                            }
                            Err(e) => {
                                tracing::warn!("storing healed case for {} failed: {e}", t.id);
                            }
                        }
                    }
                    Ok(_) => {
                        tracing::warn!("heal for {} did not return a JSON object", t.id);
                    }
                    Err(e) => {
                        tracing::warn!("heal_test failed for {}: {e}", t.id);
                    }
                }
            }
        }

        if !outcome.passed {
            failed += 1;
        }

        if json {
            let mut entry = serde_json::json!({
                "id": t.id,
                "title": t.title,
                "passed": outcome.passed,
                "healed": healed,
            });
            if let Some(v) = &verdict {
                entry["verdict"] = serde_json::json!(v);
            }
            report.push(entry);
        } else if outcome.passed && healed {
            println!("HEALED  {}  {}", t.id, t.title);
        } else if outcome.passed {
            println!("PASS  {}  {}", t.id, t.title);
        } else if heal && verdict.as_deref() == Some("fragility") {
            println!("STILL-FAILING  {}  {}", t.id, t.title);
            if !outcome.error.is_empty() {
                println!("      {}", outcome.error);
            }
        } else {
            let verdict_str = verdict.as_deref().unwrap_or("?");
            let cause = analysis
                .as_ref()
                .and_then(|a| a.get("cause"))
                .and_then(Value::as_str)
                .unwrap_or("?");
            println!("FAIL  {}  {}  [{verdict_str}] {cause}", t.id, t.title);
            if !outcome.error.is_empty() {
                println!("      {}", outcome.error);
            }
        }

        store::write_result(root, &t.id, &outcome, analysis.as_ref()).await?;
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        let passed = total - failed;
        println!("\n{passed}/{total} passed");
    }

    Ok(if failed == 0 { 0 } else { 1 })
}
