//! `loop` — the regression loop in one call.
//!
//! Composes, in one agent-facing step, what the pieces otherwise make you wire
//! by hand after a build:
//!   1. GENERATE (optional) — tests for functions changed since a ref that no
//!      stored test covers yet, so new code enters the suite.
//!   2. RUN — the changed subset in `--changed` mode (falling back to the full
//!      suite when a change can't be attributed to a test, never an empty
//!      green), otherwise the whole suite.
//!   3. TRIAGE — group the failures into root-cause clusters.
//!   4. SURFACE — one actionable result: counts, the failing tests, the
//!      clusters, and a single next-action line.
//!
//! This is "the regression suite is the loop's memory": lock in every shipped
//! feature, re-run the whole surface, hand the breaks back to the agent.

use std::path::Path;

use serde::Serialize;
use serde_json::{Value, json};

use super::{changed, generate, run, triage};

/// One pass of the loop, ready to hand back to the agent (or a CI gate).
#[derive(Debug, Clone, Serialize)]
pub struct CycleReport {
    /// Ids of tests generated this pass (empty unless `generate` + `changed`).
    pub generated: Vec<String>,
    /// What was run: `all` | `changed` | `unattributable` | `no_changes`.
    pub selection: &'static str,
    pub total: usize,
    pub passed: usize,
    /// Real product failures (verdict `failed`).
    pub failed: usize,
    /// Couldn't-verify runs (verdict `blocked`: auth / network / infra).
    pub blocked: usize,
    /// One entry per non-passing test: `{id,title,verdict,failureKind,cause}`.
    pub failures: Vec<Value>,
    pub clusters: Vec<triage::Cluster>,
    pub next_action: String,
    /// True iff nothing failed and nothing was left unverified.
    pub green: bool,
}

/// Knobs for one [`cycle`] pass.
pub struct CycleOpts<'a> {
    pub changed: bool,
    pub since: Option<&'a str>,
    pub generate: bool,
    pub model: &'a str,
    pub fix: bool,
    pub serve: bool,
    pub require_approved_prd: bool,
}

/// Run one full loop pass and return its [`CycleReport`].
pub async fn cycle(root: &Path, opts: CycleOpts<'_>) -> anyhow::Result<CycleReport> {
    // 1. Generate for the changed surface (best-effort; needs an OpenAI key).
    //    Scoped to the change, not the whole repo — "test the new code", not
    //    "regenerate everything".
    let mut generated = Vec::new();
    if opts.generate && opts.changed {
        let since = opts.since.unwrap_or("HEAD");
        match generate::generate_changed(root, since, opts.model).await {
            Ok(g) => generated = g.test_ids,
            Err(e) => tracing::warn!("cycle: generate step skipped ({e})"),
        }
    } else if opts.generate {
        tracing::info!("cycle: `generate` only acts in `changed` mode; skipping");
    }

    // 2. Decide what to run.
    let (ids, selection) = if opts.changed {
        let since = opts.since.unwrap_or("HEAD");
        let cs = changed::changed_surface(root, since)?;
        match changed::select(root, &cs).await? {
            changed::Selection::NoChanges => (Vec::new(), "no_changes"),
            changed::Selection::Affected(ids) => (ids, "changed"),
            // Never report an empty success: run the whole suite and say so.
            changed::Selection::Unattributable { .. } => (Vec::new(), "unattributable"),
        }
    } else {
        (Vec::new(), "all")
    };

    // Nothing changed and nothing generated → nothing to verify. Don't spin the
    // whole suite; report an honest, empty green.
    if selection == "no_changes" && generated.is_empty() {
        return Ok(CycleReport {
            generated,
            selection,
            total: 0,
            passed: 0,
            failed: 0,
            blocked: 0,
            failures: Vec::new(),
            clusters: Vec::new(),
            next_action: "no source changes and nothing generated — nothing to verify".to_string(),
            green: true,
        });
    }

    // 3. Run. Empty ids = the whole suite (all / unattributable / generated).
    if opts.require_approved_prd {
        crate::local::store::assert_prds_approved(root, &ids).await?;
    }
    let results =
        run::run_collect(root, &ids, None, opts.model, opts.fix, None, 1, opts.serve).await?;

    // 4. Tally, then triage the failures we just recorded.
    let (passed, failed, blocked, failures) = summarize(&results);
    let total = results.len();
    let clusters = triage::triage(root).await?;

    let green = failed == 0 && blocked == 0;
    let next_action = next_action(
        selection, total, passed, failed, blocked, &clusters, &generated,
    );

    Ok(CycleReport {
        generated,
        selection,
        total,
        passed,
        failed,
        blocked,
        failures,
        clusters,
        next_action,
        green,
    })
}

/// Split run results into `(passed, failed, blocked, failures)`. A `blocked`
/// verdict (auth/network/infra) is counted apart from a real `failed`: it means
/// "couldn't verify", not "product bug".
fn summarize(results: &[Value]) -> (usize, usize, usize, Vec<Value>) {
    let mut passed = 0;
    let mut failed = 0;
    let mut blocked = 0;
    let mut failures = Vec::new();
    for r in results {
        if r.get("passed").and_then(Value::as_bool).unwrap_or(false) {
            passed += 1;
            continue;
        }
        let verdict = r.get("verdict").and_then(Value::as_str).unwrap_or("failed");
        if verdict == "blocked" {
            blocked += 1;
        } else {
            failed += 1;
        }
        let cause = r
            .get("analysis")
            .and_then(|a| a.get("cause"))
            .or_else(|| r.get("cause"))
            .cloned()
            .unwrap_or(Value::Null);
        failures.push(json!({
            "id": r.get("id").cloned().unwrap_or(Value::Null),
            "title": r.get("title").cloned().unwrap_or(Value::Null),
            "verdict": verdict,
            "failureKind": r.get("failureKind").cloned().unwrap_or(Value::Null),
            "cause": cause,
        }));
    }
    (passed, failed, blocked, failures)
}

/// The single line an agent reads to know what to do next.
fn next_action(
    selection: &str,
    total: usize,
    passed: usize,
    failed: usize,
    blocked: usize,
    clusters: &[triage::Cluster],
    generated: &[String],
) -> String {
    let mut parts = Vec::new();
    if !generated.is_empty() {
        parts.push(format!("generated {} new test(s)", generated.len()));
    }
    if selection == "unattributable" {
        parts.push("changes couldn't be attributed to any test — ran the full suite".to_string());
    }
    if total == 0 {
        // Vacuously "green" (nothing failed) but nothing was verified either —
        // don't claim the surface is locked when the suite is empty.
        parts.push(
            "no tests were run — the suite is empty; store or generate tests first".to_string(),
        );
        return parts.join("; ");
    }
    if failed == 0 && blocked == 0 {
        parts.push(format!("green: {passed}/{total} passed — surface locked"));
        return parts.join("; ");
    }
    if failed > 0 {
        let top: Vec<String> = clusters
            .iter()
            .take(3)
            .map(|c| format!("{}×{}", c.failure_kind, c.count))
            .collect();
        parts.push(format!(
            "{failed} failing in {} root-cause cluster(s) [{}] — fix the causes, then \
             `rerun --failed --heal` for fragility or `run --fix` for a patch",
            clusters.len(),
            top.join(", ")
        ));
    }
    if blocked > 0 {
        parts.push(format!(
            "{blocked} blocked (auth/network/infra, not scored as failures) — bring the target \
             up (serve) or fix credentials, then re-loop"
        ));
    }
    parts.join("; ")
}

/// Print a [`CycleReport`] (human lines or one JSON object) and return the exit
/// code: 0 when green, 1 otherwise (so the loop is CI-gateable).
pub async fn cycle_report(root: &Path, opts: CycleOpts<'_>, json_out: bool) -> anyhow::Result<i32> {
    let report = cycle(root, opts).await?;

    if json_out {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(if report.green { 0 } else { 1 });
    }

    if !report.generated.is_empty() {
        println!("generated {} new test(s)", report.generated.len());
    }
    println!(
        "loop [{}]: {}/{} passed, {} failed, {} blocked",
        report.selection, report.passed, report.total, report.failed, report.blocked
    );
    for f in &report.failures {
        let s = |k: &str| f.get(k).and_then(Value::as_str).unwrap_or("");
        println!(
            "  {}  {}  [{} / {}]",
            s("id"),
            s("title"),
            s("verdict"),
            s("failureKind")
        );
    }
    if !report.clusters.is_empty() {
        println!("root-cause clusters:");
        for c in &report.clusters {
            println!("  [{}] x{}", c.failure_kind, c.count);
        }
    }
    println!("→ {}", report.next_action);
    Ok(if report.green { 0 } else { 1 })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(id: &str, passed: bool, verdict: &str, kind: &str) -> Value {
        json!({ "id": id, "title": id, "passed": passed, "verdict": verdict, "failureKind": kind })
    }

    #[test]
    fn summarize_splits_pass_fail_blocked() {
        let results = vec![
            result("a", true, "passed", ""),
            result("b", false, "failed", "assertion"),
            result("c", false, "blocked", "network"),
            result("d", false, "failed", "routing_404"),
        ];
        let (passed, failed, blocked, failures) = summarize(&results);
        assert_eq!((passed, failed, blocked), (1, 2, 1));
        // A blocked run is surfaced but not counted as a product failure.
        assert_eq!(failures.len(), 3);
        assert!(
            failures
                .iter()
                .any(|f| f["id"] == "c" && f["verdict"] == "blocked")
        );
    }

    #[test]
    fn next_action_is_green_when_clean() {
        let msg = next_action("all", 5, 5, 0, 0, &[], &[]);
        assert!(msg.contains("green"), "{msg}");
        assert!(msg.contains("surface locked"), "{msg}");
    }

    #[test]
    fn next_action_names_clusters_and_blocked_when_red() {
        let clusters = vec![
            triage::Cluster {
                failure_kind: "assertion".into(),
                count: 2,
                test_ids: vec!["b".into(), "d".into()],
                sample_cause: None,
            },
            triage::Cluster {
                failure_kind: "network".into(),
                count: 1,
                test_ids: vec!["c".into()],
                sample_cause: None,
            },
        ];
        let msg = next_action("all", 4, 1, 2, 1, &clusters, &[]);
        assert!(msg.contains("2 failing"), "{msg}");
        assert!(msg.contains("assertion×2"), "{msg}");
        assert!(msg.contains("1 blocked"), "{msg}");
        assert!(!msg.contains("green"), "{msg}");
    }

    #[test]
    fn next_action_does_not_claim_locked_on_an_empty_suite() {
        let msg = next_action("all", 0, 0, 0, 0, &[], &[]);
        assert!(msg.contains("empty"), "{msg}");
        assert!(!msg.contains("surface locked"), "{msg}");
    }

    #[test]
    fn next_action_flags_unattributable_and_generated() {
        let msg = next_action("unattributable", 10, 10, 0, 0, &[], &["g1".to_string()]);
        assert!(msg.contains("generated 1"), "{msg}");
        assert!(msg.contains("couldn't be attributed"), "{msg}");
    }
}
