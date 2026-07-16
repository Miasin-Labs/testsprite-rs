//! Acceptance gate for freshly generated tests.
//!
//! An LLM-generated oracle is guilty until proven runnable: studies measure
//! 28–58% of generated assertions wrong even when they compile, and a wrong
//! oracle stored as a test later surfaces as a false-positive "bug". So before
//! a generated case counts as part of the suite, it runs ONCE against the
//! current (presumed green) baseline:
//!
//! - **passed** → accepted; the run row doubles as its first history entry.
//! - **blocked** (target down, auth wall, missing toolchain) → accepted with a
//!   note — the gate could not evaluate the oracle, and refusing to store a
//!   case because the app wasn't running would punish offline generation.
//! - **failed** → quarantined as `suspect_oracle`: the case stays stored (it
//!   may be a genuine bug-revealer) but is excluded from whole-suite runs
//!   until a human runs it by id or `test release <id>` reinstates it.

use std::path::Path;
use std::sync::Arc;

use super::{project, store};
use crate::server::executors::{ExecCtx, Outcome};

/// What the gate decided for one batch of candidate ids.
#[derive(Debug, Default)]
pub struct Screen {
    pub accepted: Vec<String>,
    pub quarantined: Vec<String>,
    /// Blocked before the oracle could be judged (env problem, not oracle).
    pub unevaluated: Vec<String>,
}

/// Run each candidate once against the current baseline and quarantine the
/// ones whose oracle fails on it. Prints a one-line summary to stderr.
pub async fn screen(root: &Path, ids: &[String], model: &str) -> anyhow::Result<Screen> {
    let project = project::load(root).await.ok();
    let target = project
        .as_ref()
        .and_then(|p| p.target_url.clone())
        .unwrap_or_else(|| "http://127.0.0.1:8080".to_string());
    let default_kind = project.as_ref().map(|p| p.kind).unwrap_or_default();
    let llm = crate::server::llm::LlmClient::from_env(model);
    let ctx = ExecCtx {
        target,
        llm: llm.clone(),
        prd: Arc::new(serde_json::json!({})),
        browser: None,
        shots_dir: None,
        root: root.to_path_buf(),
        variables: project::load_variables(root),
    };

    let mut screen = Screen::default();
    for id in ids {
        let test = store::load_one(root, id).await?;
        let kind = test.kind.unwrap_or(default_kind);
        let before = llm.as_ref().map(|l| l.usage());
        let (outcome, elapsed_ms) =
            crate::local::run::execute_case(&test, &ctx, default_kind).await;
        let tokens = llm.as_ref().zip(before).map(|(l, b)| {
            let d = l.usage().since(&b);
            (d.prompt_tokens, d.completion_tokens)
        });
        let meta = store::RunMeta {
            elapsed_ms: Some(elapsed_ms),
            model: llm.as_ref().map(|_| model.to_string()),
            prompt_tokens: tokens.map(|(p, _)| p),
            completion_tokens: tokens.map(|(_, c)| c),
        };

        let (verdict, _) = crate::local::verdict::classify(outcome.passed, &outcome.error, kind);
        match verdict {
            crate::local::verdict::Verdict::Passed => {
                store::write_result_with_meta(root, id, &outcome, None, kind, &meta).await?;
                screen.accepted.push(id.clone());
            }
            crate::local::verdict::Verdict::Blocked => {
                store::write_result_with_meta(root, id, &outcome, None, kind, &meta).await?;
                screen.unevaluated.push(id.clone());
            }
            crate::local::verdict::Verdict::Failed => {
                // Re-stamp the failure as ours: the ORACLE failed against a
                // baseline presumed green, so the verdict module records
                // `suspect_oracle`, not a product bug.
                let flagged = Outcome::fail(
                    format!(
                        "suspect oracle: failed against the current baseline at generation \
                         time: {}",
                        outcome.error
                    ),
                    outcome.code.clone(),
                );
                store::write_result_with_meta(root, id, &flagged, None, kind, &meta).await?;
                store::set_quarantine(root, id, Some("suspect_oracle")).await?;
                screen.quarantined.push(id.clone());
            }
        }
    }

    if !screen.quarantined.is_empty() || !screen.unevaluated.is_empty() {
        eprintln!(
            "acceptance gate: {} accepted, {} quarantined as suspect-oracle{}{}",
            screen.accepted.len(),
            screen.quarantined.len(),
            if screen.quarantined.is_empty() {
                String::new()
            } else {
                format!(" ({})", screen.quarantined.join(", "))
            },
            if screen.unevaluated.is_empty() {
                String::new()
            } else {
                format!(
                    ", {} not evaluated (target unreachable/blocked)",
                    screen.unevaluated.len()
                )
            },
        );
    }
    Ok(screen)
}

/// True when a loaded test carries a quarantine marker.
pub fn is_quarantined_test(t: &super::LocalTest) -> bool {
    t.extra.get("quarantine").is_some_and(|q| !q.is_null())
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    fn is_quarantined(body: &Value) -> bool {
        body.get("quarantine").is_some_and(|q| !q.is_null())
    }

    /// Command-kind cases execute deterministically through the real shell
    /// executor, so the gate's three outcomes can be exercised end-to-end
    /// with no LLM and no network.
    #[tokio::test]
    async fn failing_candidate_is_quarantined_and_passing_one_accepted() {
        let root = crate::local::tmp_root();
        let good = store::add_value(
            &root,
            serde_json::json!({"id":"ok1","title":"passes","kind":"command","code":"exit 0"}),
        )
        .await
        .unwrap();
        let bad = store::add_value(
            &root,
            serde_json::json!({"id":"bad1","title":"fails on green","kind":"command","code":"echo broken oracle; exit 1"}),
        )
        .await
        .unwrap();

        let screen = screen_ids(&root, &[good.clone(), bad.clone()]).await;
        assert_eq!(screen.accepted, vec![good.clone()]);
        assert_eq!(screen.quarantined, vec![bad.clone()]);

        // The quarantined case is marked in the store and its run history
        // records the suspect_oracle kind.
        let body = store::get_value(&root, &bad).await.unwrap();
        assert!(is_quarantined(&body));
        assert_eq!(body["quarantine"], "suspect_oracle");
        let runs = store::run_history(&root, &bad).await.unwrap();
        assert_eq!(runs[0]["failureKind"], "suspect_oracle");

        // The accepted case is not marked, and got a passing first run.
        let body = store::get_value(&root, &good).await.unwrap();
        assert!(!is_quarantined(&body));
        let runs = store::run_history(&root, &good).await.unwrap();
        assert_eq!(runs[0]["passed"], true);
        assert!(
            runs[0]["elapsed_ms"].is_number(),
            "the gate's baseline run must record telemetry: {}",
            runs[0]
        );

        // Release reinstates it for whole-suite runs.
        store::set_quarantine(&root, &bad, None).await.unwrap();
        let body = store::get_value(&root, &bad).await.unwrap();
        assert!(!is_quarantined(&body));

        std::fs::remove_dir_all(root).ok();
    }

    async fn screen_ids(root: &Path, ids: &[String]) -> Screen {
        screen(root, ids, "no-such-model").await.unwrap()
    }
}
