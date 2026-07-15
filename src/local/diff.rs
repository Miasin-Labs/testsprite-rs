//! `testsprite-rs test diff` — compare two stored results
//! (`results/<id>.json`) offline. Purely informational; always exits 0.

use std::path::Path;

use anyhow::Context;
use serde_json::Value;

use super::results_dir;

struct Summary {
    passed: Option<bool>,
    verdict: Option<String>,
    error: Option<String>,
}

fn load(root: &Path, id: &str) -> anyhow::Result<Summary> {
    let path = results_dir(root).join(format!("{id}.json"));
    let body = std::fs::read_to_string(&path)
        .with_context(|| format!("no result for {id} at {}", path.display()))?;
    let value: Value =
        serde_json::from_str(&body).with_context(|| format!("parsing {}", path.display()))?;

    let passed = value.get("passed").and_then(Value::as_bool);
    let error = value
        .get("error")
        .and_then(Value::as_str)
        .map(|s| s.to_string());
    let verdict = value
        .get("analysis")
        .and_then(|a| a.get("verdict"))
        .and_then(Value::as_str)
        .map(|s| s.to_string());

    Ok(Summary {
        passed,
        verdict,
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

/// Print a compact comparison of two stored results. Always returns 0.
pub fn diff(root: &Path, id_a: &str, id_b: &str) -> anyhow::Result<i32> {
    let a = load(root, id_a)?;
    let b = load(root, id_b)?;

    println!(
        "{id_a}: {} verdict={} error={}",
        fmt_passed(a.passed),
        a.verdict.as_deref().unwrap_or("-"),
        a.error.as_deref().unwrap_or("-")
    );
    println!(
        "{id_b}: {} verdict={} error={}",
        fmt_passed(b.passed),
        b.verdict.as_deref().unwrap_or("-"),
        b.error.as_deref().unwrap_or("-")
    );

    let passed_changed = a.passed != b.passed;
    let verdict_changed = a.verdict != b.verdict;
    println!(
        "changed: passed={passed_changed} verdict={verdict_changed}"
    );

    Ok(0)
}
