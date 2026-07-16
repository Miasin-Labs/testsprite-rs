//! `testsprite-rs test lint` — validate every stored test case offline
//! (no network, no LLM). Flags malformed backend specs and cases with
//! nothing runnable.

use std::path::Path;

use serde_json::Value;

use super::store;
use crate::server::executors::TestKind;

const VALIDATION_ERROR: i32 = 5;

enum Issue {
    Warn(String),
    Hard { field: &'static str, msg: String },
}

/// Validate every stored test in `root`; print `[ok] <id>` or
/// `[issue] <id>: <problem>` per test plus a summary line (or a single
/// `CliLintReport` JSON object when `json` is set). Returns exit code `5`
/// (VALIDATION_ERROR) if any hard issue was found, else `0`.
pub async fn lint(root: &Path, json: bool) -> anyhow::Result<i32> {
    let tests = store::list(root).await?;
    let mut issue_count = 0usize;
    let mut hard_found = false;
    let mut valid_count = 0usize;
    let mut report_issues = Vec::new();

    for test in &tests {
        let issues = check(&test.id, test);
        let has_hard = issues.iter().any(|i| matches!(i, Issue::Hard { .. }));
        if !has_hard {
            valid_count += 1;
        }
        if issues.is_empty() {
            if !json {
                println!("[ok] {}", test.id);
            }
            continue;
        }
        for issue in issues {
            issue_count += 1;
            match issue {
                Issue::Warn(msg) => {
                    if !json {
                        println!("[issue] {}: {msg} (warn)", test.id);
                    }
                }
                Issue::Hard { field, msg } => {
                    hard_found = true;
                    if json {
                        report_issues.push(serde_json::json!({
                            "file": test.id,
                            "field": field,
                            "reason": msg,
                        }));
                    } else {
                        println!("[issue] {}: {msg}", test.id);
                    }
                }
            }
        }
    }

    if json {
        let obj = serde_json::json!({
            "checked": tests.len(),
            "valid": valid_count,
            "issues": report_issues,
        });
        println!("{}", serde_json::to_string_pretty(&obj)?);
    } else {
        println!("lint: {} tests, {issue_count} issues", tests.len());
    }

    Ok(if hard_found { VALIDATION_ERROR } else { 0 })
}

fn check(id: &str, test: &super::LocalTest) -> Vec<Issue> {
    let mut issues = Vec::new();

    if id.trim().is_empty() {
        issues.push(Issue::Hard {
            field: "id",
            msg: "empty id".to_string(),
        });
    }
    if test.title.trim().is_empty() {
        issues.push(Issue::Warn("empty title".to_string()));
    }

    let kind = test.kind.unwrap_or_default();

    if kind == TestKind::Backend {
        if let Some(spec) = &test.spec {
            issues.extend(check_backend_spec(spec));
        } else if let Some(steps) = test.extra.get("steps").and_then(Value::as_array) {
            if steps.is_empty() {
                issues.push(Issue::Hard {
                    field: "steps",
                    msg: "steps must contain at least one request".to_string(),
                });
            }
            for (i, step) in steps.iter().enumerate() {
                for issue in check_backend_spec(step) {
                    issues.push(match issue {
                        Issue::Warn(msg) => Issue::Warn(format!("steps[{i}]: {msg}")),
                        Issue::Hard { field, msg } => Issue::Hard { field, msg },
                    });
                }
            }
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
    let has_graphql = spec.get("graphql").is_some();

    match spec.get("method").and_then(Value::as_str) {
        Some(m) if ["GET", "POST", "PUT", "DELETE", "PATCH"].contains(&m) => {}
        Some(m) => issues.push(Issue::Hard {
            field: "method",
            msg: format!("invalid spec.method {m:?}"),
        }),
        None if has_graphql => {}
        None => issues.push(Issue::Hard {
            field: "method",
            msg: "spec.method missing or not a string".to_string(),
        }),
    }

    match spec.get("path").and_then(Value::as_str) {
        Some(p) if p.starts_with('/') => {}
        Some(p) => issues.push(Issue::Hard {
            field: "path",
            msg: format!("spec.path {p:?} must start with '/'"),
        }),
        None if has_graphql => {}
        None => issues.push(Issue::Hard {
            field: "path",
            msg: "spec.path missing or not a string".to_string(),
        }),
    }

    // `expect_status` is either an exact code or a band name (see
    // `server::engine::Expect`). Absent means the `success` band.
    if let Some(status) = spec.get("expect_status") {
        let ok = match status {
            Value::Number(_) => status
                .as_i64()
                .is_some_and(|code| (100..=599).contains(&code)),
            Value::String(s) => matches!(s.as_str(), "success" | "accepted" | "any"),
            _ => false,
        };
        if !ok {
            issues.push(Issue::Hard {
                field: "expect_status",
                msg: "spec.expect_status must be an integer in 100..=599 or one of \
                      \"success\" | \"accepted\" | \"any\""
                    .to_string(),
            });
        }
    }

    issues
}
