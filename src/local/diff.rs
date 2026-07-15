//! `testsprite-rs test diff` — compare two stored results
//! (`results/<id>.json`) offline. CI-scriptable: exits 0 when verdicts
//! match, 1 when they differ (mirrors the real CLI's `CliRunDiff`).

use std::path::Path;

use anyhow::Context;
use serde_json::Value;

use super::results_dir;
use super::verdict::{self, Verdict};

struct Summary {
    passed: Option<bool>,
    verdict: Option<Verdict>,
    failure_kind: Option<&'static str>,
    error: Option<String>,
}

fn load(root: &Path, id: &str) -> anyhow::Result<Summary> {
    let path = results_dir(root).join(format!("{id}.json"));
    let body = match std::fs::read_to_string(&path) {
        Ok(body) => body,
        Err(_) => {
            // Missing result file: treat verdict as "unknown" rather than
            // erroring the whole comparison out.
            return Ok(Summary {
                passed: None,
                verdict: None,
                failure_kind: None,
                error: None,
            });
        }
    };
    let value: Value =
        serde_json::from_str(&body).with_context(|| format!("parsing {}", path.display()))?;

    let passed = value.get("passed").and_then(Value::as_bool);
    let error = value
        .get("error")
        .and_then(Value::as_str)
        .map(|s| s.to_string());

    let (verdict, failure_kind) =
        verdict::classify(passed.unwrap_or(false), error.as_deref().unwrap_or(""));

    Ok(Summary {
        passed,
        verdict: Some(verdict),
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

/// Print a compact comparison of two stored results. Returns `0` when both
/// verdicts match, `1` when they differ.
pub fn diff(root: &Path, id_a: &str, id_b: &str, json: bool) -> anyhow::Result<i32> {
    let a = load(root, id_a)?;
    let b = load(root, id_b)?;

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
