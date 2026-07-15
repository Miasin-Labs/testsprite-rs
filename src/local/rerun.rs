//! Re-run stored test cases through the existing Executor seam, optionally
//! auto-healing fragility failures via the LLM.

use std::path::Path;
use std::sync::Arc;

use serde_json::Value;

use super::{project, store};
use crate::server::engine::{Band, Expect};
use crate::server::executors::ExecCtx;

const DEFAULT_TARGET: &str = "http://127.0.0.1:8080";

/// How strict a pass criterion is. A heal may tighten it, never loosen it.
fn strictness(e: Expect) -> u8 {
    match e {
        Expect::Exact(_) => 3,
        Expect::Band(Band::Success) => 2,
        Expect::Band(Band::Accepted) => 1,
        Expect::Band(Band::Any) => 0,
    }
}

fn expect_of(case: &Value) -> Expect {
    case.get("spec")
        .and_then(|s| s.get("expect_status"))
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default()
}

/// Reject a "healed" case that would pass by proving less than the original.
///
/// Returns `Some(reason)` when the rewrite weakens the test. These checks are
/// deliberately mechanical and incomplete — they cannot catch every way an LLM
/// might hollow out a test, but they catch the ones that turn it into a no-op:
/// re-pointing it at a different endpoint, widening the accepted status, or
/// deleting the assertions outright.
fn weakened(original: &Value, healed: &Value) -> Option<String> {
    // A heal fixes HOW a test runs, never WHAT it targets. The method is
    // compared case-insensitively — a faithful heal commonly echoes the verb in
    // a different case ("GET" -> "get") without re-pointing the test, and
    // rejecting that would silently discard a valid heal. The path stays exact.
    for field in ["method", "path"] {
        let before = original.get("spec").and_then(|s| s.get(field));
        let after = healed.get("spec").and_then(|s| s.get(field));
        let differs = if field == "method" {
            let norm =
                |v: Option<&Value>| v.and_then(Value::as_str).map(|s| s.to_ascii_uppercase());
            norm(before) != norm(after)
        } else {
            before != after
        };
        if before.is_some() && differs {
            return Some(format!(
                "spec.{field} changed ({} -> {})",
                before.map(ToString::to_string).unwrap_or_default(),
                after.map(ToString::to_string).unwrap_or_default(),
            ));
        }
    }

    // Dropping the spec entirely leaves nothing to execute.
    if original.get("spec").is_some() && healed.get("spec").is_none() {
        return Some("spec removed".to_string());
    }

    if original.get("spec").is_some() {
        let before = expect_of(original);
        let after = expect_of(healed);
        if strictness(after) < strictness(before) {
            return Some(format!(
                "expect_status widened ({} -> {})",
                before.describe(),
                after.describe()
            ));
        }
    }

    // Code tests: don't let the assertions be deleted.
    let assertions = |v: &Value| -> usize {
        v.get("code")
            .and_then(Value::as_str)
            .map(|c| c.matches("assert").count())
            .unwrap_or(0)
    };
    let before = assertions(original);
    if before > 0 {
        let after = assertions(healed);
        if after < before {
            return Some(format!("assertions dropped ({before} -> {after})"));
        }
    }

    None
}

/// Persist a heal only after it is verified, snapshotting the original first.
///
/// This encodes the two invariants a destructive rewrite must hold, in one
/// place a test can drive without a live LLM:
///   1. **Verify before persist** — an unverified (`retry` failed) heal is
///      never written, so a failed heal leaves the stored test untouched.
///   2. **Snapshot before overwrite** — `snapshot_revision` (which captures the
///      *current* stored body) runs before `add_value` upserts the rewrite, so
///      the original is always recoverable. If the snapshot fails we do NOT
///      overwrite.
///
/// Returns `Ok(true)` iff the heal was persisted.
async fn persist_heal(
    root: &Path,
    id: &str,
    healed_case: Value,
    retry_passed: bool,
) -> anyhow::Result<bool> {
    if !retry_passed {
        return Ok(false);
    }
    store::snapshot_revision(root, id, "rerun --heal").await?;
    store::add_value(root, healed_case).await?;
    Ok(true)
}

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
        variables: crate::local::project::load_variables(root),
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
        let mut rejected_heal: Option<String> = None;
        let mut verdict: Option<String> = None;
        let mut analysis: Option<Value> = None;

        if !outcome.passed {
            analysis = match &llm {
                Some(c) => match c
                    .analyze_failure(&case, &outcome.code, &outcome.error)
                    .await
                {
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

            if heal
                && v.as_deref() == Some("fragility")
                && let Some(c) = &llm
            {
                match c.heal_test(&case, &outcome.code, &outcome.error).await {
                    Ok(Value::Object(mut improved)) => {
                        improved.insert("id".to_string(), Value::String(t.id.clone()));
                        let healed_case = Value::Object(improved);

                        // The heal prompt asks the model not to weaken the
                        // assertion, but a prompt is not an enforcement
                        // mechanism — and `retry.passed` is trivially satisfied
                        // by a case that asserts nothing. Check mechanically.
                        if let Some(why) = weakened(&case, &healed_case) {
                            tracing::warn!("rejected heal for {}: {why}", t.id);
                            rejected_heal = Some(why);
                        } else {
                            // Verify BEFORE persisting, and snapshot the original
                            // BEFORE overwriting it — both invariants live in
                            // `persist_heal` so a failed heal can never lose the
                            // original assertion. A failed retry changes nothing
                            // on disk: the stored test still asserts what it did.
                            let retry = ex.run(&healed_case, &ctx).await;
                            let retry_passed = retry.passed;
                            match persist_heal(root, &t.id, healed_case, retry_passed).await {
                                Ok(true) => {
                                    healed = true;
                                    outcome = retry;
                                }
                                Ok(false) => {}
                                Err(e) => {
                                    tracing::warn!("persisting heal for {} failed: {e}", t.id);
                                }
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
            if let Some(why) = &rejected_heal {
                entry["healRejected"] = serde_json::json!(why);
            }
            report.push(entry);
        } else if outcome.passed && healed {
            println!("HEALED  {}  {}", t.id, t.title);
        } else if outcome.passed {
            println!("PASS  {}  {}", t.id, t.title);
        } else if let Some(why) = &rejected_heal {
            println!("HEAL-REJECTED  {}  {}", t.id, t.title);
            println!("      the rewrite would have weakened the test: {why}");
            println!("      kept the original; it still fails");
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

        store::write_result(root, &t.id, &outcome, analysis.as_ref(), kind).await?;
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        let passed = total - failed;
        println!("\n{passed}/{total} passed");
    }

    Ok(if failed == 0 { 0 } else { 1 })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_faithful_heal_is_allowed() {
        let original = json!({
            "title": "t",
            "spec": {"method": "GET", "path": "/todos", "expect_status": 200},
        });
        // Same target, same expectation, adapted elsewhere: this is a real heal.
        let healed = json!({
            "title": "t",
            "spec": {"method": "GET", "path": "/todos", "expect_status": 200,
                     "headers": {"Accept": "application/json"}},
        });
        assert_eq!(weakened(&original, &healed), None);
    }

    #[test]
    fn a_method_case_change_alone_is_not_a_repoint() {
        // Regression: `weakened` compared method by strict JSON inequality, but
        // stored methods are upper-cased while an LLM heal commonly returns
        // "get". A faithful, same-target heal was discarded as a re-point.
        let original = json!({"spec": {"method": "GET", "path": "/todos", "expect_status": 200}});
        let healed = json!({"spec": {"method": "get", "path": "/todos", "expect_status": 200}});
        assert_eq!(weakened(&original, &healed), None);
        // A genuine method change is still rejected.
        let repointed =
            json!({"spec": {"method": "DELETE", "path": "/todos", "expect_status": 200}});
        let why = weakened(&original, &repointed).expect("must reject");
        assert!(why.contains("spec.method"), "{why}");
    }

    #[test]
    fn tightening_the_expectation_is_allowed() {
        let original = json!({"spec": {"method": "GET", "path": "/a", "expect_status": "any"}});
        let healed = json!({"spec": {"method": "GET", "path": "/a", "expect_status": 200}});
        assert_eq!(weakened(&original, &healed), None);
    }

    #[test]
    fn widening_the_expectation_is_rejected() {
        // The classic hollow-out: make it pass by accepting anything.
        let original = json!({"spec": {"method": "GET", "path": "/a", "expect_status": 200}});
        let healed = json!({"spec": {"method": "GET", "path": "/a", "expect_status": "any"}});
        let why = weakened(&original, &healed).expect("must reject");
        assert!(why.contains("widened"), "{why}");

        // Absent means the success band, so sliding to `any` is still a widening.
        let original = json!({"spec": {"method": "GET", "path": "/a"}});
        let healed = json!({"spec": {"method": "GET", "path": "/a", "expect_status": "any"}});
        assert!(weakened(&original, &healed).is_some());
    }

    #[test]
    fn repointing_the_test_at_another_endpoint_is_rejected() {
        let original = json!({"spec": {"method": "GET", "path": "/todos"}});
        let healed = json!({"spec": {"method": "GET", "path": "/health"}});
        let why = weakened(&original, &healed).expect("must reject");
        assert!(why.contains("spec.path"), "{why}");
    }

    #[test]
    fn deleting_the_spec_or_the_assertions_is_rejected() {
        let original = json!({"spec": {"method": "GET", "path": "/a"}});
        assert!(weakened(&original, &json!({"title": "t"})).is_some());

        let original = json!({"code": "assert_eq!(a(), 1);\nassert!(b());"});
        let healed = json!({"code": "let _ = a();"});
        let why = weakened(&original, &healed).expect("must reject");
        assert!(why.contains("assertions dropped"), "{why}");
    }

    #[tokio::test]
    async fn snapshot_preserves_the_definition_a_heal_would_overwrite() {
        let root = crate::local::tmp_root();
        let id = store::add_value(
            &root,
            json!({"title": "original", "code": "assert_eq!(compute(), 42);"}),
        )
        .await
        .unwrap();

        store::snapshot_revision(&root, &id, "rerun --heal")
            .await
            .unwrap();
        store::add_value(
            &root,
            json!({"id": id, "title": "rewritten", "code": "// nothing"}),
        )
        .await
        .unwrap();

        // The upsert clobbered the live row...
        let live = store::load_one(&root, &id).await.unwrap();
        assert_eq!(live.title, "rewritten");
        // ...but what it used to assert is still recoverable.
        let revs = store::revisions(&root, &id).await.unwrap();
        assert_eq!(revs.len(), 1);
        assert_eq!(revs[0]["body"]["title"], "original");
        assert_eq!(revs[0]["body"]["code"], "assert_eq!(compute(), 42);");
        assert_eq!(revs[0]["reason"], "rerun --heal");

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn snapshot_of_an_unknown_id_is_a_noop() {
        let root = crate::local::tmp_root();
        store::snapshot_revision(&root, "nope", "x").await.unwrap();
        assert!(store::revisions(&root, "nope").await.unwrap().is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn persist_heal_verifies_before_writing_and_snapshots_the_original() {
        let root = crate::local::tmp_root();
        let id = store::add_value(
            &root,
            json!({"title": "original", "code": "assert_eq!(compute(), 42);"}),
        )
        .await
        .unwrap();
        let healed = json!({"id": id, "title": "healed", "code": "assert!(healthy());"});

        // A retry that FAILED must not touch disk: no overwrite, no revision.
        assert!(
            !persist_heal(&root, &id, healed.clone(), false)
                .await
                .unwrap()
        );
        assert_eq!(store::load_one(&root, &id).await.unwrap().title, "original");
        assert!(store::revisions(&root, &id).await.unwrap().is_empty());

        // A verified retry overwrites — but only after snapshotting the original,
        // so the revision holds the ORIGINAL body. If the order were ever flipped
        // (overwrite then snapshot) the revision would capture "healed" and this
        // assertion would fail, catching the silent reintroduction of data loss.
        assert!(persist_heal(&root, &id, healed, true).await.unwrap());
        assert_eq!(store::load_one(&root, &id).await.unwrap().title, "healed");
        let revs = store::revisions(&root, &id).await.unwrap();
        assert_eq!(revs.len(), 1);
        assert_eq!(revs[0]["body"]["title"], "original");
        assert_eq!(revs[0]["body"]["code"], "assert_eq!(compute(), 42);");

        std::fs::remove_dir_all(&root).ok();
    }
}
