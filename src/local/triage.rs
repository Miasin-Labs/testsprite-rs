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
}

/// Group every currently-failing test (by its latest run) into clusters by
/// `failureKind`, sorted worst-first (count desc, then failure_kind).
pub async fn triage(root: &Path) -> anyhow::Result<Vec<Cluster>> {
    let results = store::latest_results(root).await?;

    // failure_kind -> (test_ids, sample_cause)
    let mut groups: BTreeMap<String, (Vec<String>, Option<String>)> = BTreeMap::new();
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

        let entry = groups.entry(kind).or_insert_with(|| (Vec::new(), None));
        entry.0.push(id);
        if entry.1.is_none() {
            entry.1 = cause;
        }
    }

    let mut clusters: Vec<Cluster> = groups
        .into_iter()
        .map(|(failure_kind, (test_ids, sample_cause))| Cluster {
            count: test_ids.len(),
            failure_kind,
            test_ids,
            sample_cause,
        })
        .collect();
    clusters.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
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
        println!("  [{}] x{}  {}", c.failure_kind, c.count, ids);
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
