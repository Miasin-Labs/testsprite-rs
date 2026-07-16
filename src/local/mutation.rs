//! Mutation testing — the adequacy metric coverage cannot give.
//!
//! Every other signal the tool surfaces (line/branch coverage, pass/fail,
//! flaky, visual) can be high while the assertions catch zero bugs — the
//! single most-repeated finding across the test-generation literature
//! (coverage and fault-detection dissociate hard: near-identical coverage,
//! 17% vs 69% fault detection). Mutation testing closes that blind spot: seed
//! small faults into the source and measure how many the suite KILLS.
//!
//! For Rust crates this delegates to `cargo-mutants` (the mature, correct tool
//! the ecosystem already uses) and reads its machine-readable `outcomes.json`.
//! When `cargo-mutants` is not installed the report says so rather than
//! reporting a fabricated score.

use std::path::Path;

use serde::Serialize;

/// The result of a mutation run.
#[derive(Debug, Clone, Serialize)]
pub struct MutationReport {
    /// Mutants the suite killed (a test failed on the mutated code).
    pub caught: usize,
    /// Mutants that survived (no test noticed the seeded fault) — the ones that
    /// expose weak or missing oracles.
    pub missed: usize,
    /// Mutants that did not compile / timed out — excluded from the score, not
    /// counted as kills or misses.
    pub unviable: usize,
    pub timeout: usize,
    /// `caught / (caught + missed)` as a percentage; `None` when nothing
    /// testable was produced.
    pub kill_score: Option<f64>,
    /// Present only when a real measurement could not be taken.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unavailable: Option<String>,
    /// A few surviving-mutant descriptions, for the "strengthen these" worklist.
    pub survivors: Vec<String>,
}

impl MutationReport {
    fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            caught: 0,
            missed: 0,
            unviable: 0,
            timeout: 0,
            kill_score: None,
            unavailable: Some(reason.into()),
            survivors: Vec::new(),
        }
    }

    /// Tally from a set of per-mutant outcome labels.
    fn from_outcomes(labels: &[String], survivors: Vec<String>) -> Self {
        let mut r = Self {
            caught: 0,
            missed: 0,
            unviable: 0,
            timeout: 0,
            kill_score: None,
            unavailable: None,
            survivors,
        };
        for label in labels {
            match classify_outcome(label) {
                Outcome::Caught => r.caught += 1,
                Outcome::Missed => r.missed += 1,
                Outcome::Unviable => r.unviable += 1,
                Outcome::Timeout => r.timeout += 1,
                Outcome::Other => {}
            }
        }
        let denom = r.caught + r.missed;
        if denom > 0 {
            r.kill_score = Some(100.0 * r.caught as f64 / denom as f64);
        }
        r
    }
}

enum Outcome {
    Caught,
    Missed,
    Unviable,
    Timeout,
    Other,
}

/// Map a cargo-mutants outcome summary string to our tally bucket. Accepts the
/// several spellings the tool has used across versions.
fn classify_outcome(summary: &str) -> Outcome {
    let s = summary.to_ascii_lowercase();
    if s.contains("caught") {
        Outcome::Caught
    } else if s.contains("missed") {
        Outcome::Missed
    } else if s.contains("unviable") {
        Outcome::Unviable
    } else if s.contains("timeout") {
        Outcome::Timeout
    } else {
        // "Success" means the baseline (unmutated) build/test — not a mutant.
        Outcome::Other
    }
}

/// Parse cargo-mutants `outcomes.json`. Its shape has drifted across versions;
/// we look for an `outcomes` array and read each entry's `summary` plus a
/// human label for survivors. Returns `(labels, survivor_descriptions)`.
fn parse_outcomes(json: &serde_json::Value) -> (Vec<String>, Vec<String>) {
    let mut labels = Vec::new();
    let mut survivors = Vec::new();
    let Some(arr) = json.get("outcomes").and_then(|v| v.as_array()) else {
        return (labels, survivors);
    };
    for o in arr {
        let summary = o
            .get("summary")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if matches!(classify_outcome(&summary), Outcome::Missed) {
            let desc = o
                .get("scenario")
                .and_then(|s| s.get("Mutant"))
                .and_then(|m| m.get("describe").or_else(|| m.get("genre")))
                .and_then(|d| d.as_str())
                .or_else(|| o.get("name").and_then(|v| v.as_str()))
                .unwrap_or("<mutant>")
                .to_string();
            survivors.push(desc);
        }
        labels.push(summary);
    }
    (labels, survivors)
}

/// Run mutation testing for a Rust crate at `crate_dir`. Requires a
/// `Cargo.toml` and the `cargo-mutants` subcommand; degrades to an
/// `unavailable` report otherwise.
pub fn run_rust(crate_dir: &Path, timeout_secs: u64) -> MutationReport {
    if !crate_dir.join("Cargo.toml").exists() {
        return MutationReport::unavailable(format!(
            "no Cargo.toml at {} — mutation testing here supports Rust crates via cargo-mutants",
            crate_dir.display()
        ));
    }
    let installed = std::process::Command::new("cargo")
        .args(["mutants", "--version"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !installed {
        return MutationReport::unavailable(
            "cargo-mutants is not installed — run `cargo install cargo-mutants` to measure \
             oracle strength"
                .to_string(),
        );
    }

    let out_dir =
        std::env::temp_dir().join(format!("testsprite-rs-mutants-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out_dir);
    let status = std::process::Command::new("cargo")
        .args(["mutants", "--no-shuffle", "--timeout"])
        .arg(timeout_secs.to_string())
        .arg("--output")
        .arg(&out_dir)
        .current_dir(crate_dir)
        .output();
    if let Err(e) = status {
        return MutationReport::unavailable(format!("cargo mutants failed to launch: {e}"));
    }

    let outcomes_path = out_dir.join("mutants.out").join("outcomes.json");
    let report = match std::fs::read_to_string(&outcomes_path) {
        Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(json) => {
                let (labels, survivors) = parse_outcomes(&json);
                if labels.is_empty() {
                    MutationReport::unavailable(
                        "cargo-mutants produced no mutant outcomes (no mutable functions found?)"
                            .to_string(),
                    )
                } else {
                    MutationReport::from_outcomes(&labels, survivors)
                }
            }
            Err(e) => MutationReport::unavailable(format!("parsing cargo-mutants outcomes: {e}")),
        },
        Err(e) => MutationReport::unavailable(format!(
            "reading cargo-mutants outcomes at {}: {e}",
            outcomes_path.display()
        )),
    };
    let _ = std::fs::remove_dir_all(&out_dir);
    report
}

/// `coverage --mutation`: measure oracle strength for the crate at `path` and
/// print a report (JSON when `json`). Returns exit `0` (measurement is
/// informational; `gate --min-mutation` is what enforces a floor).
pub fn mutation_report(path: &Path, json: bool, timeout_secs: u64) -> anyhow::Result<i32> {
    let report = run_rust(path, timeout_secs);
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else if let Some(reason) = &report.unavailable {
        println!("mutation: unavailable — {reason}");
    } else {
        println!(
            "mutation: {:.1}% kill ({} caught, {} missed, {} unviable, {} timeout)",
            report.kill_score.unwrap_or(0.0),
            report.caught,
            report.missed,
            report.unviable,
            report.timeout,
        );
        if !report.survivors.is_empty() {
            println!("surviving mutants (strengthen the oracles that should catch these):");
            for s in report.survivors.iter().take(15) {
                println!("  - {s}");
            }
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kill_score_excludes_unviable_and_timeout() {
        let labels = vec![
            "CaughtMutant".to_string(),
            "CaughtMutant".to_string(),
            "MissedMutant".to_string(),
            "Unviable".to_string(),
            "Timeout".to_string(),
            "Success".to_string(), // baseline, ignored
        ];
        let r = MutationReport::from_outcomes(&labels, Vec::new());
        assert_eq!(r.caught, 2);
        assert_eq!(r.missed, 1);
        assert_eq!(r.unviable, 1);
        assert_eq!(r.timeout, 1);
        // 2 / (2+1) = 66.7%, ignoring unviable/timeout/baseline.
        assert!((r.kill_score.unwrap() - 66.666).abs() < 0.1);
    }

    #[test]
    fn no_testable_mutants_yields_no_score() {
        let r = MutationReport::from_outcomes(&["Success".to_string()], Vec::new());
        assert_eq!(r.kill_score, None);
    }

    #[test]
    fn parse_outcomes_reads_summaries_and_survivor_descriptions() {
        let json = serde_json::json!({
            "outcomes": [
                { "summary": "CaughtMutant", "scenario": {"Mutant": {"describe": "replace + with -"}} },
                { "summary": "MissedMutant", "scenario": {"Mutant": {"describe": "replace > with >="}} },
                { "summary": "Unviable" },
            ]
        });
        let (labels, survivors) = parse_outcomes(&json);
        assert_eq!(labels.len(), 3);
        assert_eq!(survivors, vec!["replace > with >=".to_string()]);
    }

    #[test]
    fn missing_cargo_toml_is_unavailable_not_a_fake_zero() {
        let dir = crate::local::tmp_root();
        let r = run_rust(&dir, 60);
        assert!(r.unavailable.is_some());
        assert_eq!(r.kill_score, None);
        std::fs::remove_dir_all(dir).ok();
    }
}
