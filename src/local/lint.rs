//! `testsprite-rs test lint` — validate every stored test case offline
//! (no network, no LLM). Flags malformed backend specs and cases with
//! nothing runnable.

use std::path::Path;

use serde_json::Value;

use crate::server::executors::TestKind;

use super::store;

enum Issue {
    Warn(String),
    Hard(String),
}

/// Validate every stored test in `root`; print `[ok] <id>` or
/// `[issue] <id>: <problem>` per test plus a summary line. Returns exit code
/// 1 if any hard issue was found, else 0.
pub fn lint(root: &Path) -> anyhow::Result<i32> {
    let tests = store::list(root)?;
    let mut issue_count = 0usize;
    let mut hard_found = false;

    for test in &tests {
        let issues = check(&test.id, test);
        if issues.is_empty() {
            println!("[ok] {}", test.id);
            continue;
        }
        for issue in issues {
            issue_count += 1;
            match issue {
                Issue::Warn(msg) => println!("[issue] {}: {msg} (warn)", test.id),
                Issue::Hard(msg) => {
                    hard_found = true;
                    println!("[issue] {}: {msg}", test.id);
                }
            }
        }
    }

    println!("lint: {} tests, {issue_count} issues", tests.len());
    Ok(if hard_found { 1 } else { 0 })
}

fn check(id: &str, test: &super::LocalTest) -> Vec<Issue> {
    let mut issues = Vec::new();

    if id.trim().is_empty() {
        issues.push(Issue::Hard("empty id".to_string()));
    }
    if test.title.trim().is_empty() {
        issues.push(Issue::Warn("empty title".to_string()));
    }

    let kind = test.kind.unwrap_or_default();

    if kind == TestKind::Backend {
        if let Some(spec) = &test.spec {
            issues.extend(check_backend_spec(spec));
        } else if test.description.trim().is_empty() {
            issues.push(Issue::Warn(
                "nothing to run: no spec and no description for LLM".to_string(),
            ));
        }
    }

    if kind == TestKind::Frontend
        && let Some(steps) = test.extra.get("planSteps").and_then(Value::as_array)
        && steps.iter().any(|s| !s.is_object())
    {
        issues.push(Issue::Warn(
            "planSteps contains a non-object step".to_string(),
        ));
    }

    issues
}

fn check_backend_spec(spec: &Value) -> Vec<Issue> {
    let mut issues = Vec::new();

    match spec.get("method").and_then(Value::as_str) {
        Some(m) if ["GET", "POST", "PUT", "DELETE", "PATCH"].contains(&m) => {}
        Some(m) => issues.push(Issue::Hard(format!("invalid spec.method {m:?}"))),
        None => issues.push(Issue::Hard("spec.method missing or not a string".to_string())),
    }

    match spec.get("path").and_then(Value::as_str) {
        Some(p) if p.starts_with('/') => {}
        Some(p) => issues.push(Issue::Hard(format!("spec.path {p:?} must start with '/'"))),
        None => issues.push(Issue::Hard("spec.path missing or not a string".to_string())),
    }

    if let Some(status) = spec.get("expect_status") {
        match status.as_i64() {
            Some(code) if (100..=599).contains(&code) => {}
            _ => issues.push(Issue::Hard(
                "spec.expect_status must be an integer in 100..=599".to_string(),
            )),
        }
    }

    issues
}
