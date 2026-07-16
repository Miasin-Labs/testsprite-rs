//! `bench` — per-model telemetry scoreboard and drift detection.
//!
//! The runs table now records the model, wall-clock, and token spend behind
//! every executed test (see [`super::store::RunMeta`]). This turns those rows
//! into the questions the test-generation literature treats as first-class but
//! the tool could not previously answer: is generation getting slower or
//! pricier? which model is cost-efficient? has a model upgrade silently
//! changed pass rate or yield (model-version drift)? — none of which is
//! visible from pass/fail alone.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// Aggregate outcomes and telemetry for one model across the runs table.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelStats {
    pub model: String,
    pub runs: u64,
    pub passed: u64,
    /// Fraction in [0,1].
    pub pass_rate: f64,
    /// Mean wall-clock over runs that recorded one (ms).
    pub avg_elapsed_ms: Option<f64>,
    pub total_prompt_tokens: u64,
    pub total_completion_tokens: u64,
}

impl ModelStats {
    #[cfg(test)]
    pub fn total_tokens(&self) -> u64 {
        self.total_prompt_tokens + self.total_completion_tokens
    }
}

/// One telemetry row as stored (nullable columns).
type RunRow = (
    Option<String>, // model
    i64,            // passed
    Option<i64>,    // elapsed_ms
    Option<i64>,    // prompt_tokens
    Option<i64>,    // completion_tokens
);

/// Fold raw run rows into a per-model scoreboard, sorted by model name for
/// determinism. Rows with no model are grouped under `"(none)"` so runs that
/// used no LLM (pure spec/command execution) are still accounted for.
pub fn aggregate(rows: &[RunRow]) -> Vec<ModelStats> {
    use std::collections::BTreeMap;
    struct Acc {
        runs: u64,
        passed: u64,
        elapsed_sum: u64,
        elapsed_n: u64,
        prompt: u64,
        completion: u64,
    }
    let mut by: BTreeMap<String, Acc> = BTreeMap::new();
    for (model, passed, elapsed, prompt, completion) in rows {
        let key = model.clone().unwrap_or_else(|| "(none)".to_string());
        let a = by.entry(key).or_insert(Acc {
            runs: 0,
            passed: 0,
            elapsed_sum: 0,
            elapsed_n: 0,
            prompt: 0,
            completion: 0,
        });
        a.runs += 1;
        a.passed += (*passed != 0) as u64;
        if let Some(ms) = elapsed {
            a.elapsed_sum += (*ms).max(0) as u64;
            a.elapsed_n += 1;
        }
        a.prompt += prompt.unwrap_or(0).max(0) as u64;
        a.completion += completion.unwrap_or(0).max(0) as u64;
    }
    by.into_iter()
        .map(|(model, a)| ModelStats {
            model,
            runs: a.runs,
            passed: a.passed,
            pass_rate: if a.runs > 0 {
                a.passed as f64 / a.runs as f64
            } else {
                0.0
            },
            avg_elapsed_ms: (a.elapsed_n > 0).then(|| a.elapsed_sum as f64 / a.elapsed_n as f64),
            total_prompt_tokens: a.prompt,
            total_completion_tokens: a.completion,
        })
        .collect()
}

/// Query the runs table and compute the scoreboard.
pub async fn scoreboard(root: &Path) -> anyhow::Result<Vec<ModelStats>> {
    let pool = crate::local::db::open(root).await?;
    let rows: Vec<RunRow> = sqlx::query_as(
        "SELECT model, passed, elapsed_ms, prompt_tokens, completion_tokens FROM runs",
    )
    .fetch_all(&pool)
    .await?;
    Ok(aggregate(&rows))
}

/// A model whose metric moved beyond the drift threshold vs. the baseline.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Drift {
    pub model: String,
    pub metric: String,
    pub baseline: f64,
    pub current: f64,
}

/// Compare a current scoreboard against a stored baseline and flag any model
/// whose pass-rate moved by more than `pass_delta`, or whose avg latency moved
/// by more than `latency_frac` (fractional). New/removed models are not drift.
pub fn detect_drift(
    baseline: &[ModelStats],
    current: &[ModelStats],
    pass_delta: f64,
    latency_frac: f64,
) -> Vec<Drift> {
    let mut out = Vec::new();
    for cur in current {
        let Some(base) = baseline.iter().find(|b| b.model == cur.model) else {
            continue;
        };
        if (cur.pass_rate - base.pass_rate).abs() > pass_delta {
            out.push(Drift {
                model: cur.model.clone(),
                metric: "pass_rate".to_string(),
                baseline: base.pass_rate,
                current: cur.pass_rate,
            });
        }
        if let (Some(bl), Some(cl)) = (base.avg_elapsed_ms, cur.avg_elapsed_ms)
            && bl > 0.0
            && (cl - bl).abs() / bl > latency_frac
        {
            out.push(Drift {
                model: cur.model.clone(),
                metric: "avg_elapsed_ms".to_string(),
                baseline: bl,
                current: cl,
            });
        }
    }
    out
}

/// `bench`: print the per-model scoreboard (JSON when `json`). When a baseline
/// file exists it also reports drift; `--save-baseline` overwrites it.
pub async fn bench_report(root: &Path, json: bool, save_baseline: bool) -> anyhow::Result<i32> {
    let board = scoreboard(root).await?;
    let baseline_path = super::ts_dir(root).join("bench-baseline.json");
    let baseline: Vec<ModelStats> = std::fs::read_to_string(&baseline_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let drift = detect_drift(&baseline, &board, 0.1, 0.5);

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "scoreboard": board,
                "drift": drift,
            }))?
        );
    } else if board.is_empty() {
        println!("bench: no runs recorded yet");
    } else {
        println!("bench: per-model scoreboard");
        for s in &board {
            println!(
                "  {}  runs={} pass={:.0}% avg={} prompt_tok={} completion_tok={}",
                s.model,
                s.runs,
                s.pass_rate * 100.0,
                s.avg_elapsed_ms
                    .map(|m| format!("{m:.0}ms"))
                    .unwrap_or_else(|| "-".to_string()),
                s.total_prompt_tokens,
                s.total_completion_tokens,
            );
        }
        for d in &drift {
            println!(
                "  ⚠ drift: {} {} {:.3} → {:.3}",
                d.model, d.metric, d.baseline, d.current
            );
        }
    }

    if save_baseline {
        std::fs::create_dir_all(super::ts_dir(root))?;
        std::fs::write(&baseline_path, serde_json::to_string_pretty(&board)?)?;
        if !json {
            println!("bench: saved baseline to {}", baseline_path.display());
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(model: &str, passed: bool, ms: i64, p: i64, c: i64) -> RunRow {
        (
            Some(model.to_string()),
            passed as i64,
            Some(ms),
            Some(p),
            Some(c),
        )
    }

    #[test]
    fn aggregate_folds_per_model_pass_rate_latency_and_tokens() {
        let rows = vec![
            row("gpt-a", true, 100, 10, 5),
            row("gpt-a", false, 300, 20, 5),
            row("gpt-b", true, 50, 0, 0),
            (None, 1, None, None, None), // an LLM-free run
        ];
        let board = aggregate(&rows);
        // Sorted by model: "(none)", "gpt-a", "gpt-b".
        assert_eq!(board[0].model, "(none)");
        let a = board.iter().find(|s| s.model == "gpt-a").unwrap();
        assert_eq!(a.runs, 2);
        assert_eq!(a.passed, 1);
        assert!((a.pass_rate - 0.5).abs() < 1e-9);
        assert_eq!(a.avg_elapsed_ms, Some(200.0));
        assert_eq!(a.total_prompt_tokens, 30);
        assert_eq!(a.total_completion_tokens, 10);
        assert_eq!(a.total_tokens(), 40);
        // The model-less run is still counted, with no token/latency data.
        let none = board.iter().find(|s| s.model == "(none)").unwrap();
        assert_eq!(none.runs, 1);
        assert_eq!(none.avg_elapsed_ms, None);
    }

    #[test]
    fn drift_flags_pass_rate_and_latency_moves_but_not_new_models() {
        let baseline = vec![ModelStats {
            model: "m".into(),
            runs: 10,
            passed: 9,
            pass_rate: 0.9,
            avg_elapsed_ms: Some(100.0),
            total_prompt_tokens: 0,
            total_completion_tokens: 0,
        }];
        let current = vec![
            ModelStats {
                model: "m".into(),
                runs: 10,
                passed: 5,
                pass_rate: 0.5,              // dropped 0.4 > 0.1 → drift
                avg_elapsed_ms: Some(300.0), // +200% > 50% → drift
                total_prompt_tokens: 0,
                total_completion_tokens: 0,
            },
            ModelStats {
                model: "brand-new".into(), // not in baseline → not drift
                runs: 1,
                passed: 1,
                pass_rate: 1.0,
                avg_elapsed_ms: Some(10.0),
                total_prompt_tokens: 0,
                total_completion_tokens: 0,
            },
        ];
        let drift = detect_drift(&baseline, &current, 0.1, 0.5);
        assert_eq!(drift.len(), 2);
        assert!(drift.iter().any(|d| d.metric == "pass_rate"));
        assert!(drift.iter().any(|d| d.metric == "avg_elapsed_ms"));
        assert!(drift.iter().all(|d| d.model == "m"));
    }

    #[tokio::test]
    async fn scoreboard_reads_recorded_run_telemetry() {
        let root = crate::local::tmp_root();
        let id = crate::local::store::add_value(
            &root,
            serde_json::json!({"id":"t1","title":"x","kind":"command","code":"true"}),
        )
        .await
        .unwrap();
        let meta = crate::local::store::RunMeta {
            elapsed_ms: Some(42),
            model: Some("gpt-test".to_string()),
            prompt_tokens: Some(100),
            completion_tokens: Some(20),
        };
        let ok = crate::server::executors::Outcome::pass("code".to_string());
        crate::local::store::write_result_with_meta(
            &root,
            &id,
            &ok,
            None,
            crate::server::executors::TestKind::Command,
            &meta,
        )
        .await
        .unwrap();

        let board = scoreboard(&root).await.unwrap();
        let s = board.iter().find(|s| s.model == "gpt-test").unwrap();
        assert_eq!(s.runs, 1);
        assert_eq!(s.passed, 1);
        assert_eq!(s.avg_elapsed_ms, Some(42.0));
        assert_eq!(s.total_tokens(), 120);

        std::fs::remove_dir_all(root).ok();
    }
}
