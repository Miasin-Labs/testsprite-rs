//! Flaky-test detection (hackathon issue #199) — replay a stored test N times
//! and report a stability score. Auth-aware: a `blocked` run (auth/infra/
//! network) is not flakiness and is excluded from the stability denominator,
//! so a test that can never authenticate reads "failing"/"inconclusive",
//! never "flaky".

use std::path::Path;

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct FlakyReport {
    pub id: String,
    pub runs: usize,
    pub passed: usize,
    pub failed: usize,
    pub blocked: usize,
    pub stability: f64,
    pub verdict: String,
}

/// Classify `passed`/`failed`/`blocked` tallies into `(stability, verdict)`.
/// `blocked` runs (auth/infra/network) are excluded from the stability
/// denominator — they are never scored as flakiness.
fn classify_flaky(passed: usize, failed: usize, blocked: usize) -> (f64, &'static str) {
    let _ = blocked;
    let effective = passed + failed;
    if effective == 0 {
        return (0.0, "inconclusive");
    }
    let stability = passed as f64 / effective as f64;
    let verdict = if passed == effective {
        "stable"
    } else if passed == 0 {
        "failing"
    } else {
        "flaky"
    };
    (stability, verdict)
}

/// Replay `id` `runs` times and tally passed/failed/blocked verdicts.
pub async fn flaky(root: &Path, id: &str, runs: usize, model: &str) -> anyhow::Result<FlakyReport> {
    let mut passed = 0usize;
    let mut failed = 0usize;
    let mut blocked = 0usize;

    for _ in 0..runs {
        let results =
            crate::local::run::run_collect(root, std::slice::from_ref(&id.to_string()), None, model, false, None, 1, false)
                .await?;
        for r in &results {
            match r.get("verdict").and_then(serde_json::Value::as_str) {
                Some("passed") => passed += 1,
                Some("blocked") => blocked += 1,
                _ => failed += 1,
            }
        }
    }

    let (stability, verdict) = classify_flaky(passed, failed, blocked);
    Ok(FlakyReport {
        id: id.to_string(),
        runs,
        passed,
        failed,
        blocked,
        stability,
        verdict: verdict.to_string(),
    })
}

/// Compute [`flaky`] and print a human report or one JSON object. Returns
/// exit code 0 if the verdict is "stable", else 1.
pub async fn flaky_report(root: &Path, id: &str, runs: usize, model: &str, json: bool) -> anyhow::Result<i32> {
    let report = flaky(root, id, runs, model).await?;
    let code = if report.verdict == "stable" { 0 } else { 1 };

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(code);
    }

    let effective = report.passed + report.failed;
    let pct = report.stability * 100.0;
    let mut line = format!(
        "flaky {}: {} — {}/{} passed over {} runs (stability {:.1}%)",
        report.id, report.verdict, report.passed, effective, report.runs, pct
    );
    if report.blocked > 0 {
        line.push_str(&format!(" [{} blocked/inconclusive run(s) excluded]", report.blocked));
    }
    println!("{line}");
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_when_all_pass() {
        assert_eq!(classify_flaky(3, 0, 0), (1.0, "stable"));
    }

    #[test]
    fn failing_when_all_fail() {
        assert_eq!(classify_flaky(0, 3, 0), (0.0, "failing"));
    }

    #[test]
    fn flaky_when_mixed() {
        let (stability, verdict) = classify_flaky(2, 1, 0);
        assert_eq!(verdict, "flaky");
        assert!((stability - (2.0 / 3.0)).abs() < 1e-9);
    }

    #[test]
    fn inconclusive_when_all_blocked() {
        assert_eq!(classify_flaky(0, 0, 3), (0.0, "inconclusive"));
    }

    #[test]
    fn blocked_excluded_from_denominator() {
        let (stability, verdict) = classify_flaky(2, 1, 5);
        assert_eq!(verdict, "flaky");
        assert!((stability - (2.0 / 3.0)).abs() < 1e-9);
    }
}
