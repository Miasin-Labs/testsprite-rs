//! Failure triage — group FAILING tests by root cause (`failureKind`) into a
//! handful of clusters instead of N separate failures, so an agent fixes the
//! few underlying problems rather than symptom-by-symptom.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Serialize;

use super::store;

/// One root-cause cluster of failing tests.
#[derive(Debug, Clone, Serialize)]
pub struct Cluster {
    pub failure_kind: String,
    pub count: usize,
    pub test_ids: Vec<String>,
    pub sample_cause: Option<String>,
    /// Highest per-case divergence score in the cluster (most-actionable case).
    pub divergence_max: f64,
    /// Mean divergence over the cluster's cases.
    pub divergence_mean: f64,
}

/// How INFORMATIVE (debuggable) a failure is, in `[0,1]` — higher = more
/// localized and actionable. A sharp "expected 200, got 404" points straight
/// at the fix; a diffuse 40-line "everything differs" blob does not. Ranking
/// by this surfaces the failures a human can act on fastest first, which is a
/// better triage order than raw count. Pure and deterministic.
pub fn divergence_score(error: &str, failure_kind: Option<&str>) -> f64 {
    if error.trim().is_empty() {
        // A kind with no error text still carries the kind's inherent sharpness.
        return kind_sharpness(failure_kind) * 0.4;
    }
    let lower = error.to_lowercase();

    // A concrete "expected X, got Y" (or "got <status>") is maximally sharp.
    let concrete = (lower.contains("expected") && lower.contains("got"))
        || lower.contains("got 40")
        || lower.contains("got 50");
    let concrete_score = if concrete { 0.5 } else { 0.0 };

    // Short, single-line errors are more localized than long multi-line dumps.
    let lines = error.lines().filter(|l| !l.trim().is_empty()).count();
    let brevity = if lines <= 1 {
        0.3
    } else if lines <= 4 {
        0.2
    } else if lines <= 12 {
        0.1
    } else {
        0.0
    };

    // Inherent sharpness of the kind.
    let kind = kind_sharpness(failure_kind) * 0.2;

    (concrete_score + brevity + kind).clamp(0.0, 1.0)
}

/// Per-kind inherent sharpness in `[0,1]`: some kinds pin the fix location
/// (a 404 names a missing route), others are inherently diffuse (unknown).
fn kind_sharpness(kind: Option<&str>) -> f64 {
    match kind.unwrap_or("unknown") {
        "routing_404" | "assertion" | "auth" | "build_error" => 1.0,
        "suspect_oracle" | "residual_alignment" | "network_timeout" => 0.7,
        "timeout" | "browser_crash" => 0.5,
        "network" | "infra" | "dependency" => 0.3,
        _ => 0.1,
    }
}

/// Group every currently-failing test (by its latest run) into clusters by
/// `failureKind`, sorted most-DEBUGGABLE-first: by max divergence, then mean,
/// then count, then kind. Ranking by informativeness (not raw count) surfaces
/// the root cause a human can act on fastest.
pub async fn triage(root: &Path) -> anyhow::Result<Vec<Cluster>> {
    let results = store::latest_results(root).await?;

    // failure_kind -> (test_ids, sample_cause, divergence scores)
    let mut groups: BTreeMap<String, (Vec<String>, Option<String>, Vec<f64>)> = BTreeMap::new();
    for r in results {
        let passed = r
            .get("passed")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        if passed {
            continue;
        }
        let id = r
            .get("id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        let kind = r
            .get("failureKind")
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or("unknown")
            .to_string();
        let cause = r
            .get("cause")
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let error = r
            .get("error")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let score = divergence_score(error, Some(&kind));

        let entry = groups
            .entry(kind)
            .or_insert_with(|| (Vec::new(), None, Vec::new()));
        entry.0.push(id);
        if entry.1.is_none() {
            entry.1 = cause;
        }
        entry.2.push(score);
    }

    let mut clusters: Vec<Cluster> = groups
        .into_iter()
        .map(|(failure_kind, (test_ids, sample_cause, scores))| {
            let divergence_max = scores.iter().cloned().fold(0.0_f64, f64::max);
            let divergence_mean = if scores.is_empty() {
                0.0
            } else {
                scores.iter().sum::<f64>() / scores.len() as f64
            };
            Cluster {
                count: test_ids.len(),
                failure_kind,
                test_ids,
                sample_cause,
                divergence_max,
                divergence_mean,
            }
        })
        .collect();
    clusters.sort_by(|a, b| {
        b.divergence_max
            .partial_cmp(&a.divergence_max)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                b.divergence_mean
                    .partial_cmp(&a.divergence_mean)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| b.count.cmp(&a.count))
            .then_with(|| a.failure_kind.cmp(&b.failure_kind))
    });

    Ok(clusters)
}

/// Compute [`triage`] and print a human report or one JSON object; returns
/// exit code 0 (a report, not a pass/fail gate).
pub async fn triage_report(root: &Path, json: bool) -> anyhow::Result<i32> {
    let clusters = triage(root).await?;
    let failing: usize = clusters.iter().map(|c| c.count).sum();

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "clusters": clusters,
                "failing": failing,
            }))?
        );
        return Ok(0);
    }

    if clusters.is_empty() {
        println!("triage: no failing tests");
        return Ok(0);
    }

    println!(
        "triage: {} failing test(s) in {} cluster(s) — fix root causes first:",
        failing,
        clusters.len()
    );
    for c in &clusters {
        let shown: Vec<&str> = c.test_ids.iter().take(8).map(String::as_str).collect();
        let mut ids = shown.join(", ");
        if c.test_ids.len() > 8 {
            ids.push_str(&format!(", +{} more", c.test_ids.len() - 8));
        }
        println!(
            "  [{}] x{}  (divergence {:.2})  {}",
            c.failure_kind, c.count, c.divergence_max, ids
        );
        if let Some(cause) = &c.sample_cause {
            let truncated: String = cause.chars().take(140).collect();
            println!("      likely: {truncated}");
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::executors::{Outcome, TestKind};

    #[tokio::test]
    async fn groups_failures_by_root_cause() {
        let root = crate::local::tmp_root();

        for (id, title) in [("t1", "T1"), ("t2", "T2"), ("t3", "T3")] {
            store::add_value(&root, serde_json::json!({"id": id, "title": title}))
                .await
                .unwrap();
        }

        // t1, t2 -> routing_404; t3 -> assertion.
        store::write_result(
            &root,
            "t1",
            &Outcome::fail("expected Some(200), got 404", String::new()),
            None,
            TestKind::Backend,
        )
        .await
        .unwrap();
        store::write_result(
            &root,
            "t2",
            &Outcome::fail("expected Some(200), got 404", String::new()),
            None,
            TestKind::Backend,
        )
        .await
        .unwrap();
        store::write_result(
            &root,
            "t3",
            &Outcome::fail("AssertionError: 1 != 2", String::new()),
            None,
            TestKind::Backend,
        )
        .await
        .unwrap();

        let clusters = triage(&root).await.unwrap();
        assert_eq!(clusters.len(), 2);

        let routing = clusters
            .iter()
            .find(|c| c.failure_kind == "routing_404")
            .unwrap();
        assert_eq!(routing.count, 2);
        let mut ids = routing.test_ids.clone();
        ids.sort();
        assert_eq!(ids, vec!["t1".to_string(), "t2".to_string()]);

        let assertion = clusters
            .iter()
            .find(|c| c.failure_kind == "assertion")
            .unwrap();
        assert_eq!(assertion.count, 1);
        assert_eq!(assertion.test_ids, vec!["t3".to_string()]);

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn divergence_scores_sharp_failures_above_diffuse_ones() {
        // A concrete "expected/got" outscores a long diffuse blob.
        let sharp = divergence_score("expected a 2xx response, got 404", Some("routing_404"));
        let diffuse = divergence_score(&"line\n".repeat(30), Some("unknown"));
        assert!(sharp > diffuse, "sharp {sharp} vs diffuse {diffuse}");
        // A sharp kind beats a diffuse kind at equal error text.
        assert!(divergence_score("", Some("routing_404")) > divergence_score("", Some("network")));
        // Scores stay bounded.
        for (e, k) in [
            ("expected x got y", Some("assertion")),
            ("", None),
            ("z", Some("x")),
        ] {
            let s = divergence_score(e, k);
            assert!((0.0..=1.0).contains(&s), "{s}");
        }
    }

    #[tokio::test]
    async fn triage_orders_clusters_by_divergence_not_just_count() {
        let root = crate::local::tmp_root();
        // Two diffuse network failures vs one sharp routing_404. Count would
        // rank network first; divergence must rank the actionable 404 first.
        for id in ["n1", "n2", "r1"] {
            store::add_value(&root, serde_json::json!({"id": id, "title": id}))
                .await
                .unwrap();
        }
        for id in ["n1", "n2"] {
            store::write_result(
                &root,
                id,
                &Outcome::fail(
                    "request failed: error sending request: connection refused",
                    String::new(),
                ),
                None,
                TestKind::Backend,
            )
            .await
            .unwrap();
        }
        store::write_result(
            &root,
            "r1",
            &Outcome::fail("expected a 2xx response, got 404", String::new()),
            None,
            TestKind::Backend,
        )
        .await
        .unwrap();

        let clusters = triage(&root).await.unwrap();
        assert_eq!(clusters[0].failure_kind, "routing_404", "{clusters:?}");
        assert!(clusters[0].divergence_max > clusters[1].divergence_max);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn no_failures_yields_no_clusters() {
        let root = crate::local::tmp_root();
        store::add_value(&root, serde_json::json!({"id": "ok1", "title": "OK"}))
            .await
            .unwrap();
        store::write_result(
            &root,
            "ok1",
            &Outcome::pass(String::new()),
            None,
            TestKind::Backend,
        )
        .await
        .unwrap();

        let clusters = triage(&root).await.unwrap();
        assert!(clusters.is_empty());

        std::fs::remove_dir_all(&root).unwrap();
    }
}
