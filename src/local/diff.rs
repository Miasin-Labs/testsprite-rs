//! `testsprite-rs test diff` — compare two stored results (`runs` table,
//! latest row per test id) offline. CI-scriptable: exits 0 when verdicts
//! match, 1 when they differ (mirrors the real CLI's `CliRunDiff`).

use std::path::Path;

use serde_json::Value;

use super::store;
use super::verdict::{self, Verdict};

struct Summary {
    passed: Option<bool>,
    verdict: Option<Verdict>,
    failure_kind: Option<&'static str>,
    error: Option<String>,
}

async fn load(root: &Path, id: &str) -> anyhow::Result<Summary> {
    let Some(value) = store::load_result(root, id).await? else {
        // Missing result: treat verdict as "unknown" rather than erroring
        // the whole comparison out.
        return Ok(Summary {
            passed: None,
            verdict: None,
            failure_kind: None,
            error: None,
        });
    };

    let passed = value.get("passed").and_then(Value::as_bool);
    let error = value
        .get("error")
        .and_then(Value::as_str)
        .map(|s| s.to_string());

    // Use the verdict recorded at run time rather than re-deriving it from the
    // error text. The run knew which executor produced that text; we don't, and
    // guessing here would classify a subprocess's output as if it were our own
    // transport's (see `local::verdict`).
    let verdict = value
        .get("verdict")
        .and_then(Value::as_str)
        .and_then(Verdict::parse);
    let failure_kind = value
        .get("failureKind")
        .and_then(Value::as_str)
        .and_then(verdict::known_failure_kind);

    Ok(Summary {
        passed,
        verdict,
        failure_kind,
        error,
    })
}

fn fmt_passed(passed: Option<bool>) -> &'static str {
    match passed {
        Some(true) => "PASS",
        Some(false) => "FAIL",
        None => "UNKNOWN",
    }
}

fn verdict_str(verdict: Option<Verdict>) -> &'static str {
    match verdict {
        Some(v) => v.as_str(),
        None => "unknown",
    }
}

/// Data-only comparison for the `testsprite_diff` MCP tool: the same object
/// `--json` prints, without touching stdout.
pub async fn diff_data(root: &Path, id_a: &str, id_b: &str) -> anyhow::Result<Value> {
    let a = load(root, id_a).await?;
    let b = load(root, id_b).await?;
    Ok(serde_json::json!({
        "runA": { "id": id_a, "verdict": verdict_str(a.verdict), "failureKind": a.failure_kind },
        "runB": { "id": id_b, "verdict": verdict_str(b.verdict), "failureKind": b.failure_kind },
        "verdictChanged": a.verdict != b.verdict,
        "failureKindChanged": a.failure_kind != b.failure_kind,
        "crossTest": false,
        "changedSteps": [],
    }))
}

/// Print a compact comparison of two stored results. Returns `0` when both
/// verdicts match, `1` when they differ.
pub async fn diff(root: &Path, id_a: &str, id_b: &str, json: bool) -> anyhow::Result<i32> {
    let a = load(root, id_a).await?;
    let b = load(root, id_b).await?;

    let verdict_changed = a.verdict != b.verdict;
    let failure_kind_changed = a.failure_kind != b.failure_kind;

    if json {
        let obj = serde_json::json!({
            "runA": {
                "id": id_a,
                "verdict": verdict_str(a.verdict),
                "failureKind": a.failure_kind,
            },
            "runB": {
                "id": id_b,
                "verdict": verdict_str(b.verdict),
                "failureKind": b.failure_kind,
            },
            "verdictChanged": verdict_changed,
            "failureKindChanged": failure_kind_changed,
            "crossTest": false,
            "changedSteps": [],
        });
        println!("{}", serde_json::to_string_pretty(&obj)?);
    } else {
        println!(
            "{id_a}: {} verdict={} failureKind={} error={}",
            fmt_passed(a.passed),
            verdict_str(a.verdict),
            a.failure_kind.unwrap_or("-"),
            a.error.as_deref().unwrap_or("-")
        );
        println!(
            "{id_b}: {} verdict={} failureKind={} error={}",
            fmt_passed(b.passed),
            verdict_str(b.verdict),
            b.failure_kind.unwrap_or("-"),
            b.error.as_deref().unwrap_or("-")
        );

        let passed_changed = a.passed != b.passed;
        println!(
            "changed: passed={passed_changed} verdict={verdict_changed} failureKind={failure_kind_changed}"
        );
        println!("verdictChanged: {verdict_changed}");
    }

    Ok(if verdict_changed { 1 } else { 0 })
}
