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

/// Data-only lint for the `testsprite_lint` MCP tool: the same
/// `{checked, valid, issues}` object `--json` prints (hard issues only),
/// without touching stdout.
pub async fn lint_data(root: &Path) -> anyhow::Result<Value> {
    let tests = store::list(root).await?;
    let mut valid = 0usize;
    let mut issues = Vec::new();
    for test in &tests {
        let found = check(&test.id, test);
        if !found.iter().any(|i| matches!(i, Issue::Hard { .. })) {
            valid += 1;
        }
        for issue in found {
            if let Issue::Hard { field, msg } = issue {
                issues.push(serde_json::json!({
                    "file": test.id, "field": field, "reason": msg,
                }));
            }
        }
    }
    Ok(serde_json::json!({
        "checked": tests.len(), "valid": valid, "issues": issues,
    }))
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
            issues.extend(check_oracle_strength(spec, None));
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
                issues.extend(check_oracle_strength(step, Some(i)));
            }
            issues.extend(check_step_flow(steps));
        } else if test.description.trim().is_empty() {
            issues.push(Issue::Warn(
                "nothing to run: no spec and no description for LLM".to_string(),
            ));
        }
        issues.extend(check_hardcoded_secrets(&test.to_case_value()));
    }

    if kind == TestKind::Frontend
        && let Some(steps) = test.extra.get("planSteps").and_then(Value::as_array)
    {
        if steps.iter().any(|s| !s.is_object() && !s.is_string()) {
            issues.push(Issue::Warn(
                "planSteps contains a non-object step".to_string(),
            ));
        }
        if !steps.is_empty() && !plan_steps_have_oracle(steps) {
            issues.push(Issue::Warn(
                "unknown-test: planSteps only act and never assert/verify — the case \
                 cannot fail on wrong behavior, only on crashes"
                    .to_string(),
            ));
        }
    }

    if kind == TestKind::Command {
        let has_code = test
            .extra
            .get("code")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        if !has_code {
            issues.push(Issue::Hard {
                field: "code",
                msg: "command tests must contain non-empty code".to_string(),
            });
        }
        if test.extra.get("steps").is_some() {
            issues.push(Issue::Hard {
                field: "steps",
                msg: "command tests use code, not backend steps".to_string(),
            });
        }
    }

    issues
}

/// True when a single spec/step carries ANY oracle beyond "did not 5xx":
/// a non-`any` status expectation, a body/parse/JSON check, a graphql
/// assertion, or a chained follow-up (which asserts on its own).
fn spec_has_oracle(spec: &Value) -> bool {
    let status_is_vacuous = spec.get("expect_status").and_then(Value::as_str) == Some("any");
    if !status_is_vacuous && spec.get("expect_status").is_some() {
        return true;
    }
    if spec.get("expect_status").is_none() {
        // Absent = the success band, a real oracle.
        return true;
    }
    spec.get("expect_json").is_some()
        || spec.get("expect_body").is_some()
        || spec.get("expect_parses").is_some()
        || spec.get("then").is_some()
        || spec
            .get("graphql")
            .is_some_and(|g| g.get("expect_no_errors").is_some() || g.get("expect_data").is_some())
}

/// Vacuous-oracle smell ("Unknown Test" in the smell literature — up to 77% of
/// LLM-generated tests): a case that runs requests but cannot fail on wrong
/// behavior.
fn check_oracle_strength(spec: &Value, step: Option<usize>) -> Vec<Issue> {
    if spec_has_oracle(spec) {
        return Vec::new();
    }
    let at = step.map(|i| format!("steps[{i}]: ")).unwrap_or_default();
    vec![Issue::Warn(format!(
        "{at}unknown-test: expect_status \"any\" with no body/parse assertion — \
         this cannot fail on wrong behavior, only on a 5xx"
    ))]
}

/// Flow-level smells over a multi-step case: masking (only the final step
/// asserts, so a mid-flow bug whose effect is overwritten passes silently)
/// and eager-test (one case sprawling across many unrelated endpoints).
fn check_step_flow(steps: &[Value]) -> Vec<Issue> {
    let mut issues = Vec::new();
    if steps.len() > 1 {
        let mid_asserts = steps[..steps.len() - 1]
            .iter()
            .any(|s| spec_has_oracle(s) || s.get("save").is_some());
        if !mid_asserts {
            issues.push(Issue::Warn(
                "masking-prone: only the final step asserts or saves — a mid-flow bug \
                 whose effect is overwritten before the last step passes silently"
                    .to_string(),
            ));
        }
    }
    let distinct_paths: std::collections::BTreeSet<&str> = steps
        .iter()
        .filter_map(|s| s.get("path").and_then(Value::as_str))
        .collect();
    if distinct_paths.len() > 8 {
        issues.push(Issue::Warn(format!(
            "eager-test: one case hits {} distinct endpoints — split it so a failure \
             names one behavior",
            distinct_paths.len()
        )));
    }
    issues
}

/// A frontend plan needs at least one asserting step (object action
/// `assert*`/`verify`/`expect*`, or a text step phrased as a check).
fn plan_steps_have_oracle(steps: &[Value]) -> bool {
    steps.iter().any(|s| match s {
        Value::Object(o) => o
            .get("action")
            .and_then(Value::as_str)
            .is_some_and(|a| a.starts_with("assert") || a == "verify" || a.starts_with("expect")),
        Value::String(t) => {
            let t = t.to_lowercase();
            t.starts_with("verify") || t.starts_with("assert") || t.starts_with("check")
        }
        _ => false,
    })
}

/// Credential-looking literals embedded in a case body instead of `${VAR}`
/// placeholders from `.testsprite.env` / `variables.json`.
fn check_hardcoded_secrets(case: &Value) -> Vec<Issue> {
    let mut hits = Vec::new();
    find_secretish(case, &mut hits);
    hits.into_iter()
        .map(|key| {
            Issue::Warn(format!(
                "hardcoded-secret: field {key:?} carries a literal credential-looking \
                 value — use a ${{VAR}} placeholder instead"
            ))
        })
        .collect()
}

fn find_secretish(v: &Value, hits: &mut Vec<String>) {
    const KEYS: &[&str] = &["password", "token", "secret", "api_key", "apikey"];
    // `save` maps names to JSONPath extractors and `expect_body` asserts on
    // RESPONSE content — neither sends a credential, so neither is scanned.
    const SKIP_SUBTREES: &[&str] = &["save", "expect_body"];
    if let Value::Object(obj) = v {
        for (k, val) in obj {
            if SKIP_SUBTREES.contains(&k.as_str()) {
                continue;
            }
            let kl = k.to_lowercase();
            if KEYS.iter().any(|s| kl.contains(s))
                && let Some(s) = val.as_str()
                && !s.is_empty()
                && !s.contains("${")
            {
                hits.push(k.clone());
            }
            find_secretish(val, hits);
        }
    } else if let Value::Array(arr) = v {
        for item in arr {
            find_secretish(item, hits);
        }
    }
}

fn check_backend_spec(spec: &Value) -> Vec<Issue> {
    let mut issues = Vec::new();
    let has_graphql = spec.get("graphql").is_some();

    match spec.get("method").and_then(Value::as_str) {
        Some(m)
            if [
                "GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS", "QUERY",
            ]
            .contains(&m) => {}
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a LocalTest from a raw case object (like the store round-trips).
    fn test_from(case: serde_json::Value) -> super::super::LocalTest {
        serde_json::from_value(case).unwrap()
    }

    fn warns(test: &super::super::LocalTest) -> Vec<String> {
        check(&test.id, test)
            .into_iter()
            .map(|i| match i {
                Issue::Warn(m) => m,
                Issue::Hard { msg, .. } => msg,
            })
            .collect()
    }

    #[test]
    fn vacuous_status_any_oracle_is_flagged_unknown_test() {
        let ok = test_from(serde_json::json!({
            "id":"a","title":"real oracle","kind":"backend",
            "spec":{"method":"GET","path":"/health","expect_status":200}
        }));
        assert!(
            !warns(&ok).iter().any(|w| w.contains("unknown-test")),
            "a status oracle is not vacuous: {:?}",
            warns(&ok)
        );

        let vacuous = test_from(serde_json::json!({
            "id":"b","title":"asserts nothing","kind":"backend",
            "spec":{"method":"GET","path":"/health","expect_status":"any"}
        }));
        assert!(
            warns(&vacuous).iter().any(|w| w.contains("unknown-test")),
            "expect_status any with no body check must warn: {:?}",
            warns(&vacuous)
        );

        // ...unless it pairs the lenient band with a body assertion.
        let saved = test_from(serde_json::json!({
            "id":"c","title":"any + body","kind":"backend",
            "spec":{"method":"GET","path":"/health","expect_status":"any","expect_body":{"ok":true}}
        }));
        assert!(!warns(&saved).iter().any(|w| w.contains("unknown-test")));
    }

    #[test]
    fn masking_prone_flow_only_asserts_at_the_end() {
        let masking = test_from(serde_json::json!({
            "id":"m","title":"write then read","kind":"backend",
            "steps":[
                {"method":"POST","path":"/items","expect_status":"any"},
                {"method":"GET","path":"/items/1","expect_status":200,"expect_body":{"ok":true}}
            ]
        }));
        assert!(
            warns(&masking).iter().any(|w| w.contains("masking-prone")),
            "{:?}",
            warns(&masking)
        );

        // A mid-flow save (captures an intermediate value) is enough.
        let saved = test_from(serde_json::json!({
            "id":"s","title":"login then call","kind":"backend",
            "steps":[
                {"method":"POST","path":"/login","expect_status":200,"save":{"tok":"$.token"}},
                {"method":"GET","path":"/me","expect_status":200}
            ]
        }));
        assert!(!warns(&saved).iter().any(|w| w.contains("masking-prone")));
    }

    #[test]
    fn hardcoded_credentials_warn_but_placeholders_and_response_asserts_do_not() {
        let literal = test_from(serde_json::json!({
            "id":"h","title":"inline secret","kind":"backend",
            "spec":{"method":"POST","path":"/login","body":{"password":"hunter2"}}
        }));
        assert!(
            warns(&literal)
                .iter()
                .any(|w| w.contains("hardcoded-secret")),
            "{:?}",
            warns(&literal)
        );

        let placeholder = test_from(serde_json::json!({
            "id":"p","title":"env-driven","kind":"backend",
            "spec":{"method":"POST","path":"/login","body":{"password":"${PASSWORD}"}}
        }));
        assert!(
            !warns(&placeholder)
                .iter()
                .any(|w| w.contains("hardcoded-secret"))
        );

        // A `token` key inside expect_body asserts on the RESPONSE, not a sent
        // credential — must not warn.
        let response_assert = test_from(serde_json::json!({
            "id":"r","title":"asserts token present","kind":"backend",
            "spec":{"method":"POST","path":"/login","expect_status":200,"expect_body":{"token":"abc"}}
        }));
        assert!(
            !warns(&response_assert)
                .iter()
                .any(|w| w.contains("hardcoded-secret"))
        );
    }

    #[test]
    fn frontend_plan_with_no_assertion_is_unknown_test() {
        let no_oracle = test_from(serde_json::json!({
            "id":"f","title":"acts only","kind":"frontend",
            "planSteps":["Click Sign In","Input Email: a@b.co"]
        }));
        assert!(warns(&no_oracle).iter().any(|w| w.contains("unknown-test")));

        let with_oracle = test_from(serde_json::json!({
            "id":"g","title":"acts then verifies","kind":"frontend",
            "planSteps":["Click Sign In",{"action":"assert_text","text":"Welcome"}]
        }));
        assert!(
            !warns(&with_oracle)
                .iter()
                .any(|w| w.contains("unknown-test"))
        );
    }

    #[test]
    fn backend_spec_allows_query_method() {
        let issues = check_backend_spec(&serde_json::json!({
            "method": "QUERY",
            "path": "/search"
        }));
        assert!(
            !issues.iter().any(|i| matches!(
                i,
                Issue::Hard {
                    field: "method",
                    ..
                }
            )),
            "QUERY is a real HTTP method and should lint cleanly"
        );
    }

    #[test]
    fn command_tests_require_code_not_steps() {
        let test = super::super::LocalTest {
            id: "bad-command".to_string(),
            title: "bad".to_string(),
            description: String::new(),
            kind: Some(TestKind::Command),
            spec: None,
            extra: serde_json::json!({"steps":[{"run":"echo nope"}]})
                .as_object()
                .unwrap()
                .clone(),
        };
        let issues = check(&test.id, &test);
        assert!(
            issues
                .iter()
                .any(|i| matches!(i, Issue::Hard { field: "code", .. }))
        );
        assert!(
            issues
                .iter()
                .any(|i| matches!(i, Issue::Hard { field: "steps", .. }))
        );
    }
}
